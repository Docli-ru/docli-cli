// SPDX-FileCopyrightText: 2026 OOO Agitek
// SPDX-License-Identifier: MIT

//! `docli sync` / `docli sync --check` / `docli sync --full` — the pull orchestrator
//! (v0.28.0 D2/D2a/D3/D8).
//!
//! Staleness FAILS, quietly-stale doesn't exist (D8): `--check` exits non-zero when behind, on
//! any D2 invalidator, on the D2a bounds, and on transient parks — agents branch on exit codes,
//! not prose. Structural parks are REPORTED (doctor + the sync summary) but do not fail
//! `--check`: a signal that cannot stop firing stops informing.

use std::collections::BTreeSet;
use std::path::Path;

use anyhow::{bail, Context, Result};
use docli_sync_wire::{PullRequest, PullResponse, WireCursor};
use uuid::Uuid;

use crate::apply::{apply_page, prune_undelivered};
use crate::config::{mount_abs, validate_geometry, Mount, Project};
use crate::http::{Api, ApiFailure};
use crate::mountfs::{claim_mount, set_incomplete_marker, MountHandle};
use crate::platform::FsRules;
use crate::state::{ControlRoot, Park, ParkClass, TrackedKind, WsState};

/// The page size. MUST stay inside the server's clamp range `[1, 2000]`: the head-reaching
/// predicate (`nodes.len() < limit`) is computed by each side against the value it believes in,
/// and a clamped limit would split them.
const PAGE_LIMIT: i64 = 500;
/// The manifest retention bound (D2a): a cursor that last reached head longer ago than this
/// hard-forces from-zero (`purge` retention is 30 days — an older cursor may have missed
/// tombstones).
const MAX_HEAD_AGE_SECS: i64 = 30 * 24 * 60 * 60;

pub struct SyncOptions {
    pub check: bool,
    pub full: bool,
}

/// The no-access refusal (D4): the AUTHOR's mount name, «попросите доступ» — and the word
/// «токен» must not appear in this branch (pinned by a copy test; the publish-substring-ban
/// precedent). Designation without authority: the config NAMES the workspace, login state
/// decides reach.
pub fn no_access_message(mount_name: &str) -> String {
    format!(
        "нет доступа к «{mount_name}» — попросите доступ у владельца пространства \
         (монтирование пропущено, остальные продолжают работать)"
    )
}

/// The rollback detector's warning (Scope §rollback): a head-reaching ephemeral response with
/// no live-node count means the server did not honor ephemeral sync — a pre-v0.28.0 api took
/// the REGISTERED path and silently re-created the phantom-device horizon pin.
pub fn rollback_warning(ws: Uuid) -> String {
    format!(
        "the server did not honor ephemeral sync for workspace {ws}: a head-reaching page came \
         back without the live-node count. This CLI's pulls are being REGISTERED as a sync \
         device (one hidden sync_clients row per pulled workspace — not visible in «Доступ», \
         and revoking the OAuth connection does not clear it). Stopping before persisting this \
         cycle. Fix: redeploy a v0.28.0+ API; the stray row ages out of the purge horizon on \
         its own after 30 days (the trash panel's «deletes after device sync» badge disappears \
         when the row expires) — the only faster path is operator SQL."
    )
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub fn run(project: &Project, api: &Api, opts: &SyncOptions) -> Result<i32> {
    validate_geometry(&project.root, &project.config)?;
    let rules = FsRules::native();
    let control = ControlRoot::new(&project.root);
    std::fs::create_dir_all(&control.dir).context("creating .docli/")?;

    let mut worst_exit = 0;
    for mount in &project.config.mounts {
        match sync_mount(project, api, &control, &rules, mount, opts) {
            Ok(code) => worst_exit = worst_exit.max(code),
            Err(e) => {
                if is_no_access(&e) {
                    // Partial success (D4): one unreachable mount never aborts the others.
                    eprintln!("{}", no_access_message(mount.display_name()));
                    worst_exit = worst_exit.max(1);
                } else if e.downcast_ref::<NotEntitled>().is_some() {
                    eprintln!(
                        "«{}»: синхронизация не включена для вашего аккаунта — монтирование \
                         пропущено",
                        mount.display_name()
                    );
                    worst_exit = worst_exit.max(1);
                } else {
                    return Err(e.context(format!("mount `{}`", mount.display_name())));
                }
            }
        }
    }
    Ok(worst_exit)
}

pub(crate) fn is_no_access(e: &anyhow::Error) -> bool {
    e.downcast_ref::<NoAccess>().is_some()
}

/// Typed marker for the partial-success class (no thiserror dep — two impls by hand).
#[derive(Debug)]
pub(crate) struct NoAccess;

impl std::fmt::Display for NoAccess {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "no access")
    }
}
impl std::error::Error for NoAccess {}

