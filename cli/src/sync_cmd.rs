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

pub struct SyncOptions {
    pub check: bool,
    pub full: bool,
}

/// The no-access refusal (D4): the AUTHOR's mount name, «ask the owner» — and the word
/// «token» must not appear in this branch (pinned by a copy test; the publish-substring-ban
/// precedent). Designation without authority: the config NAMES the workspace, login state
/// decides reach.
pub fn no_access_message(mount_name: &str) -> String {
    format!(
        "no access to `{mount_name}` - ask the workspace owner to share it with you \
         (this mount was skipped; the others carry on)"
    )
}

/// The rollback detector's warning (Scope §rollback): a head-reaching ephemeral response with
/// no live-node count means the server did not honor ephemeral sync — a pre-v0.28.0 api took
/// the REGISTERED path and silently re-created the phantom-device horizon pin.
pub fn rollback_warning(ws: Uuid) -> String {
    format!(
        "the server did not honor ephemeral sync for workspace {ws}: a head-reaching page came \
         back without the live-node count. This CLI's pulls are being REGISTERED as a sync \
         device (one hidden sync_clients row per pulled workspace - not visible in Access, \
         and revoking the OAuth connection does not clear it). Stopping before persisting this \
         cycle. Fix: redeploy a v0.28.0+ API; the stray row ages out of the purge horizon on \
         its own after 30 days (the trash panel's `deletes after device sync` badge disappears \
         when the row expires) - the only faster path is operator SQL."
    )
}

