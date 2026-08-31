// SPDX-FileCopyrightText: 2026 OOO Agitek
// SPDX-License-Identifier: MIT

//! `docli doctor` (v0.28.0 D7) — the three-way CONTROL ACCOUNT: the server tree (a fresh scoped
//! EPHEMERAL pull — D2a: no client registration, no purge side-effects), the disk (mirror
//! scan), and `.docli/` state, reconciled READ-ONLY. Output is DISCREPANCIES per class; no
//! repair verbs beyond naming `docli sync --full` (the invocable repair for the classes no
//! invalidator trips: a hand-edited mirror — count unchanged, scope unchanged, head fresh —
//! only digest-mismatch sees it; and, since D12, write-temp crash residue).
//!
//! This is the HEAVYWEIGHT check by design — a from-zero body re-download per mounted
//! workspace; `sync --check` is the cheap one. This is the reconciler whose absence the
//! 2026-08-28 recon identified as the single bug behind all four live sync-integrity defects.
//!
//! Class mapping, stated honestly (the plan's six labels + D12's crash-residue, as this code
//! emits them):
//! `missing-local` = a live in-scope server node with no mirror file/dir/marker (parked nodes
//! are excluded — the park already explains them); `digest-mismatch` = mirror note bytes ≠ the
//! server body (the hand-edit detector); `marker-drift` = marker bytes ≠ the deterministic
//! render; `id-mismatch` = state tracks a DIFFERENT id at a local path than the server's node
//! there; `missing-remote` = a disk file/dir no server node explains (hand-created files
//! included); `state-orphan` = a state entry whose id is gone from the server. A state entry
//! whose DISK file vanished surfaces as `missing-local` via the server walk while the node is
//! live, and as `state-orphan` once it is not — the "and/or no disk file" half of the plan's
//! label folds into those two rather than being its own row. The one genuine addition is
//! `crash-residue` (D12): a `.docli-write-*.tmp` a process death left mid-swap, at either
//! write destination — its remediation (`sync --full` sweeps it) actually works, which is why
//! it is not lumped into `missing-remote`'s "hand-created files are never synced" advice.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::Result;
use docli_sync_wire::{WireCursor, WireNode};
use serde::Serialize;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::config::{mount_abs, validate_geometry, Mount, Project};
use crate::http::{Api, ApiFailure};
use crate::localpath::{self, has_reserved_segment, in_docli_namespace, scope_relative};
use crate::markers::render_marker;
use crate::mountfs::{claim_mount, INCOMPLETE_MARKER, MOUNT_MARKER};
use crate::platform::FsRules;
use crate::state::{ControlRoot, TrackedKind, WsState};

const PAGE_LIMIT: i64 = 500;

#[derive(Debug, Clone, Serialize)]
pub struct Discrepancy {
    pub class: &'static str,
    pub path: String,
    pub detail: String,
}

pub fn run(project: &Project, api: &Api, json: bool) -> Result<i32> {
    let all = collect(project, api)?;
    let total: usize = all.iter().map(|(_, d)| d.len()).sum();
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!(all
                .iter()
                .map(|(m, d)| serde_json::json!({"mount": m, "discrepancies": d}))
                .collect::<Vec<_>>()))?
        );
    } else {
        for (m, ds) in &all {
            if ds.is_empty() {
                println!("{m}: clean");
            } else {
                println!("{m}: discrepancies: {}", ds.len());
                for d in ds {
                    println!("  {:<18} {:<40} {}", d.class, d.path, d.detail);
                }
            }
        }
        if total > 0 {
            println!(
                "\nrepair: `docli sync --full` rebuilds the mirror from the server; the \
                      manual equivalent is to delete the cache and then run `docli sync`"
            );
        }
    }
    Ok(if total == 0 { 0 } else { 1 })
}