/// Classify an [`ApiFailure`]: 403 (scope / pin / ownership) is the partial-success no-access
/// class; 402 is the caller's OWN entitlement — «попросите доступ» would be wrong guidance, so
/// it gets its own message (still partial-success: one unentitled account state must not abort
/// the other mounts of a multi-server future, and the copy names the real fix).
pub(crate) fn map_failure(f: ApiFailure) -> anyhow::Error {
    match &f {
        ApiFailure::Refused { status: 403, .. } => anyhow::Error::new(NoAccess),
        ApiFailure::Refused { status: 402, .. } => anyhow::Error::new(NotEntitled),
        _ => anyhow::anyhow!("{f}"),
    }
}

/// The 402 marker: vault sync is off for the ACCOUNT (entitlement), not a grant someone else
/// must give.
#[derive(Debug)]
pub(crate) struct NotEntitled;

impl std::fmt::Display for NotEntitled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "vault sync is not enabled for this account")
    }
}
impl std::error::Error for NotEntitled {}

fn ephemeral_request(ws: Uuid, cursor: WireCursor, epoch: i64, limit: i64) -> PullRequest {
    PullRequest {
        workspace_id: ws,
        client_id: "ephemeral".into(),
        cursor,
        epoch,
        limit: Some(limit),
        ack: None,
        ephemeral: true,
    }
}

/// True when this response is the HEAD-REACHING page — RESPONSE-DERIVED on both sides
/// (`nodes.len() < limit`), NEVER cursor-vs-head equality (purges mint row-less barrier revs,
/// so an equality test is permanently unsatisfiable after any purge).
fn head_reaching(resp: &PullResponse, limit: i64) -> bool {
    (resp.nodes.len() as i64) < limit
}

/// The D2 invalidators, applied to a LOADED state. Returns the reason a from-zero is forced.
fn invalidator(
    state: &WsState,
    mount: &Mount,
    mount_root: &Path,
    control: &ControlRoot,
) -> Option<String> {
    if state.from_zero {
        return Some("a full rebuild is pending".into());
    }
    if state.scope_key != mount.folder {
        // The cursor advanced past out-of-scope nodes, so a widened scope must backfill.
        return Some("the mount's folder scope changed".into());
    }
    match state.head_reached_at {
        None => return Some("the mirror has never reached head".into()),
        Some(t) if now_unix() - t > MAX_HEAD_AGE_SECS => {
            return Some("the cursor last reached head more than 30 days ago".into());
        }
        _ => {}
    }
    // Mirror-vs-manifest: every tracked materialization must exist on disk, so `rm -rf mirror/`
    // with `.docli/` left behind reads as from-zero, never as healthy.
    for n in state.nodes.values() {
        let there = match n.kind {
            TrackedKind::Folder => crate::mountfs::contained_join(mount_root, &n.local_path)
                .map(|p| p.is_dir())
                .unwrap_or(false),
            TrackedKind::Note => crate::mountfs::contained_join(mount_root, &n.local_path)
                .map(|p| p.is_file())
                .unwrap_or(false),
            TrackedKind::Attachment => match n.marker_path.as_deref() {
                // A `.docli/` path resolves ONLY through this workspace's own namespace
                // (Codex round 13): a sibling/traversal shape reads as missing, which routes
                // into the from-zero repair rather than consulting a file that is not ours.
                Some(mp) if mp.starts_with(".docli/") => {
                    match crate::apply::relocated_leaf(mp, mount.workspace) {
                        Some(leaf) => control
                            .markers_dir()
                            .join(mount.workspace.to_string())
                            .join(leaf)
                            .is_file(),
                        None => false,
                    }
                }
                // Mount-local marker paths are state-derived too: containment or missing
                // (Codex round 15 — a corrupted `../outside.docli` must not satisfy the
                // manifest with a file that is not ours).
                Some(mp) => crate::mountfs::contained_join(mount_root, mp)
                    .map(|p| p.is_file())
                    .unwrap_or(false),
                None => false,
            },
        };
        if !there {
            return Some(format!(
                "the mirror no longer matches its manifest ({} is gone)",
                n.local_path
            ));
        }
    }
    None
}

