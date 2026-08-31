// SPDX-FileCopyrightText: 2026 OOO Agitek
// SPDX-License-Identifier: MIT

//! The APPLY engine (v0.28.0 D3) — the read subset of the plugin's `applyRemote`, re-shaped for
//! a client that owns no push side: converge the mirror to delivered state, id-keyed (a remote
//! rename lands as a real MOVE — same id, state re-keyed; the identity-churn pin).
//!
//! NO reconcile, NO push, NO tombstone outbox, NO 3-way merge. Two ordering laws keep every
//! crash window byte-equal-adoptable (D2):
//!  - a delivery changing path and/or body NEVER renames — it writes the FINAL bytes at the
//!    destination, then deletes the old path. (The pure-move `rename()` fast-path the law
//!    permits is deliberately not taken: write-then-delete is its universally-safe degrade
//!    branch, correct for compound moves, occupied destinations, and every crash window alike —
//!    and the server re-delivers every descendant of a moved folder anyway, so there is no
//!    subtree to relocate wholesale.)
//!  - removal of tracked FOLDERS is LEAF-FIRST and never recursive: tracked children are
//!    removed individually, and a directory left non-empty by an untracked occupant PARKS in
//!    place (the CLI never deletes content it does not own, in the removal direction too).
//!
//! Occupancy: emptiness-at-claim does not protect LATER untracked occupants, so every write to
//! a path NOT in state checks the disk — a byte-equal occupant is ADOPTED silently (an
//! idempotent redelivery, which is exactly what a crash between the FS write and the state
//! commit produces on the next cycle, so every write-then-commit window self-heals without a
//! journal), while a DIVERGENT occupant PARKS the delivery.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use docli_sync_wire::WireNode;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::localpath::{
    fold_key, has_reserved_segment, in_docli_namespace, project, scope_relative,
};
use crate::markers::{derived_marker_path, render_marker, CONTROL_FILES};
use crate::mountfs::{contained_join, remove_owned_file, set_readonly};
use crate::platform::FsRules;
use crate::state::{ControlRoot, NodeState, Park, ParkClass, TrackedKind, WsState};

#[derive(Debug, Default)]
pub struct ApplyStats {
    pub written: usize,
    pub adopted: usize,
    pub removed: usize,
    pub parked: usize,
    /// Directories whose removal is still pending (non-empty when tried) — MOUNT-RELATIVE
    /// paths. The sync loop folds these into the DURABLE `WsState::pending_removals`
    /// (Codex round 2; path-keyed and park-decoupled per round 3).
    pub pending_dir_removals: Vec<String>,
}

/// Fold an optional path (the marker half of the physical-move predicate).
fn fold_opt(p: Option<&str>, rules: &FsRules) -> Option<String> {
    p.map(|v| fold_key(v, rules))
}

fn sha_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn kind_of(node: &WireNode) -> Option<TrackedKind> {
    match node.kind.as_str() {
        "file" => Some(TrackedKind::Note),
        "folder" => Some(TrackedKind::Folder),
        "attachment" => Some(TrackedKind::Attachment),
        _ => None,
    }
}

/// Where an attachment's marker lives (see `NodeState.marker_path`): a mount-relative path, or
/// the relocated control-root form `.docli/markers/<ws>/<id>.docli` (unambiguous — the mount
/// is canonically disjoint from the control plane, so no mount path starts with `.docli/`).
/// The subdir is PER WORKSPACE (Codex round 12): the markers dir is project-global while mount
/// locks are per-mount, so any cross-workspace inventory (sweep, stray scan) would race a
/// concurrent sibling sync; a namespace makes each workspace's set self-contained instead.
fn relocated_marker(ws: Uuid, id: Uuid) -> String {
    format!(".docli/markers/{ws}/{id}.docli")
}

/// The leaf of a relocated marker path — ONLY when it sits in THIS workspace's own namespace
/// and is a single clean component (Codex round 13): a state path under a SIBLING's subdir (or
/// a traversal shape) must never resolve as ours, or A's delete/read reaches B's marker across
/// their independent mount locks. Every consumer of a state `marker_path` that starts with
/// `.docli/` goes through this.
pub(crate) fn relocated_leaf(marker_path: &str, ws: Uuid) -> Option<&str> {
    let leaf = marker_path
        .strip_prefix(".docli/markers/")?
        .strip_prefix(ws.to_string().as_str())?
        .strip_prefix('/')?;
    // The strictest possible shape (Codex rounds 14–15): every legitimate leaf is EXACTLY the
    // `<uuid>.docli` that `relocated_marker` generates, so validate that shape outright —
    // separator smuggling (`/`, `\`), traversal, and Windows drive prefixes (`C:x`, which
    // `PathBuf::join` treats as a base-discarding prefix) all fail the uuid parse.
    let stem = leaf.strip_suffix(".docli")?;
    Uuid::parse_str(stem).ok()?;
    Some(leaf)
}

fn marker_abs(
    control: &ControlRoot,
    mount_root: &Path,
    ws: Uuid,
    marker_path: &str,
) -> Result<PathBuf> {
    if marker_path.starts_with(".docli/") {
        // Same containment as the write side (`perform_put`): state-derived strings are
        // trusted for paths only after containment, and the DELETE path must never be weaker
        // than the write path (Codex round 6 — a tampered `.docli/markers/../../x` leaf would
        // have escaped `markers/` into `remove_owned_file`; round 13 — a path under a SIBLING
        // workspace's subdir would delete that sibling's marker).
        let Some(leaf) = relocated_leaf(marker_path, ws) else {
            anyhow::bail!(
                "marker path {marker_path:?} escapes this workspace's marker namespace — \
                 refusing (containment)"
            );
        };
        Ok(control.markers_dir().join(ws.to_string()).join(leaf))
    } else if marker_path.is_empty() {
        anyhow::bail!("empty marker path — refusing (containment)")
    } else {
        contained_join(mount_root, marker_path)
    }
}

#[derive(Debug)]
enum WriteOutcome {
    Written,
    Adopted,
    Parked(String),
    /// The target is occupied by a DIRECTORY the CLI tracks (a legal `Архив.md` folder being
    /// replaced by a note in this same page): deletions are deferred, so the write must RETRY
    /// after the delete phase rather than abort (an abort here would never advance the cursor —
    /// the same page would replay forever, and `--full` would reproduce it).
    DirInTheWay,
}

/// Write `bytes` at `local` under the D2 occupancy rules. `idx` and `vacated` are FOLD-keyed.
fn write_tracked(
    idx: &ClaimIndex,
    rules: &FsRules,
    mount_root: &Path,
    local: &str,
    bytes: &[u8],
    vacated: &std::collections::HashSet<String>,
) -> Result<WriteOutcome> {
    let target = contained_join(mount_root, local)?;
    if let Some(parent) = target.parent() {
        // A failed ancestor mkdir (a FILE where a directory must go — a cross-page collision
        // park's descendant, a hand-dropped occupant) PARKS the delivery rather than aborting
        // the page: an abort never advances the cursor and wedges the mount (Codex round 4).
        if let Err(e) = fs::create_dir_all(parent) {
            return Ok(WriteOutcome::Parked(format!(
                "cannot create the parent directory for {local} ({e}) — run `docli sync --full` \
                 once the blocking file is gone"
            )));
        }
    }
    // A path some tracked node vacated EARLIER IN THIS PAGE is still ours (the swap shape:
    // a→b + b→a re-keys state before the occupant check runs) — owned, not an occupant.
    let k = fold_key(local, rules);
    let occupant = idx
        .get(&k)
        .map(|_| ())
        .or_else(|| vacated.contains(&k).then_some(()));
    if target.exists() {
        if fs::metadata(&target)?.is_dir() {
            return Ok(match occupant {
                // A tracked directory (folders may legally be named `Архив.md`) being vacated
                // this page — deletions are deferred, so retry after them.
                Some(_) => WriteOutcome::DirInTheWay,
                None => WriteOutcome::Parked(format!(
                    "an untracked directory occupies {local} — remove it and run `docli sync --full`"
                )),
            });
        }
        match occupant {
            // Our own path (an in-place body update) — or a path another TRACKED node is
            // vacating this same page (a swap): tracked either way, so the CLI owns the bytes.
            Some(_) => {
                // Lift read-only first (Windows refuses a rename OVER a read-only target),
                // then the atomic swap — the temp carries the read-only bit through.
                set_readonly(&target, false)?;
                crate::mountfs::write_atomic(&target, bytes)?;
                Ok(WriteOutcome::Written)
            }
            None => {
                let existing = fs::read(&target)
                    .with_context(|| format!("reading occupant {}", target.display()))?;
                if existing == bytes {
                    // Byte-equal ⇒ ADOPT (crash-consistency's happy path).
                    let _ = set_readonly(&target, true);
                    Ok(WriteOutcome::Adopted)
                } else {
                    Ok(WriteOutcome::Parked(format!(
                        "a divergent untracked file occupies {local} — remove it and run \
                         `docli sync --full`"
                    )))
                }
            }
        }
    } else {
        crate::mountfs::write_atomic(&target, bytes)?;
        Ok(WriteOutcome::Written)
    }
}