/// The reconcile itself, exposed apart from the rendering so the e2e suite can assert CLASSES,
/// not just the exit code.
pub fn collect(project: &Project, api: &Api) -> Result<Vec<(String, Vec<Discrepancy>)>> {
    validate_geometry(&project.root, &project.config)?;
    let rules = FsRules::native();
    let control = ControlRoot::new(&project.root);
    let mut all: Vec<(String, Vec<Discrepancy>)> = Vec::new();
    for mount in &project.config.mounts {
        // Partial success (D4, Codex round 11): one revoked/unentitled mount reports as its
        // own row and never aborts the others — the same contract sync and search honor.
        // Anything else (IO, protocol) still aborts: it is doctor's own failure, not access.
        let ds = match doctor_mount(project, api, &control, &rules, mount) {
            Ok(ds) => ds,
            Err(e) if crate::sync_cmd::is_no_access(&e) => vec![Discrepancy {
                class: "no-access",
                path: mount.dir.clone(),
                detail: crate::sync_cmd::no_access_message(mount.display_name()),
            }],
            Err(e) if e.downcast_ref::<crate::sync_cmd::NotEntitled>().is_some() => {
                vec![Discrepancy {
                    class: "no-access",
                    path: mount.dir.clone(),
                    detail: "синхронизация не включена для вашего аккаунта".into(),
                }]
            }
            Err(e) => return Err(e.context(format!("mount `{}`", mount.display_name()))),
        };
        all.push((mount.display_name().to_string(), ds));
    }
    Ok(all)
}

/// Pull the whole live server tree into memory, ephemerally (latest row per id; tombstones
/// drop). Never touches state or the mirror.
fn server_tree(api: &Api, ws: Uuid) -> Result<BTreeMap<Uuid, WireNode>> {
    let mut out = BTreeMap::new();
    let mut cursor = WireCursor {
        rev: 0,
        id: Uuid::nil(),
    };
    let mut req = docli_sync_wire::PullRequest {
        workspace_id: ws,
        client_id: "ephemeral".into(),
        cursor,
        epoch: 0,
        limit: Some(PAGE_LIMIT),
        ack: None,
        ephemeral: true,
    };
    let mut resp = match api.bootstrap(&req)? {
        Ok(r) => r,
        // The typed classification sync uses (403 → NoAccess, 402 → NotEntitled) so
        // `collect` can apply the same partial-success contract (Codex round 11).
        Err(f) => return Err(crate::sync_cmd::map_failure(f)),
    };
    let epoch = resp.epoch;
    loop {
        let head = (resp.nodes.len() as i64) < PAGE_LIMIT;
        // The rollback detector holds HERE too (round-1 §4.5): doctor's whole D7 claim is
        // "read-only, no server side effects beyond read-audit" — against a pre-v0.28.0 api the
        // pull silently takes the REGISTERED path, which is exactly the phantom-device outcome.
        // Stop and say so, like every sync call site.
        if head && resp.live_nodes.is_none() {
            anyhow::bail!(crate::sync_cmd::rollback_warning(ws));
        }
        for n in &resp.nodes {
            if n.trashed {
                out.remove(&n.id);
            } else {
                out.insert(n.id, n.clone());
            }
        }
        cursor = resp.cursor;
        if head {
            // The strict-detector role needs the count too (Codex round 1): a hard purge
            // LANDING DURING this scan removes a row this map already holds — the head page's
            // count is smaller, and ignoring it would let doctor report clean over exactly the
            // class it exists to catch.
            let live = resp.live_nodes.expect("checked above");
            if live != out.len() as i64 {
                anyhow::bail!(
                    "the workspace changed mid-scan (server live count {live} vs {} collected) \
                     — the reconciliation is inconclusive; run `docli doctor` again",
                    out.len()
                );
            }
            return Ok(out);
        }
        req.cursor = cursor;
        req.epoch = epoch;
        resp = match api.pull(&req)? {
            Ok(r) => r,
            Err(ApiFailure::EpochChanged { .. }) => {
                anyhow::bail!("the workspace was resynced mid-scan — run `docli doctor` again")
            }
            // The typed classification survives past page one too (Codex round 12): access
            // revoked between pages must still land in the partial-success arm.
            Err(f) => return Err(crate::sync_cmd::map_failure(f)),
        };
    }
}