fn persist_incomplete(
    control: &ControlRoot,
    handle: &MountHandle,
    ws: Uuid,
    state: &WsState,
) -> Result<()> {
    control.save_state(ws, state)?;
    let incomplete = !state.at_head
        || state.from_zero
        || state.has_transient_parks()
        || !state.pending_removals.is_empty();
    set_incomplete_marker(&handle.root, incomplete)
}

#[allow(clippy::too_many_lines)]
fn sync_mount(
    project: &Project,
    api: &Api,
    control: &ControlRoot,
    rules: &FsRules,
    mount: &Mount,
    opts: &SyncOptions,
) -> Result<i32> {
    let ws = mount.workspace;
    let mount_path = mount_abs(&project.root, mount);
    let handle = claim_mount(&mount_path, &control.dir, ws)?;
    // The control root is the SECOND validated root (D2): same symlink discipline as the mount,
    // re-checked each run (its marker/state writes are containment-validated separately).
    if control.dir.exists() {
        crate::mountfs::refuse_symlinks(&control.dir)?;
    }
    let mut state = control
        .load_state(ws)?
        .unwrap_or_else(|| WsState::fresh(mount.folder.clone()));

    if opts.full && !opts.check {
        state.from_zero = true;
    }
    if let Some(reason) = invalidator(&state, mount, &handle.root, control) {
        if opts.check {
            eprintln!("{}: stale — {reason}", mount.display_name());
            // Make the pending repair durable + visible exactly as a sync would.
            state.from_zero = true;
            persist_incomplete(control, &handle, ws, &state)?;
            return Ok(1);
        }
        state.from_zero = true;
    }

    if opts.check {
        return check_mount(api, control, &handle, mount, &mut state);
    }

    // Retry the OWED directory removals first — the occupant may be gone since last run, and
    // healing must not require `--full` (the durable ledger is what makes this crash-safe).
    settle_pending_removals(&mut state, rules, &handle.root);
    persist_incomplete(control, &handle, ws, &state)?;

    // Up to two passes: an incremental sync that detects a count mismatch flags from-zero and
    // the second pass repairs it in the same invocation (the flag is durable either way).
    for _pass in 0..2 {
        if state.from_zero {
            // The authoritative repair also owns write_atomic's crash residue (read-only
            // `.docli-write-*.tmp` strays a process death left mid-swap) — doctor names them
            // `crash-residue` and points here. BOTH write destinations: the mount tree and
            // this workspace's relocated-marker dir (round-2 R4).
            let mut swept = crate::mountfs::sweep_write_temps(&handle.root);
            let ws_markers = control.markers_dir().join(ws.to_string());
            if ws_markers.is_dir() {
                swept += crate::mountfs::sweep_write_temps(&ws_markers);
            }
            if swept > 0 {
                println!(
                    "{}: removed interrupted-write temporary files: {swept}",
                    mount.display_name()
                );
            }
            from_zero_sync(api, control, &handle, rules, mount, &mut state)?;
            break;
        }
        if incremental_sync(api, control, &handle, rules, mount, &mut state)? {
            break; // clean
        }
        // A mismatch flagged from-zero; loop into the repair.
    }
    persist_incomplete(control, &handle, ws, &state)?;
    report(mount, &state);
    Ok(0)
}

