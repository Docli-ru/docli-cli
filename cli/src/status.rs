// SPDX-FileCopyrightText: 2026 OOO Agitek
// SPDX-License-Identifier: MIT

//! `docli status` (0.1.1) — one screen answering «в каком я состоянии».
//!
//! Everything the CLI knew was previously spread across four commands and one file nobody
//! reads: whether this device is signed in (only discoverable by running something that
//! fails), what is mounted (`docli.toml`), how fresh each mirror is (`.docli/` state), and
//! which agent configurations carry the connection (grep). This gathers them.
//!
//! **Offline by default.** Every field except the account identity is read from disk, so
//! `docli status` answers on a plane or behind a captive portal; the identity line degrades to
//! what the local credential can say (present / expired) instead of failing the command.
//! `--online` is not a flag: the identity probe is attempted when a credential exists and
//! silently downgrades, because a status command that can hang is a status command people
//! stop running.

use std::path::Path;

use anyhow::Result;
use serde::Serialize;

use crate::config::{self, Mount};
use crate::creds::CredsStore;
use crate::state::ControlRoot;
use crate::ui;

#[derive(Serialize)]
pub struct MountStatus {
    pub name: String,
    pub workspace: String,
    pub dir: String,
    pub folder: Option<String>,
    /// None when the mount has never synced.
    pub nodes: Option<usize>,
    pub at_head: bool,
    pub incomplete: bool,
    pub parks: usize,
    /// Seconds since the cursor last reached head.
    pub head_age_secs: Option<i64>,
    pub exists_on_disk: bool,
    /// The state tracks nodes but the mirror directory is empty - deleted out from under us.
    pub emptied: bool,
    /// The `.docli/` state for this workspace exists but could not be read or parsed.
    pub state_error: Option<String>,
    /// The configured directory carries OUR `MOUNT.docli` (this control plane, this workspace).
    /// False after a re-point that has not been synced, or when someone replaced the directory.
    pub claimed: bool,
}

#[derive(Serialize)]
pub struct Status {
    pub version: &'static str,
    pub server: String,
    pub signed_in: bool,
    /// The credential store could not be opened or read — distinct from «not signed in», and
    /// not repaired by the `docli login` that answer would send the reader to.
    pub credential_error: Option<String>,
    pub account: Option<String>,
    /// Seconds until the access token's nominal expiry (negative once past it).
    pub token_expires_in: Option<i64>,
    pub project_root: Option<String>,
    /// Set when the project here is configured for a DIFFERENT server than the one reported:
    /// its mounts and agents are then deliberately absent rather than misattributed.
    pub project_other_server: Option<String>,
    pub mounts: Vec<MountStatus>,
    pub agents_wired: Vec<String>,
    pub gitignore_missing: Vec<String>,
    /// git could not answer whether a mirror is ignored (broken repository, `safe.directory`).
    /// Reported and counted as degraded: `sync` refuses on exactly this.
    pub gitignore_unknown: Vec<String>,
    /// The two hook-capable agents and what this project carries for each (v0.28.6 D2).
    ///
    /// This is the COUNTERWEIGHT to the guarded hook command. Making a missing binary silent
    /// keeps a stale entry from breaking every tool call, and the vendor docs warn about exactly
    /// that trade — *"a mistyped path in `settings.json` leaves the gate silently disabled"*.
    /// A disabled gate has to be noticeable somewhere, and this is the somewhere.
    pub hooks: Vec<crate::hooks::HookStatus>,
    /// «docli-cli 0.1.2 -> 0.1.3 - update: docli self-update», when the signed manifest advertises
    /// a newer version (D11). A field, not just a line: machine consumers should be able to see
    /// a stale binary, and hiding it from `--json` would be the same silent-omission mistake
    /// this whole slice is about.
    pub update_available: Option<String>,
}

/// Long enough for a healthy round-trip, short enough that a blackholed network cannot hold an
/// offline-first status screen hostage.
const IDENTITY_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// «3h ago» / «just now» — a duration a human reads without doing arithmetic.
pub fn human_age(secs: i64) -> String {
    let s = secs.max(0);
    match s {
        0..=59 => "just now".to_string(),
        60..=3599 => format!("{}m ago", s / 60),
        3600..=86399 => format!("{}h ago", s / 3600),
        _ => format!("{}d ago", s / 86400),
    }
}