fn sha_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn doctor_mount(
    project: &Project,
    api: &Api,
    control: &ControlRoot,
    rules: &FsRules,
    mount: &Mount,
) -> Result<Vec<Discrepancy>> {
    let ws = mount.workspace;
    let mount_path = mount_abs(&project.root, mount);
    // READ-ONLY honesty: a never-synced mount must not be CREATED by doctor (claim_mount mints
    // the dir + ownership marker on a first claim, which is a write). Report and move on.
    if !mount_path.join(crate::mountfs::MOUNT_MARKER).is_file() {
        return Ok(vec![Discrepancy {
            class: "missing-local",
            path: mount.dir.clone(),
            detail: "this mount has never been synced (neither the mirror nor its ownership \
                     marker exists) — run `docli sync`"
                .into(),
        }]);
    }
    // Claim (lock) so doctor never scans a mirror mid-apply; read-only past the lock.
    let handle = claim_mount(&mount_path, &control.dir, ws)?;
    if control.dir.exists() {
        crate::mountfs::refuse_symlinks(&control.dir)?;
    }
    let mut out: Vec<Discrepancy> = Vec::new();
    let state = match control.load_state(ws)? {
        Some(s) => s,
        None => {
            // The mirror exists (the never-synced short-circuit is behind us) but the state
            // file is gone: every per-node check below would silently degrade to disk-vs-server
            // and report CLEAN over a mount that search treats as unmirrored and sync must
            // from-zero repair (Codex round 9). Say so, then keep going with fresh state — the
            // remaining checks still find disk drift.
            out.push(Discrepancy {
                class: "state-missing",
                path: mount.dir.clone(),
                detail: "the workspace state file is missing but the mirror still exists — \
                         run `docli sync` to rebuild the state and repair the mirror"
                    .into(),
            });
            WsState::fresh(mount.folder.clone())
        }
    };
    // A durable pending repair IS a discrepancy (Codex round 29): with the flag set (or the
    // cursor short of head) the mirror is not a complete projection, `--check` fails and
    // search refuses local paths — doctor must not out-vote them with "clean".
    if state.from_zero || !state.at_head {
        out.push(Discrepancy {
            class: "repair-pending",
            path: mount.dir.clone(),
            detail: "the durable state says a sync/repair has not completed — run `docli sync`"
                .into(),
        });
    }
    let server = server_tree(api, ws)?;

    // PARKED deliveries first — doctor is where structural parks are REPORTED (the sync summary
    // points here, and `--check` deliberately ignores them: a signal that cannot stop firing
    // stops informing). A park row names the node, the class, and the reason.
    for (id, park) in &state.parks {
        out.push(Discrepancy {
            class: "parked",
            path: park.server_path.clone(),
            detail: format!(
                "{:?} delivery for node {id} is parked: {}",
                park.class, park.reason
            ),
        });
    }

    // Expected materializations from the SERVER view (the same guard pipeline as apply, minus
    // parking — a guarded node simply has no expectation here; parks are sync's report).
    // A Vec, deliberately NOT keyed by the projected spelling (Codex round 8): two distinct
    // server paths can project onto one EXACT local spelling (Windows: `a:b` and `a%3Ab` both
    // become `a%3Ab`), and a map insert would swallow one before the collision pass below can
    // report it. Exact-equal spellings are trivially fold-equal, so the fold pass catches both
    // classes once it sees every expectation.
    let mut expected_locals: Vec<(String, (Uuid, TrackedKind))> = Vec::new();
    for (id, node) in &server {
        // A PARKED node has no materialization expectation AT ALL (Codex round 2): its "park"
        // row above is the whole report. Running it through the checks below would resolve a
        // fold/projection-collision park onto the SURVIVING node's physical file and emit
        // spurious digest-/id-mismatch rows next to the honest one.
        if state.parks.contains_key(id) {
            continue;
        }
        let kind = match node.kind.as_str() {
            "file" => TrackedKind::Note,
            "folder" => TrackedKind::Folder,
            "attachment" => TrackedKind::Attachment,
            _ => continue,
        };
        if has_reserved_segment(&node.path) || in_docli_namespace(&node.path) {
            continue;
        }
        let Some(rel) = scope_relative(&node.path, mount.folder.as_deref()) else {
            continue;
        };
        if rel.is_empty() {
            continue;
        }
        if kind == TrackedKind::Note && !docli_rules::is_note_name(&node.name) {
            continue;
        }
        let Ok(local) = localpath::project(rel, rules) else {
            continue;
        };
        // The DISK expectation for an attachment is its MARKER, never the binary path — a file
        // sitting at the binary's own path is exactly what the marker-only contract forbids,
        // and listing that path as "expected" would suppress the report (Codex round 1).
        match kind {
            TrackedKind::Attachment => {
                let marker_rel = state
                    .nodes
                    .get(id)
                    .and_then(|n| n.marker_path.clone())
                    .unwrap_or_else(|| format!("{local}.docli"));
                expected_locals.push((marker_rel, (*id, kind)));
            }
            _ => {
                expected_locals.push((local.clone(), (*id, kind)));
            }
        }

        // Materialized on disk but ABSENT from state: the crash window between an applied
        // page and its state save, or a hand-made file shadowing an unsynced node — either
        // way search treats it as unmirrored and only a `--full` reconciles, so "the disk
        // matches" must not read as clean (Codex round 10). Checked where the existence
        // checks below pass; a missing file already gets its `missing-local` row.
        let untracked_row = |out: &mut Vec<Discrepancy>, path: &str| {
            // Not mere id presence (Codex round 11): a crash between a MOVE's write and its
            // state save leaves the id tracked at the OLD path — state must track the node at
            // THIS expectation's materializing path to count.
            let tracked_here = state.nodes.get(id).is_some_and(|n| match n.kind {
                TrackedKind::Attachment => n.marker_path.as_deref() == Some(path),
                _ => n.local_path == path,
            });
            if !tracked_here {
                out.push(Discrepancy {
                    class: "untracked",
                    path: path.to_string(),
                    detail: format!(
                        "server node {} is present at this local path but the workspace state \
                         does not track it there — an interrupted sync or a manually created \
                         file; run `docli sync --full`",
                        node.path
                    ),
                });
            }
        };
        match kind {
            TrackedKind::Folder => {
                if !handle.root.join(&local).is_dir() {
                    out.push(Discrepancy {
                        class: "missing-local",
                        path: local.clone(),
                        detail: format!("folder {} is not in the mirror", node.path),
                    });
                } else {
                    untracked_row(&mut out, &local);
                }
            }
            TrackedKind::Note => {
                let p = handle.root.join(&local);
                if !p.is_file() {
                    out.push(Discrepancy {
                        class: "missing-local",
                        path: local.clone(),
                        detail: format!("note {} is not in the mirror", node.path),
                    });
                    continue;
                }
                let disk = std::fs::read(&p)?;
                let server_sha = sha_hex(node.body.as_deref().unwrap_or_default().as_bytes());
                if sha_hex(&disk) != server_sha {
                    out.push(Discrepancy {
                        class: "digest-mismatch",
                        path: local.clone(),
                        detail: "the mirror bytes differ from the server body; if this is a \
                                 local edit, it will never be synced and will be overwritten \
                                 when the note next changes server-side"
                            .into(),
                    });
                }
                if let Some(st_id) = state.id_at_local(&local) {
                    if st_id != *id {
                        out.push(Discrepancy {
                            class: "id-mismatch",
                            path: local,
                            detail: format!("state tracks {st_id}, the server says {id}"),
                        });
                    }
                } else {
                    untracked_row(&mut out, &local);
                }
            }
            TrackedKind::Attachment => {
                // The marker resolves through STATE (relocation, D6), falling back to the
                // derived path for an unsynced-yet node.
                let marker_rel = state
                    .nodes
                    .get(id)
                    .and_then(|n| n.marker_path.clone())
                    .unwrap_or_else(|| format!("{local}.docli"));
                let abs = if marker_rel.starts_with(".docli/") {
                    // Only this workspace's own namespace resolves (Codex round 13); a
                    // sibling/traversal shape reads as missing-local, never as a sibling read.
                    match crate::apply::relocated_leaf(&marker_rel, ws) {
                        Some(leaf) => control.markers_dir().join(ws.to_string()).join(leaf),
                        None => std::path::PathBuf::from(""),
                    }
                } else {
                    // State-derived: containment or an unreadable sentinel (Codex round 15).
                    crate::mountfs::contained_join(&handle.root, &marker_rel)
                        .unwrap_or_else(|_| std::path::PathBuf::from(""))
                };
                if !abs.is_file() {
                    out.push(Discrepancy {
                        class: "missing-local",
                        path: marker_rel,
                        detail: format!("marker for {} is not in the mirror", node.path),
                    });
                    continue;
                }
                let disk = std::fs::read(&abs)?;
                if disk != render_marker(node).as_bytes() {
                    // `sha256 unknown` is an honest state, not a mismatch — but the whole
                    // marker is deterministic, so any byte drift IS drift.
                    out.push(Discrepancy {
                        class: "marker-drift",
                        path: marker_rel,
                        detail: "marker content differs from the attachment metadata returned by the server"
                            .into(),
                    });
                } else {
                    untracked_row(&mut out, &marker_rel);
                }
            }
        }
    }

    // Disk view: files the server view does not expect. Compared through FOLD keys (Codex
    // round 5): after a case-only rename the directory entry keeps its old case spelling — a
    // cosmetic filesystem alias of the expected path, not a stray file.
    // Two DISTINCT expectations landing on one fold key mean two server nodes project onto
    // the same physical file on this filesystem — apply would structurally park one, but a
    // silent collapse here would let doctor report CLEAN over the live collision (Codex round
    // 7: sync `Foo`, then the server grows `foo` before the next sync). The first claimant
    // keeps the expectation; the loser gets its own row.
    let mut folded_full: BTreeMap<String, (String, Uuid, TrackedKind)> = BTreeMap::new();
    for (k, v) in &expected_locals {
        let fk = localpath::fold_key(k, rules);
        match folded_full.get_mut(&fk) {
            Some(cur) if cur.1 != v.0 => {
                // The side STATE tracks at its exact path keeps the expectation (it owns the
                // physical file); the interloper gets the row. Ties fall to map order.
                // Trackedness compares the node's MATERIALIZING path (an attachment's
                // expectation key is its MARKER, never its binary path — Codex round 9).
                let tracks = |nid: &Uuid, key: &str| {
                    state.nodes.get(nid).is_some_and(|n| match n.kind {
                        TrackedKind::Attachment => n.marker_path.as_deref() == Some(key),
                        _ => n.local_path == key,
                    })
                };
                let cur_tracked = tracks(&cur.1, &cur.0);
                let new_tracked = tracks(&v.0, k);
                let (loser_path, winner_id) = if new_tracked && !cur_tracked {
                    let loser = (cur.0.clone(), cur.1);
                    *cur = (k.clone(), v.0, v.1);
                    (loser.0, v.0)
                } else {
                    (k.clone(), cur.1)
                };
                out.push(Discrepancy {
                    class: "fold-collision",
                    path: loser_path,
                    detail: format!(
                        "this server path maps to the same physical path as node {winner_id} \
                         on this filesystem — `docli sync` will leave one of the conflicting \
                         nodes unmaterialized"
                    ),
                });
            }
            Some(_) => {}
            None => {
                folded_full.insert(fk, (k.clone(), v.0, v.1));
            }
        }
    }
    let expected_folded: BTreeMap<String, (Uuid, TrackedKind)> = folded_full
        .into_iter()
        .map(|(fk, (_, id, kind))| (fk, (id, kind)))
        .collect();
    scan_disk(
        &handle.root,
        &handle.root,
        rules,
        &expected_folded,
        &mut out,
    )?;

    // Stray relocated markers: files in this workspace's OWN `.docli/markers/<ws>/` subdir no
    // expectation names — state loss plus a remote hard delete leaves one that no replay can
    // name again (Codex round 10; the from-zero sweep in sync is the healer, this is the
    // detector). The subdir is per-workspace BY CONSTRUCTION (Codex rounds 11–12: a shared
    // dir forced cross-workspace inventory, which raced concurrent sibling syncs), so the scan
    // never reasons about siblings at all.
    // Only RELOCATED expectations feed the keep-set (Codex round 13): a mount-local derived
    // marker's basename would mask a same-named orphan in the namespace.
    let expected_markers: std::collections::BTreeSet<String> = expected_locals
        .iter()
        .filter_map(|(k, _)| crate::apply::relocated_leaf(k, ws))
        .map(str::to_string)
        .collect();
    let ws_markers = control.markers_dir().join(ws.to_string());
    if ws_markers.is_dir() {
        for e in std::fs::read_dir(&ws_markers)? {
            let name = e?.file_name();
            let leaf = name.to_string_lossy();
            if !expected_markers.contains(leaf.as_ref()) {
                // write_atomic also writes relocated markers, so its crash residue can land
                // HERE too (round-2 R4) — same class, same working remediation as in the
                // mount tree, never mislabeled as a stray marker.
                let (class, detail) = if crate::mountfs::is_write_temp(leaf.as_ref()) {
                    (
                        "crash-residue",
                        "a temporary file left by an interrupted docli write — \
                         `docli sync --full` removes it",
                    )
                } else {
                    (
                        "missing-remote",
                        "stray relocated marker — no server attachment names it; \
                         `docli sync --full` removes it",
                    )
                };
                out.push(Discrepancy {
                    class,
                    path: format!(".docli/markers/{ws}/{leaf}"),
                    detail: detail.into(),
                });
            }
        }
    }

    // State orphans: entries with no server node.
    for (id, n) in &state.nodes {
        if !server.contains_key(id) {
            out.push(Discrepancy {
                class: "state-orphan",
                path: n.local_path.clone(),
                detail: format!("state tracks node {id}, which is no longer on the server"),
            });
        }
    }
    Ok(out)
}