fn report(mount: &Mount, state: &WsState) {
    let structural: Vec<&Park> = state
        .parks
        .values()
        .filter(|p| p.class == ParkClass::Structural)
        .collect();
    let transient = state.parks.len() - structural.len();
    print!(
        "{}: узлов в зеркале: {}",
        mount.display_name(),
        state.nodes.len()
    );
    if transient > 0 {
        print!(", временно отложено: {transient} — см. `docli sync --check`");
    }
    if !state.pending_removals.is_empty() {
        print!(
            ", удаление каталогов заблокировано посторонним содержимым: {} — см. `docli sync --check`",
            state.pending_removals.len()
        );
    }
    if !structural.is_empty() {
        print!(
            ", структурных конфликтов: {} — см. `docli doctor`",
            structural.len()
        );
    }
    println!();
}

/// One incremental resume-to-head. Returns `false` when a count mismatch flagged from-zero
/// (the caller repairs in the same invocation).
fn incremental_sync(
    api: &Api,
    control: &ControlRoot,
    handle: &MountHandle,
    rules: &FsRules,
    mount: &Mount,
    state: &mut WsState,
) -> Result<bool> {
    let ws = mount.workspace;
    loop {
        let req = ephemeral_request(ws, state.cursor, state.epoch, PAGE_LIMIT);
        let resp = match api.pull(&req)? {
            Ok(r) => r,
            Err(ApiFailure::EpochChanged { .. }) => {
                // Force from-zero like any client; the bootstrap learns the new epoch.
                state.from_zero = true;
                persist_incomplete(control, handle, ws, state)?;
                return Ok(false);
            }
            Err(f) => return Err(map_failure(f)),
        };
        let head = head_reaching(&resp, PAGE_LIMIT);
        if head && resp.live_nodes.is_none() {
            // The rollback detector: STOP before persisting this page.
            bail!(rollback_warning(ws));
        }
        // Persist `at_head = false` BEFORE the first filesystem mutation (Codex round 31): a
        // concurrent lock-free search reads the state FILE, and until this write it still
        // said head over a mirror this page is about to move under it. Only when the page
        // carries deliveries — a no-op probe must not flicker search to «not mirrored».
        if !resp.nodes.is_empty() && state.at_head {
            state.at_head = false;
            persist_incomplete(control, handle, ws, state)?;
        }
        let stats = apply_page(
            state,
            rules,
            &handle.root,
            control,
            ws,
            mount.folder.as_deref(),
            &resp.nodes,
        )?;
        // Owed removals go DURABLE before the page's own persist — a crash between pages must
        // not lose them (Codex round 2).
        state.pending_removals.extend(stats.pending_dir_removals);
        state.cursor = resp.cursor;
        // The intermediate incomplete persist is for pages that MOVED something (Codex round
        // 32): an empty page is by the wire contract head-reaching with an unchanged cursor,
        // so flipping a current mirror to incomplete here would flicker a concurrent search
        // and — on a crash before the head commit — durably mislabel an untouched mirror.
        if !resp.nodes.is_empty() {
            state.at_head = false;
            persist_incomplete(control, handle, ws, state)?;
        }
        if head {
            let live = resp.live_nodes.expect("checked above");
            if live != state.ledger.len() as i64 {
                // A hard purge (`purgeNode`/`emptyTrash` at retention zero) landed between
                // syncs — the one class an ephemeral cursor can miss. Set the durable flag AT
                // DETECTION, before returning (a mismatch noticed at head would otherwise
                // satisfy neither marker predicate and read as healthy until the next run).
                state.from_zero = true;
                persist_incomplete(control, handle, ws, state)?;
                eprintln!(
                    "{}: server live-node count {live} != mirror ledger {} — a hard delete was \
                     missed; running a full resync",
                    mount.display_name(),
                    state.ledger.len()
                );
                return Ok(false);
            }
            state.at_head = true;
            state.head_reached_at = Some(now_unix());
            settle_pending_removals(state, rules, &handle.root);
            persist_incomplete(control, handle, ws, state)?;
            return Ok(true);
        }
    }
}

