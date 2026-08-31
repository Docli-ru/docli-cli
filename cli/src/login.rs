// SPDX-FileCopyrightText: 2026 OOO Agitek
// SPDX-License-Identifier: MIT

//! `docli login` (v0.28.0 D4) — loopback PKCE (RFC 8252 §7.3) against the docli authorization
//! server, as the preregistered first-party `docli-cli` client, yielding a device-class
//! sync-plane credential. No persona: a device grant acts as the OWNER, and migration `0045`'s
//! `oauth_grant_class_shape_ck` forbids a persona on it — a pin that holds ONLY while the cache
//! is read-only (the two decisions pin each other; D10.1).
//!
//! The authorize request sends **`resource=<origin>/api/sync` EXPLICITLY** — an absent
//! `resource` defaults to the bare MCP connection (v0.25.11) and the audience fence would then
//! reject every sync call.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;

use anyhow::{bail, Context, Result};
use base64::Engine;
use rand::RngCore;
use sha2::{Digest, Sha256};

use crate::creds::{CredsStore, ServerCreds};
use crate::http::CLI_CLIENT_ID;

fn b64url(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn urlencode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

pub struct AuthorizeRound {
    pub url: String,
    pub redirect_uri: String,
    pub state: String,
    pub verifier: String,
}

/// Build the authorize URL for a loopback `redirect_uri` on `port`.
pub fn authorize_round(server: &str, port: u16, install_id: &str) -> AuthorizeRound {
    let mut vbytes = [0u8; 48];
    rand::thread_rng().fill_bytes(&mut vbytes);
    let verifier = b64url(&vbytes);
    let challenge = b64url(&Sha256::digest(verifier.as_bytes()));
    let mut sbytes = [0u8; 24];
    rand::thread_rng().fill_bytes(&mut sbytes);
    let state = b64url(&sbytes);
    // The IP-literal form (RFC 8252 §7.3 recommends it; both spellings are registered).
    let redirect_uri = format!("http://127.0.0.1:{port}/callback");
    let url = format!(
        "{server}/api/oauth/authorize?response_type=code&client_id={}&redirect_uri={}&state={}\
         &code_challenge={}&code_challenge_method=S256&scope=sync&resource={}&install_id={}",
        urlencode(CLI_CLIENT_ID),
        urlencode(&redirect_uri),
        urlencode(&state),
        urlencode(&challenge),
        urlencode(&format!("{server}/api/sync")),
        urlencode(install_id),
    );
    AuthorizeRound {
        url,
        redirect_uri,
        state,
        verifier,
    }
}

/// Parse the loopback callback's request line → (code, state). Refuses anything but
/// `GET /callback?...`.
pub fn parse_callback(request_line: &str) -> Result<(String, String)> {
    let path = request_line
        .split_whitespace()
        .nth(1)
        .context("malformed callback request")?;
    let Some(query) = path.strip_prefix("/callback?") else {
        bail!("unexpected callback path: {path}");
    };
    let mut code = None;
    let mut state = None;
    let mut error = None;
    for pair in query.split('&') {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        let v = percent_decode(v);
        match k {
            "code" => code = Some(v),
            "state" => state = Some(v),
            "error" => error = Some(v),
            _ => {}
        }
    }
    if let Some(e) = error {
        bail!("the authorization was refused: {e}");
    }
    Ok((
        code.context("no code in the callback")?,
        state.context("no state in the callback")?,
    ))
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            // The bounds live in the two `get`s below — both hex digits or a literal `%`.
            if let (Some(a), Some(b)) = (
                bytes.get(i + 1).and_then(|c| (*c as char).to_digit(16)),
                bytes.get(i + 2).and_then(|c| (*c as char).to_digit(16)),
            ) {
                out.push((a * 16 + b) as u8);
                i += 3;
                continue;
            }
        }
        if bytes[i] == b'+' {
            out.push(b' ');
        } else {
            out.push(bytes[i]);
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn open_browser(url: &str) {
    #[cfg(target_os = "macos")]
    let cmd = std::process::Command::new("open").arg(url).spawn();
    #[cfg(target_os = "windows")]
    // Shell-free (Codex round 1): `cmd /C start` parses the URL's `&` as command separators,
    // truncating every authorize URL at the first parameter. `explorer.exe <url>` hands the
    // string to ShellExecute untouched.
    let cmd = std::process::Command::new("explorer").arg(url).spawn();
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let cmd = std::process::Command::new("xdg-open").arg(url).spawn();
    if cmd.is_err() {
        // The printed URL below is the fallback path.
    }
}

pub fn run_login(server: &str, creds: &CredsStore) -> Result<()> {
    let listener =
        TcpListener::bind("127.0.0.1:0").context("binding the loopback callback port")?;
    let port = listener.local_addr()?.port();
    let install_id = creds.install_id(server)?;
    let round = authorize_round(server, port, &install_id);

    println!("Открываю браузер для входа в docli…");
    println!(
        "Если браузер не открылся, откройте эту ссылку вручную:\n\n  {}\n",
        round.url
    );
    open_browser(&round.url);

    // Serve until the CALLBACK arrives — browsers pre-connect, probe /favicon.ico, and send
    // other strays on the same port; treating the first connection as the callback would fail a
    // perfectly good login round.
    let outcome = loop {
        let (mut stream, _) = listener
            .accept()
            .context("waiting for the browser callback")?;
        // A speculative pre-connect that never sends bytes must not wedge the round (Codex
        // round 2): bound the read and keep listening on timeout/EOF.
        let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(10)));
        let mut reader = BufReader::new(stream.try_clone()?);
        let mut request_line = String::new();
        if reader.read_line(&mut request_line).is_err() {
            continue;
        }
        let line = request_line.trim_end().to_string();
        let path = line.split_whitespace().nth(1).unwrap_or("");
        // EXACTLY the callback endpoint — `/callback-probe` must not end the round as an error.
        if path != "/callback" && !path.starts_with("/callback?") {
            // A pre-connect (zero bytes) or a stray probe: answer and keep listening.
            let _ = write!(
                stream,
                "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
            continue;
        }
        let outcome = parse_callback(&line);
        let body = match &outcome {
            Ok(_) => "<html><body><p>docli подключён — можно закрыть это окно.</p></body></html>",
            Err(_) => "<html><body><p>Вход не удался — вернитесь в терминал.</p></body></html>",
        };
        let _ = write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        break outcome;
    };
    let (code, cb_state) = outcome?;
    if cb_state != round.state {
        bail!("state mismatch in the callback — refusing the code");
    }

    // Exchange the code.
    let http = reqwest::blocking::Client::new();
    let resp = http
        .post(format!("{server}/api/oauth/token"))
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", &code),
            ("redirect_uri", &round.redirect_uri),
            ("client_id", CLI_CLIENT_ID),
            ("code_verifier", &round.verifier),
        ])
        .send()
        .context("redeeming the authorization code")?;
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().unwrap_or_default();
        bail!("the token exchange failed ({status}): {text}");
    }
    #[derive(serde::Deserialize)]
    struct TokenResp {
        access_token: String,
        refresh_token: String,
        expires_in: i64,
    }
    let t: TokenResp = resp.json().context("parsing the token response")?;
    creds.put(
        server,
        ServerCreds {
            access_token: t.access_token,
            refresh_token: t.refresh_token,
            expires_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0)
                + t.expires_in,
            install_id,
        },
    )?;
    println!("Вход выполнен — устройство подключено к {server}.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authorize_url_carries_the_sync_resource_and_pkce() {
        let r = authorize_round("https://docli.ru", 49213, "inst-1");
        assert!(r.url.contains("client_id=docli-cli"), "{}", r.url);
        assert!(
            r.url
                .contains("redirect_uri=http%3A%2F%2F127.0.0.1%3A49213%2Fcallback"),
            "{}",
            r.url
        );
        // The load-bearing parameter: without it the token lands on the MCP plane and the sync
        // fence rejects everything (v0.25.11's default).
        assert!(
            r.url
                .contains("resource=https%3A%2F%2Fdocli.ru%2Fapi%2Fsync"),
            "{}",
            r.url
        );
        assert!(r.url.contains("code_challenge_method=S256"), "{}", r.url);
        assert!(r.url.contains("scope=sync"), "{}", r.url);
        assert!(r.url.contains("install_id=inst-1"), "{}", r.url);
        // The challenge is the S256 of the verifier.
        let expect = b64url(&Sha256::digest(r.verifier.as_bytes()));
        assert!(r.url.contains(&format!("code_challenge={expect}")));
    }

    #[test]
    fn callback_parsing_extracts_code_and_state_and_refuses_errors() {
        let (code, state) =
            parse_callback("GET /callback?code=abc%2B1&state=xyz HTTP/1.1").unwrap();
        assert_eq!(code, "abc+1");
        assert_eq!(state, "xyz");
        assert!(parse_callback("GET /favicon.ico HTTP/1.1").is_err());
        let err = parse_callback("GET /callback?error=access_denied&state=x HTTP/1.1")
            .unwrap_err()
            .to_string();
        assert!(err.contains("access_denied"), "{err}");
    }
}