/// «in 42m» / «expired» — the same for a deadline.
pub fn human_until(secs: i64) -> String {
    if secs <= 0 {
        return "expired".to_string();
    }
    match secs {
        1..=3599 => format!("in {}m", (secs / 60).max(1)),
        3600..=86399 => format!("in {}h", secs / 3600),
        _ => format!("in {}d", secs / 86400),
    }
}

pub fn gather(cwd: &Path, server: &str) -> Result<Status> {
    // A store that cannot be OPENED or READ is not «signed out»: rendering it that way sent the
    // reader to `docli login`, which fails on the same broken file. Report the condition.
    let (store, store_error) = match CredsStore::open_default() {
        Ok(s) => match s.get(server) {
            Ok(_) => (Some(s), None),
            Err(e) => (None, Some(format!("{e:#}"))),
        },
        Err(e) => (None, Some(format!("{e:#}"))),
    };
    let had_credential = store
        .as_ref()
        .and_then(|s| s.get(server).ok().flatten())
        .is_some();

    // The identity probe: `viewer` self-introspection is open to a sync-scoped PAT by design,
    // so this needs no extra scope. Any failure downgrades the line, never the command. A
    // SHORT-timeout client, because status is offline-first: a captive portal that blackholes
    // packets must not hold the screen for the API client's default two minutes.
    // (A second handle, because `Api::with_timeout` takes ownership of the store.)
    // …and ONLY while the stored token is still valid — plus `viewer_label` itself never
    // rotates on 401 (`graphql_no_refresh`). Both halves are needed: the refresh path retries a
    // `503 Retry-After` three times at two minutes each, which would hold this offline-first
    // screen for six, and the 5s client bounds ONE request, not that loop. A status command is
    // a reader: it reports what the credential says, it does not renew it.
    let token_live = store
        .as_ref()
        .and_then(|s| s.get(server).ok().flatten())
        .is_some_and(|c| c.expires_at > now_unix() + 60);
    let account = if had_credential && token_live {
        CredsStore::open_default()
            .ok()
            .and_then(|own| {
                crate::http::Api::with_timeout(server, own, IDENTITY_PROBE_TIMEOUT).ok()
            })
            .and_then(|api| api.viewer_label().ok())
    } else {
        None
    };
    // Re-read the credential AFTER the probe — BOTH fields come from this read. The probe
    // refreshes a lapsed token (so a pre-refresh expiry read «токен истёк» on a working
    // sign-in), and an `invalid_grant` makes the refresh DELETE the entry (so a pre-probe
    // `signed_in` would keep claiming «вход выполнен» for a credential the server just
    // rejected, and exit 0 with it).
    let after = store.as_ref().and_then(|s| s.get(server).ok().flatten());
    let signed_in = after.is_some();
    let token_expires_in = after.map(|c| c.expires_at - now_unix());

    let mut status = Status {
        version: env!("CARGO_PKG_VERSION"),
        server: server.to_string(),
        signed_in,
        credential_error: store_error,
        account,
        token_expires_in,
        project_root: None,
        project_other_server: None,
        mounts: Vec::new(),
        agents_wired: Vec::new(),
        gitignore_missing: Vec::new(),
        gitignore_unknown: Vec::new(),
        hooks: Vec::new(),
        // Cached, so this costs the network at most once a day and never fails the command.
        update_available: crate::selfupdate::notice(),
    };

    let Some(root) = config::find_project(cwd) else {
        return Ok(status);
    };
    let project = config::load_project(&root)?;
    status.project_root = Some(root.display().to_string());
    // Everything below — mounts, mirrors, wired agents — belongs to the project's OWN server.
    // Reporting it under a different `--server` would label another origin's state as this
    // one's; the header still names the project so the reader knows why the rest is missing.
    if project.config.server.trim_end_matches('/') != server {
        status.project_other_server = Some(project.config.server.clone());
        return Ok(status);
    }
    // The PROJECT's control root — `~/.docli` since the cache became per-machine, not
    // `<project>/.docli`. Built from the project rather than the project ROOT: comparing a
    // mirror's ownership marker against the wrong directory made `docli status` report a
    // perfectly synced mount as «never synced / not this mirror», with its own node count
    // printed on the line above.
    let control = project.control_root();
    for m in &project.config.mounts {
        status.mounts.push(mount_status(&root, &control, m));
        // A git question nobody can answer is REPORTED, not silently rendered as «fine»: this
        // screen would otherwise exit 0 while `sync` refuses on the very same check.
        match crate::wizard::missing_ignores(&root, &m.dir) {
            Ok(fixes) => {
                for fix in fixes {
                    // Serialized as the label, so `--json` shows WHICH .gitignore a
                    // nested-repository entry belongs in, not a bare pattern nobody can place.
                    let label = fix.label(&root);
                    if !status.gitignore_missing.contains(&label) {
                        status.gitignore_missing.push(label);
                    }
                }
            }
            Err(e) => {
                let why = format!("{e:#}");
                if !status.gitignore_unknown.contains(&why) {
                    status.gitignore_unknown.push(why);
                }
            }
        }
    }
    status.agents_wired = crate::agents::wired_here(&root, &project.config.server);
    status.hooks = crate::hooks::HookAgent::all()
        .into_iter()
        .map(|a| crate::hooks::status(&root, a))
        .collect();
    Ok(status)
}