/// Re-attempt every OWED directory removal from the durable ledger (Codex round 2: an
/// in-memory list dies with a crash between pages, and `--full`'s prune walks `nodes`, so a
/// lost entry meant a stray directory no CLI verb could ever remove). Deliberately touches NO
/// parks (Codex round 3: a debt keyed to a node id clobbered that node's structural park, and
/// clearing it deleted the park outright) — the staleness signal comes from `--check` and the
/// incompleteness marker consulting the debt set DIRECTLY. Survivors stay owed: an untracked
/// occupant keeps the dir alive, and the CLI never force-removes.
fn settle_pending_removals(state: &mut WsState, rules: &FsRules, mount_root: &Path) {
    // A path LIVE state has re-claimed since the debt was recorded is no longer owed — a new
    // server folder took the old name, and "settling" would delete a tracked live directory
    // (Codex round 4). The comparison is FOLD-keyed and MATERIALIZATION-aware (round 5): a
    // reclaimed `foo` cancels the debt for `Foo` (one physical dir), while an attachment's
    // binary-path claim — which materializes nothing — does not.
    let materialized: std::collections::HashSet<String> = state
        .nodes
        .values()
        .flat_map(|n| {
            let mut v = Vec::with_capacity(2);
            if n.kind != crate::state::TrackedKind::Attachment {
                v.push(crate::localpath::fold_key(&n.local_path, rules));
            }
            if let Some(mp) = &n.marker_path {
                if !mp.starts_with(".docli/") {
                    v.push(crate::localpath::fold_key(mp, rules));
                }
            }
            v
        })
        .collect();
    let entries: Vec<String> = state.pending_removals.iter().cloned().collect();
    for rel in entries {
        if materialized.contains(&crate::localpath::fold_key(&rel, rules)) {
            state.pending_removals.remove(&rel);
            continue;
        }
        if rel.is_empty() {
            // Empty resolves to the mount root itself — a debt that can never settle
            // (`MOUNT.docli` keeps the root non-empty); drop it (Codex round 17).
            state.pending_removals.remove(&rel);
            continue;
        }
        let Ok(abs) = crate::mountfs::contained_join(mount_root, &rel) else {
            // A tampered/corrupt entry: drop it (containment refuses the path everywhere else).
            state.pending_removals.remove(&rel);
            continue;
        };
        if !abs.is_dir() || std::fs::remove_dir(&abs).is_ok() {
            state.pending_removals.remove(&rel);
        }
    }
}

/// Remove relocated markers the replayed state does not name (Codex round 10): after state
/// loss plus a remote hard delete, a marker belongs to no node any replay can deliver — and
/// `prune_undelivered` walks STATE, so it can never see the file. The sweep touches ONLY this
/// workspace's own `.docli/markers/<ws>/` subdir (Codex rounds 11–12: the markers dir is
/// project-global while mount locks are per-mount, so any cross-workspace inventory races a
/// concurrent sibling sync — the per-workspace namespace makes the sweep self-contained under
/// this mount's own lock). From-zero only — an incremental state is not a complete inventory.
fn sweep_orphan_markers(control: &ControlRoot, ws: Uuid, state: &WsState) -> anyhow::Result<()> {
    let dir = control.markers_dir().join(ws.to_string());
    if !dir.is_dir() {
        return Ok(());
    }
    // Only RELOCATED paths in THIS workspace's namespace feed the keep-set (Codex round 13):
    // a mount-local derived marker (`<name>.docli` beside the binary) shares a basename shape
    // with relocated leaves, and letting it in would mask a same-named orphan forever.
    let named: std::collections::BTreeSet<String> = state
        .nodes
        .values()
        .filter_map(|n| n.marker_path.as_deref())
        .filter_map(|mp| crate::apply::relocated_leaf(mp, ws))
        .map(str::to_string)
        .collect();
    for e in std::fs::read_dir(&dir)? {
        let e = e?;
        if !named.contains(e.file_name().to_string_lossy().as_ref()) {
            crate::mountfs::remove_owned_file(&e.path())?;
        }
    }
    Ok(())
}