/// The page-local CLAIM INDEX: every local path a tracked node MATERIALIZES (a note/folder's
/// `local_path`, an attachment's marker path — never an attachment's binary path, which holds
/// no file under the marker-only contract; Codex round 7: counting it as "ours" let a
/// same-fold successor overwrite a user's untracked file there instead of parking) → the
/// owning id, **keyed by FOLD KEY** (Codex round 5):
/// ownership answers "is this PHYSICAL file ours?", and on a folding filesystem `Foo.md` and
/// `foo.md` are one physical file — an exact-string index read a case-only respelling as an
/// untracked divergent occupant. The fold-guard invariant (no two live nodes fold-equal) is
/// what makes a fold key an unambiguous claim. `WsState::id_at_local` stays the exact-string
/// point lookup for search/doctor; this index exists for the apply pass. Built once per
/// `apply_page`/`prune_undelivered` call, mutated ONLY through [`track_node`]/[`untrack_node`]
/// so it cannot drift from `state.nodes`.
type ClaimIndex = std::collections::HashMap<String, Uuid>;

fn build_claim_index(state: &WsState, rules: &FsRules) -> ClaimIndex {
    let mut idx = ClaimIndex::with_capacity(state.nodes.len() * 2);
    for (id, n) in &state.nodes {
        // Seed claims only from nodes the LEDGER still holds live (Codex round 2): during a
        // from-zero replay the ledger was reset and refills from the replay itself, so a
        // hard-purged incumbent — one the server will never redeliver — cannot structurally
        // park its same-path replacement (the prune would later remove the incumbent, but
        // nothing would retry the parked replacement, and `--check` would pass over the
        // absence). Outside a from-zero this filter is a no-op: every tracked node was
        // delivered live, so `nodes ⊆ ledger` by construction. Ownership stays PAGE-START
        // (`pre_locals` is built unfiltered), so replay overwrites of stale incumbents' files
        // still take the owned branch.
        if !state.ledger.contains(id) {
            continue;
        }
        if n.kind != TrackedKind::Attachment {
            idx.insert(fold_key(&n.local_path, rules), *id);
        }
        if let Some(mp) = &n.marker_path {
            idx.insert(fold_key(mp, rules), *id);
        }
    }
    idx
}

fn track_node(state: &mut WsState, idx: &mut ClaimIndex, rules: &FsRules, id: Uuid, ns: NodeState) {
    if let Some(prev) = state.nodes.get(&id) {
        let k = fold_key(&prev.local_path, rules);
        if idx.get(&k) == Some(&id) {
            idx.remove(&k);
        }
        if let Some(mp) = &prev.marker_path {
            let k = fold_key(mp, rules);
            if idx.get(&k) == Some(&id) {
                idx.remove(&k);
            }
        }
    }
    if ns.kind != TrackedKind::Attachment {
        idx.insert(fold_key(&ns.local_path, rules), id);
    }
    if let Some(mp) = &ns.marker_path {
        idx.insert(fold_key(mp, rules), id);
    }
    state.nodes.insert(id, ns);
}

fn untrack_node(
    state: &mut WsState,
    idx: &mut ClaimIndex,
    rules: &FsRules,
    id: Uuid,
) -> Option<NodeState> {
    let prev = state.nodes.remove(&id)?;
    // Remove only entries THIS id still owns — a same-page successor may have re-claimed them.
    let k = fold_key(&prev.local_path, rules);
    if idx.get(&k) == Some(&id) {
        idx.remove(&k);
    }
    if let Some(mp) = &prev.marker_path {
        let k = fold_key(mp, rules);
        if idx.get(&k) == Some(&id) {
            idx.remove(&k);
        }
    }
    Some(prev)
}

/// One delivered node's planned effect, decided in the PLAN phase (no disk yet).
enum Disp {
    /// Ledger-only: unknown kind, reserved-segment skip, never-tracked out-of-scope, scope root.
    LedgerOnly,
    /// Untrack + delete the previous materialization (tombstone, scope-exit, guard eviction).
    Remove,
    /// Park (guards) — also removes any previous materialization.
    Park { class: ParkClass, reason: String },
    /// Materialize at `local`.
    Put {
        kind: TrackedKind,
        local: String,
        bytes: Vec<u8>,
        /// For attachments: where the sidecar goes (mount-relative or the relocated form).
        marker_path: Option<String>,
    },
}

struct Delivery<'a> {
    node: &'a WireNode,
    disp: Disp,
}

