//! Agent presence for reads inside a docli Desktop agent session.
//!
//! Desktop starts Claude and Codex with a loopback gateway — the agent's docli MCP connection — and
//! puts its address and handle in the environment. A read through that gateway shows the user, on
//! the note itself, that the agent is reading it. `docli read` answers from the local mirror and
//! never reaches the server, so inside Desktop the agent read invisibly. After serving a note, the
//! CLI now tells the gateway which notes it read, as one ordinary `read_notes` call per workspace.
//!
//! Best-effort and bounded: only inside Desktop (both variables set), only for notes, after the
//! note is printed, with a short timeout and every error ignored. Outside Desktop nothing changes —
//! `docli read` stays offline. The handle is Desktop's per-session loopback credential, inherited,
//! never stored.

use std::time::Duration;

use uuid::Uuid;

const URL_ENV: &str = "DOCLI_DESKTOP_BRIDGE_URL";
const TOKEN_ENV: &str = "DOCLI_DESKTOP_BRIDGE_TOKEN";
const TIMEOUT: Duration = Duration::from_secs(2);

/// One served note: its id and its workspace.
pub type NoteRead = (Uuid, Uuid);

/// The gateway calls to make: one `read_notes` per workspace, its ids in read order. Empty outside
/// Desktop or when nothing was read.
pub fn calls(
    url: Option<&str>,
    token: Option<&str>,
    reads: &[NoteRead],
) -> Vec<(String, String, serde_json::Value)> {
    let (Some(url), Some(token)) = (
        url.filter(|u| !u.is_empty()),
        token.filter(|t| !t.is_empty()),
    ) else {
        return Vec::new();
    };
    let mut workspaces: Vec<(Uuid, Vec<String>)> = Vec::new();
    for (note, workspace) in reads {
        match workspaces.iter_mut().find(|(w, _)| w == workspace) {
            Some((_, ids)) => ids.push(note.to_string()),
            None => workspaces.push((*workspace, vec![note.to_string()])),
        }
    }
    workspaces
        .into_iter()
        .map(|(workspace, ids)| {
            let body = serde_json::json!({
                "jsonrpc": "2.0",
                "id": "docli-cli-read",
                "method": "tools/call",
                "params": {
                    "name": "read_notes",
                    "arguments": { "ids": ids, "workspace": workspace.to_string() },
                },
            });
            (url.to_owned(), token.to_owned(), body)
        })
        .collect()
}

/// Tell the Desktop gateway about the notes this command served. Silent, whatever happens.
pub fn report(reads: &[NoteRead]) {
    send(calls(
        std::env::var(URL_ENV).ok().as_deref(),
        std::env::var(TOKEN_ENV).ok().as_deref(),
        reads,
    ));
}

fn send(calls: Vec<(String, String, serde_json::Value)>) {
    if calls.is_empty() {
        return;
    }
    let Ok(client) = reqwest::blocking::Client::builder()
        .timeout(TIMEOUT)
        .build()
    else {
        return;
    };
    for (url, token, body) in calls {
        let _ = client.post(url).bearer_auth(token).json(&body).send();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_is_sent_outside_a_desktop_session() {
        let read = [(Uuid::new_v4(), Uuid::new_v4())];
        assert!(calls(None, Some("t"), &read).is_empty());
        assert!(calls(Some("http://127.0.0.1:1/mcp"), None, &read).is_empty());
        assert!(calls(Some(""), Some("t"), &read).is_empty());
        assert!(calls(Some("http://127.0.0.1:1/mcp"), Some("t"), &[]).is_empty());
    }

    #[test]
    fn the_request_reaches_the_gateway_with_its_handle_and_a_loopback_host() {
        use std::io::{Read, Write};
        // The gateway refuses a request without its bearer, or naming another host.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut request = Vec::new();
            let mut buffer = [0u8; 4096];
            while let Ok(n @ 1..) = stream.read(&mut buffer) {
                request.extend_from_slice(&buffer[..n]);
                let text = String::from_utf8_lossy(&request);
                if let Some(end) = text.find("\r\n\r\n") {
                    let length = text[..end]
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse::<usize>().unwrap())
                        })
                        .unwrap_or(0);
                    if request.len() >= end + 4 + length {
                        break;
                    }
                }
            }
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\n{}");
            String::from_utf8(request).unwrap()
        });
        let (note, workspace) = (Uuid::new_v4(), Uuid::new_v4());
        send(calls(
            Some(&format!("http://127.0.0.1:{port}/mcp")),
            Some("handle"),
            &[(note, workspace)],
        ));
        let request = server.join().unwrap();
        assert!(request.starts_with("POST /mcp "), "{request}");
        let lower = request.to_ascii_lowercase();
        assert!(
            lower.contains(&format!("host: 127.0.0.1:{port}")),
            "{request}"
        );
        assert!(lower.contains("authorization: bearer handle"), "{request}");
        assert!(request.contains(&note.to_string()), "{request}");
    }

    #[test]
    fn one_read_notes_call_per_workspace_in_read_order() {
        let (w1, w2) = (Uuid::new_v4(), Uuid::new_v4());
        let (a, b, c) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let sent = calls(
            Some("http://127.0.0.1:43123/mcp"),
            Some("handle"),
            &[(a, w1), (b, w2), (c, w1)],
        );
        assert_eq!(sent.len(), 2);
        let (url, token, body) = &sent[0];
        assert_eq!(url, "http://127.0.0.1:43123/mcp");
        assert_eq!(token, "handle");
        assert_eq!(body["method"], "tools/call");
        assert_eq!(body["params"]["name"], "read_notes");
        assert_eq!(body["params"]["arguments"]["workspace"], w1.to_string());
        assert_eq!(
            body["params"]["arguments"]["ids"],
            serde_json::json!([a.to_string(), c.to_string()])
        );
        assert_eq!(
            sent[1].2["params"]["arguments"]["ids"],
            serde_json::json!([b.to_string()])
        );
    }
}