/// The authoritative from-zero (D3): restart-never-resume; ledger + parks REBUILT from the
/// replay; prune after head; the interrupted case restarts because the durable flag stays set
/// until the very end.
fn from_zero_sync(
    api: &Api,
    control: &ControlRoot,
    handle: &MountHandle,
    rules: &FsRules,
    mount: &Mount,
    state: &mut WsState,
) -> Result<()> {
    let ws = mount.workspace;
    // The flag is durable and visible BEFORE any work (an interrupted from-zero must fail
    // `--check` — it re-writes the same paths, so cursor and counts would look healthy).
    state.from_zero = true;
    state.at_head = false;
    state.scope_key = mount.folder.clone();
    // REBUILD, not resume: the ledger and parks come from THIS replay only (a repaired hard
    // purge would otherwise leave the purged id in the ledger forever, re-firing the count
    // mismatch every cycle).
    state.ledger.clear();
    state.parks.clear();
    persist_incomplete(control, handle, ws, state)?;

    let mut cursor = WireCursor {
        rev: 0,
        id: Uuid::nil(),
    };
    // Bootstrap is the CLI's first call — the only way to learn the epoch.
    let first = match api.bootstrap(&ephemeral_request(ws, cursor, state.epoch, PAGE_LIMIT))? {
        Ok(r) => r,
        Err(ApiFailure::EpochChanged { .. }) => {
            // A resync landing inside bootstrap's own epoch-read/pull window (Codex round 12):
            // the exact self-healing event the mid-replay arm below already absorbs — the
            // durable flag stays set, the next run replays against the new epoch.
            eprintln!(
                "{}: the workspace was resynced mid-replay — the repair stays pending; \
                 run `docli sync` again",
                mount.display_name()
            );
            return Ok(());
        }
        Err(f) => return Err(map_failure(f)),
    };
    let epoch = first.epoch;
    let mut resp = first;
    loop {
        let head = head_reaching(&resp, PAGE_LIMIT);
        if head && resp.live_nodes.is_none() {
            bail!(rollback_warning(ws));
        }
        let stats = apply_page(
            state,
            rules,
            &handle.root,
            control,
            ws,
            mount.folder.as_deref(),
            &resp.nodes,
        )?;
        state.pending_removals.extend(stats.pending_dir_removals);
        cursor = resp.cursor;
        // Persist per page (adoption makes rewrites idempotent) but the from_zero flag stays
        // SET — a crash here restarts from (0,0), never resumes (the prune's delivered-set
        // comparand must come from ONE complete replay).
        persist_incomplete(control, handle, ws, state)?;
        if head {
            let live = resp.live_nodes.expect("checked above");
            let delivered: BTreeSet<Uuid> = state.ledger.clone();
            if live != delivered.len() as i64 {
                // Offsetting add+miss inside one replay window; leave the flag set — the next
                // run replays again, `doctor` is the strict detector.
                eprintln!(
                    "{}: count still mismatched after a full replay ({live} vs {}) — \
                     leaving the repair pending; run `docli doctor`",
                    mount.display_name(),
                    delivered.len()
                );
                return Ok(());
            }
            // The PRUNE arm: a from-zero is AUTHORITATIVE.
            let pending = prune_undelivered(state, rules, &handle.root, control, ws, &delivered)?;
            state.pending_removals.extend(pending);
            sweep_orphan_markers(control, ws, state)?;
            state.cursor = cursor;
            state.epoch = epoch;
            state.from_zero = false;
            state.at_head = true;
            state.head_reached_at = Some(now_unix());
            settle_pending_removals(state, rules, &handle.root);
            persist_incomplete(control, handle, ws, state)?;
            return Ok(());
        }
        resp = match api.pull(&ephemeral_request(ws, cursor, epoch, PAGE_LIMIT))? {
            Ok(r) => r,
            Err(ApiFailure::EpochChanged { .. }) => {
                // Mid-replay resync: an epoch bump is a normal, self-healing server event, not
                // an error an agent should see as exit 2 — the durable flag is still set, so
                // leave the repair pending (CACHE_INCOMPLETE present, `--check` failing) and let
                // the caller's loop/the next run replay from (0,0) against the new epoch.
                eprintln!(
                    "{}: the workspace was resynced mid-replay — the repair stays pending; \
                     run `docli sync` again",
                    mount.display_name()
                );
                return Ok(());
            }
            Err(f) => return Err(map_failure(f)),
        };
    }
}

