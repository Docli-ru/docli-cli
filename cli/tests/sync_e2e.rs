//! End-to-end tests for the sync orchestrator (v0.28.0) against a SCRIPTED stub server — the
//! whole client loop (bootstrap → pages → head + count → prune, invalidators, `--check`, the
//! rollback detector) without a live api. The server half of the same contract is pinned by
//! `apps/api/tests/sync_test.rs`'s v0.28.0 section; the wire shapes are ONE crate, so the two
//! suites cannot drift on the field level.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use docli_cli::config::{DocliToml, Mount, Project};
use docli_cli::creds::{CredsStore, ServerCreds};
use docli_cli::http::Api;
use docli_cli::state::ControlRoot;
use docli_cli::sync_cmd::{self, SyncOptions};
use serde_json::{json, Value};
use uuid::Uuid;

const WS: Uuid = Uuid::from_u128(0xAA);

// ---- the stub server ------------------------------------------------------------------------

type Handler = Arc<dyn Fn(&str, &Value) -> (u16, Value) + Send + Sync>;

fn spawn_stub(handler: Handler) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let handler = handler.clone();
            std::thread::spawn(move || {
                loop {
                    // Minimal HTTP/1.1: headers to \r\n\r\n, then content-length bytes.
                    let mut buf = Vec::new();
                    let mut byte = [0u8; 1];
                    while !buf.ends_with(b"\r\n\r\n") {
                        match stream.read(&mut byte) {
                            Ok(1) => buf.push(byte[0]),
                            _ => return,
                        }
                    }
                    let head = String::from_utf8_lossy(&buf).to_string();
                    let path = head
                        .lines()
                        .next()
                        .and_then(|l| l.split_whitespace().nth(1))
                        .unwrap_or("/")
                        .to_string();
                    let len: usize = head
                        .lines()
                        .find_map(|l| {
                            let (k, v) = l.split_once(':')?;
                            k.eq_ignore_ascii_case("content-length")
                                .then(|| v.trim().parse().ok())?
                        })
                        .unwrap_or(0);
                    let mut body = vec![0u8; len];
                    if len > 0 && stream.read_exact(&mut body).is_err() {
                        return;
                    }
                    let parsed: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                    let (status, resp) = handler(&path, &parsed);
                    let resp_bytes = resp.to_string().into_bytes();
                    let _ = write!(
                        stream,
                        "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n",
                        resp_bytes.len()
                    );
                    let _ = stream.write_all(&resp_bytes);
                    let _ = stream.flush();
                }
            });
        }
    });
    format!("http://127.0.0.1:{}", addr.port())
}

// ---- fixtures -------------------------------------------------------------------------------

fn wire_node(id: u128, kind: &str, path: &str, rev: i64, body: Option<&str>) -> Value {
    json!({
        "id": Uuid::from_u128(id).to_string(),
        "parentId": null,
        "kind": kind,
        "name": path.rsplit('/').next().unwrap(),
        "path": path,
        "rev": rev,
        "trashed": false,
        "mime": if kind == "attachment" { json!("image/png") } else { Value::Null },
        "contentBytes": body.map(|b| b.len()).unwrap_or(0),
        "body": body,
        "blobUrl": if kind == "attachment" { json!("/api/attachments/x") } else { Value::Null },
    })
}

fn page(epoch: i64, nodes: Vec<Value>, live: Option<i64>) -> Value {
    let cursor = nodes
        .last()
        .map(|n| json!({"rev": n["rev"], "id": n["id"]}))
        .unwrap_or(json!({"rev": 0, "id": Uuid::nil().to_string()}));
    let mut out = json!({
        "epoch": epoch,
        "cursor": cursor,
        "nodes": nodes,
        "resyncRequired": false,
        "lastMutationId": 0,
    });
    if let Some(l) = live {
        out["liveNodes"] = json!(l);
    }
    // Head-reaching iff nodes.len() < the client's limit — the stub scripts that by count.
    out
}

struct Fx {
    _tmp: tempfile::TempDir,
    project: Project,
    mirror: PathBuf,
}

fn fx() -> Fx {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();
    let control_dir = root.join(".docli");
    let mirror = root.join("mirror");
    let project = Project {
        root,
        config: DocliToml {
            server: "http://stub".into(), // replaced per test via Api
            mounts: vec![Mount {
                workspace: WS,
                dir: "mirror".into(),
                folder: None,
                name: Some("тест".into()),
                derived_dir: false,
                workspace_label: String::new(),
            }],
            mcp_label: None,
        },
        control: control_dir.clone(),
    };
    Fx {
        _tmp: tmp,
        project,
        mirror,
    }
}

/// Wall-clock unix seconds, the same value `sync_cmd` stamps with (that one is crate-private).
fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn api_for(fx_root: &Path, server: &str) -> Api {
    let creds = CredsStore::open(fx_root.join("home/.docli")).unwrap();
    creds
        .put(
            server,
            ServerCreds {
                access_token: "docli_pat_test".into(),
                refresh_token: "r".into(),
                expires_at: i64::MAX / 2,
                install_id: "i".into(),
            },
        )
        .unwrap();
    Api::new(server, creds).unwrap()
}

fn run_sync(fx: &mut Fx, server: &str, opts: SyncOptions) -> anyhow::Result<i32> {
    fx.project.config.server = server.to_string();
    let api = api_for(&fx.project.root, server);
    sync_cmd::run(&fx.project, &api, &opts)
}

fn sync(fx: &mut Fx, server: &str) -> i32 {
    run_sync(
        fx,
        server,
        SyncOptions {
            check: false,
            full: false,
        },
    )
    .unwrap()
}

fn sync_full(fx: &mut Fx, server: &str) -> i32 {
    run_sync(
        fx,
        server,
        SyncOptions {
            check: false,
            full: true,
        },
    )
    .unwrap()
}

fn check(fx: &mut Fx, server: &str) -> i32 {
    run_sync(
        fx,
        server,
        SyncOptions {
            check: true,
            full: false,
        },
    )
    .unwrap()
}

// ---- tests ----------------------------------------------------------------------------------

/// A scripted server whose tree can be swapped between syncs. `bootstrap` serves the whole tree
/// in one head-reaching page; `pull` serves the delta above the request cursor.
fn tree_server(tree: Arc<Mutex<BTreeMap<u128, Value>>>) -> Handler {
    Arc::new(move |path: &str, body: &Value| -> (u16, Value) {
        let tree = tree.lock().unwrap();
        let nodes: Vec<Value> = tree.values().cloned().collect();
        let live = nodes.iter().filter(|n| n["trashed"] != json!(true)).count() as i64;
        match path {
            "/api/sync/bootstrap" => (200, page(1, nodes, Some(live))),
            "/api/sync/pull" => {
                assert_eq!(
                    body["ephemeral"],
                    json!(true),
                    "every CLI pull is ephemeral"
                );
                let req_cursor = body["cursor"].clone();
                let cur_rev = body["cursor"]["rev"].as_i64().unwrap();
                let cur_id = body["cursor"]["id"].as_str().unwrap().to_string();
                let delta: Vec<Value> = nodes
                    .iter()
                    .filter(|n| {
                        let r = n["rev"].as_i64().unwrap();
                        r > cur_rev || (r == cur_rev && n["id"].as_str().unwrap() > cur_id.as_str())
                    })
                    .cloned()
                    .collect();
                let mut resp = page(1, delta, Some(live));
                // The WIRE CONTRACT: an empty page leaves the cursor where it was (the real
                // server echoes the request cursor) — a stub that resets it to (0, nil) makes
                // the client legitimately regress and re-pull, masking staleness semantics.
                if resp["nodes"].as_array().unwrap().is_empty() {
                    resp["cursor"] = req_cursor;
                }
                (200, resp)
            }
            other => panic!("unexpected path {other}"),
        }
    })
}