fn scan_disk(
    root: &Path,
    dir: &Path,
    rules: &FsRules,
    expected: &BTreeMap<String, (Uuid, TrackedKind)>,
    out: &mut Vec<Discrepancy>,
) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let p = entry.path();
        let rel = p
            .strip_prefix(root)
            .expect("under root")
            .to_string_lossy()
            .replace('\\', "/");
        if rel == MOUNT_MARKER || rel == INCOMPLETE_MARKER {
            continue;
        }
        let folded = localpath::fold_key(&rel, rules);
        if entry.file_type()?.is_dir() {
            if !expected.contains_key(&folded) {
                out.push(Discrepancy {
                    class: "missing-remote",
                    path: rel.clone(),
                    detail: "directory exists locally but not on the server".into(),
                });
            }
            scan_disk(root, &p, rules, expected, out)?;
        } else {
            if !expected.contains_key(&folded) {
                // write_atomic's crash residue gets its own class — the generic
                // missing-remote advice ("hand-created files are never synced") is wrong for
                // a file the CLI itself left behind, and this one's remediation works.
                let leaf = rel.rsplit('/').next().unwrap_or(&rel);
                if crate::mountfs::is_write_temp(leaf) {
                    out.push(Discrepancy {
                        class: "crash-residue",
                        path: rel.clone(),
                        detail: "a temporary file left by an interrupted docli write — \
                                 `docli sync --full` removes it"
                            .into(),
                    });
                } else {
                    out.push(Discrepancy {
                        class: "missing-remote",
                        path: rel,
                        detail: "file exists locally but not on the server (hand-created files \
                                 in the mirror are never synced)"
                            .into(),
                    });
                }
            }
        }
    }
    Ok(())
}