/// `sync --check` (D8): the cheap head probe — a limit-1 ephemeral pull (no head endpoint
/// exists and none is added; its empty/short page IS head-reaching by the response-derived
/// predicate, so it CARRIES the live-node count). Non-zero when behind, on transient parks, or
/// on any invalidator (already handled by the caller); zero at head with only structural parks.
/// Persistence discipline inside `--check` (deliberate asymmetry — do not "simplify"): the
/// transient-parks arm writes NOTHING (parks are already durable state); the count-mismatch and
/// epoch arms DO persist, because they are the DETECTION moment of a new fact and the
/// durable-flag-at-detection rule (D2a) says the pending repair must be visible from that
/// moment, not from the next sync.
fn check_mount(
    api: &Api,
    control: &ControlRoot,
    handle: &MountHandle,
    mount: &Mount,
    state: &mut WsState,
) -> Result<i32> {
    let ws = mount.workspace;
    if state.has_transient_parks() {
        // Every stale return reconciles the marker (Codex round 31): a crash between the
        // state save and the marker write must not leave them contradicting.
        persist_incomplete(control, handle, ws, state)?;
        eprintln!(
            "{}: stale — parked deliveries are waiting (remove the occupants, then \
             `docli sync --full`)",
            mount.display_name()
        );
        return Ok(1);
    }
    // Owed directory removals consult the DEBT SET directly (Codex round 3 — pairing them
    // through parks both clobbered structural parks and let a restart's `--check` read a
    // healthy probe over a nonempty debt map).
    if !state.pending_removals.is_empty() {
        persist_incomplete(control, handle, ws, state)?;
        eprintln!(
            "{}: stale — directory removals blocked by untracked occupants: {} (remove \
             them, then run `docli sync`)",
            mount.display_name(),
            state.pending_removals.len()
        );
        return Ok(1);
    }
    let req = ephemeral_request(ws, state.cursor, state.epoch, 1);
    let resp = match api.pull(&req)? {
        Ok(r) => r,
        Err(ApiFailure::EpochChanged { .. }) => {
            state.from_zero = true;
            persist_incomplete(control, handle, ws, state)?;
            eprintln!(
                "{}: stale — the workspace was resynced",
                mount.display_name()
            );
            return Ok(1);
        }
        Err(f) => return Err(map_failure(f)),
    };
    if !head_reaching(&resp, 1) {
        // Behind the server means the mirror is NOT a complete projection (Codex round 31):
        // say so durably, or a lock-free search keeps rendering local paths off it.
        state.at_head = false;
        persist_incomplete(control, handle, ws, state)?;
        eprintln!(
            "{}: stale — behind the server; run `docli sync`",
            mount.display_name()
        );
        return Ok(1);
    }
    let Some(live) = resp.live_nodes else {
        bail!(rollback_warning(ws));
    };
    if live != state.ledger.len() as i64 {
        // A hard purge landed between syncs (it mints only a row-less barrier rev, so the
        // cursor reads as caught-up over a stale file). Durable flag + marker + non-zero.
        state.from_zero = true;
        persist_incomplete(control, handle, ws, state)?;
        eprintln!(
            "{}: stale — server live count {live} != mirror {}; run `docli sync`",
            mount.display_name(),
            state.ledger.len()
        );
        return Ok(1);
    }
    // Confirmed at head with a matching count: HEAL the crash-window state (a crash between a
    // page commit and the head commit leaves `at_head = false` with a complete mirror and the
    // incomplete marker present — the probe just proved completeness, so make the marker and
    // the manifest say so rather than printing «fresh» over a lying marker; Codex round 1).
    if !state.at_head {
        state.at_head = true;
        state.head_reached_at = Some(now_unix());
    }
    // ALWAYS reconcile (Codex round 30): a crash between the state save and the marker
    // removal leaves a lying CACHE_INCOMPLETE over a state that already says head — the probe
    // just proved freshness, so the marker must say so too.
    persist_incomplete(control, handle, ws, state)?;
    println!("{}: fresh", mount.display_name());
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The D4/D10.3 copy pin (the publish-substring-ban precedent): the no-access branch never
    /// says «токен» — the answer to a colleague without access is a request for ACCESS, not a
    /// credential to paste.
    #[test]
    fn the_marker_sweep_stays_inside_its_own_workspace_subdir() {
        // Per-workspace namespace (Codex round 12): the sweep removes this workspace's
        // orphans and structurally cannot touch a sibling's subdir — no cross-workspace
        // inventory, no race with a concurrent sibling sync.
        use crate::state::{NodeState, TrackedKind};
        let tmp = tempfile::tempdir().unwrap();
        let control = ControlRoot::new(tmp.path());
        let (ws_a, ws_b) = (Uuid::from_u128(0xA), Uuid::from_u128(0xB));
        let (kept, orphan, sib) = (Uuid::from_u128(1), Uuid::from_u128(2), Uuid::from_u128(3));
        let dir_a = control.markers_dir().join(ws_a.to_string());
        let dir_b = control.markers_dir().join(ws_b.to_string());
        std::fs::create_dir_all(&dir_a).unwrap();
        std::fs::create_dir_all(&dir_b).unwrap();
        std::fs::write(dir_a.join(format!("{kept}.docli")), "m").unwrap();
        std::fs::write(dir_a.join(format!("{orphan}.docli")), "m").unwrap();
        std::fs::write(dir_b.join(format!("{sib}.docli")), "m").unwrap();
        let mut state = WsState::fresh(None);
        state.nodes.insert(
            kept,
            NodeState {
                server_path: "k.png".into(),
                local_path: "k.png".into(),
                kind: TrackedKind::Attachment,
                rev: 1,
                content_sha256: String::new(),
                marker_path: Some(format!(".docli/markers/{ws_a}/{kept}.docli")),
            },
        );
        sweep_orphan_markers(&control, ws_a, &state).unwrap();
        assert!(dir_a.join(format!("{kept}.docli")).exists());
        assert!(!dir_a.join(format!("{orphan}.docli")).exists());
        assert!(
            dir_b.join(format!("{sib}.docli")).exists(),
            "sibling untouched"
        );
    }

    #[test]
    fn the_no_access_branch_never_says_token() {
        let msg = no_access_message("книга продаж");
        let lower = msg.to_lowercase();
        assert!(!lower.contains("токен"), "{msg}");
        assert!(!lower.contains("token"), "{msg}");
        assert!(lower.contains("попросите доступ"), "{msg}");
        assert!(
            msg.contains("книга продаж"),
            "the AUTHOR's mount name: {msg}"
        );
    }

    #[test]
    fn head_reaching_is_response_derived() {
        let resp = |n: usize| PullResponse {
            epoch: 1,
            cursor: WireCursor {
                rev: 9,
                id: Uuid::nil(),
            },
            nodes: (0..n)
                .map(|i| docli_sync_wire::WireNode {
                    id: Uuid::from_u128(i as u128 + 1),
                    parent_id: None,
                    kind: "file".into(),
                    name: "a.md".into(),
                    path: "a.md".into(),
                    rev: 1,
                    trashed: false,
                    mime: None,
                    content_bytes: 0,
                    body: Some(String::new()),
                    blob_url: None,
                    position: None,
                    sha256: None,
                    blob_generation: None,
                })
                .collect(),
            capabilities: vec![],
            resync_required: false,
            last_mutation_id: 0,
            live_nodes: None,
        };
        assert!(head_reaching(&resp(0), 1));
        assert!(!head_reaching(&resp(1), 1));
        assert!(head_reaching(&resp(499), 500));
        assert!(!head_reaching(&resp(500), 500));
    }
}