#[test]
fn first_sync_mirrors_the_tree_and_check_reports_fresh() {
    let mut f = fx();
    let tree: Arc<Mutex<BTreeMap<u128, Value>>> = Arc::new(Mutex::new(BTreeMap::from([
        (1, wire_node(1, "folder", "docs", 1, None)),
        (2, wire_node(2, "file", "docs/a.md", 2, Some("# привет"))),
        (3, wire_node(3, "attachment", "docs/pic.png", 3, None)),
    ])));
    let server = spawn_stub(tree_server(tree.clone()));

    assert_eq!(sync(&mut f, &server), 0);
    assert_eq!(
        std::fs::read_to_string(f.mirror.join("docs/a.md")).unwrap(),
        "# привет"
    );
    assert!(f.mirror.join("docs/pic.png.docli").exists());
    assert!(
        !f.mirror.join("docs/pic.png").exists(),
        "markers, never bytes"
    );
    assert!(
        !f.mirror.join("CACHE_INCOMPLETE.docli").exists(),
        "complete after reaching head"
    );
    assert_eq!(check(&mut f, &server), 0, "fresh right after a sync");

    // A remote rename lands as a MOVE next sync.
    tree.lock()
        .unwrap()
        .insert(2, wire_node(2, "file", "docs/b.md", 4, Some("# привет")));
    assert_eq!(sync(&mut f, &server), 0);
    assert!(f.mirror.join("docs/b.md").exists());
    assert!(!f.mirror.join("docs/a.md").exists());
}

/// v0.29.1 Half 2 — the graph rides the head-reaching page of a run that ASKED, lands in the
/// control root stamped with that page's position, and is then what `docli read` answers from.
///
/// Also pinned here, because it is the whole reason the flag is opt-in: **`--check` must not ask.**
/// It runs at every session start of every wired agent, reads no graph, and a default-on payload
/// would make a freshness probe the most expensive call in the system.
#[test]
fn the_graph_rides_the_head_page_and_check_never_asks_for_one() {
    let mut f = fx();
    let tree: Arc<Mutex<BTreeMap<u128, Value>>> = Arc::new(Mutex::new(BTreeMap::from([
        (1, wire_node(1, "file", "a.md", 1, Some("# a"))),
        (2, wire_node(2, "file", "b.md", 2, Some("# b"))),
    ])));
    // Every request's `graph` flag, in order, so the opt-out is observed rather than assumed.
    let asked: Arc<Mutex<Vec<bool>>> = Arc::new(Mutex::new(Vec::new()));
    let inner = tree_server(tree.clone());
    let seen = asked.clone();
    let server = spawn_stub(Arc::new(move |path: &str, body: &Value| {
        if path == "/api/sync/pull" || path == "/api/sync/bootstrap" {
            seen.lock().unwrap().push(body["graph"] == json!(true));
        }
        let (code, mut resp) = inner(path, body);
        // The graph is emitted only when asked AND only on the head-reaching page.
        if body["graph"] == json!(true) && resp["nodes"].as_array().is_some_and(|n| n.len() < 500) {
            resp["graph"] = json!({
                "nodes": [
                    {"id": Uuid::from_u128(1).to_string(), "kind": "file", "name": "a.md",
                     "path": "a.md", "title": "Alpha", "contentBytes": 3},
                    {"id": Uuid::from_u128(2).to_string(), "kind": "file", "name": "b.md",
                     "path": "b.md", "contentBytes": 3},
                ],
                "edges": [{"src": 0, "dst": 1, "ref": "b", "kind": "wikilink"}],
                "tags": [{"node": 0, "tag": "work"}],
            });
        }
        (code, resp)
    }));

    assert_eq!(sync(&mut f, &server), 0);
    assert!(
        asked.lock().unwrap().iter().all(|a| *a),
        "every request of a sync run asks — which page reaches head is not knowable in advance"
    );

    let control = ControlRoot::at(&f.project.control);
    let st = control.load_state(WS).unwrap().unwrap();
    let held = control
        .load_graph(WS, st.epoch, st.cursor)
        .expect("the graph is stored against the head page's own position");
    assert_eq!(held.graph.nodes.len(), 2);

    // …and `read` answers from it.
    let out = docli_cli::read_cmd::resolve(
        &f.project,
        &docli_cli::read_cmd::ReadArgs {
            path: Some("a.md".into()),
            id: None,
            mount: None,
            lines: None,
            json: false,
        },
        unix_now(),
    );
    let docli_cli::read_cmd::Outcome::Served(s) = out else {
        panic!("expected a served note");
    };
    let docli_cli::read_cmd::Envelope::Note(n) = &s.envelope else {
        panic!("expected a note envelope");
    };
    assert_eq!(n.title.as_deref(), Some("Alpha"));
    assert_eq!(n.links.as_ref().unwrap().len(), 1);
    assert_eq!(n.tags.as_deref(), Some(&["work".to_string()][..]));
    assert!(!n.absent.contains_key("links"), "{:?}", n.absent);

    asked.lock().unwrap().clear();
    assert_eq!(check(&mut f, &server), 0);
    assert_eq!(
        asked.lock().unwrap().as_slice(),
        &[false],
        "`--check` reads no graph, so it never pays for one"
    );
}

/// A NEW CLI against an OLDER api: the request carries the flag, the server ignores it, and the
/// client must say «this server serves no graph» rather than «no backlinks».
#[test]
fn a_server_that_serves_no_graph_degrades_to_a_named_absence() {
    let mut f = fx();
    let tree: Arc<Mutex<BTreeMap<u128, Value>>> = Arc::new(Mutex::new(BTreeMap::from([(
        1,
        wire_node(1, "file", "a.md", 1, Some("# a")),
    )])));
    let server = spawn_stub(tree_server(tree.clone()));
    assert_eq!(sync(&mut f, &server), 0);

    let control = ControlRoot::at(&f.project.control);
    let st = control.load_state(WS).unwrap().unwrap();
    assert!(st.graph_asked, "the run asked");
    assert!(!control.graph_path(WS).exists(), "and got nothing to store");

    let out = docli_cli::read_cmd::resolve(
        &f.project,
        &docli_cli::read_cmd::ReadArgs {
            path: Some("a.md".into()),
            id: None,
            mount: None,
            lines: None,
            json: false,
        },
        unix_now(),
    );
    let docli_cli::read_cmd::Outcome::Served(s) = out else {
        panic!("expected a served note");
    };
    let docli_cli::read_cmd::Envelope::Note(n) = &s.envelope else {
        panic!("expected a note envelope");
    };
    assert!(n.backlinks.is_none(), "absent, never []");
    assert!(
        n.absent["backlinks"].contains("serves no note graph"),
        "{:?}",
        n.absent
    );
}

#[test]
fn check_exits_nonzero_when_behind() {
    let mut f = fx();
    let tree: Arc<Mutex<BTreeMap<u128, Value>>> = Arc::new(Mutex::new(BTreeMap::from([(
        1,
        wire_node(1, "file", "a.md", 1, Some("x")),
    )])));
    let server = spawn_stub(tree_server(tree.clone()));
    assert_eq!(sync(&mut f, &server), 0);
    assert_eq!(check(&mut f, &server), 0);
    // A new note lands server-side → behind.
    tree.lock()
        .unwrap()
        .insert(2, wire_node(2, "file", "b.md", 2, Some("y")));
    assert_eq!(check(&mut f, &server), 1, "agents branch on the exit code");
}