/// Apply one delivered page, in FOUR phases — plan, collision-park, write, delete — because a
/// one-pass loop cannot tell a SWAP (a↔b in one page: both paths stay claimed, nothing parks)
/// from a GENUINE projection/fold collision (two live nodes on one local spelling: the later
/// one parks). The collision verdict needs the page's FINAL claim map, so classification runs
/// before any disk write and deletions run after all of them.
pub fn apply_page(
    state: &mut WsState,
    rules: &FsRules,
    mount_root: &Path,
    control: &ControlRoot,
    ws: Uuid,
    scope: Option<&str>,
    nodes: &[WireNode],
) -> Result<ApplyStats> {
    let mut stats = ApplyStats::default();
    let mut idx = build_claim_index(state, rules);

    // The locals the CLI owned BEFORE this page — a write landing on any of them overwrites
    // bytes the CLI itself put there (a move target mid-swap included), never a user's file.
    let pre_locals: std::collections::HashSet<String> = state
        .nodes
        .values()
        .flat_map(|n| {
            // Same materializing rule as the ClaimIndex (Codex round 7): an attachment's
            // binary path holds no file, so it is never "ours" for the vacated-path check.
            let mut v = Vec::new();
            if n.kind != TrackedKind::Attachment {
                v.push(fold_key(&n.local_path, rules));
            }
            if let Some(m) = &n.marker_path {
                if !m.starts_with(".docli/") {
                    v.push(fold_key(m, rules));
                }
            }
            v
        })
        .collect();

    // ---- PLAN ------------------------------------------------------------------------------
    let mut deliveries: Vec<Delivery> = nodes
        .iter()
        .map(|node| Delivery {
            node,
            disp: classify(state, &idx, rules, ws, scope, node),
        })
        .collect();

    // ---- COLLISION + ANCESTOR PARKS (re-derived to a fixpoint) -------------------------------
    // Earlier claimants win: incumbents seed the maps, then non-parked deliveries in wire
    // order. The verdict is RE-DERIVED FROM SCRATCH each round against the previous round's
    // park set (Codex round 5): monotone accumulation cannot release the claim of a delivery
    // that a LATER round parks — a parked folder's child kept its projected claim and
    // suppressed the legitimate sibling for good. Re-deriving lets that claim evaporate.
    // Ancestor prefixes seed from DURABLE `state.parks` (a folder parked on page 1 takes its
    // page-2 children down) plus classify-level parks plus the previous round's set. Rounds
    // are capped (the function is not monotone, so a pathological input could oscillate); on
    // cap-hit the LAST derivation stands — deterministic, and only ever over-parks.
    let delivered_ids: std::collections::HashSet<Uuid> =
        deliveries.iter().map(|d| d.node.id).collect();
    let durable_prefixes: Vec<String> = state
        .parks
        .iter()
        .filter(|(pid, _)| !delivered_ids.contains(pid))
        .filter_map(|(_, park)| scope_relative(&park.server_path, scope))
        .filter(|rel| !rel.is_empty())
        .map(|rel| format!("{rel}/"))
        .collect();
    let incumbent_claims: Vec<(String, String, Uuid)> = state
        .nodes
        .iter()
        // Incumbents claim only while the LEDGER holds them live — the same from-zero rule as
        // `build_claim_index` (a hard-purged incumbent must not park its replacement).
        .filter(|(id, _)| !delivered_ids.contains(id) && state.ledger.contains(id))
        .map(|(id, n)| (n.local_path.clone(), fold_key(&n.local_path, rules), *id))
        .collect();
    let mut parked: std::collections::HashMap<Uuid, String> = std::collections::HashMap::new();
    for _round in 0..deliveries.len() + 2 {
        let mut prefixes = durable_prefixes.clone();
        for d in &deliveries {
            let is_parked = matches!(d.disp, Disp::Park { .. }) || parked.contains_key(&d.node.id);
            if is_parked {
                if let Some(rel) = scope_relative(&d.node.path, scope) {
                    if !rel.is_empty() {
                        prefixes.push(format!("{rel}/"));
                    }
                }
            }
        }
        let mut exact_claims: std::collections::HashMap<&str, Uuid> =
            std::collections::HashMap::new();
        let mut fold_claims: std::collections::HashMap<String, Uuid> =
            std::collections::HashMap::new();
        for (local, fk, id) in &incumbent_claims {
            exact_claims.entry(local.as_str()).or_insert(*id);
            fold_claims.entry(fk.clone()).or_insert(*id);
        }
        let mut next: std::collections::HashMap<Uuid, String> = std::collections::HashMap::new();
        for d in &deliveries {
            let Disp::Put { local, .. } = &d.disp else {
                continue;
            };
            let id = d.node.id;
            let rel = scope_relative(&d.node.path, scope).unwrap_or(&d.node.path);
            if prefixes.iter().any(|p| rel.starts_with(p.as_str())) {
                next.insert(
                    id,
                    "inside a parked folder — healed by a server-side rename of the ancestor"
                        .to_string(),
                );
                continue;
            }
            if let Some(oid) = exact_claims.get(local.as_str()).filter(|oid| **oid != id) {
                next.insert(
                    id,
                    format!("local path {local} is already the projection of another node ({oid})"),
                );
                continue;
            }
            let fk = fold_key(local, rules);
            if let Some(oid) = fold_claims.get(&fk).filter(|oid| **oid != id) {
                next.insert(
                    id,
                    format!("folds onto the same file as another node ({oid})"),
                );
                continue;
            }
            exact_claims.entry(local.as_str()).or_insert(id);
            fold_claims.entry(fk).or_insert(id);
        }
        let stable = next.len() == parked.len() && next.keys().all(|k| parked.contains_key(k));
        parked = next;
        if stable {
            break;
        }
    }
    for (id, reason) in parked {
        for d in deliveries.iter_mut().filter(|d| d.node.id == id) {
            d.disp = Disp::Park {
                class: ParkClass::Structural,
                reason: reason.clone(),
            };
        }
    }

    // ---- WRITES (wire order; deletions deferred) ---------------------------------------------
    let mut removals: Vec<(Uuid, NodeState)> = Vec::new();
    // Puts blocked by a TRACKED occupant of the other kind (a folder legally named `Архив.md`
    // replaced by a note, or the inverse) — the blocker's deletion is deferred, so these RETRY
    // once after the delete phase instead of aborting or mis-parking.
    struct RetryPut {
        id: Uuid,
        kind: TrackedKind,
        local: String,
        bytes: Vec<u8>,
        marker_path: Option<String>,
        server_path: String,
        rev: i64,
        prev: Option<NodeState>,
    }
    let mut retries: Vec<RetryPut> = Vec::new();
    for d in &deliveries {
        let node = d.node;
        let id = node.id;
        // Ledger first — it is wire-derived and disposition-independent.
        if node.trashed {
            state.ledger.remove(&id);
        } else {
            state.ledger.insert(id);
        }
        match &d.disp {
            // A ledger-only delivery still clears any standing park for the id (Codex round
            // 6): a PARKED node is untracked, so when it is later trashed / moved out of
            // scope / renamed onto a reserved segment, `classify` lands here — and without
            // this the obsolete park outlives its premise forever (`--check` red, its old
            // path structurally parking later descendants via the durable prefixes).
            Disp::LedgerOnly => {
                state.parks.remove(&id);
            }
            Disp::Remove => {
                state.parks.remove(&id);
                if let Some(prev) = untrack_node(state, &mut idx, rules, id) {
                    removals.push((id, prev));
                    stats.removed += 1;
                }
            }
            Disp::Park { class, reason } => {
                if let Some(prev) = untrack_node(state, &mut idx, rules, id) {
                    removals.push((id, prev));
                }
                state.parks.insert(
                    id,
                    Park {
                        class: *class,
                        reason: reason.clone(),
                        server_path: node.path.clone(),
                    },
                );
                stats.parked += 1;
            }
            Disp::Put {
                kind,
                local,
                bytes,
                marker_path,
            } => {
                let prev = state.nodes.get(&id).cloned();
                let outcome = perform_put(
                    &idx,
                    rules,
                    mount_root,
                    control,
                    kind,
                    local,
                    bytes,
                    marker_path,
                    &pre_locals,
                )?;
                match outcome {
                    WriteOutcome::DirInTheWay => {
                        retries.push(RetryPut {
                            id,
                            kind: *kind,
                            local: local.clone(),
                            bytes: bytes.clone(),
                            marker_path: marker_path.clone(),
                            server_path: node.path.clone(),
                            rev: node.rev,
                            prev,
                        });
                        continue;
                    }
                    WriteOutcome::Parked(reason) => {
                        if let Some(prev) = untrack_node(state, &mut idx, rules, id) {
                            removals.push((id, prev));
                        }
                        state.parks.insert(
                            id,
                            Park {
                                class: ParkClass::Transient,
                                reason,
                                server_path: node.path.clone(),
                            },
                        );
                        stats.parked += 1;
                        continue;
                    }
                    WriteOutcome::Written => stats.written += 1,
                    WriteOutcome::Adopted => stats.adopted += 1,
                }
                // Healed: this node materialized, so any prior park is gone; a moved node's old
                // materialization goes to the deferred delete pass (write-then-delete, D2).
                state.parks.remove(&id);
                if let Some(p) = prev {
                    // PHYSICAL move only (Codex round 4): a case-only rename (`Foo.md` →
                    // `foo.md`) folds onto the SAME file on a case-insensitive filesystem —
                    // scheduling the "old" path for deletion would delete the note we just
                    // adopted. The directory entry keeps its old case spelling (cosmetic,
                    // exactly what Finder/Obsidian do); the fold guard already parks
                    // cross-NODE case twins, so a fold-equal old path is always our own file.
                    let moved = fold_key(&p.local_path, rules) != fold_key(local, rules)
                        || (p.kind == TrackedKind::Attachment
                            && fold_opt(p.marker_path.as_deref(), rules)
                                != fold_opt(marker_path.as_deref(), rules));
                    if moved {
                        removals.push((id, p));
                    }
                }
                track_node(
                    state,
                    &mut idx,
                    rules,
                    id,
                    NodeState {
                        server_path: node.path.clone(),
                        local_path: local.clone(),
                        kind: *kind,
                        rev: node.rev,
                        content_sha256: if *kind == TrackedKind::Folder {
                            String::new()
                        } else {
                            sha_hex(bytes)
                        },
                        marker_path: marker_path.clone(),
                    },
                );
            }
        }
    }

    // ---- DELETES (write-then-delete: only now do old paths go) -------------------------------
    let mut dirs: Vec<(Uuid, String)> = Vec::new();
    for (id, prev) in &removals {
        remove_materialization(&idx, rules, mount_root, control, ws, *id, prev, &mut dirs)?;
    }
    // Leaf-first directory removal: deepest first; a non-empty dir stays for the caller's
    // end-of-run re-attempt (its tracked children may arrive on a later page).
    dirs.sort_by_key(|(_, d)| std::cmp::Reverse(d.matches('/').count()));
    for (_, rel) in dirs {
        if rel.is_empty() {
            continue; // empty resolves to the root itself — never ours (Codex round 17)
        }
        let Ok(d) = contained_join(mount_root, &rel) else {
            continue; // corrupted state path — nothing we materialized (Codex round 16)
        };
        if d.exists() && fs::remove_dir(&d).is_err() {
            stats.pending_dir_removals.push(rel);
        }
    }

    // ---- RETRIES (kind-swap deliveries whose blocker just vacated) ---------------------------
    // Note the ordering caveat this pass accepts, stated: for a kind swap the delete necessarily
    // precedes the write (a dir and a file cannot coexist at one path). A crash in that window
    // leaves the path absent with state still tracking the OLD node — the mirror-vs-manifest
    // invalidator reads that as from-zero on the next run, which converges.
    // `pre_locals` is deliberately PAGE-START-scoped here too: a path the CLI owned at page
    // start stays ours through the retry even though the vacating node's state entry is gone by
    // now — recomputing ownership per phase would flip that branch silently.
    for r in retries {
        let outcome = perform_put(
            &idx,
            rules,
            mount_root,
            control,
            &r.kind,
            &r.local,
            &r.bytes,
            &r.marker_path,
            &pre_locals,
        )?;
        match outcome {
            ok @ (WriteOutcome::Written | WriteOutcome::Adopted) => {
                match ok {
                    WriteOutcome::Adopted => stats.adopted += 1,
                    _ => stats.written += 1,
                }
                state.parks.remove(&r.id);
                // INSERT FIRST, delete second — the same ordering the write phase gets from its
                // deferred-removals design. `remove_materialization`'s ownership guards ask
                // "does any tracked node still claim this path?", and running the delete while
                // the node's own STALE entry still points at the old path answers yes — the
                // delete is skipped and the old file orphans where neither the invalidator nor
                // the prune (both walk state) can ever see it (round-2 finding).
                let moved = r.prev.as_ref().is_some_and(|p| {
                    // The same PHYSICAL-move rule as the write phase (a case-only respelling
                    // must not delete its own file).
                    fold_key(&p.local_path, rules) != fold_key(&r.local, rules)
                        || (p.kind == TrackedKind::Attachment
                            && fold_opt(p.marker_path.as_deref(), rules)
                                != fold_opt(r.marker_path.as_deref(), rules))
                });
                track_node(
                    state,
                    &mut idx,
                    rules,
                    r.id,
                    NodeState {
                        server_path: r.server_path,
                        local_path: r.local.clone(),
                        kind: r.kind,
                        rev: r.rev,
                        content_sha256: if r.kind == TrackedKind::Folder {
                            String::new()
                        } else {
                            sha_hex(&r.bytes)
                        },
                        marker_path: r.marker_path.clone(),
                    },
                );
                if moved {
                    let p = r.prev.expect("moved implies prev");
                    // The new bytes exist, so write-then-delete ordering still holds.
                    let mut late_dirs: Vec<(Uuid, String)> = Vec::new();
                    remove_materialization(
                        &idx,
                        rules,
                        mount_root,
                        control,
                        ws,
                        r.id,
                        &p,
                        &mut late_dirs,
                    )?;
                    for (_, rel) in late_dirs {
                        if rel.is_empty() {
                            continue; // empty = the root itself (Codex round 17)
                        }
                        let Ok(d) = contained_join(mount_root, &rel) else {
                            continue; // corrupted state path (Codex round 16)
                        };
                        if d.exists() && fs::remove_dir(&d).is_err() {
                            stats.pending_dir_removals.push(rel);
                        }
                    }
                }
            }
            blocked @ (WriteOutcome::Parked(_) | WriteOutcome::DirInTheWay) => {
                let reason = match blocked {
                    WriteOutcome::DirInTheWay => format!(
                        "a directory still occupies {} — remove it and run `docli sync --full`",
                        r.local
                    ),
                    WriteOutcome::Parked(reason) => reason,
                    _ => unreachable!(),
                };
                if let Some(prev) = untrack_node(state, &mut idx, rules, r.id) {
                    let mut late_dirs: Vec<(Uuid, String)> = Vec::new();
                    remove_materialization(
                        &idx,
                        rules,
                        mount_root,
                        control,
                        ws,
                        r.id,
                        &prev,
                        &mut late_dirs,
                    )?;
                    for (_, rel) in late_dirs {
                        if rel.is_empty() {
                            continue; // empty = the root itself (Codex round 17)
                        }
                        let Ok(d) = contained_join(mount_root, &rel) else {
                            continue; // corrupted state path (Codex round 16)
                        };
                        if d.exists() && fs::remove_dir(&d).is_err() {
                            stats.pending_dir_removals.push(rel);
                        }
                    }
                }
                state.parks.insert(
                    r.id,
                    Park {
                        class: ParkClass::Transient,
                        reason,
                        server_path: r.server_path,
                    },
                );
                stats.parked += 1;
            }
        }
    }
    Ok(stats)
}