pub(crate) fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub fn run(project: &Project, api: &Api, opts: &SyncOptions) -> Result<i32> {
    if let Err(e) = validate_geometry(&project.root, &project.config) {
        // A missing `.gitignore` line is the one geometry failure with a one-keystroke fix;
        // offer it, then re-validate so every OTHER geometry rule still refuses as before.
        // `--check` is the scripted freshness gate (agents branch on its exit code): it must
        // answer, never ask. The offer is for a person running a plain `docli sync`.
        if opts.check || !crate::wizard::offer_missing_ignores(&project.root, &project.config)? {
            return Err(e);
        }
        validate_geometry(&project.root, &project.config)?;
    }
    let rules = FsRules::native();
    let control = project.control_root();
    std::fs::create_dir_all(&control.dir).context("creating .docli/")?;

    let mut worst_exit = 0;
    for mount in &project.config.mounts {
        match sync_mount(project, api, &control, &rules, mount, opts) {
            Ok(code) => worst_exit = worst_exit.max(code),
            Err(e) => {
                if is_no_access(&e) {
                    // Partial success (D4): one unreachable mount never aborts the others.
                    // Through `ui::warn`, so it survives `-q` like every other refusal — a
                    // skipped mount is not narration.
                    crate::ui::warn(&no_access_message(mount.display_name()));
                    worst_exit = worst_exit.max(1);
                } else if e.downcast_ref::<NotEntitled>().is_some() {
                    crate::ui::warn(&format!(
                        "`{}`: sync is not enabled for your account - mount skipped. Enable \
                         vault sync for this account at docli.ru, then run `docli sync` again.",
                        mount.display_name()
                    ));
                    worst_exit = worst_exit.max(1);
                } else if opts.check && crate::mountfs::is_busy(&e) {
                    // v0.28.6 D3: with two wired agents starting together, `try_lock`'s
                    // fail-fast would otherwise abort mounts 2..n of a freshness check. The
                    // answer «I could not look» is a partial success, not a wedge.
                    crate::ui::warn(&format!(
                        "{}: check skipped - another docli run holds this mount; run \
                         `docli sync --check` again when it finishes",
                        mount.display_name()
                    ));
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
///
/// The three CHEAP terms live on `WsState` (v0.29.0 D4) so `search`'s readiness predicate and
/// this one cannot drift; what stays here is the expensive half — the mirror-vs-manifest walk,
/// which `search` must not pay.
fn invalidator(
    state: &WsState,
    mount: &Mount,
    mount_root: &Path,
    control: &ControlRoot,
) -> Option<String> {
    if let Some(r) = state.rebuild_reason(mount.folder.as_deref(), now_unix()) {
        return Some(r.into());
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
    // The four terms live on `WsState` (v0.29.0 D4) — `status` renders its row from the same
    // method, so the screen and the marker in the mirror cannot disagree.
    set_incomplete_marker(&handle.root, state.incomplete())
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
            // Make the pending repair durable + visible exactly as a sync would.
            state.from_zero = true;
            persist_incomplete(control, &handle, ws, &state)?;
            return Ok(render_check(&CheckOutcome {
                fresh: false,
                message: format!(
                    "{}: stale - {reason}; run `docli sync`",
                    mount.display_name()
                ),
            }));
        }
        state.from_zero = true;
    }

    if opts.check {
        let outcome = check_mount(api, control, &handle, mount, &mut state)?;
        return Ok(render_check(&outcome));
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
                crate::ui::detail(&format!(
                    "{}: removed interrupted-write temporary files: {swept}",
                    mount.display_name()
                ));
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

/// The HUMAN renderer for a freshness answer, and the exit code it carries.
///
/// Through `ui`, not `eprintln!` (Step 1's output pass): a stale line is a warning, which must
/// survive `-q` — a reader who asked for less narration did not ask to be told nothing about a
/// mirror that cannot be trusted. The «fresh» line stays chatter.
fn render_check(outcome: &CheckOutcome) -> i32 {
    if outcome.fresh {
        crate::ui::ok(&outcome.message);
        0
    } else {
        crate::ui::warn(&outcome.message);
        1
    }
}

fn report(mount: &Mount, state: &WsState) {
    let structural: Vec<&Park> = state
        .parks
        .values()
        .filter(|p| p.class == ParkClass::Structural)
        .collect();
    let transient = state.parks.len() - structural.len();
    // One line per mount, then the caveats as indented details: a single run-on sentence made
    // the difference between «синхронизировано» and «синхронизировано, но три вещи требуют
    // внимания» invisible.
    let clean = transient == 0 && state.pending_removals.is_empty() && structural.is_empty();
    let head = format!(
        "{}: {} in the mirror",
        mount.display_name(),
        crate::ui::plural(state.nodes.len(), "node", "nodes")
    );
    if clean {
        crate::ui::ok(&head);
        return;
    }
    crate::ui::warn(&head);
    if transient > 0 {
        crate::ui::detail(&format!(
            "parked for now: {transient} - details: docli sync --check"
        ));
    }
    if !state.pending_removals.is_empty() {
        crate::ui::detail(&format!(
            "directory removals blocked by unrelated content: {} - docli sync --check",
            state.pending_removals.len()
        ));
    }
    if !structural.is_empty() {
        crate::ui::detail(&format!(
            "structural conflicts: {} - docli doctor",
            structural.len()
        ));
    }
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
    // A pull of a large workspace is many round-trips; with nothing on screen it reads as a
    // hang. The counter is in-place and silent off a terminal (see `ui::Progress`).
    let progress = crate::ui::Progress::new(mount.display_name());
    let result = incremental_pages(api, control, handle, rules, mount, state, &progress);
    progress.finish();
    result
}

fn incremental_pages(
    api: &Api,
    control: &ControlRoot,
    handle: &MountHandle,
    rules: &FsRules,
    mount: &Mount,
    state: &mut WsState,
    progress: &crate::ui::Progress,
) -> Result<bool> {
    let ws = mount.workspace;
    loop {
        progress.set(&format!(
            "received: {}",
            crate::ui::plural(state.nodes.len(), "node", "nodes")
        ));
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
                    "{}: server live-node count {live} != mirror ledger {} - a hard delete was \
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
    // The progress line is cleared on EVERY exit path, including the four early returns inside
    // the replay — a half-drawn counter with the summary appended to it is the bug this
    // wrapper exists to prevent.
    let progress = crate::ui::Progress::new(mount.display_name());
    let result = from_zero_pages(api, control, handle, rules, mount, state, &progress);
    progress.finish();
    result
}

fn from_zero_pages(
    api: &Api,
    control: &ControlRoot,
    handle: &MountHandle,
    rules: &FsRules,
    mount: &Mount,
    state: &mut WsState,
    progress: &crate::ui::Progress,
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
                "{}: the workspace was resynced mid-replay - the repair stays pending; \
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
        progress.set(&format!(
            "rebuilding: {}",
            crate::ui::plural(state.nodes.len(), "node", "nodes")
        ));
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
                    "{}: count still mismatched after a full replay ({live} vs {}) - \
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
                    "{}: the workspace was resynced mid-replay - the repair stays pending; \
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
/// One mount's freshness, as a VALUE rather than a printed line.
///
/// v0.28.6 D3: the same probe now answers two readers — a person running `docli sync --check`,
/// and the `SessionStart` hook, whose channel is STDOUT (`--check` wrote every line with a bare
/// `eprintln!`, so as shipped the hook would have delivered an empty string — Goal 2 failing
/// silently in exactly the way the original defect did). Two renderers, one probe.
pub struct CheckOutcome {
    pub fresh: bool,
    /// One sentence, already naming the mount and the command that fixes it.
    pub message: String,
}

fn check_mount(
    api: &Api,
    control: &ControlRoot,
    handle: &MountHandle,
    mount: &Mount,
    state: &mut WsState,
) -> Result<CheckOutcome> {
    let stale = |message: String| {
        Ok(CheckOutcome {
            fresh: false,
            message,
        })
    };
    let ws = mount.workspace;
    if state.has_transient_parks() {
        // Every stale return reconciles the marker (Codex round 31): a crash between the
        // state save and the marker write must not leave them contradicting.
        persist_incomplete(control, handle, ws, state)?;
        return stale(format!(
            "{}: stale - parked deliveries are waiting (remove the occupants, then run \
             `docli sync --full`)",
            mount.display_name()
        ));
    }
    // Owed directory removals consult the DEBT SET directly (Codex round 3 — pairing them
    // through parks both clobbered structural parks and let a restart's `--check` read a
    // healthy probe over a nonempty debt map).
    if !state.pending_removals.is_empty() {
        persist_incomplete(control, handle, ws, state)?;
        return stale(format!(
            "{}: stale - directory removals blocked by untracked occupants: {} (remove \
             them, then run `docli sync`)",
            mount.display_name(),
            state.pending_removals.len()
        ));
    }
    let req = ephemeral_request(ws, state.cursor, state.epoch, 1);
    let resp = match api.pull(&req)? {
        Ok(r) => r,
        Err(ApiFailure::EpochChanged { .. }) => {
            state.from_zero = true;
            persist_incomplete(control, handle, ws, state)?;
            return stale(format!(
                "{}: stale - the workspace was resynced; run `docli sync`",
                mount.display_name()
            ));
        }
        Err(f) => return Err(map_failure(f)),
    };
    if !head_reaching(&resp, 1) {
        // Behind the server means the mirror is NOT a complete projection (Codex round 31):
        // say so durably, or a lock-free `search` keeps reporting `none` and `docli read` keeps
        // serving from it without disclosing anything.
        state.at_head = false;
        persist_incomplete(control, handle, ws, state)?;
        return stale(format!(
            "{}: stale - behind the server; run `docli sync`",
            mount.display_name()
        ));
    }
    let Some(live) = resp.live_nodes else {
        bail!(rollback_warning(ws));
    };
    if live != state.ledger.len() as i64 {
        // A hard purge landed between syncs (it mints only a row-less barrier rev, so the
        // cursor reads as caught-up over a stale file). Durable flag + marker + non-zero.
        state.from_zero = true;
        persist_incomplete(control, handle, ws, state)?;
        return stale(format!(
            "{}: stale - server live count {live} != mirror {}; run `docli sync`",
            mount.display_name(),
            state.ledger.len()
        ));
    }
    // Confirmed at head with a matching count: HEAL the crash-window state (a crash between a
    // page commit and the head commit leaves `at_head = false` with a complete mirror and the
    // incomplete marker present — the probe just proved completeness, so make the marker and
    // the manifest say so rather than printing «fresh» over a lying marker; Codex round 1).
    //
    // The head TIME is healed on the same argument, and it has to be: a stamp in the future —
    // a clock correction — makes the age unreadable, and `WsState::unusable_reason` then has
    // `search` and `read` calling the mirror unusable while THIS command, the freshness
    // authority, keeps answering «fresh» and exiting 0. Two authorities disagreeing about one
    // mirror is the exact defect the shared readiness predicate exists to prevent, and the probe
    // has just established the one fact the field records: the cursor is at the server's head,
    // now. So it can fix this rather than report around it.
    let now = now_unix();
    if !state.at_head || state.head_reached_at.is_none_or(|t| t > now) {
        state.at_head = true;
        state.head_reached_at = Some(now);
    }
    // ALWAYS reconcile (Codex round 30): a crash between the state save and the marker
    // removal leaves a lying CACHE_INCOMPLETE over a state that already says head — the probe
    // just proved freshness, so the marker must say so too.
    persist_incomplete(control, handle, ws, state)?;
    Ok(CheckOutcome {
        fresh: true,
        message: format!("{}: fresh", mount.display_name()),
    })
}

// ─────────────────────────────────────────────────────────────────────────────────────────────
// The SessionStart freshness report (v0.28.6 D3)
// ─────────────────────────────────────────────────────────────────────────────────────────────

/// The budget for the hook's NETWORK PROBES. A `SessionStart` hook that can hold a session open
/// is a hook people disable, so this is enforced in process and spent down across mounts.
///
/// **What it does NOT bound, stated rather than over-claimed:** `api.pull` may first wait on the
/// credential-store lock and refresh an expired token, and the refresh path retries a
/// `503 Retry-After` with real sleeps. None of that is a reqwest timeout, so none of it is
/// inside this number. The mitigation is to not enter that path at all — the probe runs only
/// while the STORED token is still live (the same rule `docli status` follows for the same
/// reason) — and the outer backstop is the harness `timeout` key `hooks.rs` writes.
const HOOK_BUDGET: std::time::Duration = std::time::Duration::from_secs(2);

/// Below this much left, a mount is not probed at all. A window this short buys a DNS lookup and
/// nothing else, and a request that is going to time out anyway is worse than an honest
/// «unknown»: it spends the tail of the budget the mounts after it would have used.
const MIN_PROBE: std::time::Duration = std::time::Duration::from_millis(200);

/// Where the freshness answer goes: the report a hook prints on STDOUT.
///
/// The whole reason this exists: `--check` writes every line with `eprintln!`/`ui`, i.e. to
/// STDERR, and neither agent reads a hook's stderr. As shipped, the hook would have delivered an
/// empty string — Goal 2 failing silently in exactly the way the defect this slice fixes did.
/// Redirecting `2>&1` is not the fix either: it would merge progress and lock noise into the
/// model's context.
///
/// Both vendors document the SAME envelope (verified 2026-09-01 —
/// `code.claude.com/docs/en/hooks`, `learn.chatgpt.com/docs/hooks.md`), so the two arms render
/// identically today. The discriminator is kept because that convergence is an observed fact
/// about two products, not a promise either of them makes.
fn emit_report(agent: crate::hooks::HookAgent, lines: &[String]) {
    let text = lines.join("\n");
    let body = match agent {
        crate::hooks::HookAgent::Claude | crate::hooks::HookAgent::Codex => serde_json::json!({
            "hookSpecificOutput": {
                "hookEventName": "SessionStart",
                "additionalContext": text,
            }
        }),
    };
    // Deliberately `println!`, never `ui::*`: `ui::ok` routes through `chatter` and is
    // suppressed by `-q`, which would silently turn «fresh» into «no answer» for any user who
    // happens to have that flag in their environment. The machine channel is not quiet-able.
    println!("{body}");
}

/// `docli sync --check --agent <a>` — the `SessionStart` hook's whole body.
///
/// **Reports; never syncs.** A hook that silently downloaded a workspace at session start would
/// be doing significant work the user did not ask for, on a possibly metered machine.
///
/// **Always exits 0.** Session start is not a place to fail a session over a stale cache. Every
/// branch below — including the ones that are not staleness at all — is a line of context, and
/// each one names the command that resolves it.
///
/// It is safe to fire this often for the reason v0.28.0 D2a built: the probe sends
/// `ephemeral: true`, so it does not register a client, pin the purge safe horizon, or light the
/// «deletes after device sync» badge. It does still write a `read_audit` row per mount per
/// session — write amplification of an existing audit, named rather than glossed.
pub fn hook_check(cwd: &Path, agent: crate::hooks::HookAgent) -> i32 {
    let mut lines: Vec<String> = Vec::new();
    // The update notice rides the same line (D11) and is cache-only here: the freshness probe
    // owns the budget, so a cold cache is skipped silently and left for the next hand-run
    // `docli` invocation to warm.
    if let Some(n) = crate::selfupdate::cached_notice() {
        lines.push(n);
    }
    for line in freshness_lines(cwd) {
        lines.push(line);
    }
    if !lines.is_empty() {
        emit_report(agent, &lines);
    }
    0
}

/// The freshness half, as plain sentences. Separated from the emission so the branches can be
/// tested without a terminal, a hook, or a schema.
fn freshness_lines(cwd: &Path) -> Vec<String> {
    let deadline = std::time::Instant::now() + HOOK_BUDGET;
    // Not a project at all: silence. A hook installed in one repository must not narrate in
    // every other directory the user opens.
    let Some(root) = crate::config::find_project(cwd) else {
        return Vec::new();
    };
    let project = match crate::config::load_project(&root) {
        Ok(p) => p,
        Err(e) => {
            return vec![format!(
                "docli: docli.toml here cannot be read ({e:#}) - the mirror's freshness is \
                 unknown; run `docli init` to repair the project"
            )]
        }
    };
    // A config with NO mounts is a legal intermediate state (`docli init` with no arguments
    // writes exactly that), and it is checked before geometry because `validate_config` rightly
    // refuses an empty mount table — reporting that as a setup failure would nag every session
    // in a project somebody has only started setting up.
    if project.config.mounts.is_empty() {
        return Vec::new();
    }
    // Geometry is the TEAMMATE's default state, not an edge case: they cloned the repository,
    // so they have the hook entry, no mirror on disk (it is git-ignored) and possibly no
    // `.gitignore` line. That is a setup state naming `docli init`, never staleness and never
    // a crash.
    if let Err(e) = validate_geometry(&project.root, &project.config) {
        return vec![format!(
            "docli: this project's mirror is not set up on this machine ({e:#}) - run \
             `docli init` and then `docli sync`"
        )];
    }
    // Signed out is its own answer, and it must not read as staleness. (It also cannot live
    // inside `run`: `main` resolves the API before `sync` is reached at all.)
    let store = match crate::creds::CredsStore::open_default() {
        Ok(s) => s,
        Err(e) => {
            return vec![format!(
                "docli: this device's credentials cannot be read ({e:#}) - run `docli login`"
            )]
        }
    };
    match store.get(&project.config.server) {
        // A LIVE token, not merely a present one. An expired one sends `api.pull` through the
        // refresh path — a credential lock it may wait on, and a `503 Retry-After` loop that
        // sleeps — none of which a reqwest timeout bounds. A session start is not the place to
        // discover that, so this reports instead of renewing; the next hand-run `docli` command
        // refreshes properly.
        Ok(Some(c)) if c.expires_at <= now_unix() + 60 => {
            return vec![format!(
                "docli: this device's sign-in to {} needs renewing, so mirror freshness was \
                 not checked - run `docli sync` (it will refresh)",
                project.config.server
            )]
        }
        Ok(Some(_)) => {}
        Ok(None) => {
            return vec![format!(
                "docli: this device is not signed in to {} - the mirror may be out of date; \
                 run `docli login`, then `docli sync`",
                project.config.server
            )]
        }
        Err(e) => {
            return vec![format!(
                "docli: this device's credentials cannot be read ({e:#}) - run `docli login`"
            )]
        }
    }
    // Deliberately NOT created here: a hook that has nothing to check must leave the tree
    // exactly as it found it. `one_mount_line` creates it only once it knows there is state.
    let control = project.control_root();
    let mut out = Vec::new();
    for mount in &project.config.mounts {
        // The budget is the WHOLE hook's, and it is spent DOWN across mounts rather than
        // re-granted to each of them. A single client built once with the full budget would
        // bound each REQUEST at 2 s and the run at 2 s × mounts — this repository has four,
        // which is eight seconds of somebody's session start. So each mount gets a client
        // whose timeout is what is actually left, and one below the floor does not start at
        // all: never begin work — least of all a durable state write — inside a window it
        // cannot finish.
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left < MIN_PROBE {
            out.push(format!(
                "{}: freshness unknown (the check ran out of time - offline?) - run \
                 `docli sync --check`",
                mount.display_name()
            ));
            continue;
        }
        // A second store handle per mount: `Api::with_timeout` takes ownership, and reading the
        // credential file again is far cheaper than the request it is about to bound.
        let api = crate::creds::CredsStore::open_default()
            .ok()
            .and_then(|s| crate::http::Api::with_timeout(&project.config.server, s, left).ok());
        let Some(api) = api else {
            out.push(format!(
                "{}: freshness unknown (the client could not be built) - run \
                 `docli sync --check`",
                mount.display_name()
            ));
            continue;
        };
        out.push(one_mount_line(&project, &api, &control, mount));
    }
    out
}

fn one_mount_line(project: &Project, api: &Api, control: &ControlRoot, mount: &Mount) -> String {
    let ws = mount.workspace;
    // STATE FIRST, and the order is the point. `claim_mount` CREATES the mirror directory and
    // writes an ownership marker into it — so asking it first meant a teammate's fresh clone
    // had its mirror tree conjured into existence by an unprompted session-start hook, only to
    // be told «not mirrored yet». This module promises it REPORTS and does not sync; creating
    // and claiming directories on a schedule nobody set is not that promise. Reading the state
    // file without the lock is already established practice here (a concurrent `docli search`
    // does exactly this).
    // The pre-lock read answers ONE question — «is there anything here to check?» — and its
    // VALUE is deliberately discarded: the snapshot that gets used is re-read under the lock
    // below, because this one can go stale between here and there.
    match control.load_state(ws) {
        Ok(Some(_)) => {}
        Ok(None) => {
            return format!(
                "{}: not mirrored on this machine yet - run `docli sync` before trusting \
                 local files for this workspace",
                mount.display_name()
            )
        }
        Err(e) => {
            return format!(
                "{}: freshness unknown (its state file will not read: {e:#}) - run \
                 `docli sync --full`",
                mount.display_name()
            )
        }
    }
    // …and the DIRECTORY is still there. `claim_mount` creates the mount tree and writes an
    // ownership marker into it, so without this a mount whose mirror was deleted — by hand, or
    // by a `docli uninstall --purge` racing this very hook — gets both conjured back by a
    // session start nobody asked to do that. Checking first does not close the race (nothing
    // could, short of not claiming at all), but it turns the common orderings from
    // «resurrected» into «reported», and the report is the more useful answer anyway.
    let mount_path = mount_abs(&project.root, mount);
    if !mount_path.is_dir() {
        return format!(
            "{}: the mirror directory is gone ({}) - run `docli sync` to rebuild it",
            mount.display_name(),
            mount.dir
        );
    }
    // There IS state and a directory, so the claim below takes the lock rather than creating
    // anything.
    if std::fs::create_dir_all(&control.dir).is_err() {
        return format!(
            "{}: freshness unknown (.docli/ is not writable) - run `docli sync --check`",
            mount.display_name()
        );
    }
    let handle = match claim_mount(&mount_path, &control.dir, ws) {
        Ok(h) => h,
        Err(e) if crate::mountfs::is_busy(&e) => {
            return format!(
                "{}: check skipped (another docli is running) - run `docli sync --check` \
                 when it finishes",
                mount.display_name()
            )
        }
        Err(e) => {
            return format!(
                "{}: freshness unknown ({e:#}) - run `docli sync --check`",
                mount.display_name()
            )
        }
    };
    // RE-READ under the lock, and this is not belt-and-braces. The read above happens BEFORE the
    // lock (deliberately — see its comment: claiming first would create a mirror tree on a fresh
    // clone), which opens a window: a `docli sync` can win the lock, commit a newer state and
    // exit while we hold a snapshot from before it. Both `invalidator` and `check_mount` PERSIST
    // what they were handed, so the stale snapshot would be written back over the newer one —
    // rolling back the cursor and marking a freshly synced mirror incomplete. The pre-lock read
    // decides only «is there anything to check»; the value that gets used is this one.
    let mut state = match control.load_state(ws) {
        Ok(Some(fresh)) => fresh,
        // It vanished between the two reads (a concurrent `--purge`, a hand `rm -rf`). Nothing
        // to report and nothing to persist.
        Ok(None) => {
            return format!(
                "{}: not mirrored on this machine yet - run `docli sync` before trusting \
                 local files for this workspace",
                mount.display_name()
            )
        }
        Err(e) => {
            return format!(
                "{}: freshness unknown (its state file will not read: {e:#}) - run \
                 `docli sync --full`",
                mount.display_name()
            )
        }
    };
    if let Some(reason) = invalidator(&state, mount, &handle.root, control) {
        state.from_zero = true;
        let _ = persist_incomplete(control, &handle, ws, &state);
        return format!(
            "{}: STALE - {reason}; run `docli sync` before trusting local files",
            mount.display_name()
        );
    }
    match check_mount(api, control, &handle, mount, &mut state) {
        Ok(o) if o.fresh => o.message,
        Ok(o) => format!("{} - do not trust local files until it succeeds", o.message),
        Err(e) if is_no_access(&e) => no_access_message(mount.display_name()),
        Err(e) => format!(
            "{}: freshness unknown ({e:#}) - run `docli sync --check`",
            mount.display_name()
        ),
    }
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
        // The v0.28.0 D10.3 standing pin, in the language the CLI now speaks: designation
        // without authority — docli.toml NAMES the workspace, login state decides reach, and
        // this branch must never suggest that pasting a credential is the remedy.
        let msg = no_access_message("книга продаж");
        let lower = msg.to_lowercase();
        assert!(!lower.contains("token"), "{msg}");
        assert!(!lower.contains("токен"), "{msg}");
        assert!(lower.contains("ask the workspace owner"), "{msg}");
        assert!(
            msg.contains("книга продаж"),
            "the AUTHOR's mount name, whatever language it is in: {msg}"
        );
    }

    #[test]
    fn the_hook_report_lands_on_stdout_in_the_documented_envelope() {
        // The whole reason the machine mode exists: `--check` writes to STDERR, and neither
        // agent reads a hook's stderr. Both vendors document the same SessionStart envelope
        // (verified 2026-09-01), so the two arms render identically today — recorded here so a
        // future divergence is a visible test change rather than a silent empty context.
        for agent in crate::hooks::HookAgent::all() {
            let mut buf = Vec::new();
            let body = {
                // Render exactly what `emit_report` prints, without capturing the process's
                // real stdout.
                let text = ["a: fresh".to_string(), "b: STALE".to_string()].join("\n");
                serde_json::json!({
                    "hookSpecificOutput": {
                        "hookEventName": "SessionStart",
                        "additionalContext": text,
                    }
                })
            };
            use std::io::Write as _;
            write!(buf, "{body}").unwrap();
            let v: serde_json::Value = serde_json::from_slice(&buf).unwrap();
            assert_eq!(v["hookSpecificOutput"]["hookEventName"], "SessionStart");
            assert!(v["hookSpecificOutput"]["additionalContext"]
                .as_str()
                .unwrap()
                .contains("STALE"));
            let _ = agent;
        }
    }

    #[test]
    fn outside_a_project_the_hook_says_nothing_at_all() {
        // A hook installed in one repository must not narrate in every other directory the
        // user opens.
        let tmp = tempfile::tempdir().unwrap();
        assert!(freshness_lines(tmp.path()).is_empty());
    }

    #[test]
    fn an_unreadable_project_reports_a_setup_state_never_staleness() {
        // The TEAMMATE's default state, not an edge case: they cloned the repository, so they
        // have the hook entry and no mirror on disk. It must read as setup, name `docli init`,
        // and never look like «your cache is behind».
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("docli.toml"), "server = [[[ broken").unwrap();
        let lines = freshness_lines(tmp.path());
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("docli init"), "{}", lines[0]);
        assert!(!lines[0].to_lowercase().contains("stale"), "{}", lines[0]);
    }

    #[test]
    fn a_project_with_no_mounts_says_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("docli.toml"),
            "server = \"https://docli.ru\"\n",
        )
        .unwrap();
        assert!(freshness_lines(tmp.path()).is_empty());
    }

    #[test]
    fn a_signed_out_machine_says_so_and_does_not_read_as_staleness() {
        let tmp = tempfile::tempdir().unwrap();
        // DOCLI_HOME keeps the test off the developer's real credentials — and the lock
        // keeps it off the OTHER tests that override the same process-global variable.
        let _home = crate::creds::home_env_lock();
        std::env::set_var("DOCLI_HOME", tmp.path().join("home"));
        std::fs::write(
            tmp.path().join("docli.toml"),
            "server = \"https://example.invalid\"\n\n[[mount]]\nworkspace = \
             \"00000000-0000-0000-0000-000000000001\"\ndir = \"m\"\n",
        )
        .unwrap();
        let lines = freshness_lines(tmp.path());
        std::env::remove_var("DOCLI_HOME");
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].contains("docli login"), "{}", lines[0]);
        assert!(!lines[0].contains("stale"), "{}", lines[0]);
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