/// A head stamp in the FUTURE — the realistic cause is an NTP correction after a sync that ran
/// while the clock was ahead — makes the mirror's age unreadable, so `WsState::unusable_reason`
/// stops vouching for it and `docli read`/`docli search` say so. **`docli sync --check` must
/// HEAL that**, not report «fresh» over it: two authorities disagreeing about one mirror is the
/// exact split the shared readiness predicate exists to prevent. The probe establishes the one
/// fact the field records — the cursor is at the server's head, now — so it can fix it.
#[test]
fn check_heals_a_head_time_left_in_the_future_by_a_clock_correction() {
    let mut f = fx();
    let tree: Arc<Mutex<BTreeMap<u128, Value>>> = Arc::new(Mutex::new(BTreeMap::from([(
        1,
        wire_node(1, "file", "a.md", 1, Some("x")),
    )])));
    let server = spawn_stub(tree_server(tree.clone()));
    assert_eq!(sync(&mut f, &server), 0);

    // The clock ran ahead when the mirror synced; NTP has since pulled it back.
    let control = ControlRoot::new(&f.project.root);
    let mut st = control.load_state(WS).unwrap().unwrap();
    let ahead = st.head_reached_at.unwrap() + 86_400;
    st.head_reached_at = Some(ahead);
    control.save_state(WS, &st).unwrap();
    let now = unix_now();
    assert!(
        st.unusable_reason(None, now).is_some(),
        "the read verbs stop vouching for it"
    );

    // …and `--check` both passes AND repairs, so the disagreement cannot persist.
    assert_eq!(check(&mut f, &server), 0);
    let healed = control.load_state(WS).unwrap().unwrap();
    assert!(
        healed.head_reached_at.unwrap() <= unix_now(),
        "the stamp must be re-anchored to real time, not left in the future"
    );
    assert_eq!(
        healed.unusable_reason(None, unix_now()),
        None,
        "and the read verbs must agree with the gate again"
    );
}

#[test]
fn a_hard_purge_between_syncs_is_caught_by_the_count_and_pruned() {
    let mut f = fx();
    let tree: Arc<Mutex<BTreeMap<u128, Value>>> = Arc::new(Mutex::new(BTreeMap::from([
        (1, wire_node(1, "file", "keep.md", 1, Some("k"))),
        (2, wire_node(2, "file", "purged.md", 2, Some("p"))),
    ])));
    let server = spawn_stub(tree_server(tree.clone()));
    assert_eq!(sync(&mut f, &server), 0);
    assert!(f.mirror.join("purged.md").exists());

    // `purgeNode` («delete forever»): the row VANISHES (no tombstone) — only a barrier rev
    // moves, which the pull cannot see. The count mismatch at head is the detector; the prune
    // arm removes the stale file in the same invocation.
    tree.lock().unwrap().remove(&2);
    assert_eq!(sync(&mut f, &server), 0);
    assert!(!f.mirror.join("purged.md").exists(), "the prune pin");
    assert!(f.mirror.join("keep.md").exists());
    // Healed: the repair completed, so the mirror reads complete again.
    assert!(!f.mirror.join("CACHE_INCOMPLETE.docli").exists());
    assert_eq!(check(&mut f, &server), 0);
}

#[test]
fn check_detects_a_hard_purge_and_makes_the_repair_durable() {
    let mut f = fx();
    let tree: Arc<Mutex<BTreeMap<u128, Value>>> = Arc::new(Mutex::new(BTreeMap::from([
        (1, wire_node(1, "file", "keep.md", 1, Some("k"))),
        (2, wire_node(2, "file", "purged.md", 2, Some("p"))),
    ])));
    let server = spawn_stub(tree_server(tree.clone()));
    assert_eq!(sync(&mut f, &server), 0);
    tree.lock().unwrap().remove(&2);
    // The probe's empty page IS head-reaching, carries the count, and the mismatch sets the
    // durable from-zero flag + the visible marker + a non-zero exit.
    assert_eq!(check(&mut f, &server), 1);
    assert!(f.mirror.join("CACHE_INCOMPLETE.docli").exists());
    let control = ControlRoot::new(&f.project.root);
    assert!(control.load_state(WS).unwrap().unwrap().from_zero);
    // The next sync repairs.
    assert_eq!(sync(&mut f, &server), 0);
    assert!(!f.mirror.join("purged.md").exists());
}

#[test]
fn a_deleted_mirror_over_live_state_reads_as_from_zero_never_healthy() {
    let mut f = fx();
    let tree: Arc<Mutex<BTreeMap<u128, Value>>> = Arc::new(Mutex::new(BTreeMap::from([(
        1,
        wire_node(1, "file", "a.md", 1, Some("x")),
    )])));
    let server = spawn_stub(tree_server(tree.clone()));
    assert_eq!(sync(&mut f, &server), 0);
    std::fs::remove_dir_all(&f.mirror).unwrap();
    assert_eq!(check(&mut f, &server), 1, "rm -rf mirror/ must read stale");
    assert_eq!(sync(&mut f, &server), 0);
    assert_eq!(
        std::fs::read_to_string(f.mirror.join("a.md")).unwrap(),
        "x",
        "the from-zero re-derives the mirror"
    );
}

#[test]
fn a_scope_change_forces_a_per_mount_from_zero() {
    let mut f = fx();
    let tree: Arc<Mutex<BTreeMap<u128, Value>>> = Arc::new(Mutex::new(BTreeMap::from([
        (1, wire_node(1, "folder", "docs", 1, None)),
        (2, wire_node(2, "file", "docs/in.md", 2, Some("in"))),
        (3, wire_node(3, "file", "другое.md", 3, Some("out"))),
    ])));
    let server = spawn_stub(tree_server(tree.clone()));
    f.project.config.mounts[0].folder = Some("docs".into());
    assert_eq!(sync(&mut f, &server), 0);
    assert!(f.mirror.join("in.md").exists(), "scope-relative");
    assert!(!f.mirror.join("другое.md").exists());

    // Widening the scope must BACKFILL (the cursor advanced past the out-of-scope node).
    f.project.config.mounts[0].folder = None;
    assert_eq!(sync(&mut f, &server), 0);
    assert!(
        f.mirror.join("другое.md").exists(),
        "widened scope backfills"
    );
    assert!(f.mirror.join("docs/in.md").exists());
    assert!(
        !f.mirror.join("in.md").exists(),
        "the old scope-relative spelling pruned"
    );
}

#[test]
fn an_interrupted_from_zero_restarts_from_the_very_start() {
    let mut f = fx();
    let tree: Arc<Mutex<BTreeMap<u128, Value>>> = Arc::new(Mutex::new(BTreeMap::from([(
        1,
        wire_node(1, "file", "a.md", 1, Some("x")),
    )])));
    let starts = Arc::new(Mutex::new(Vec::<i64>::new()));
    let inner = tree_server(tree.clone());
    let starts2 = starts.clone();
    let handler: Handler = Arc::new(move |path, body| {
        if path == "/api/sync/bootstrap" {
            starts2.lock().unwrap().push(0);
        }
        inner(path, body)
    });
    let server = spawn_stub(handler);
    assert_eq!(sync(&mut f, &server), 0);

    // Simulate a crash mid-from-zero: the durable flag is set with a mid-way cursor.
    let control = ControlRoot::new(&f.project.root);
    let mut st = control.load_state(WS).unwrap().unwrap();
    st.from_zero = true;
    control.save_state(WS, &st).unwrap();
    docli_cli::mountfs::set_incomplete_marker(&f.mirror, true).unwrap();
    assert_eq!(
        check(&mut f, &server),
        1,
        "an interrupted from-zero fails --check"
    );

    assert_eq!(sync(&mut f, &server), 0);
    // The repair went through BOOTSTRAP (replay from (0,0)) — restart, never resume.
    assert_eq!(
        starts.lock().unwrap().len(),
        2,
        "initial + the restarted repair"
    );
    assert!(!f.mirror.join("CACHE_INCOMPLETE.docli").exists());
}