/// One put's disk effect (shared by the write phase and the retry pass).
#[allow(clippy::too_many_arguments)]
fn perform_put(
    idx: &ClaimIndex,
    rules: &FsRules,
    mount_root: &Path,
    control: &ControlRoot,
    kind: &TrackedKind,
    local: &str,
    bytes: &[u8],
    marker_path: &Option<String>,
    pre_locals: &std::collections::HashSet<String>,
) -> Result<WriteOutcome> {
    match kind {
        TrackedKind::Folder => {
            let dir = contained_join(mount_root, local)?;
            if dir.exists() && !fs::metadata(&dir)?.is_dir() {
                // A file where the folder goes. Tracked (a note legally named like a folder,
                // vacating this page) → retry after deletes; untracked → the occupant park.
                let k = fold_key(local, rules);
                let owned = idx.contains_key(&k) || pre_locals.contains(&k);
                return Ok(if owned {
                    WriteOutcome::DirInTheWay
                } else {
                    WriteOutcome::Parked(format!(
                        "an untracked file occupies the folder path {local} — remove it and                          run `docli sync --full`"
                    ))
                });
            }
            if let Err(e) = fs::create_dir_all(&dir) {
                // Same non-fatal rule as write_tracked's ancestor mkdir (a page must never
                // abort on a blocked path — it would replay forever).
                return Ok(WriteOutcome::Parked(format!(
                    "cannot create the directory {local} ({e}) — run `docli sync --full` once \
                     the blocking file is gone"
                )));
            }
            Ok(WriteOutcome::Written)
        }
        TrackedKind::Note => write_tracked(idx, rules, mount_root, local, bytes, pre_locals),
        TrackedKind::Attachment => {
            let mp = marker_path
                .as_deref()
                .expect("attachments carry a marker path");
            if let Some(leaf) = mp.strip_prefix(".docli/markers/") {
                // A relocated marker is a legitimate write OUTSIDE the mount root — the
                // containment model names two validated roots, and this one is validated too:
                // the leaf must resolve INSIDE markers/ (state-derived strings are trusted for
                // paths only after containment, D2). DELIBERATELY no occupancy rules here
                // (Codex round 6): `.docli/` is the CLI-owned, git-ignored, disposable control
                // plane — a pre-existing `markers/<id>.docli` is by construction a leftover of
                // a lost state file, and adopting/parking against our own leftovers would
                // wedge syncs to protect nothing. D2's occupancy contract guards USER files in
                // the MOUNT; it does not extend into the control root.
                let markers = control.markers_dir();
                let abs = crate::mountfs::contained_join(&markers, leaf)?;
                if let Some(parent) = abs.parent() {
                    fs::create_dir_all(parent)?;
                }
                // Our previous write left it read-only — lift before rewriting, or every
                // later revision of a relocated attachment wedges the page (Codex round 7).
                let _ = set_readonly(&abs, false);
                crate::mountfs::write_atomic(&abs, bytes)?;
                Ok(WriteOutcome::Written)
            } else {
                write_tracked(idx, rules, mount_root, mp, bytes, pre_locals)
            }
        }
    }
}

/// The PLAN-phase classification: guards, scope, projection — everything except the collision
/// verdict (which needs the whole page) and the disk (which waits for the verdict).
fn classify(
    state: &WsState,
    idx: &ClaimIndex,
    rules: &FsRules,
    ws: Uuid,
    scope: Option<&str>,
    node: &WireNode,
) -> Disp {
    let id = node.id;
    let tracked = state.nodes.contains_key(&id);
    let remove_if_tracked = || {
        if tracked {
            Disp::Remove
        } else {
            Disp::LedgerOnly
        }
    };
    if node.trashed {
        return remove_if_tracked();
    }
    // Unknown future kind: ledger-only (the plugin's isKnownKind shape) — but a node that was
    // PREVIOUSLY a known kind and became unknown must drop its stale materialization, or it
    // survives every prune (its id stays in the delivered ledger) and no CLI verb ever removes
    // it (Codex round 1).
    let Some(kind) = kind_of(node) else {
        return remove_if_tracked();
    };
    // Reserved segments are SKIPPED, not parked (creatable today via the unguarded web arm; a
    // mirror growing an `.obsidian/` would trip the CLI's own vault-ancestor rule against
    // itself). The id stays in the ledger like any parked delivery.
    if has_reserved_segment(&node.path) {
        return remove_if_tracked();
    }
    // The `.docli` control-namespace park (ANY segment suffix — a descendant of a grandfathered
    // `foo.docli/` folder would ancestor-create the parked folder, so the whole subtree stays
    // ledger-only).
    if in_docli_namespace(&node.path) {
        return Disp::Park {
            class: ParkClass::Structural,
            reason: "the .docli namespace is reserved for CLI control files".into(),
        };
    }
    // Scope: never-tracked out-of-scope nodes are skipped; a TRACKED node whose delivered path
    // leaves the scope is REMOVED (the plugin leaves it in place because its push side
    // re-mirrors it — a healer the read-only CLI deliberately lacks).
    let Some(rel) = scope_relative(&node.path, scope) else {
        return remove_if_tracked();
    };
    if rel.is_empty() {
        // The scope folder itself IS the mount root — nothing to materialize; and if this node
        // WAS materialized under a previous (wider) scope, that materialization must go with
        // the re-scope (the narrowing repro: an unscoped `docs/` dir would otherwise survive
        // every `--full`, tracked forever at a path the new scope maps to the root).
        return remove_if_tracked();
    }
    // The `.md` mirror guard (shared code via `docli_rules::is_note_name` — not a twin): a
    // `kind='file'` node at a non-note-named path is the A3 truncation class.
    if kind == TrackedKind::Note && !docli_rules::is_note_name(&node.name) {
        return Disp::Park {
            class: ParkClass::Structural,
            reason: format!("a note must be named *.md (server name: {})", node.name),
        };
    }
    // Project into the local namespace; the winPath length park lives in `project`.
    let local = match project(rel, rules) {
        Ok(l) => l,
        Err(e) => {
            return Disp::Park {
                class: ParkClass::Structural,
                reason: format!("not representable on this filesystem: {e:?}"),
            };
        }
    };
    let (bytes, marker_path) = match kind {
        TrackedKind::Folder => (Vec::new(), None),
        TrackedKind::Note => (node.body.clone().unwrap_or_default().into_bytes(), None),
        TrackedKind::Attachment => {
            let collides = |candidate: &str| -> bool {
                CONTROL_FILES
                    .iter()
                    .any(|c| c.eq_ignore_ascii_case(candidate))
                    || idx
                        .get(&fold_key(candidate, rules))
                        .is_some_and(|o| *o != id)
            };
            let mp = derived_marker_path(&local, rules, collides)
                .unwrap_or_else(|| relocated_marker(ws, id));
            (render_marker(node).into_bytes(), Some(mp))
        }
    };
    Disp::Put {
        kind,
        local,
        bytes,
        marker_path,
    }
}

/// Remove a previous materialization — skipping any path a tracked node currently claims (the
/// swap rule), lifting the read-only attribute (the Windows shape), collecting directories for
/// the leaf-first pass. `full` removes the node's own file; `!full` (a move source) removes
/// only paths the node no longer occupies.
/// Does some current claimant MATERIALIZE a file/dir at `path`? Since Codex round 7 the
/// ClaimIndex holds ONLY materializing paths (a note/folder's local, an attachment's marker —
/// never an attachment's binary path, which holds no file under the marker-only contract), so
/// this is a plain lookup (round 3's kind-aware check moved into the index build).
fn claim_materializes(idx: &ClaimIndex, rules: &FsRules, path: &str) -> bool {
    idx.contains_key(&fold_key(path, rules))
}