fn mount_status(root: &Path, control: &ControlRoot, m: &Mount) -> MountStatus {
    // A state file that will not READ is not «never synced»: `docli sync` will fail parsing it
    // rather than perform a first sync, so the screen must not send the reader that way.
    let (state, state_error) = match control.load_state(m.workspace) {
        Ok(s) => (s, None),
        Err(e) => (None, Some(format!("{e:#}"))),
    };
    let dir = config::mount_abs(root, m);
    MountStatus {
        name: m.display_name().to_string(),
        workspace: m.workspace.to_string(),
        dir: m.dir.clone(),
        folder: m.folder.clone(),
        nodes: state.as_ref().map(|s| s.nodes.len()),
        at_head: state.as_ref().is_some_and(|s| s.at_head),
        // The same predicates `CACHE_INCOMPLETE.docli` is written for — a status screen that
        // disagreed with the marker in the mirror would be worse than no status screen. The four
        // state-only terms are `WsState::incomplete` (v0.29.0 D4, one home shared with
        // `persist_incomplete`); the scope term is added HERE because it compares state against
        // `docli.toml`, which the state cannot see: a folder scope edited since the last sync
        // means the cursor already ran past out-of-scope nodes, so the mirror answers for a
        // scope that is no longer configured, and `sync` forces from-zero on that comparison.
        incomplete: state
            .as_ref()
            .is_some_and(|s| s.incomplete() || s.scope_key != m.folder),
        parks: state.as_ref().map(|s| s.parks.len()).unwrap_or(0),
        // SATURATING, and clamped at zero: `.docli/state` is untrusted input, so a hand-edited
        // `i64::MIN` would panic in a debug build and wrap in release — where it renders as
        // «just now», which is the one thing an age is supposed to rule out. A stamp in the
        // future clamps to 0 rather than going negative; `WsState::unusable_reason` is what
        // reports that condition, and this column is not the place to invent a second answer.
        head_age_secs: state
            .as_ref()
            .and_then(|s| s.head_reached_at)
            .map(|t| now_unix().saturating_sub(t).max(0)),
        exists_on_disk: dir.is_dir(),
        state_error,
        // State is keyed by WORKSPACE, so it says nothing about the directory configured NOW.
        // Re-point a synced workspace at any other directory and the old clean state plus the
        // new directory's existence would have rendered a healthy row for a mirror that does
        // not exist. The marker in the directory itself is the only proof that binds the two.
        claimed: crate::mountfs::verify_mount_identity(&dir, &control.dir, m.workspace),
        // State says N nodes; the directory says nothing at all. Deleting a mirror and
        // recreating the empty directory left `exists_on_disk` true and every state predicate
        // clean, so the screen called a missing mirror healthy. This is a CHEAP check (one
        // readdir, no tree walk) and deliberately not a reconciliation — a partially deleted
        // mirror is `docli doctor`'s question, and the row says so.
        emptied: state.as_ref().is_some_and(|s| !s.nodes.is_empty())
            && dir.is_dir()
            && std::fs::read_dir(&dir)
                .map(|mut d| d.next().is_none())
                .unwrap_or(false),
    }
}