#[test]
fn the_rollback_detector_stops_on_a_head_page_without_the_count() {
    let mut f = fx();
    // An old (pre-v0.28.0) server: honors the pull but never emits liveNodes.
    let handler: Handler = Arc::new(move |path, _body| match path {
        "/api/sync/bootstrap" | "/api/sync/pull" => (
            200,
            page(1, vec![wire_node(1, "file", "a.md", 1, Some("x"))], None),
        ),
        other => panic!("unexpected {other}"),
    });
    let server = spawn_stub(handler);
    let err = format!(
        "{:#}",
        run_sync(
            &mut f,
            &server,
            SyncOptions {
                check: false,
                full: false,
            },
        )
        .unwrap_err()
    );
    assert!(err.contains("did not honor ephemeral"), "{err}");
    assert!(
        err.contains("sync_clients"),
        "names the stray artifact: {err}"
    );
}

#[test]
fn multi_page_pulls_only_trip_the_detector_on_the_head_page() {
    // 500-node pages: page 1 is FULL (legitimately count-free), page 2 short + counted.
    let mut f = fx();
    let total = 501usize;
    let handler: Handler = Arc::new(move |path, body| {
        let all: Vec<Value> = (1..=total)
            .map(|i| {
                wire_node(
                    i as u128,
                    "file",
                    &format!("n{i:04}.md"),
                    i as i64,
                    Some("x"),
                )
            })
            .collect();
        match path {
            "/api/sync/bootstrap" => {
                let first: Vec<Value> = all[..500].to_vec();
                (200, page(1, first, None)) // full page: NO count, and that is fine
            }
            "/api/sync/pull" => {
                let cur = body["cursor"]["rev"].as_i64().unwrap();
                let rest: Vec<Value> = all
                    .iter()
                    .filter(|n| n["rev"].as_i64().unwrap() > cur)
                    .cloned()
                    .collect();
                (200, page(1, rest, Some(total as i64)))
            }
            other => panic!("unexpected {other}"),
        }
    });
    let server = spawn_stub(handler);
    assert_eq!(sync(&mut f, &server), 0);
    assert!(f.mirror.join("n0001.md").exists());
    assert!(f.mirror.join("n0501.md").exists());
}

#[test]
fn epoch_change_forces_from_zero_like_any_client() {
    let mut f = fx();
    let tree: Arc<Mutex<BTreeMap<u128, Value>>> = Arc::new(Mutex::new(BTreeMap::from([(
        1,
        wire_node(1, "file", "a.md", 1, Some("v1")),
    )])));
    let epoch_bumped = Arc::new(Mutex::new(false));
    let inner = tree_server(tree.clone());
    let bumped = epoch_bumped.clone();
    let handler: Handler = Arc::new(move |path, body| {
        if path == "/api/sync/pull" && *bumped.lock().unwrap() {
            return (
                409,
                json!({"code": "EPOCH_CHANGED", "message": "resynced", "epoch": 2}),
            );
        }
        inner(path, body)
    });
    let server = spawn_stub(handler);
    assert_eq!(sync(&mut f, &server), 0);
    *epoch_bumped.lock().unwrap() = true;
    tree.lock()
        .unwrap()
        .insert(1, wire_node(1, "file", "a.md", 1, Some("v2-after-resync")));
    assert_eq!(sync(&mut f, &server), 0);
    assert_eq!(
        std::fs::read_to_string(f.mirror.join("a.md")).unwrap(),
        "v2-after-resync",
        "the 409 rerouted through bootstrap"
    );
}

#[test]
fn a_folder_scoped_mount_with_guard_parked_twins_syncs_clean() {
    // The ledger-comparand pin: scope and count coexist — out-of-scope AND parked ids are in
    // the ledger, so a folder-scoped mount matches exactly like an unscoped one.
    let mut f = fx();
    let tree: Arc<Mutex<BTreeMap<u128, Value>>> = Arc::new(Mutex::new(BTreeMap::from([
        (1, wire_node(1, "folder", "docs", 1, None)),
        (2, wire_node(2, "file", "docs/in.md", 2, Some("in"))),
        (3, wire_node(3, "file", "вне-скоупа.md", 3, Some("out"))),
        (4, wire_node(4, "attachment", "docs/x.png.docli", 4, None)), // the .docli namespace park
    ])));
    let server = spawn_stub(tree_server(tree.clone()));
    f.project.config.mounts[0].folder = Some("docs".into());
    assert_eq!(sync(&mut f, &server), 0);
    assert!(f.mirror.join("in.md").exists());
    // NO mismatch fired: a second sync is a clean no-op, not a from-zero loop.
    let control = ControlRoot::new(&f.project.root);
    assert!(!control.load_state(WS).unwrap().unwrap().from_zero);
    assert_eq!(check(&mut f, &server), 0);
}

#[test]
fn transient_parks_fail_check_but_structural_parks_do_not() {
    let mut f = fx();
    let tree: Arc<Mutex<BTreeMap<u128, Value>>> = Arc::new(Mutex::new(BTreeMap::from([
        (1, wire_node(1, "file", "ok.md", 1, Some("fine"))),
        // A structural park: the .docli namespace.
        (2, wire_node(2, "file", "x.docli/y.md", 2, Some("parked"))),
    ])));
    let server = spawn_stub(tree_server(tree.clone()));
    assert_eq!(sync(&mut f, &server), 0);
    // Structural alone: complete + check green (a signal that cannot stop firing stops
    // informing — doctor reports it instead).
    assert!(!f.mirror.join("CACHE_INCOMPLETE.docli").exists());
    assert_eq!(check(&mut f, &server), 0);

    // Now a TRANSIENT park: a divergent untracked occupant blocks a new note.
    std::fs::write(f.mirror.join("занято.md"), "моё").unwrap();
    tree.lock()
        .unwrap()
        .insert(3, wire_node(3, "file", "занято.md", 3, Some("серверное")));
    assert_eq!(sync(&mut f, &server), 0);
    assert_eq!(
        std::fs::read_to_string(f.mirror.join("занято.md")).unwrap(),
        "моё",
        "never overwrites what it does not own"
    );
    assert!(f.mirror.join("CACHE_INCOMPLETE.docli").exists());
    assert_eq!(
        check(&mut f, &server),
        1,
        "a transient park keeps --check failing"
    );

    // The user removes the occupant; `--full` heals (exactly what the park message says).
    std::fs::remove_file(f.mirror.join("занято.md")).unwrap();
    assert_eq!(
        run_sync(
            &mut f,
            &server,
            SyncOptions {
                check: false,
                full: true
            }
        )
        .unwrap(),
        0
    );
    assert_eq!(
        std::fs::read_to_string(f.mirror.join("занято.md")).unwrap(),
        "серверное"
    );
    assert_eq!(check(&mut f, &server), 0);
}