#[allow(clippy::too_many_arguments)]
fn remove_materialization(
    idx: &ClaimIndex,
    rules: &FsRules,
    mount_root: &Path,
    control: &ControlRoot,
    ws: Uuid,
    id: Uuid,
    prev: &NodeState,
    dirs: &mut Vec<(Uuid, String)>,
) -> Result<()> {
    match prev.kind {
        TrackedKind::Folder => {
            if !claim_materializes(idx, rules, &prev.local_path) {
                dirs.push((id, prev.local_path.clone()));
            }
        }
        TrackedKind::Note => {
            if !claim_materializes(idx, rules, &prev.local_path) {
                // An UNRESOLVABLE stored path is discarded, not an error (Codex round 16): a
                // corrupted `../outside.md` was never materialized by us, and erroring here
                // wedges the from-zero repair forever (every replay re-hits the same entry).
                // EMPTY resolves to the mount root itself (round 17) — equally never ours.
                if !prev.local_path.is_empty() {
                    if let Ok(p) = contained_join(mount_root, &prev.local_path) {
                        remove_owned_file(&p)?;
                    }
                }
            }
        }
        TrackedKind::Attachment => {
            if let Some(mp) = &prev.marker_path {
                if !claim_materializes(idx, rules, mp.as_str()) {
                    if let Ok(p) = marker_abs(control, mount_root, ws, mp) {
                        remove_owned_file(&p)?;
                    }
                }
            }
        }
    }
    Ok(())
}