pub fn run(cwd: &Path, server: &str, json: bool) -> Result<i32> {
    if json {
        ui::machine_mode();
    } else {
        // The whole screen is this command's product (see `ui::report_mode`).
        ui::report_mode();
    }
    let status = gather(cwd, server)?;
    // The exit code answers the same question in both modes — `--json` is the form a health
    // check actually uses, so returning 0 there regardless made the code useless exactly where
    // it matters. A mount that has NEVER synced counts as degraded too: `incomplete` is false
    // for it (there is no state to be incomplete) while the screen says «не синхронизировано».
    let degraded = !status.signed_in
        || status.credential_error.is_some()
        || status.mounts.iter().any(|m| {
            m.incomplete || !m.exists_on_disk || m.nodes.is_none() || m.emptied || !m.claimed
        })
        || !status.gitignore_missing.is_empty()
        || !status.gitignore_unknown.is_empty();
    if json {
        println!("{}", serde_json::to_string_pretty(&status)?);
        return Ok(if degraded { 1 } else { 0 });
    }
    render(&status);
    Ok(if degraded { 1 } else { 0 })
}

fn render(s: &Status) {
    ui::heading(&format!("docli-cli {}", s.version));
    let w = ui::label_width(["server", "signed in", "project"]);
    ui::field("server", &s.server, w);
    match (&s.account, s.signed_in) {
        (Some(who), _) => {
            let exp = s
                .token_expires_in
                .map(|t| format!("  {}", ui::dim(&format!("token {}", human_until(t)))))
                .unwrap_or_default();
            ui::field("signed in", &format!("{who}{exp}"), w);
        }
        (None, true) => ui::field(
            "signed in",
            &format!("yes {}", ui::dim("(account name unavailable offline)")),
            w,
        ),
        (None, false) => match &s.credential_error {
            Some(why) => ui::field("signed in", &format!("unknown - {why}"), w),
            None => ui::field("signed in", &format!("no - {}", ui::cmd("docli login")), w),
        },
    }
    // BEFORE the early returns below, not after them. `main` exempts `status` from the general
    // notice on the grounds that this screen renders it itself — so a return that skips it means
    // nobody prints it at all. `docli status` from a home directory (no project) or in a project
    // configured for another server was fetching the manifest and then saying nothing.
    if let Some(n) = &s.update_available {
        // On STDOUT here, deliberately: `status` runs in report mode, where the screen IS the
        // product. The rule the notice follows is «never interleaved into another command's
        // stdout product», not «never on stdout».
        ui::next(n);
    }
    match &s.project_root {
        Some(root) => {
            ui::field("project", root, w);
            if let Some(other) = &s.project_other_server {
                ui::detail(&format!(
                    "this project is configured for {other} - its mounts and agents are not \
                     shown here",
                ));
                return;
            }
        }
        None => {
            ui::field(
                "project",
                &format!("none found - {}", ui::cmd("docli init")),
                w,
            );
            return;
        }
    }

    ui::heading("Mounts");
    if s.mounts.is_empty() {
        ui::detail("none - add one: docli init");
    }
    for m in &s.mounts {
        let head = match (&m.nodes, m.incomplete) {
            (None, _) if m.state_error.is_some() => format!(
                "{}  {}",
                m.name,
                ui::dim("state unreadable - docli sync --full rebuilds it")
            ),
            (None, _) => format!("{}  {}", m.name, ui::dim("never synced")),
            (Some(n), false) => {
                format!("{}  {}", m.name, ui::dim(&ui::plural(*n, "node", "nodes")))
            }
            (Some(n), true) => format!(
                "{}  {}",
                m.name,
                ui::dim(&format!(
                    "{}, cache incomplete",
                    ui::plural(*n, "node", "nodes")
                ))
            ),
        };
        if m.incomplete
            || m.nodes.is_none()
            || !m.exists_on_disk
            || m.emptied
            || !m.claimed
            || m.state_error.is_some()
        {
            ui::warn(&head);
        } else {
            ui::ok(&head);
        }
        let mw = ui::label_width(["cache", "scope", "updated", "parked"]);
        // The DIAGNOSIS without the PATH. `status` used to print the mount directory here, which
        // was harmless while it was a short project-relative name and became the leak the
        // live-agent gate measured on 2026-09-03: given only a `docli.toml`, Codex ran `docli
        // status`, took the absolute cache path out of this row, and grepped the mirror —
        // defeating v0.29.1 D1 through a field nobody thought of as an address.
        //
        // Nothing is lost that the reader can act on: the cache is not theirs to manage, every
        // state below names its own remedy, and `docli doctor` still prints real paths because
        // reconciling the filesystem is its job.
        let cache_state = match (m.exists_on_disk, m.emptied) {
            (false, _) => "not built yet - run docli sync",
            (true, true) => "empty - docli sync --full rebuilds it",
            (true, false) if !m.claimed => "not this mirror - run docli sync",
            (true, false) => "in this machine's docli cache",
        };
        ui::field("cache", cache_state, mw);
        if let Some(f) = &m.folder {
            ui::field("scope", f, mw);
        }
        if let Some(age) = m.head_age_secs {
            ui::field("updated", &human_age(age), mw);
        }
        if m.parks > 0 {
            ui::field(
                "parked",
                &format!("{} - {}", m.parks, ui::cmd("docli doctor")),
                mw,
            );
        }
    }

    ui::heading("Agents");
    if s.agents_wired.is_empty() {
        ui::detail(&format!(
            "the MCP connection is not wired into any configuration - {}",
            ui::cmd("docli init --mcp auto")
        ));
    } else {
        for a in &s.agents_wired {
            ui::ok(a);
        }
    }
    let installed: Vec<&crate::hooks::HookStatus> =
        s.hooks.iter().filter(|h| h.installed).collect();
    if installed.is_empty() {
        ui::detail(&format!(
            "no docli hooks here - the mirror is marked read-only and the contract asks agents \
             not to edit it, but nothing refuses a write. Add enforcement: {}",
            ui::cmd("docli init --hooks auto")
        ));
    } else {
        for h in installed {
            // The KEY is what `--json` carries (a stable identifier a script can match); the
            // screen gets the name a person would say.
            let name = crate::hooks::HookAgent::parse(h.agent)
                .map(|a| a.display())
                .unwrap_or(h.agent);
            // A gate that cannot run is worse than no gate, because it looks like one.
            if h.binary_resolves == Some(false) {
                ui::warn(&format!(
                    "{name}: hooks are installed but `docli` is not on PATH - they do nothing. \
                     Reinstall the CLI, or run `docli uninstall` to take the entries back out."
                ));
            } else {
                ui::ok(&format!(
                    "{name}: hooks installed - writes into the mirror are refused (shell writes \
                     are not covered)"
                ));
            }
        }
    }

    if !s.gitignore_missing.is_empty() || !s.gitignore_unknown.is_empty() {
        ui::heading("Git");
        if !s.gitignore_missing.is_empty() {
            ui::warn(".gitignore is missing lines - docli sync will refuse to run:");
            for e in &s.gitignore_missing {
                ui::detail(e);
            }
        }
        for e in &s.gitignore_unknown {
            ui::warn(e);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ages_and_deadlines_read_as_prose() {
        assert_eq!(human_age(0), "just now");
        assert_eq!(human_age(59), "just now");
        assert_eq!(human_age(60), "1m ago");
        assert_eq!(human_age(7200), "2h ago");
        assert_eq!(human_age(172_800), "2d ago");
        // A clock that moved backwards must not print a negative age.
        assert_eq!(human_age(-5), "just now");

        assert_eq!(human_until(0), "expired");
        assert_eq!(human_until(-1), "expired");
        // Under a minute left is still "in 1m", never "in 0m".
        assert_eq!(human_until(30), "in 1m");
        assert_eq!(human_until(3600), "in 1h");
    }

    #[test]
    fn the_update_notice_is_rendered_before_every_early_return() {
        // `main` exempts `status` from the general notice because this screen renders it
        // itself — so a `return` that skips it means NOBODY prints it. `docli status` from a
        // home directory, or in a project configured for another server, was fetching the
        // signed manifest and then saying nothing about it.
        //
        // Asserted structurally: the notice must appear in `render` before the first `return`.
        let src = include_str!("status.rs");
        let body = src
            .split_once("fn render(s: &Status) {")
            .expect("render exists")
            .1;
        let notice = body
            .find("s.update_available")
            .expect("the notice is rendered");
        let first_return = body
            .find("\n                return;")
            .expect("an early return");
        assert!(
            notice < first_return,
            "the update notice must be rendered before the first early return"
        );
    }

    #[test]
    fn status_outside_a_project_reports_no_project_and_does_not_fail() {
        let tmp = tempfile::tempdir().unwrap();
        // DOCLI_HOME keeps the test off the developer's real credentials — and the lock
        // keeps it off the OTHER tests that override the same process-global variable.
        let _home = crate::creds::home_env_lock();
        let home = tmp.path().join("home");
        std::env::set_var("DOCLI_HOME", &home);
        let s = gather(tmp.path(), "https://example.invalid").unwrap();
        std::env::remove_var("DOCLI_HOME");
        assert!(s.project_root.is_none());
        assert!(s.mounts.is_empty());
        assert!(!s.signed_in);
        assert_eq!(s.version, env!("CARGO_PKG_VERSION"));
    }
}