#[test]
fn no_access_is_partial_success_not_an_abort() {
    // Two mounts; the second workspace refuses with 403 — the first must still sync.
    let mut f = fx();
    let ws2 = Uuid::from_u128(0xBB);
    f.project.config.mounts.push(Mount {
        workspace: ws2,
        dir: "mirror2".into(),
        folder: None,
        name: Some("чужое".into()),
        derived_dir: false,
        workspace_label: String::new(),
    });
    let tree: Arc<Mutex<BTreeMap<u128, Value>>> = Arc::new(Mutex::new(BTreeMap::from([(
        1,
        wire_node(1, "file", "a.md", 1, Some("x")),
    )])));
    let inner = tree_server(tree.clone());
    let handler: Handler = Arc::new(move |path, body| {
        if body["workspaceId"] == json!(ws2.to_string()) {
            return (
                403,
                json!({"code": "FORBIDDEN", "message": "you don't have access to that workspace"}),
            );
        }
        inner(path, body)
    });
    let server = spawn_stub(handler);
    let code = run_sync(
        &mut f,
        &server,
        SyncOptions {
            check: false,
            full: false,
        },
    )
    .unwrap();
    assert_eq!(code, 1, "reported, not fatal");
    assert!(f.mirror.join("a.md").exists(), "the reachable mount synced");
}

#[test]
fn a_forced_hand_edit_persists_across_incremental_sync_until_the_rev_advances() {
    // D3's honest-contract triple, end to end.
    let mut f = fx();
    let tree: Arc<Mutex<BTreeMap<u128, Value>>> = Arc::new(Mutex::new(BTreeMap::from([(
        1,
        wire_node(1, "file", "a.md", 1, Some("server-v1")),
    )])));
    let server = spawn_stub(tree_server(tree.clone()));
    assert_eq!(sync(&mut f, &server), 0);

    // Force an outside edit (lift read-only like a rogue editor would).
    let p = f.mirror.join("a.md");
    let mut perms = std::fs::metadata(&p).unwrap().permissions();
    #[allow(clippy::permissions_set_readonly_false)]
    perms.set_readonly(false);
    std::fs::set_permissions(&p, perms).unwrap();
    std::fs::write(&p, "MY EDIT").unwrap();

    // An incremental sync does NOT touch it (rev unchanged ⇒ not redelivered): the edit
    // silently persists as fake server truth.
    assert_eq!(sync(&mut f, &server), 0);
    assert_eq!(std::fs::read_to_string(&p).unwrap(), "MY EDIT");

    // …until the note changes server-side: destroyed, no conflict copy.
    tree.lock()
        .unwrap()
        .insert(1, wire_node(1, "file", "a.md", 2, Some("server-v2")));
    assert_eq!(sync(&mut f, &server), 0);
    assert_eq!(std::fs::read_to_string(&p).unwrap(), "server-v2");
}

// ---- doctor (D7) ----------------------------------------------------------------------------

fn run_doctor(fx: &mut Fx, server: &str) -> anyhow::Result<i32> {
    fx.project.config.server = server.to_string();
    let api = api_for(&fx.project.root, server);
    docli_cli::doctor::run(&fx.project, &api, true)
}

#[test]
fn doctor_reports_clean_then_detects_each_seeded_class() {
    let mut f = fx();
    let tree: Arc<Mutex<BTreeMap<u128, Value>>> = Arc::new(Mutex::new(BTreeMap::from([
        (1, wire_node(1, "folder", "docs", 1, None)),
        (2, wire_node(2, "file", "docs/a.md", 2, Some("server body"))),
        (3, wire_node(3, "attachment", "docs/pic.png", 3, None)),
        (4, wire_node(4, "file", "orphan.md", 4, Some("o"))),
    ])));
    let server = spawn_stub(tree_server(tree.clone()));
    assert_eq!(sync(&mut f, &server), 0);
    assert_eq!(
        run_doctor(&mut f, &server).unwrap(),
        0,
        "clean tree reports clean"
    );

    // Seed the classes AFTER the last sync — a sync in between would HEAL half of them via its
    // own mirror-vs-manifest invalidator (correct sync behavior, not what this test measures).
    // digest-mismatch: a hand edit (the one class no sync invalidator trips).
    let p = f.mirror.join("docs/a.md");
    let mut perms = std::fs::metadata(&p).unwrap().permissions();
    #[allow(clippy::permissions_set_readonly_false)]
    perms.set_readonly(false);
    std::fs::set_permissions(&p, perms).unwrap();
    std::fs::write(&p, "MY EDIT").unwrap();
    // missing-remote: a hand-created stray.
    std::fs::write(f.mirror.join("stray.txt"), "mine").unwrap();
    // missing-local: a mirrored marker deleted from disk.
    let marker = f.mirror.join("docs/pic.png.docli");
    let mut mp = std::fs::metadata(&marker).unwrap().permissions();
    #[allow(clippy::permissions_set_readonly_false)]
    mp.set_readonly(false);
    std::fs::set_permissions(&marker, mp).unwrap();
    std::fs::remove_file(&marker).unwrap();
    // state-orphan: a node hard-purged server-side that the state still tracks.
    tree.lock().unwrap().remove(&4);
    // crash-residue: a write_atomic temp a process death left mid-swap (read-only, like the
    // real thing) — at BOTH write destinations (round-3 F4): the mount tree and the
    // workspace's relocated-marker dir.
    let residue = f.mirror.join("docs/.docli-write-00aa11bb22cc33dd.tmp");
    std::fs::write(&residue, "partial").unwrap();
    let mut rp = std::fs::metadata(&residue).unwrap().permissions();
    rp.set_readonly(true);
    std::fs::set_permissions(&residue, rp).unwrap();
    let ws_markers = f.project.root.join(format!(".docli/markers/{WS}"));
    std::fs::create_dir_all(&ws_markers).unwrap();
    let marker_residue = ws_markers.join(".docli-write-ffee00112233ddee.tmp");
    std::fs::write(&marker_residue, "partial").unwrap();
    let mut mp2 = std::fs::metadata(&marker_residue).unwrap().permissions();
    mp2.set_readonly(true);
    std::fs::set_permissions(&marker_residue, mp2).unwrap();

    // Doctor sees all four CLASSES (not merely a non-zero exit). Its fresh pull is EPHEMERAL —
    // the registered-arm assertions live server-side; here the stub asserts `ephemeral: true`
    // on every pull.
    f.project.config.server = server.clone();
    let api = api_for(&f.project.root, &server);
    let all = docli_cli::doctor::collect(&f.project, &api).unwrap();
    let classes: std::collections::BTreeSet<&str> = all
        .iter()
        .flat_map(|(_, ds)| ds.iter().map(|d| d.class))
        .collect();
    for want in [
        "digest-mismatch",
        "missing-remote",
        "missing-local",
        "state-orphan",
        "crash-residue",
    ] {
        assert!(classes.contains(want), "missing {want} in {classes:?}");
    }
    assert_eq!(run_doctor(&mut f, &server).unwrap(), 1);

    // The markers-dir residue is classified as crash-residue too (round-3 F4), never as a
    // stray relocated marker.
    let marker_row = all
        .iter()
        .flat_map(|(_, ds)| ds.iter())
        .find(|d| d.path.contains(".docli-write-ffee00112233ddee.tmp"))
        .expect("markers-dir residue must be reported");
    assert_eq!(marker_row.class, "crash-residue");

    // And the remediation doctor names actually works: the authoritative resync sweeps the
    // residue at BOTH destinations (round-2/3 pin — nothing else pins that `sync --full`
    // calls the sweep).
    assert_eq!(sync_full(&mut f, &server), 0);
    assert!(!residue.exists(), "sync --full must sweep write temps");
    assert!(
        !marker_residue.exists(),
        "sync --full must sweep the markers dir too"
    );
}