/// The PRUNE arm (D3): a from-zero sync is AUTHORITATIVE — after replaying to head, delete
/// every tracked mirror file and marker the replay did not deliver. Exists because D2a
/// synthesizes `resync_required = false` (the server can never order a full reconcile for an
/// ephemeral client) and a from-zero PULL alone only re-delivers, never deletes.
pub fn prune_undelivered(
    state: &mut WsState,
    rules: &FsRules,
    mount_root: &Path,
    control: &ControlRoot,
    ws: Uuid,
    delivered: &BTreeSet<Uuid>,
) -> Result<Vec<String>> {
    let stale: Vec<Uuid> = state
        .nodes
        .keys()
        .filter(|id| !delivered.contains(id))
        .copied()
        .collect();
    let mut idx = build_claim_index(state, rules);
    let mut dirs: Vec<(Uuid, String)> = Vec::new();
    for id in stale {
        if let Some(prev) = untrack_node(state, &mut idx, rules, id) {
            remove_materialization(&idx, rules, mount_root, control, ws, id, &prev, &mut dirs)?;
        }
        state.ledger.remove(&id);
        state.parks.remove(&id);
    }
    dirs.sort_by_key(|(_, d)| std::cmp::Reverse(d.matches('/').count()));
    let mut pending = Vec::new();
    for (_, rel) in dirs {
        if rel.is_empty() {
            continue; // empty resolves to the root itself — never ours (Codex round 17)
        }
        let Ok(d) = contained_join(mount_root, &rel) else {
            continue; // corrupted state path — nothing we materialized (Codex round 16)
        };
        if d.exists() && fs::remove_dir(&d).is_err() {
            pending.push(rel);
        }
    }
    Ok(pending)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules() -> FsRules {
        FsRules {
            fold_case_insensitive: true,
            win_names: false,
            max_component_bytes: 255,
        }
    }

    fn win_rules() -> FsRules {
        FsRules {
            fold_case_insensitive: true,
            win_names: true,
            max_component_bytes: 255,
        }
    }

    struct Fx {
        _tmp: tempfile::TempDir,
        mount: PathBuf,
        control: ControlRoot,
        state: WsState,
    }

    fn fx() -> Fx {
        let tmp = tempfile::tempdir().unwrap();
        let mount = tmp.path().join("mirror");
        fs::create_dir_all(&mount).unwrap();
        let control = ControlRoot::new(tmp.path());
        fs::create_dir_all(&control.dir).unwrap();
        Fx {
            mount,
            control,
            state: WsState::fresh(None),
            _tmp: tmp,
        }
    }

    fn node(id: u128, kind: &str, path: &str, rev: i64, body: Option<&str>) -> WireNode {
        WireNode {
            id: Uuid::from_u128(id),
            parent_id: None,
            kind: kind.into(),
            name: path.rsplit('/').next().unwrap().into(),
            path: path.into(),
            rev,
            trashed: false,
            mime: (kind == "attachment").then(|| "image/png".to_string()),
            content_bytes: body.map(|b| b.len() as i32).unwrap_or(0),
            body: body.map(|b| b.to_string()),
            blob_url: (kind == "attachment").then(|| "/api/attachments/x".to_string()),
            position: None,
            sha256: None,
            blob_generation: (kind == "attachment").then_some(0),
        }
    }

    fn trashed(mut n: WireNode) -> WireNode {
        n.trashed = true;
        n
    }

    fn apply(fxt: &mut Fx, r: &FsRules, nodes: &[WireNode]) -> ApplyStats {
        apply_page(
            &mut fxt.state,
            r,
            &fxt.mount,
            &fxt.control,
            Uuid::from_u128(0xF),
            None,
            nodes,
        )
        .unwrap()
    }

    #[test]
    fn fresh_mirror_equals_server_tree_and_files_are_read_only() {
        let mut f = fx();
        apply(
            &mut f,
            &rules(),
            &[
                node(1, "folder", "docs", 1, None),
                node(2, "file", "docs/a.md", 2, Some("# hello")),
                node(3, "attachment", "docs/pic.png", 3, None),
            ],
        );
        let a = f.mount.join("docs/a.md");
        assert_eq!(fs::read_to_string(&a).unwrap(), "# hello");
        assert!(fs::metadata(&a).unwrap().permissions().readonly());
        let marker = f.mount.join("docs/pic.png.docli");
        assert!(fs::read_to_string(&marker)
            .unwrap()
            .contains("sha256 unknown"));
        // The binary's own path is never materialized (the A3 class).
        assert!(!f.mount.join("docs/pic.png").exists());
        assert_eq!(f.state.ledger.len(), 3);
    }

    #[test]
    fn a_remote_rename_lands_as_a_move_with_the_same_id() {
        let mut f = fx();
        apply(
            &mut f,
            &rules(),
            &[node(1, "file", "a.md", 1, Some("body"))],
        );
        apply(
            &mut f,
            &rules(),
            &[node(1, "file", "b.md", 2, Some("body"))],
        );
        assert!(!f.mount.join("a.md").exists(), "old path removed");
        assert_eq!(fs::read_to_string(f.mount.join("b.md")).unwrap(), "body");
        // Same id, state re-keyed — the identity-churn pin.
        assert_eq!(f.state.nodes[&Uuid::from_u128(1)].local_path, "b.md");
        assert_eq!(f.state.nodes.len(), 1);
    }

    #[test]
    fn a_swap_never_deletes_the_claimed_path() {
        let mut f = fx();
        apply(
            &mut f,
            &rules(),
            &[
                node(1, "file", "a.md", 1, Some("one")),
                node(2, "file", "b.md", 1, Some("two")),
            ],
        );
        // a↔b swap in one page.
        apply(
            &mut f,
            &rules(),
            &[
                node(1, "file", "b.md", 2, Some("one")),
                node(2, "file", "a.md", 2, Some("two")),
            ],
        );
        assert_eq!(fs::read_to_string(f.mount.join("b.md")).unwrap(), "one");
        assert_eq!(fs::read_to_string(f.mount.join("a.md")).unwrap(), "two");
    }

    #[test]
    fn folder_move_redelivery_converges_and_removes_the_old_tree() {
        let mut f = fx();
        apply(
            &mut f,
            &rules(),
            &[
                node(1, "folder", "old", 1, None),
                node(2, "file", "old/a.md", 2, Some("x")),
            ],
        );
        // The server stamps ONE rev across every descendant on a folder move — all re-delivered.
        apply(
            &mut f,
            &rules(),
            &[
                node(1, "folder", "new", 3, None),
                node(2, "file", "new/a.md", 3, Some("x")),
            ],
        );
        assert!(f.mount.join("new/a.md").exists());
        assert!(!f.mount.join("old").exists(), "old dir removed leaf-first");
    }

    #[test]
    fn trashed_removes_and_read_only_is_lifted() {
        let mut f = fx();
        apply(&mut f, &rules(), &[node(1, "file", "a.md", 1, Some("x"))]);
        assert!(fs::metadata(f.mount.join("a.md"))
            .unwrap()
            .permissions()
            .readonly());
        apply(
            &mut f,
            &rules(),
            &[trashed(node(1, "file", "a.md", 2, None))],
        );
        assert!(!f.mount.join("a.md").exists());
        assert!(f.state.ledger.is_empty());
        assert!(f.state.nodes.is_empty());
    }

    #[test]
    fn trashed_folder_with_untracked_occupant_is_preserved() {
        let mut f = fx();
        apply(
            &mut f,
            &rules(),
            &[
                node(1, "folder", "d", 1, None),
                node(2, "file", "d/a.md", 2, Some("x")),
            ],
        );
        // A hand-dropped untracked file inside the tracked folder.
        fs::write(f.mount.join("d/stray.txt"), "mine").unwrap();
        let stats = apply(
            &mut f,
            &rules(),
            &[
                trashed(node(1, "folder", "d", 3, None)),
                trashed(node(2, "file", "d/a.md", 3, None)),
            ],
        );
        assert!(!f.mount.join("d/a.md").exists(), "tracked child removed");
        assert!(
            f.mount.join("d/stray.txt").exists(),
            "untracked occupant preserved"
        );
        assert_eq!(
            stats.pending_dir_removals.len(),
            1,
            "the dir stays, reported"
        );
    }

    #[test]
    fn divergent_untracked_occupant_parks_and_byte_equal_adopts() {
        let mut f = fx();
        fs::write(f.mount.join("same.md"), "identical").unwrap();
        fs::write(f.mount.join("diff.md"), "MINE").unwrap();
        let stats = apply(
            &mut f,
            &rules(),
            &[
                node(1, "file", "same.md", 1, Some("identical")),
                node(2, "file", "diff.md", 1, Some("THEIRS")),
            ],
        );
        assert_eq!(stats.adopted, 1);
        assert_eq!(stats.parked, 1);
        assert_eq!(
            fs::read_to_string(f.mount.join("diff.md")).unwrap(),
            "MINE",
            "never overwrites what it does not own"
        );
        let park = &f.state.parks[&Uuid::from_u128(2)];
        assert_eq!(park.class, ParkClass::Transient);
        assert!(
            park.reason.contains("--full"),
            "the message names the repair: {}",
            park.reason
        );
        // The ledger still carries the parked id (the D2a comparand is wire-derived).
        assert!(f.state.ledger.contains(&Uuid::from_u128(2)));
    }

    #[test]
    fn fold_twins_park_instead_of_clobbering() {
        let mut f = fx();
        // NFC «Ёлка.md» vs NFD (Е + combining diaeresis) — one physical file on a folding
        // filesystem; both copies of a body would be destroyed by clobbering (the 0.4.0 e2e
        // finding this guard ports).
        let nfc = "Ёлка.md";
        let nfd = "Е\u{0308}лка.md";
        assert_ne!(nfc, nfd, "the fixture must be two spellings");
        let stats = apply(
            &mut f,
            &rules(),
            &[
                node(1, "file", nfc, 1, Some("one")),
                node(2, "file", nfd, 1, Some("two")),
            ],
        );
        assert_eq!(stats.parked, 1);
        assert_eq!(
            f.state.parks[&Uuid::from_u128(2)].class,
            ParkClass::Structural
        );
        assert_eq!(f.state.nodes.len(), 1, "only the first twin materializes");
    }

    #[test]
    fn case_twins_are_guard_legal_on_a_case_sensitive_filesystem() {
        // The GUARD decision only: an unconditional lowercase on Linux would park legitimate
        // twins (D3). The full both-files-materialize shape is not physically testable on a
        // case-folding dev disk (macOS APFS folds underneath the test), so the fold-key rule is
        // what is pinned — the vectors + the plugin-side test carry the cross-platform half.
        let cs = FsRules {
            fold_case_insensitive: false,
            ..rules()
        };
        assert_ne!(fold_key("a.md", &cs), fold_key("A.md", &cs));
        assert_eq!(fold_key("a.md", &rules()), fold_key("A.md", &rules()));
    }

    #[test]
    fn md_guard_parks_a_note_at_a_non_note_name() {
        let mut f = fx();
        let stats = apply(
            &mut f,
            &rules(),
            &[node(1, "file", "Map.canvas", 1, Some("{}"))],
        );
        assert_eq!(stats.parked, 1);
        assert!(
            !f.mount.join("Map.canvas").exists(),
            "the A3 write is refused"
        );
        assert_eq!(
            f.state.parks[&Uuid::from_u128(1)].class,
            ParkClass::Structural
        );
    }

    #[test]
    fn docli_namespace_parks_including_descendants_and_attachment_names() {
        let mut f = fx();
        let stats = apply(
            &mut f,
            &rules(),
            &[
                node(1, "folder", "foo.docli", 1, None),
                node(2, "file", "foo.docli/child.md", 2, Some("x")),
                node(3, "attachment", "x.png.docli", 3, None),
            ],
        );
        assert_eq!(stats.parked, 3);
        assert!(
            !f.mount.join("foo.docli").exists(),
            "the parked folder is never ancestor-created"
        );
        // Parked ids stay in the ledger.
        assert_eq!(f.state.ledger.len(), 3);
    }

    #[test]
    fn reserved_segments_are_skipped_but_stay_in_the_ledger() {
        let mut f = fx();
        let stats = apply(
            &mut f,
            &rules(),
            &[
                node(1, "folder", ".obsidian", 1, None),
                node(2, "file", ".git/x.md", 1, Some("s")),
            ],
        );
        assert_eq!(stats.parked, 0, "skips, not parks");
        assert!(!f.mount.join(".obsidian").exists());
        assert_eq!(f.state.ledger.len(), 2);
    }

    #[test]
    fn scope_filters_and_scope_exit_removes() {
        let mut f = fx();
        let r = rules();
        let scope = Some("docs");
        let go = |nodes: &[WireNode], f: &mut Fx| {
            apply_page(
                &mut f.state,
                &r,
                &f.mount,
                &f.control,
                Uuid::from_u128(0xF),
                scope,
                nodes,
            )
            .unwrap()
        };
        go(
            &[
                node(1, "file", "docs/in.md", 1, Some("in")),
                node(2, "file", "elsewhere/out.md", 1, Some("out")),
            ],
            &mut f,
        );
        assert!(
            f.mount.join("in.md").exists(),
            "scope-relative at the mount root"
        );
        assert!(!f.mount.join("elsewhere").exists());
        // Both ids are in the ledger (wire-derived — scope cannot fake a count mismatch).
        assert_eq!(f.state.ledger.len(), 2);
        // The tracked note moves OUT of scope ⇒ removed locally on delivery.
        go(&[node(1, "file", "moved/in.md", 2, Some("in"))], &mut f);
        assert!(!f.mount.join("in.md").exists());
        assert!(f.state.nodes.is_empty());
        assert_eq!(f.state.ledger.len(), 2, "still live on the wire");
    }

    #[test]
    fn windows_projection_writes_the_encoded_spelling_and_parks_collisions() {
        let mut f = fx();
        let stats = apply(
            &mut f,
            &win_rules(),
            &[
                node(1, "file", "a:b.md", 1, Some("colon")),
                // The literal local spelling of node 1's projection — a cross-domain collision.
                node(2, "file", "a%3Ab.md", 2, Some("literal")),
            ],
        );
        assert!(f.mount.join("a%3Ab.md").exists());
        assert_eq!(stats.parked, 1);
        assert_eq!(
            f.state.parks[&Uuid::from_u128(2)].class,
            ParkClass::Structural
        );
        // And %XX length overflow parks (never writes).
        let long = format!("{}.md", ":".repeat(120));
        let stats = apply(
            &mut f,
            &win_rules(),
            &[node(3, "file", &long, 3, Some("x"))],
        );
        assert_eq!(stats.parked, 1);
    }

    #[test]
    fn marker_relocates_on_control_file_collision_and_search_resolves_via_state() {
        let mut f = fx();
        apply(&mut f, &rules(), &[node(1, "attachment", "MOUNT", 1, None)]);
        let ns = &f.state.nodes[&Uuid::from_u128(1)];
        assert_eq!(
            ns.marker_path.as_deref(),
            Some(
                ".docli/markers/00000000-0000-0000-0000-00000000000f/\
                 00000000-0000-0000-0000-000000000001.docli"
            )
        );
        assert!(f
            .control
            .markers_dir()
            .join("00000000-0000-0000-0000-00000000000f/00000000-0000-0000-0000-000000000001.docli")
            .exists());
        assert!(
            !f.mount.join("MOUNT.docli").exists(),
            "control files win the mirror-root names"
        );
    }

    #[test]
    fn prune_removes_undelivered_but_preserves_untracked() {
        let mut f = fx();
        apply(
            &mut f,
            &rules(),
            &[
                node(1, "file", "keep.md", 1, Some("k")),
                node(2, "file", "stale.md", 1, Some("s")),
            ],
        );
        fs::write(f.mount.join("mine.txt"), "untracked").unwrap();
        let delivered: BTreeSet<Uuid> = [Uuid::from_u128(1)].into_iter().collect();
        prune_undelivered(
            &mut f.state,
            &rules(),
            &f.mount,
            &f.control,
            Uuid::from_u128(0xF),
            &delivered,
        )
        .unwrap();
        assert!(f.mount.join("keep.md").exists());
        assert!(!f.mount.join("stale.md").exists(), "the prune pin");
        assert!(
            f.mount.join("mine.txt").exists(),
            "never deletes what it does not own"
        );
        assert!(!f.state.nodes.contains_key(&Uuid::from_u128(2)));
        assert!(!f.state.ledger.contains(&Uuid::from_u128(2)));
    }

    #[test]
    fn crash_window_redelivery_adopts_byte_equal_state_drift() {
        let mut f = fx();
        // Simulate a crash between the FS write and the state commit: the file exists but the
        // state does not know it. The redelivery must ADOPT silently.
        fs::write(f.mount.join("a.md"), "body").unwrap();
        let stats = apply(
            &mut f,
            &rules(),
            &[node(1, "file", "a.md", 1, Some("body"))],
        );
        assert_eq!(stats.adopted, 1);
        assert_eq!(stats.parked, 0);
        assert!(f.state.nodes.contains_key(&Uuid::from_u128(1)));
    }

    #[test]
    fn a_kind_swap_converges_instead_of_wedging() {
        // The round-1 §4.1 repro: a TRACKED folder legally named `Архив.md` is trashed while a
        // note takes its path in the SAME page. Deletions are deferred, so the note's write
        // meets a tracked DIRECTORY — pre-fix this aborted with EISDIR, never advanced the
        // cursor, and no CLI verb could recover the mount.
        let mut f = fx();
        apply(
            &mut f,
            &rules(),
            &[
                node(1, "folder", "Архив.md", 1, None),
                node(2, "file", "Архив.md/note.md", 2, Some("x")),
            ],
        );
        assert!(f.mount.join("Архив.md").is_dir());
        let stats = apply(
            &mut f,
            &rules(),
            &[
                trashed(node(2, "file", "Архив.md/note.md", 3, None)),
                trashed(node(1, "folder", "Архив.md", 3, None)),
                node(3, "file", "Архив.md", 3, Some("now a note")),
            ],
        );
        assert_eq!(
            stats.parked, 0,
            "the retry pass converges, never parks the swap"
        );
        assert!(f.mount.join("Архив.md").is_file());
        assert_eq!(
            fs::read_to_string(f.mount.join("Архив.md")).unwrap(),
            "now a note"
        );
        // And the inverse: the note becomes a folder again.
        let stats = apply(
            &mut f,
            &rules(),
            &[
                trashed(node(3, "file", "Архив.md", 4, None)),
                node(4, "folder", "Архив.md", 4, None),
            ],
        );
        assert_eq!(stats.parked, 0);
        assert!(f.mount.join("Архив.md").is_dir());
    }

    #[test]
    fn a_retried_kind_swap_that_also_moved_deletes_its_old_path() {
        // The round-2 §2.1 repro: a note at `a.md` renames onto the path of a folder legally
        // named `X` that is trashed in the SAME page. The write retries after the delete phase;
        // the old `a.md` must then be REMOVED — with the delete running before the state
        // re-insert, the node's own stale entry used to shield it and the file orphaned where
        // neither the invalidator nor the prune (both walk state) could ever see it.
        let mut f = fx();
        apply(
            &mut f,
            &rules(),
            &[
                node(1, "folder", "X.md", 1, None),
                node(2, "file", "a.md", 1, Some("body")),
            ],
        );
        let stats = apply(
            &mut f,
            &rules(),
            &[
                trashed(node(1, "folder", "X.md", 2, None)),
                node(2, "file", "X.md", 2, Some("body")),
            ],
        );
        assert_eq!(stats.parked, 0);
        assert!(f.mount.join("X.md").is_file());
        assert!(
            !f.mount.join("a.md").exists(),
            "the moved-from path must not orphan (it is untracked after the move — no CLI verb \
             could ever remove it)"
        );
        assert_eq!(f.state.nodes[&Uuid::from_u128(2)].local_path, "X.md");
        assert_eq!(f.state.nodes.len(), 1);
    }

    #[test]
    fn a_case_only_rename_never_deletes_its_own_file() {
        // Codex round-4 P1: `Foo.md` → `foo.md` (same id, same bytes) folds onto ONE physical
        // file on a case-insensitive filesystem — the old spelling must not be scheduled for
        // deletion, or the note vanishes with state reading healthy.
        let mut f = fx();
        apply(
            &mut f,
            &rules(),
            &[node(1, "file", "Foo.md", 1, Some("body"))],
        );
        apply(
            &mut f,
            &rules(),
            &[node(1, "file", "foo.md", 2, Some("body"))],
        );
        // rules() folds case, so on ANY host filesystem the assertion below must hold through
        // the folded lens: the file exists under one of the two spellings and state tracks it.
        let survives = f.mount.join("foo.md").is_file() || f.mount.join("Foo.md").is_file();
        assert!(survives, "the note must survive a case-only respelling");
        assert_eq!(f.state.nodes[&Uuid::from_u128(1)].local_path, "foo.md");
        assert!(f.state.parks.is_empty());
    }

    #[test]
    fn a_case_only_rename_with_a_body_change_overwrites_instead_of_parking() {
        // Codex round-5 P1: `Foo.md` → `foo.md` WITH new bytes — the fold-keyed ownership must
        // read the existing physical file as OURS and overwrite it, not as a divergent
        // untracked occupant to park behind (which then deleted the old materialization).
        let mut f = fx();
        apply(
            &mut f,
            &rules(),
            &[node(1, "file", "Foo.md", 1, Some("old body"))],
        );
        let stats = apply(
            &mut f,
            &rules(),
            &[node(1, "file", "foo.md", 2, Some("new body"))],
        );
        assert_eq!(stats.parked, 0, "{:?}", f.state.parks);
        let on_disk = std::fs::read_to_string(f.mount.join("foo.md"))
            .or_else(|_| std::fs::read_to_string(f.mount.join("Foo.md")))
            .unwrap();
        assert_eq!(on_disk, "new body");
        assert_eq!(f.state.nodes[&Uuid::from_u128(1)].local_path, "foo.md");
    }

    #[test]
    fn a_parked_ancestor_from_an_earlier_page_parks_later_children_structurally() {
        // Codex round-5 P1: the folder parks on PAGE 1; its child arrives on PAGE 2. The
        // ancestor-park propagation must consult DURABLE parks, or the child falls into the
        // mkdir belt's TRANSIENT park with an unhealable "remove the blocking file" message.
        let mut f = fx();
        apply(
            &mut f,
            &rules(),
            &[node(1, "file", "A.md", 1, Some("incumbent"))],
        );
        let stats = apply(&mut f, &rules(), &[node(2, "folder", "a.md", 2, None)]);
        assert_eq!(stats.parked, 1);
        // Page 2: the child alone.
        let stats = apply(
            &mut f,
            &rules(),
            &[node(3, "file", "a.md/c.md", 3, Some("child"))],
        );
        assert_eq!(stats.parked, 1);
        let park = &f.state.parks[&Uuid::from_u128(3)];
        assert_eq!(park.class, ParkClass::Structural, "{park:?}");
        assert!(park.reason.contains("parked folder"), "{park:?}");
    }

    #[test]
    fn a_propagation_parked_child_releases_its_claim_for_a_legitimate_sibling() {
        // Codex round-5 P1 (Windows shape): tracked folder `a%3Cb`; one page delivers folder
        // `a<b` (projection-collides → parks), its child `a<b/x.md`, and the LEGITIMATE child
        // `a%3Cb/x.md`. The parked folder's child must release its projected claim
        // (`a%3Cb/x.md`) so the legitimate delivery materializes — a one-pass verdict left
        // both parked and `--check` green over the missing file.
        let mut f = fx();
        apply(&mut f, &win_rules(), &[node(1, "folder", "a%3Cb", 1, None)]);
        let stats = apply(
            &mut f,
            &win_rules(),
            &[
                node(2, "folder", "a<b", 2, None),
                node(3, "file", "a<b/x.md", 2, Some("shadow")),
                node(4, "file", "a%3Cb/x.md", 2, Some("legitimate")),
            ],
        );
        assert_eq!(stats.parked, 2, "{:?}", f.state.parks);
        assert_eq!(
            std::fs::read_to_string(f.mount.join("a%3Cb/x.md")).unwrap(),
            "legitimate"
        );
        assert!(f.state.nodes.contains_key(&Uuid::from_u128(4)));
    }

    #[test]
    fn descendants_of_a_collision_parked_folder_park_instead_of_wedging() {
        // Codex round-4 P1: incumbent note `A.md`; a server folder `a.md` fold-collides and
        // parks — its child `a.md/c.md` must park WITH it, not attempt `create_dir_all("a.md")`
        // over the incumbent file and abort the page forever.
        let mut f = fx();
        apply(
            &mut f,
            &rules(),
            &[node(1, "file", "A.md", 1, Some("incumbent"))],
        );
        let stats = apply(
            &mut f,
            &rules(),
            &[
                node(2, "folder", "a.md", 2, None),
                node(3, "file", "a.md/c.md", 2, Some("child")),
            ],
        );
        assert_eq!(stats.parked, 2, "folder AND descendant park");
        assert_eq!(
            f.state.parks[&Uuid::from_u128(3)].class,
            ParkClass::Structural
        );
        assert!(f.mount.join("A.md").is_file(), "the incumbent is untouched");
        assert_eq!(f.state.nodes.len(), 1);
    }

    #[test]
    fn a_note_replaced_by_a_same_path_attachment_loses_its_bytes() {
        // Codex round-3 P1: the attachment CLAIMS its binary path in the index (collision
        // bookkeeping) but never writes a file there — that claim must not shield the
        // tombstoned note's bytes from deletion, or they sit untracked at the exact path the
        // marker-only contract forbids, unreachable by any repair.
        let mut f = fx();
        apply(
            &mut f,
            &rules(),
            &[node(1, "file", "x.md", 1, Some("note body"))],
        );
        assert!(f.mount.join("x.md").is_file());
        let stats = apply(
            &mut f,
            &rules(),
            &[
                trashed(node(1, "file", "x.md", 2, None)),
                node(2, "attachment", "x.md", 2, None),
            ],
        );
        assert_eq!(stats.parked, 0);
        assert!(
            !f.mount.join("x.md").exists(),
            "the note's bytes must not survive at the attachment's binary path"
        );
        assert!(
            f.mount.join("x.md.docli").is_file(),
            "the marker is the materialization"
        );
    }

    #[test]
    fn a_kind_swap_blocked_by_an_untracked_dir_parks_with_the_repair_named() {
        let mut f = fx();
        fs::create_dir_all(f.mount.join("busy.md")).unwrap();
        let stats = apply(
            &mut f,
            &rules(),
            &[node(1, "file", "busy.md", 1, Some("x"))],
        );
        assert_eq!(stats.parked, 1);
        let park = &f.state.parks[&Uuid::from_u128(1)];
        assert_eq!(park.class, ParkClass::Transient);
        assert!(park.reason.contains("--full"), "{}", park.reason);
    }

    #[test]
    fn a_relocated_marker_leaf_is_containment_validated() {
        // State-derived strings are trusted for paths only after containment (D2's two
        // validated roots): a tampered marker leaf must refuse, never escape markers/.
        let f = fx();
        let err = perform_put(
            &build_claim_index(&f.state, &rules()),
            &rules(),
            &f.mount,
            &f.control,
            &TrackedKind::Attachment,
            "x.png",
            b"m",
            &Some(".docli/markers/../../evil.docli".to_string()),
            &std::collections::HashSet::new(),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("escapes"), "{err}");
    }

    #[test]
    fn a_corrupted_state_path_is_discarded_not_a_wedge() {
        // Codex round 16: an unresolvable stored path was never materialized by us — the
        // delete path discards it, so a tombstone (and a from-zero replay) completes instead
        // of erroring on the same entry forever.
        let mut f = fx();
        f.state.ledger.insert(Uuid::from_u128(1));
        f.state.nodes.insert(
            Uuid::from_u128(1),
            NodeState {
                server_path: "a.md".into(),
                local_path: "../outside.md".into(),
                kind: TrackedKind::Note,
                rev: 1,
                content_sha256: String::new(),
                marker_path: None,
            },
        );
        // …and an EMPTY stored path resolves to the mount root itself (round 17).
        f.state.ledger.insert(Uuid::from_u128(2));
        f.state.nodes.insert(
            Uuid::from_u128(2),
            NodeState {
                server_path: "b.md".into(),
                local_path: String::new(),
                kind: TrackedKind::Note,
                rev: 1,
                content_sha256: String::new(),
                marker_path: None,
            },
        );
        fs::write(f.mount.parent().unwrap().join("outside.md"), "keep me").unwrap();
        apply(
            &mut f,
            &rules(),
            &[
                trashed(node(1, "file", "a.md", 2, None)),
                trashed(node(2, "file", "b.md", 3, None)),
            ],
        );
        assert!(f.state.nodes.is_empty());
        assert!(f.mount.is_dir(), "the mount root survives");
        assert_eq!(
            fs::read_to_string(f.mount.parent().unwrap().join("outside.md")).unwrap(),
            "keep me",
            "the outside file is untouched"
        );
    }

    #[test]
    fn a_sibling_workspaces_marker_path_refuses_to_resolve() {
        // Codex round 13: a state path under ANOTHER workspace's subdir must never resolve as
        // ours — A's delete would reach B's marker across their independent mount locks.
        let f = fx();
        let (ours, sibling) = (Uuid::from_u128(0xF), Uuid::from_u128(0xB));
        let sib_path = format!(".docli/markers/{sibling}/x.docli");
        let err = marker_abs(&f.control, &f.mount, ours, &sib_path)
            .unwrap_err()
            .to_string();
        assert!(err.contains("escapes"), "{err}");
        assert!(relocated_leaf(&sib_path, ours).is_none());
        let node = Uuid::from_u128(7);
        let good = format!(".docli/markers/{ours}/{node}.docli");
        let leaf = format!("{node}.docli");
        assert_eq!(relocated_leaf(&good, ours), Some(leaf.as_str()));
        // Traversal shapes inside the namespace refuse too.
        assert!(relocated_leaf(&format!(".docli/markers/{ours}/../{node}.docli"), ours).is_none());
        assert!(relocated_leaf(&format!(".docli/markers/{ours}/a/{node}.docli"), ours).is_none());
        assert!(
            relocated_leaf(
                &format!(".docli/markers/{ours}/..\\{sibling}\\{node}.docli"),
                ours
            )
            .is_none(),
            "backslash separators must refuse too (Windows join semantics)"
        );
        assert!(
            relocated_leaf(&format!(".docli/markers/{ours}/C:{node}.docli"), ours).is_none(),
            "a Windows drive prefix must refuse (join discards the base)"
        );
        assert!(
            relocated_leaf(&format!(".docli/markers/{ours}/x.docli"), ours).is_none(),
            "only the generated <uuid>.docli shape resolves"
        );
    }

    #[test]
    fn a_tampered_relocated_marker_refuses_on_the_delete_path_too() {
        // The delete path must be exactly as contained as the write path: a state file
        // tampered to `.docli/markers/../../evil` must refuse, never resolve outside markers/.
        let f = fx();
        let err = marker_abs(
            &f.control,
            &f.mount,
            Uuid::from_u128(0xF),
            ".docli/markers/../../evil.docli",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("escapes"), "{err}");
    }

    #[test]
    fn trashing_a_parked_node_clears_its_park() {
        let mut f = fx();
        fs::write(f.mount.join("x.md"), "occupant").unwrap();
        apply(
            &mut f,
            &rules(),
            &[node(1, "file", "x.md", 1, Some("server"))],
        );
        assert!(f.state.parks.contains_key(&Uuid::from_u128(1)));
        // The parked node is UNTRACKED, so its trash tombstone classifies ledger-only — the
        // park must still clear (its premise is gone), or `--check` stays red forever and the
        // dead path keeps structurally parking descendants.
        apply(
            &mut f,
            &rules(),
            &[trashed(node(1, "file", "x.md", 2, None))],
        );
        assert!(f.state.parks.is_empty());
        assert_eq!(
            fs::read_to_string(f.mount.join("x.md")).unwrap(),
            "occupant"
        );
    }

    #[test]
    fn an_attachments_binary_path_never_shields_an_overwrite_of_a_user_file() {
        // Fold shape (Codex round 7): attachment `x.MD` is tracked (marker-only — nothing at
        // the binary path), the user drops their own `x.md`, and one page tombstones the
        // attachment while delivering note `x.md`. The attachment's binary-path bookkeeping
        // must not make the note write "owned" — the user's divergent file parks the note.
        let mut f = fx();
        apply(&mut f, &rules(), &[node(1, "attachment", "x.MD", 1, None)]);
        fs::write(f.mount.join("x.md"), "the user's own file").unwrap();
        apply(
            &mut f,
            &rules(),
            &[
                trashed(node(1, "attachment", "x.MD", 2, None)),
                node(2, "file", "x.md", 3, Some("server note")),
            ],
        );
        assert_eq!(
            fs::read_to_string(f.mount.join("x.md")).unwrap(),
            "the user's own file"
        );
        assert!(f.state.parks.contains_key(&Uuid::from_u128(2)));
    }

    #[test]
    fn a_relocated_marker_survives_a_second_revision() {
        // The first write leaves the relocated marker read-only; a later revision must lift
        // that before rewriting or the page wedges forever (Codex round 7).
        let mut f = fx();
        let long = "a".repeat(248); // `<long>.png` fits max_component_bytes, +".docli" overflows
        let path = format!("{long}.png");
        apply(&mut f, &rules(), &[node(1, "attachment", &path, 1, None)]);
        let mp = f.state.nodes[&Uuid::from_u128(1)]
            .marker_path
            .clone()
            .unwrap();
        assert!(mp.starts_with(".docli/markers/"), "{mp}");
        let stats = apply(&mut f, &rules(), &[node(1, "attachment", &path, 2, None)]);
        assert_eq!(stats.parked, 0);
    }

    #[test]
    fn a_park_heals_when_a_later_rev_materializes() {
        let mut f = fx();
        fs::write(f.mount.join("x.md"), "occupant").unwrap();
        apply(
            &mut f,
            &rules(),
            &[node(1, "file", "x.md", 1, Some("server"))],
        );
        assert!(f.state.parks.contains_key(&Uuid::from_u128(1)));
        // The user removes the occupant; a later rev renames the note — the delivery succeeds
        // and the park clears.
        fs::remove_file(f.mount.join("x.md")).unwrap();
        apply(
            &mut f,
            &rules(),
            &[node(1, "file", "y.md", 2, Some("server"))],
        );
        assert!(f.state.parks.is_empty());
        assert!(f.mount.join("y.md").exists());
    }
}