#[test]
fn doctor_reports_a_materialized_but_untracked_note() {
    // The crash window between an applied page and its state save (or a hand-made file
    // shadowing an unsynced node): disk matches the server, state has no entry — search
    // treats it as unmirrored, so doctor must not read "the disk matches" as clean (Codex
    // round 10).
    let mut f = fx();
    let tree: Arc<Mutex<BTreeMap<u128, Value>>> = Arc::new(Mutex::new(BTreeMap::from([(
        1,
        wire_node(1, "file", "a.md", 1, Some("body")),
    )])));
    let server = spawn_stub(tree_server(tree.clone()));
    assert_eq!(sync(&mut f, &server), 0);
    tree.lock()
        .unwrap()
        .insert(2, wire_node(2, "file", "new.md", 2, Some("crashed page")));
    std::fs::write(f.mirror.join("new.md"), "crashed page").unwrap();
    f.project.config.server = server.clone();
    let api = api_for(&f.project.root, &server);
    let all = docli_cli::doctor::collect(&f.project, &api).unwrap();
    assert!(
        all.iter()
            .flat_map(|(_, ds)| ds.iter())
            .any(|d| d.class == "untracked" && d.path == "new.md"),
        "{all:?}"
    );
    assert_eq!(run_doctor(&mut f, &server).unwrap(), 1);
}

#[test]
fn an_orphan_relocated_marker_is_reported_and_swept() {
    // State loss + a remote hard delete strands `.docli/markers/<id>.docli`: no replay can
    // name it, and prune walks state so it never sees the file (Codex round 10). Doctor is
    // the detector; the from-zero sweep is the healer.
    let mut f = fx();
    let long = format!("{}.png", "a".repeat(248)); // marker derivation overflows -> relocated
    let tree: Arc<Mutex<BTreeMap<u128, Value>>> = Arc::new(Mutex::new(BTreeMap::from([(
        1,
        wire_node(1, "attachment", &long, 1, None),
    )])));
    let server = spawn_stub(tree_server(tree.clone()));
    assert_eq!(sync(&mut f, &server), 0);
    let markers = f.project.root.join(format!(
        ".docli/markers/{}",
        f.project.config.mounts[0].workspace
    ));
    assert_eq!(std::fs::read_dir(&markers).unwrap().count(), 1);
    std::fs::remove_file(
        docli_cli::state::ControlRoot::new(&f.project.root)
            .state_path(f.project.config.mounts[0].workspace),
    )
    .unwrap();
    tree.lock().unwrap().remove(&1);
    f.project.config.server = server.clone();
    let api = api_for(&f.project.root, &server);
    let all = docli_cli::doctor::collect(&f.project, &api).unwrap();
    assert!(
        all.iter()
            .flat_map(|(_, ds)| ds.iter())
            .any(|d| d.class == "missing-remote" && d.path.starts_with(".docli/markers/")),
        "{all:?}"
    );
    assert_eq!(sync(&mut f, &server), 0);
    assert_eq!(std::fs::read_dir(&markers).unwrap().count(), 0);
}

#[test]
fn doctor_no_access_is_partial_success_not_an_abort() {
    // The sync/search contract holds for doctor too (Codex round 11): a 403 workspace becomes
    // its own row; the reachable mount is still checked.
    let mut f = fx();
    let ws2 = Uuid::from_u128(0xBB);
    f.project.config.mounts.push(Mount {
        workspace: ws2,
        dir: "mirror2".into(),
        folder: None,
        name: Some("чужое".into()),
        derived_dir: false,
        workspace_label: String::new(),
    });
    let tree: Arc<Mutex<BTreeMap<u128, Value>>> = Arc::new(Mutex::new(BTreeMap::from([(
        1,
        wire_node(1, "file", "a.md", 1, Some("x")),
    )])));
    let inner = tree_server(tree.clone());
    let handler: Handler = Arc::new(move |path, body| {
        if body["workspaceId"] == json!(ws2.to_string()) {
            return (
                403,
                json!({"code": "FORBIDDEN", "message": "you don't have access to that workspace"}),
            );
        }
        inner(path, body)
    });
    let server = spawn_stub(handler);
    // Sync ws1 so its mount exists (doctor's never-synced short-circuit is not the subject);
    // sync exits 1 for the refused ws2 — expected partial success.
    assert_eq!(
        run_sync(
            &mut f,
            &server,
            SyncOptions {
                check: false,
                full: false,
            },
        )
        .unwrap(),
        1
    );
    f.project.config.server = server.clone();
    let api = api_for(&f.project.root, &server);
    let all = docli_cli::doctor::collect(&f.project, &api).unwrap();
    assert_eq!(all.len(), 2, "{all:?}");
    assert!(all[0].1.is_empty(), "reachable mount is clean: {all:?}");
    assert_eq!(all[1].1[0].class, "no-access", "{all:?}");
}

#[test]
fn doctor_reports_a_move_applied_but_not_persisted() {
    // The crash window between a MOVE's write and its state save (Codex round 11): disk and
    // server both hold new.md, state still maps the id to old.md — id presence alone must not
    // read as tracked.
    let mut f = fx();
    let tree: Arc<Mutex<BTreeMap<u128, Value>>> = Arc::new(Mutex::new(BTreeMap::from([(
        1,
        wire_node(1, "file", "old.md", 1, Some("body")),
    )])));
    let server = spawn_stub(tree_server(tree.clone()));
    assert_eq!(sync(&mut f, &server), 0);
    tree.lock()
        .unwrap()
        .insert(1, wire_node(1, "file", "new.md", 2, Some("body")));
    // Simulate the applied-but-unpersisted page: the file moved on disk, state did not.
    let old = f.mirror.join("old.md");
    let mut perms = std::fs::metadata(&old).unwrap().permissions();
    #[allow(clippy::permissions_set_readonly_false)]
    perms.set_readonly(false);
    std::fs::set_permissions(&old, perms).unwrap();
    std::fs::rename(&old, f.mirror.join("new.md")).unwrap();
    f.project.config.server = server.clone();
    let api = api_for(&f.project.root, &server);
    let all = docli_cli::doctor::collect(&f.project, &api).unwrap();
    assert!(
        all.iter()
            .flat_map(|(_, ds)| ds.iter())
            .any(|d| d.class == "untracked" && d.path == "new.md"),
        "{all:?}"
    );
}

#[test]
fn doctor_reports_a_pending_repair_flag() {
    // Codex round 29: a persisted from-zero flag with an otherwise-matching disk must not
    // read as clean — `--check` fails and search refuses local paths for the same mount.
    let mut f = fx();
    let tree: Arc<Mutex<BTreeMap<u128, Value>>> = Arc::new(Mutex::new(BTreeMap::from([(
        1,
        wire_node(1, "file", "a.md", 1, Some("body")),
    )])));
    let server = spawn_stub(tree_server(tree.clone()));
    assert_eq!(sync(&mut f, &server), 0);
    let sp = docli_cli::state::ControlRoot::new(&f.project.root)
        .state_path(f.project.config.mounts[0].workspace);
    let mut st: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&sp).unwrap()).unwrap();
    st["from_zero"] = serde_json::json!(true);
    std::fs::write(&sp, st.to_string()).unwrap();
    f.project.config.server = server.clone();
    let api = api_for(&f.project.root, &server);
    let all = docli_cli::doctor::collect(&f.project, &api).unwrap();
    assert!(
        all.iter()
            .flat_map(|(_, ds)| ds.iter())
            .any(|d| d.class == "repair-pending"),
        "{all:?}"
    );
    assert_eq!(run_doctor(&mut f, &server).unwrap(), 1);
}

#[test]
fn doctor_reports_a_lost_state_file_instead_of_clean() {
    // Delete `.docli/state/<ws>.json` under an otherwise perfect mirror: every per-node check
    // degrades to disk-vs-server and used to report CLEAN, while search treats the mount as
    // unmirrored and sync must from-zero repair (Codex round 9).
    let mut f = fx();
    let tree: Arc<Mutex<BTreeMap<u128, Value>>> = Arc::new(Mutex::new(BTreeMap::from([(
        1,
        wire_node(1, "file", "a.md", 1, Some("body")),
    )])));
    let server = spawn_stub(tree_server(tree.clone()));
    assert_eq!(sync(&mut f, &server), 0);
    std::fs::remove_file(
        docli_cli::state::ControlRoot::new(&f.project.root)
            .state_path(f.project.config.mounts[0].workspace),
    )
    .unwrap();
    f.project.config.server = server.clone();
    let api = api_for(&f.project.root, &server);
    let all = docli_cli::doctor::collect(&f.project, &api).unwrap();
    assert!(
        all.iter()
            .flat_map(|(_, ds)| ds.iter())
            .any(|d| d.class == "state-missing"),
        "{all:?}"
    );
    assert_eq!(run_doctor(&mut f, &server).unwrap(), 1);
}

#[test]
fn doctor_reports_a_fold_collision_instead_of_collapsing_it() {
    // Sync `Foo.md`, then the server grows `FOO.md` BEFORE the next sync: on a folding
    // filesystem both expectations resolve to one physical file, and a silent fold-collapse
    // let doctor report CLEAN over the live collision (Codex round 7). On a case-sensitive
    // filesystem the second note is honestly missing-local — either way FOO.md gets a row.
    let mut f = fx();
    let tree: Arc<Mutex<BTreeMap<u128, Value>>> = Arc::new(Mutex::new(BTreeMap::from([(
        1,
        wire_node(1, "file", "Foo.md", 1, Some("body")),
    )])));
    let server = spawn_stub(tree_server(tree.clone()));
    assert_eq!(sync(&mut f, &server), 0);
    tree.lock()
        .unwrap()
        .insert(2, wire_node(2, "file", "FOO.md", 2, Some("body")));
    f.project.config.server = server.clone();
    let api = api_for(&f.project.root, &server);
    let all = docli_cli::doctor::collect(&f.project, &api).unwrap();
    let classes: Vec<&str> = all
        .iter()
        .flat_map(|(_, ds)| ds.iter())
        .filter(|d| d.path == "FOO.md")
        .map(|d| d.class)
        .collect();
    // Folding fs: the alias exists on disk, so the collision row (plus the honest
    // `untracked` row) fires; case-sensitive fs: the note is honestly missing.
    assert!(
        classes.contains(&"fold-collision") || classes.contains(&"missing-local"),
        "{classes:?}"
    );
    assert_eq!(run_doctor(&mut f, &server).unwrap(), 1);
}

#[test]
fn doctor_reports_structural_parks_and_never_creates_an_unsynced_mirror() {
    let mut f = fx();
    let tree: Arc<Mutex<BTreeMap<u128, Value>>> = Arc::new(Mutex::new(BTreeMap::from([
        (1, wire_node(1, "file", "ok.md", 1, Some("x"))),
        // A structural park: the .docli control namespace.
        (2, wire_node(2, "file", "x.docli/y.md", 2, Some("parked"))),
    ])));
    let server = spawn_stub(tree_server(tree.clone()));

    // BEFORE any sync: doctor must not create the mirror or its marker (read-only honesty).
    f.project.config.server = server.clone();
    let api = api_for(&f.project.root, &server);
    let all = docli_cli::doctor::collect(&f.project, &api).unwrap();
    assert!(!f.mirror.exists(), "doctor must never create the mount");
    assert_eq!(all[0].1[0].class, "missing-local");
    assert!(
        all[0].1[0].detail.contains("never been synced"),
        "{:?}",
        all[0].1[0]
    );

    // After a sync, the structural park is REPORTED here — the sync summary's «см. docli
    // doctor» pointer must not lead to `clean` (the dead-loop round-2 finding).
    assert_eq!(sync(&mut f, &server), 0);
    let all = docli_cli::doctor::collect(&f.project, &api).unwrap();
    let parked: Vec<_> = all[0].1.iter().filter(|d| d.class == "parked").collect();
    assert_eq!(parked.len(), 1, "{all:?}");
    assert!(parked[0].detail.contains("Structural"), "{:?}", parked[0]);
    assert!(parked[0].path.contains("x.docli"), "{:?}", parked[0]);
}

#[test]
fn a_mid_replay_epoch_bump_leaves_the_repair_pending_not_exit_2() {
    // The round-1 H2 regression pin: a from-zero whose PAGED pull answers 409 must not exit 2 —
    // the flag stays set, CACHE_INCOMPLETE stays present, --check fails, the next sync heals.
    let mut f = fx();
    let total = 501usize; // forces a second (paged) request during the from-zero replay
    let bumped = Arc::new(Mutex::new(false));
    let bumped2 = bumped.clone();
    let handler: Handler = Arc::new(move |path, body| {
        let all: Vec<Value> = (1..=total)
            .map(|i| {
                wire_node(
                    i as u128,
                    "file",
                    &format!("n{i:04}.md"),
                    i as i64,
                    Some("x"),
                )
            })
            .collect();
        match path {
            "/api/sync/bootstrap" => (200, page(1, all[..500].to_vec(), None)),
            "/api/sync/pull" => {
                if *bumped2.lock().unwrap() {
                    return (
                        409,
                        json!({"code": "EPOCH_CHANGED", "message": "resynced", "epoch": 2}),
                    );
                }
                let cur = body["cursor"]["rev"].as_i64().unwrap();
                let rest: Vec<Value> = all
                    .iter()
                    .filter(|n| n["rev"].as_i64().unwrap() > cur)
                    .cloned()
                    .collect();
                (200, page(1, rest, Some(total as i64)))
            }
            other => panic!("unexpected {other}"),
        }
    });
    let server = spawn_stub(handler);
    // Prime a synced state, then force a from-zero whose SECOND page will 409.
    assert_eq!(sync(&mut f, &server), 0);
    let control = ControlRoot::new(&f.project.root);
    let mut st = control.load_state(WS).unwrap().unwrap();
    st.from_zero = true;
    control.save_state(WS, &st).unwrap();
    *bumped.lock().unwrap() = true;

    let code = run_sync(
        &mut f,
        &server,
        SyncOptions {
            check: false,
            full: false,
        },
    )
    .unwrap();
    assert_eq!(
        code, 0,
        "a self-healing server event is not an agent-visible error"
    );
    assert!(f.mirror.join("CACHE_INCOMPLETE.docli").exists());
    assert!(
        control.load_state(WS).unwrap().unwrap().from_zero,
        "the repair stays pending"
    );
    // …and heals once the server stops bumping.
    *bumped.lock().unwrap() = false;
    assert_eq!(sync(&mut f, &server), 0);
    assert!(!f.mirror.join("CACHE_INCOMPLETE.docli").exists());
}

#[test]
fn doctor_stops_on_a_server_that_does_not_honor_ephemeral() {
    // A synced mirror first (a never-synced mount short-circuits before pulling), THEN the
    // server "rolls back" to one that never emits the count.
    let mut f = fx();
    let tree: Arc<Mutex<BTreeMap<u128, Value>>> = Arc::new(Mutex::new(BTreeMap::from([(
        1,
        wire_node(1, "file", "a.md", 1, Some("x")),
    )])));
    let good = spawn_stub(tree_server(tree));
    assert_eq!(sync(&mut f, &good), 0);

    let handler: Handler = Arc::new(move |path, _body| match path {
        "/api/sync/bootstrap" | "/api/sync/pull" => (
            200,
            page(1, vec![wire_node(1, "file", "a.md", 1, Some("x"))], None),
        ),
        other => panic!("unexpected {other}"),
    });
    let server = spawn_stub(handler);
    let err = format!("{:#}", run_doctor(&mut f, &server).unwrap_err());
    assert!(err.contains("did not honor ephemeral"), "{err}");
}

#[test]
fn narrowing_the_scope_removes_the_old_scope_root_materialization() {
    // Codex round-1 #1: sync unscoped (materializing `docs/` + `docs/a.md`), then narrow the
    // scope to `docs` — the from-zero replay maps the `docs` node to the mount root, and its
    // OLD `docs/` dir must be removed, not tracked forever (its id stays in the ledger, so the
    // prune alone can never touch it).
    let mut f = fx();
    let tree: Arc<Mutex<BTreeMap<u128, Value>>> = Arc::new(Mutex::new(BTreeMap::from([
        (1, wire_node(1, "folder", "docs", 1, None)),
        (2, wire_node(2, "file", "docs/a.md", 2, Some("x"))),
    ])));
    let server = spawn_stub(tree_server(tree.clone()));
    assert_eq!(sync(&mut f, &server), 0);
    assert!(f.mirror.join("docs/a.md").exists());

    f.project.config.mounts[0].folder = Some("docs".into());
    assert_eq!(sync(&mut f, &server), 0);
    assert_eq!(std::fs::read_to_string(f.mirror.join("a.md")).unwrap(), "x");
    assert!(
        !f.mirror.join("docs").exists(),
        "the old wide-scope materialization must not survive the narrowing"
    );
    // And it stays clean: no from-zero loop, `--check` green.
    assert_eq!(check(&mut f, &server), 0);
}

#[test]
fn a_hard_purged_incumbent_cannot_park_its_same_path_replacement() {
    // Codex round-2 P1: node A owns x.md, is hard-purged, and replacement B (new id) takes
    // x.md. The count mismatch forces a from-zero — during which A's STALE state entry must not
    // claim x.md and structurally park B (the prune would remove A, nothing would retry B, and
    // `--check` would pass over the missing note). The ledger-filtered claim seeding closes it.
    let mut f = fx();
    let tree: Arc<Mutex<BTreeMap<u128, Value>>> = Arc::new(Mutex::new(BTreeMap::from([
        (1, wire_node(1, "file", "x.md", 1, Some("old body"))),
        (2, wire_node(2, "file", "other.md", 2, Some("o"))),
    ])));
    let server = spawn_stub(tree_server(tree.clone()));
    assert_eq!(sync(&mut f, &server), 0);

    // Hard-purge A; mint B at the SAME path (higher rev, fresh id).
    {
        let mut t = tree.lock().unwrap();
        t.remove(&1);
        t.insert(3, wire_node(3, "file", "x.md", 3, Some("new body")));
    }
    assert_eq!(sync(&mut f, &server), 0);
    assert_eq!(
        std::fs::read_to_string(f.mirror.join("x.md")).unwrap(),
        "new body",
        "the replacement must materialize, not park behind its purged predecessor"
    );
    let control = ControlRoot::new(&f.project.root);
    let st = control.load_state(WS).unwrap().unwrap();
    assert!(st.parks.is_empty(), "{:?}", st.parks);
    assert_eq!(check(&mut f, &server), 0);
}

#[test]
fn an_owed_directory_removal_survives_restarts_and_heals_without_full() {
    // Codex round-2 P1: a trashed folder kept alive by an untracked occupant must be OWED
    // durably — not held in an in-memory vector a crash loses, and not healable only by
    // `--full` (whose prune walks tracked nodes and cannot see an untracked stray dir).
    let mut f = fx();
    let tree: Arc<Mutex<BTreeMap<u128, Value>>> = Arc::new(Mutex::new(BTreeMap::from([
        (1, wire_node(1, "folder", "d", 1, None)),
        (2, wire_node(2, "file", "d/a.md", 2, Some("x"))),
    ])));
    let server = spawn_stub(tree_server(tree.clone()));
    assert_eq!(sync(&mut f, &server), 0);
    std::fs::write(f.mirror.join("d/stray.txt"), "mine").unwrap();

    // Trash the folder + child; the dir is kept alive by the stray.
    {
        let mut t = tree.lock().unwrap();
        t.insert(1, {
            let mut n = wire_node(1, "folder", "d", 3, None);
            n["trashed"] = json!(true);
            n
        });
        t.insert(2, {
            let mut n = wire_node(2, "file", "d/a.md", 3, None);
            n["trashed"] = json!(true);
            n
        });
    }
    assert_eq!(sync(&mut f, &server), 0);
    assert!(
        f.mirror.join("d/stray.txt").exists(),
        "never deletes what it does not own"
    );
    let control = ControlRoot::new(&f.project.root);
    let st = control.load_state(WS).unwrap().unwrap();
    assert_eq!(
        st.pending_removals.len(),
        1,
        "the owed removal is DURABLE: {st:?}"
    );
    assert!(
        f.mirror.join("CACHE_INCOMPLETE.docli").exists(),
        "a nonempty debt set keeps the mirror marked incomplete"
    );
    assert_eq!(
        check(&mut f, &server),
        1,
        "owed removals keep --check failing (the debt set is consulted directly)"
    );

    // The user removes the occupant; a PLAIN sync (no --full) settles the debt.
    std::fs::remove_file(f.mirror.join("d/stray.txt")).unwrap();
    assert_eq!(sync(&mut f, &server), 0);
    assert!(
        !f.mirror.join("d").exists(),
        "the owed dir is finally removed"
    );
    let st = control.load_state(WS).unwrap().unwrap();
    assert!(st.pending_removals.is_empty());
    assert!(st.parks.is_empty());
    assert!(!f.mirror.join("CACHE_INCOMPLETE.docli").exists());
    assert_eq!(check(&mut f, &server), 0);
}

#[test]
fn a_reclaimed_path_cancels_its_removal_debt() {
    // Codex round-4 P2: folder `d` owes removal (untracked occupant), then a NEW server folder
    // claims `d`. Settling the stale debt must not delete the live tracked directory.
    let mut f = fx();
    let tree: Arc<Mutex<BTreeMap<u128, Value>>> = Arc::new(Mutex::new(BTreeMap::from([
        (1, wire_node(1, "folder", "d", 1, None)),
        (2, wire_node(2, "file", "d/a.md", 2, Some("x"))),
    ])));
    let server = spawn_stub(tree_server(tree.clone()));
    assert_eq!(sync(&mut f, &server), 0);
    std::fs::write(f.mirror.join("d/stray.txt"), "mine").unwrap();
    {
        let mut t = tree.lock().unwrap();
        t.insert(1, {
            let mut n = wire_node(1, "folder", "d", 3, None);
            n["trashed"] = json!(true);
            n
        });
        t.insert(2, {
            let mut n = wire_node(2, "file", "d/a.md", 3, None);
            n["trashed"] = json!(true);
            n
        });
    }
    assert_eq!(sync(&mut f, &server), 0);
    let control = ControlRoot::new(&f.project.root);
    assert_eq!(
        control
            .load_state(WS)
            .unwrap()
            .unwrap()
            .pending_removals
            .len(),
        1
    );

    // A NEW folder (fresh id) claims `d`; the stray is then removed by the user.
    tree.lock()
        .unwrap()
        .insert(3, wire_node(3, "folder", "d", 4, None));
    assert_eq!(sync(&mut f, &server), 0);
    std::fs::remove_file(f.mirror.join("d/stray.txt")).unwrap();
    assert_eq!(sync(&mut f, &server), 0);
    assert!(
        f.mirror.join("d").is_dir(),
        "the LIVE reclaimed folder must survive the stale debt"
    );
    let st = control.load_state(WS).unwrap().unwrap();
    assert!(
        st.pending_removals.is_empty(),
        "the debt was cancelled, not executed"
    );
    assert_eq!(check(&mut f, &server), 0);
}
