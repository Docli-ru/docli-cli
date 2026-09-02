// SPDX-FileCopyrightText: 2026 OOO Agitek
// SPDX-License-Identifier: MIT

//! `docli search` (v0.28.0 D5, client half) — server BM25 across all mounts by default; results
//! carry LOCAL paths (or an explicit «not mirrored» tag), so grep can only ever EXTEND a hit,
//! never establish absence.
//!
//! The split-brain rule, degraded-aware: only a NON-degraded server search may conclude a note
//! does not exist; a degraded answer is INCONCLUSIVE about absence, and the CLI says so rather
//! than printing a bare empty result.

use std::path::Path;

use anyhow::Result;
use docli_sync_wire::{
    MirrorPosition, SearchRequest, SearchWorkspaceOutcome, SEARCH_WORKSPACE_CAP,
};
use uuid::Uuid;

use crate::config::{validate_config, Mount, Project};
use crate::http::Api;
use crate::state::{ControlRoot, TrackedKind, WsState};

pub fn run(project: &Project, api: &Api, query: &str, json: bool) -> Result<i32> {
    if json {
        crate::ui::machine_mode();
    } else {
        // Hits are the product; so is the «index was incomplete» caveat printed beside them.
        crate::ui::report_mode();
    }
    // Request-level validation only (Codex round 24): search works without a cache, so the
    // mirror-write geometry rules must not block a server query.
    validate_config(&project.config)?;
    let cwd = std::env::current_dir().unwrap_or_else(|_| project.root.clone());
    let control = ControlRoot::new(&project.root);
    let mounts: Vec<&Mount> = project.config.mounts.iter().collect();

    // The local read happens BEFORE the request, because the position rides the request
    // (v0.29.0 D2d) and the answer must describe the position actually sent. That same snapshot
    // renders local paths, so the verdict and the addresses beneath it describe ONE instant —
    // which is the point, and which is also a trade, stated rather than glossed: the id → path
    // map is now up to a round trip old. A concurrent `docli sync` that RENAMES makes the path
    // vanish and the hit renders an honest «not mirrored» (every address is stat-checked). A
    // concurrent SWAP — `A` moved away and some other node moved into `A` inside the window —
    // is the narrow case that does not vanish: the stale address exists, passes containment, and
    // names a different note. In-mirror, unlike the identity case, and the cost of the
    // alternative is a verdict describing one instant over paths describing another.
    //
    // The mount-IDENTITY anchor deliberately does NOT ride this snapshot: it is re-measured at
    // render time, because it is the one check `canonically_under` cannot stand in for (Codex
    // round 17 — a swapped root canonicalizes consistently against itself), and measuring it
    // before the round trip would widen THAT window, whose failure renders an address outside
    // the mirror entirely.
    let mut local: std::collections::HashMap<Uuid, MountLocal> =
        std::collections::HashMap::with_capacity(mounts.len());
    for m in &mounts {
        local.insert(
            m.workspace,
            read_local(project, &control, m, crate::sync_cmd::now_unix()),
        );
    }

    // Batch: a CLI with more than 16 mounts never hard-fails "search all mounts by default".
    let mut outcomes: Vec<SearchWorkspaceOutcome> = Vec::new();
    for chunk in mounts.chunks(SEARCH_WORKSPACE_CAP) {
        let positions: std::collections::BTreeMap<Uuid, MirrorPosition> = chunk
            .iter()
            .filter_map(|m| match local.get(&m.workspace).map(|l| &l.decision) {
                Some(AskDecision::Ask(p)) => Some((m.workspace, *p)),
                _ => None,
            })
            .collect();
        let req = SearchRequest {
            workspace_ids: chunk.iter().map(|m| m.workspace).collect(),
            query: query.to_string(),
            limit: None,
            // OMITTED, never `{}`, when no mount in this chunk qualifies — the two are
            // byte-different and absence is what the server reads as «did not ask».
            positions: (!positions.is_empty()).then_some(positions),
        };
        match api.search(&req)? {
            Ok(resp) => outcomes.extend(resp.workspaces),
            Err(f) => anyhow::bail!("{f}"),
        }
    }

    let mut rendered = Vec::new();
    let mut any_hit = false;
    let mut any_degraded = false;
    let mut any_refused = false;
    for o in &outcomes {
        // A server answering for an id we never asked about is malformed, not fatal — report
        // and skip rather than panicking over fifteen good outcomes.
        let Some(mount) = mounts.iter().find(|m| m.workspace == o.workspace_id) else {
            eprintln!(
                "docli: the server answered for an unrequested workspace {} - skipping",
                o.workspace_id
            );
            continue;
        };
        let l = local.get(&o.workspace_id).expect("every mount was read");
        rendered.push(render_workspace(project, &cwd, mount, l, o, &mut any_hit));
        any_degraded |= o.degraded;
        any_refused |= o.refused.is_some();
    }

    if json {
        println!("{}", serde_json::to_string_pretty(&rendered)?);
        // The JSON is the product and carries the state as a field; the sentence still goes out
        // so a human watching a `--json` run is not the only reader left uninformed. Under
        // `--json`, `machine_mode` leaves REPORT unset, so `ui` routes this to STDERR and the
        // JSON on stdout stays parseable.
        for r in &rendered {
            mirror_notice(r);
        }
    } else {
        let show_mount = project.config.mounts.len() > 1;
        for r in &rendered {
            print_workspace(r, show_mount);
        }
        if !any_hit {
            // A REFUSED workspace was not searched at all — strictly MORE inconclusive about
            // absence than a degraded one, so it must never fold into a bare «no hits» either
            // (the split-brain rule's summary half).
            if any_refused {
                crate::ui::warn(
                    "no hits - but at least one workspace was NOT searched (see the refusal \
                     above), so this is INCONCLUSIVE about absence",
                );
            } else if any_degraded {
                // Never a bare empty result on a degraded index.
                crate::ui::warn(
                    "no hits - but the note index was DEGRADED for at least one workspace, so \
                     this is INCONCLUSIVE about absence; retry shortly",
                );
            } else {
                crate::ui::detail("no hits");
            }
        }
    }
    Ok(0)
}

/// What `search` decided to ask the server about ONE mount (v0.29.0 D7).
///
/// A bare `Option<MirrorPosition>` was the first shape and it conflated four different local
/// facts: never synced, state unreadable, an unusable projection, and a directory that is not
/// this workspace's mirror. They must not render alike — a never-synced mount has to be SILENT
/// (searching without a cache is an explicit contract, and a line on every search forever is the
/// «cannot stop firing» rule), while an unreadable state must not be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AskDecision {
    /// A usable projection: send the position and render whatever verdict comes back.
    Ask(MirrorPosition),
    /// No state file at all. Renders NOTHING.
    NeverSynced,
    /// A usable-shaped mirror that is not currently a complete projection; the string is the
    /// reason, already phrased for a human.
    Unusable(String),
    /// The state file exists and would not read. The mirror's freshness is simply unknown.
    StateUnreadable(String),
}

impl AskDecision {
    /// The machine token for `--json` (D6's third case). `Ask` has none — the answer is then the
    /// `delta` field, and a local diagnosis beside a server verdict would be two answers to one
    /// question.
    fn mirror_token(&self) -> Option<&'static str> {
        match self {
            AskDecision::Ask(_) => None,
            AskDecision::NeverSynced => Some("never_synced"),
            AskDecision::Unusable(_) => Some("unusable"),
            AskDecision::StateUnreadable(_) => Some("state_unreadable"),
        }
    }
}

/// The one [`AskDecision::Unusable`] reason `docli sync` does not resolve: the directory stopped
/// being this workspace's mirror, so there is nothing to bring to head until it is re-pointed.
/// Named because the notice must not offer a remedy that cannot work (Codex round 2).
const NOT_THIS_MIRROR: &str = "this directory is not this workspace's mirror";

/// The four values THIS build understands (v0.29.0 D2b). The wire carries a plain string so an
/// unknown value from a newer server survives the parse; the CLI's own contract — the `--json`
/// projection and the printed line — is closed over these four, and anything else is folded into
/// «no answer». Forward tolerance at the wire, a frozen set at the surface.
const KNOWN_DELTA: [&str; 4] = ["none", "pending", "epoch_mismatch", "rebuild_required"];

/// One mount's local half: the ask decision, plus the state that may render local paths.
///
/// Deliberately does NOT carry the mount-identity answer — that is re-measured at render time,
/// after the response, so its window stays as narrow as it was before this slice.
pub struct MountLocal {
    pub decision: AskDecision,
    /// `Some` only for a COMPLETE projection — local paths render off nothing less.
    pub state: Option<WsState>,
}

/// Read one mount's local state and decide whether to ask the server about it.
///
/// Extracted from `run` so it is unit-testable: `Api` is a concrete struct with no trait, and the
/// request is built inline, so there is nothing to assert a serialized request against.
fn read_local(project: &Project, control: &ControlRoot, mount: &Mount, now: i64) -> MountLocal {
    let mount_root = crate::config::mount_abs(&project.root, mount);
    // State is keyed by WORKSPACE, so it says nothing about the directory configured NOW; the
    // marker in the directory is the only thing that binds the two.
    let identity_ok =
        crate::mountfs::verify_mount_identity(&mount_root, &control.dir, mount.workspace);
    let loaded = match control.load_state(mount.workspace) {
        Ok(st) => st,
        Err(e) => {
            return MountLocal {
                decision: AskDecision::StateUnreadable(format!("{e:#}")),
                state: None,
            };
        }
    };
    let Some(st) = loaded else {
        return MountLocal {
            decision: AskDecision::NeverSynced,
            state: None,
        };
    };
    let decision = if !identity_ok {
        AskDecision::Unusable(NOT_THIS_MIRROR.into())
    } else {
        match st.unusable_reason(mount.folder.as_deref(), now) {
            Some(reason) => AskDecision::Unusable(reason.into()),
            None => AskDecision::Ask(MirrorPosition {
                cursor: st.cursor,
                epoch: st.epoch,
                ledger_count: st.ledger.len() as i64,
            }),
        }
    };
    // A corrupt or partial cache is NO cache (Codex round 25): the hits already arrived from the
    // server, and the disposable `.docli` state must not veto them — degrade to «not mirrored».
    // A cache built for a DIFFERENT folder scope maps server paths to stale local spellings
    // (round 26); a MID-REPAIR cache (round 27) or one whose cursor never reached head (round 30)
    // is the same. Local paths render only off a COMPLETE projection.
    let state = (st.scope_key == mount.folder && !st.from_zero && st.at_head).then_some(st);
    MountLocal { decision, state }
}

#[derive(Debug, serde::Serialize)]
pub struct RenderedWorkspace {
    pub mount: String,
    pub workspace: Uuid,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refused: Option<String>,
    pub degraded: bool,
    pub hits: Vec<RenderedHit>,
    pub attachments: Vec<RenderedHit>,
    pub attachments_truncated: bool,
    pub attachments_query_truncated: bool,
    /// The server's mirror-delta answer for the position this run sent (v0.29.0 D6). THREE cases,
    /// and the key is always present so a consumer never has to distinguish «absent» from
    /// «null»:
    ///
    /// * **asked and answered** — `"none" | "pending" | "epoch_mismatch" | "rebuild_required"`;
    /// * **asked, unanswered** — `null` with NO `mirror` field. Covers all four causes: a
    ///   server-side derivation failure, a refusal or INTERNAL outcome, an api older than
    ///   v0.29.0, and an unknown value from a newer one. `refused` is what distinguishes a
    ///   refusal from the rest;
    /// * **not asked** — `null` PLUS `mirror`.
    pub delta: Option<String>,
    /// The LOCAL diagnosis, present only in the not-asked case:
    /// `"never_synced" | "unusable" | "state_unreadable"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mirror: Option<&'static str>,
    /// The human-readable half of `mirror`. Not serialized — `--json` freezes the TOKEN, and a
    /// prose field would become a second contract nobody meant to make.
    #[serde(skip)]
    pub mirror_reason: Option<String>,
}

#[derive(Debug, serde::Serialize)]
pub struct RenderedHit {
    pub server_path: String,
    /// The LOCAL address — present only when the file is genuinely on disk (state AND a stat);
    /// `None` = «not mirrored» (handing an agent a nonexistent path is exactly the split-brain
    /// the rule exists to prevent).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snippet: Option<String>,
    /// True for the attachment arm: the local path is a MARKER sidecar, not the file's bytes.
    pub marker: bool,
}

/// KIND-AWARE, disk-checked projection: a NOTE hit renders its local `.md` path only when
/// present in state AND a cheap stat confirms it on disk (state can silently diverge from disk
/// — D3); an ATTACHMENT hit renders its MARKER path (resolved through state — relocation) tagged
/// as a marker. Anything else — unsynced mount, out-of-scope, guard-parked, stat-miss — renders
/// «not mirrored» INSTEAD of a path.
/// Render a filesystem path for the terminal, ANCHORED TO THE INVOCATION CWD (Codex round 1):
/// `docli search` promises "local paths you can open directly", and a project-root-relative
/// spelling printed from a subdirectory resolves to the wrong file. Relative-to-cwd when the
/// path is under it; absolute otherwise.
fn display_path(cwd: &Path, abs: &Path) -> String {
    match abs.strip_prefix(cwd) {
        Ok(rel) if !rel.as_os_str().is_empty() => rel.to_string_lossy().replace('\\', "/"),
        _ => abs.to_string_lossy().replace('\\', "/"),
    }
}

fn render_workspace(
    project: &Project,
    cwd: &Path,
    mount: &Mount,
    local: &MountLocal,
    o: &SearchWorkspaceOutcome,
    any_hit: &mut bool,
) -> RenderedWorkspace {
    let state = local.state.as_ref();
    let mount_root = crate::config::mount_abs(&project.root, mount);
    // Canonical containment (Codex round 16): `contained_join` is lexical, and search holds no
    // mount claim, so a post-sync symlink swap could point a rendered address outside the
    // mirror. A hit renders as mirrored only when its CANONICAL path stays under the canonical
    // root (canonicalize also refuses a missing file — the «not mirrored» answer).
    let canonically_under = |root: &Path, abs: &Path| -> bool {
        match (root.canonicalize(), abs.canonicalize()) {
            (Ok(r), Ok(a)) => a.starts_with(r),
            _ => false,
        }
    };
    // …and the ROOTS themselves must be anchored (Codex round 17): a swapped/symlinked mount
    // root would canonicalize consistently against itself. The mount anchors to its
    // `MOUNT.docli` identity (lock-free); the control plane anchors to the canonical project
    // root (`.docli` lives inside the project by construction). Measured HERE, after the
    // response — `read_local` asks the same question before the request for its own decision,
    // and reusing that answer would widen this window across the whole round trip.
    let control_dir = ControlRoot::new(&project.root).dir;
    let mount_identity_ok =
        crate::mountfs::verify_mount_identity(&mount_root, &control_dir, mount.workspace);
    let local_of = |id: Uuid, want_marker: bool| -> Option<String> {
        let st = state?;
        if !mount_identity_ok {
            return None;
        }
        let n = st.nodes.get(&id)?;
        if want_marker {
            if n.kind != TrackedKind::Attachment {
                return None;
            }
            let mp = n.marker_path.clone()?;
            // A RELOCATED marker lives under the project's control root, NOT the mount — the
            // rendered address must say so (a `{mount}/.docli/…` spelling names a path the
            // geometry rules guarantee cannot exist: the exact split-brain D5 forbids).
            if mp.starts_with(".docli/") {
                // Only this workspace's own namespace resolves (Codex round 13).
                let leaf = crate::apply::relocated_leaf(&mp, mount.workspace)?;
                let markers = ControlRoot::new(&project.root)
                    .markers_dir()
                    .join(mount.workspace.to_string());
                let abs = markers.join(leaf);
                (abs.is_file() && canonically_under(&project.root, &abs))
                    .then(|| display_path(cwd, &abs))
            } else {
                // State-derived: containment or «not mirrored» (Codex round 15).
                let abs = crate::mountfs::contained_join(&mount_root, &mp).ok()?;
                (abs.is_file() && canonically_under(&mount_root, &abs))
                    .then(|| display_path(cwd, &abs))
            }
        } else {
            let abs = crate::mountfs::contained_join(&mount_root, &n.local_path).ok()?;
            (abs.is_file() && canonically_under(&mount_root, &abs)).then(|| display_path(cwd, &abs))
        }
    };
    let hits: Vec<RenderedHit> = o
        .hits
        .iter()
        .map(|h| {
            *any_hit = true;
            RenderedHit {
                server_path: h.path.clone(),
                local_path: local_of(h.id, false),
                snippet: Some(h.snippet.clone()),
                marker: false,
            }
        })
        .collect();
    let attachments: Vec<RenderedHit> = o
        .attachments
        .iter()
        .map(|a| {
            *any_hit = true;
            RenderedHit {
                server_path: a.path.clone(),
                local_path: local_of(a.id, true),
                snippet: None,
                marker: true,
            }
        })
        .collect();
    RenderedWorkspace {
        mount: mount.display_name().to_string(),
        workspace: o.workspace_id,
        refused: o.refused.clone(),
        degraded: o.degraded,
        hits,
        attachments,
        attachments_truncated: o.attachments_truncated,
        attachments_query_truncated: o.attachments_query_truncated,
        // Asked ⇒ whatever the server said, IF this build understands it (an older api, a
        // derivation failure, a refusal and an unknown value all arrive here as «no answer»).
        // Not asked ⇒ null plus the local token. Never both.
        delta: match &local.decision {
            AskDecision::Ask(_) => o
                .delta
                .as_deref()
                .filter(|v| KNOWN_DELTA.contains(v))
                .map(str::to_string),
            _ => None,
        },
        mirror: local.decision.mirror_token(),
        mirror_reason: match &local.decision {
            AskDecision::Unusable(r) | AskDecision::StateUnreadable(r) => Some(r.clone()),
            _ => None,
        },
    }
}

/// The mirror line's TEXT, or `None` when there is nothing honest to say (v0.29.0 D6). Pure, so
/// the whole routing table is unit-testable without capturing a stream.
///
/// **Descriptive, never imperative**: a concurrent `docli sync` persists `at_head = false`
/// before its first filesystem mutation, so «run `docli sync`» can tell the reader to run the
/// sync that is already running.
fn mirror_sentence(r: &RenderedWorkspace) -> Option<String> {
    // A refusal is the dominant fact — the workspace was not searched at all — and the notice's
    // own guardrail sentence (see `GUARDRAIL`) would be false about results that do not exist. The rule lives HERE, not at the call sites, so the terminal and
    // `--json`'s stderr line cannot diverge. `--json` still carries both fields independently.
    if r.refused.is_some() {
        return None;
    }
    let reason = || r.mirror_reason.as_deref().unwrap_or("reason unknown");
    Some(match (r.mirror, r.delta.as_deref()) {
        // A never-synced mount says nothing, forever. Searching without a cache is an explicit
        // contract, not a defect to report on every call.
        (Some("never_synced"), _) => return None,
        (Some("state_unreadable"), _) => format!(
            "the local mirror state could not be read ({}), so no local paths are shown",
            reason()
        ),
        // The directory is not this mirror: `docli sync` cannot bring it to head, so the generic
        // remedy below would be false. Its own sentence, with the remedy that does apply.
        (Some("unusable"), _) if r.mirror_reason.as_deref() == Some(NOT_THIS_MIRROR) => {
            format!("{NOT_THIS_MIRROR} - `docli init` re-points it; no local paths are shown")
        }
        // Two deliberate word choices here, each a defect an earlier draft actually shipped:
        //
        // «not a usable projection» rather than «incomplete» — `incomplete` is a DEFINED term in
        // this codebase (`WsState::incomplete`, `CACHE_INCOMPLETE.docli`) and an over-age head is
        // not one of its terms, so a mirror can be complete and still too old to trust.
        //
        // …and the remedy REDIRECTS instead of naming a command, because the seven reasons do not
        // share one (Codex round 3): a transient park needs the occupant removed and then
        // `docli sync --full` — plain `sync` never replays a parked delivery — and a blocked
        // removal needs the occupant gone first. `sync --check` already renders the exact fix for
        // every one of them, so pointing at it is both true for all seven and one authority for
        // the remedy rather than a second copy of it here.
        //
        // «CLEARS IT or names the fix», not just the latter (Codex round 4): one reason —
        // `at_head = false` over a mirror that is actually caught up, the crash window between a
        // page commit and the head commit — is HEALED by the probe, which then exits 0 and prints
        // «fresh» (`sync_cmd.rs` check_mount's heal branch). Promising a fix there would send the
        // reader to a command that names none.
        (Some(_), _) => format!(
            "the local mirror is not a usable projection - {}; `docli sync --check` clears it or \
             names the fix",
            reason()
        ),
        // An UNKNOWN value from a newer server, an api older than v0.29.0, a refusal, or a
        // failed derivation: silence. A wrong sentence is worse than none, which is the whole
        // reason the wire carries a plain string. `none` is silent for the same reason —
        // silence is the default and this field is not a verdict to announce.
        (None, None | Some("none")) => return None,
        (None, Some("pending")) => "the server has changes this mirror has not applied yet - \
             `docli sync` brings it to head"
            .to_string(),
        (None, Some("epoch_mismatch")) => "the workspace was resynced after this mirror was \
             built - `docli sync` rebuilds the mirror"
            .to_string(),
        (None, Some("rebuild_required")) => "the server and this mirror disagree on the live \
             item count - a hard delete was missed; `docli sync` rebuilds it"
            .to_string(),
        (None, Some(_)) => return None,
    })
}

/// Print the mirror line, in both output modes.
///
/// **It carries its own limits.** `SKILL.md` is the channel v0.28.6 D1 ranks weakest and its
/// `paths:` activation does not even fire on a search, so expected delivery of the guardrail
/// through that door is ~0 — the sentence that keeps a reader from downgrading the results has
/// to travel with the notice itself.
fn mirror_notice(r: &RenderedWorkspace) {
    let Some(sentence) = mirror_sentence(r) else {
        return;
    };
    // ONE `warn`, carrying both halves. The guardrail rode a `detail` in the first draft, which
    // `--quiet` DROPS outside report mode — so `docli search --json -q` would have delivered the
    // alarm without the sentence that stops a reader downgrading the results, which is the ~0
    // delivery problem D6 exists to fix, reproduced in miniature. `warn` survives `--quiet`;
    // making the pairing one string makes it unseparable.
    crate::ui::warn(&format!("[{}] {sentence} ({GUARDRAIL})", r.mount));
}

/// The half of the notice that keeps a stale MIRROR from being read as a weaker ANSWER.
const GUARDRAIL: &str = "server results are unaffected; this concerns the local mirror only";

/// One terminal line, ellipsised. A snippet that wraps turns a list of hits into a paragraph
/// and costs the reader the scannable left edge.
fn clip(s: &str) -> String {
    let width = console::Term::stdout().size().1.clamp(40, 200) as usize;
    let max = width.saturating_sub(8);
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push_str("...");
    out
}

/// `show_mount` gates the `[mount]` prefix: with a single mount, the local path already
/// starts with the mount directory, so the tag is pure repetition on every line.
fn print_workspace(r: &RenderedWorkspace, show_mount: bool) {
    let tag = if show_mount {
        format!("{} ", crate::ui::dim(&format!("[{}]", r.mount)))
    } else {
        String::new()
    };
    if let Some(code) = &r.refused {
        // Per-code guidance, same split as sync's arms: «попросите доступ» is ONLY for the
        // no-access class — telling a user to ask a colleague about their own entitlement
        // (402) or about a server fault (INTERNAL) is wrong guidance. The no-access copy
        // itself comes from sync_cmd's single builder (one reader of one message).
        let line = match code.as_str() {
            "UPGRADE_REQUIRED" => {
                "sync is not enabled for your account - workspace skipped".to_string()
            }
            "INTERNAL" => "a temporary server error - run the search again".to_string(),
            _ => crate::sync_cmd::no_access_message(&r.mount),
        };
        crate::ui::refuse(&format!("[{}] {code}: {line}", r.mount));
        return;
    }
    if r.degraded {
        crate::ui::warn(&format!(
            "[{}] the note index was incomplete for this query - a missing result here proves \
             nothing",
            r.mount
        ));
    }
    for h in &r.hits {
        // The PATH is the answer — bold and on its own line; the snippet is context, dimmed
        // and clipped to one terminal line so a screenful of hits stays a list rather than a
        // wall of prose.
        match &h.local_path {
            Some(l) => crate::ui::line(&format!("{tag}{}", console::style(l).bold())),
            None => crate::ui::line(&format!(
                "{tag}{} {}",
                console::style(&h.server_path).bold(),
                crate::ui::dim("- not mirrored (run docli sync, then docli doctor if it persists)")
            )),
        }
        if let Some(s) = &h.snippet {
            crate::ui::line(&format!(
                "    {}",
                crate::ui::dim(&clip(&s.replace('\n', " ")))
            ));
        }
    }
    for a in &r.attachments {
        match &a.local_path {
            Some(l) => crate::ui::line(&format!(
                "{tag}{} {}",
                console::style(l).bold(),
                crate::ui::dim("(marker - the bytes live on the server)")
            )),
            None => crate::ui::line(&format!(
                "{tag}{} {}",
                console::style(&a.server_path).bold(),
                crate::ui::dim("- a file, not mirrored")
            )),
        }
    }
    if r.attachments_truncated {
        crate::ui::detail(&format!(
            "[{}] file matches truncated - more files match than are shown",
            r.mount
        ));
    }
    if r.attachments_query_truncated {
        crate::ui::detail(&format!(
            "[{}] file matches may be a SUPERSET of the query (it had more terms than the \
             file arm applies)",
            r.mount
        ));
    }
    mirror_notice(r);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DocliToml;
    use crate::state::{NodeState, WsState};
    use docli_sync_wire::{SearchAttachmentWire, SearchHitWire};

    struct Fx {
        _tmp: tempfile::TempDir,
        project: Project,
        mount: Mount,
        state: WsState,
    }

    fn fx() -> Fx {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        std::fs::create_dir_all(root.join("mirror/docs")).unwrap();
        let project = Project {
            root: root.clone(),
            config: DocliToml {
                server: "https://docli.ru".into(),
                mounts: vec![],
                mcp_label: None,
            },
        };
        let mount = Mount {
            workspace: Uuid::from_u128(1),
            dir: "mirror".into(),
            folder: None,
            name: Some("заметки".into()),
        };
        // A synced mount carries our MOUNT.docli — the round-17 identity anchor search
        // consults before rendering any local address.
        std::fs::create_dir_all(root.join(".docli")).unwrap();
        let owner = std::fs::canonicalize(root.join(".docli"))
            .unwrap()
            .display()
            .to_string();
        std::fs::write(
            root.join("mirror/MOUNT.docli"),
            serde_json::json!({"owner": owner, "workspace": mount.workspace}).to_string(),
        )
        .unwrap();
        Fx {
            _tmp: tmp,
            project,
            mount,
            state: WsState::fresh(None),
        }
    }

    fn track(state: &mut WsState, id: u128, kind: TrackedKind, local: &str, marker: Option<&str>) {
        state.nodes.insert(
            Uuid::from_u128(id),
            NodeState {
                server_path: local.to_string(),
                local_path: local.to_string(),
                kind,
                rev: 1,
                content_sha256: String::new(),
                marker_path: marker.map(|m| m.to_string()),
            },
        );
    }

    fn outcome(
        hits: Vec<SearchHitWire>,
        attachments: Vec<SearchAttachmentWire>,
    ) -> SearchWorkspaceOutcome {
        SearchWorkspaceOutcome {
            workspace_id: Uuid::from_u128(1),
            refused: None,
            hits,
            attachments,
            degraded: false,
            attachments_truncated: false,
            attachments_query_truncated: false,
            delta: None,
        }
    }

    /// The render input, as a `MountLocal`: a state complete enough to render local paths
    /// (`None` = an unsynced mount). Mount identity is NOT part of it — `render_workspace`
    /// measures that itself, which is what keeps the symlink-swap tests below exercising the
    /// real code path rather than a helper's copy of it.
    fn as_local(state: Option<&WsState>) -> MountLocal {
        MountLocal {
            decision: match state {
                Some(_) => AskDecision::Ask(MirrorPosition {
                    cursor: docli_sync_wire::WireCursor {
                        rev: 0,
                        id: Uuid::nil(),
                    },
                    epoch: 0,
                    ledger_count: 0,
                }),
                None => AskDecision::NeverSynced,
            },
            state: state.cloned(),
        }
    }

    fn hit(id: u128, path: &str) -> SearchHitWire {
        SearchHitWire {
            id: Uuid::from_u128(id),
            name: path.rsplit('/').next().unwrap().into(),
            path: path.into(),
            snippet: "...".into(),
            rank: 1.0,
        }
    }

    fn att(id: u128, path: &str) -> SearchAttachmentWire {
        SearchAttachmentWire {
            id: Uuid::from_u128(id),
            name: path.rsplit('/').next().unwrap().into(),
            path: path.into(),
            mime: Some("image/png".into()),
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_swapped_mount_root_renders_nothing_local() {
        // Codex round 17: replacing the WHOLE mount dir with a symlink canonicalizes
        // consistently against itself — the identity anchor (no-follow stat + MOUNT.docli)
        // must refuse before any address renders.
        let mut f = fx();
        track(&mut f.state, 2, TrackedKind::Note, "docs/a.md", None);
        let outside = f.project.root.join("outside");
        std::fs::create_dir_all(outside.join("docs")).unwrap();
        std::fs::write(outside.join("docs/a.md"), "x").unwrap();
        std::fs::remove_dir_all(f.project.root.join("mirror")).unwrap();
        std::os::unix::fs::symlink(&outside, f.project.root.join("mirror")).unwrap();
        let o = outcome(vec![hit(2, "docs/a.md")], vec![]);
        let mut any = false;
        let r = render_workspace(
            &f.project,
            &f.project.root,
            &f.mount,
            &as_local(Some(&f.state)),
            &o,
            &mut any,
        );
        assert_eq!(r.hits[0].local_path, None, "{:?}", r.hits[0]);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_materialization_renders_not_mirrored() {
        // Codex round 16: search holds no mount claim, so a post-sync symlink swap must not
        // hand the agent an address resolving outside the mirror.
        let mut f = fx();
        track(&mut f.state, 2, TrackedKind::Note, "docs/a.md", None);
        std::fs::write(f.project.root.join("outside.md"), "outside").unwrap();
        std::os::unix::fs::symlink(
            f.project.root.join("outside.md"),
            f.project.root.join("mirror/docs/a.md"),
        )
        .unwrap();
        let o = outcome(vec![hit(2, "docs/a.md")], vec![]);
        let mut any = false;
        let r = render_workspace(
            &f.project,
            &f.project.root,
            &f.mount,
            &as_local(Some(&f.state)),
            &o,
            &mut any,
        );
        assert_eq!(r.hits[0].local_path, None, "{:?}", r.hits[0]);
    }

    #[test]
    fn a_note_hit_renders_a_local_path_only_when_state_and_disk_agree() {
        let mut f = fx();
        track(&mut f.state, 2, TrackedKind::Note, "docs/a.md", None);
        track(&mut f.state, 3, TrackedKind::Note, "docs/gone.md", None);
        std::fs::write(f.project.root.join("mirror/docs/a.md"), "x").unwrap();
        // `gone.md` is tracked but NOT on disk (state can silently diverge — D3).
        let o = outcome(
            vec![
                hit(2, "docs/a.md"),
                hit(3, "docs/gone.md"),
                hit(4, "unsynced.md"),
            ],
            vec![],
        );
        let mut any = false;
        let r = render_workspace(
            &f.project,
            &f.project.root,
            &f.mount,
            &as_local(Some(&f.state)),
            &o,
            &mut any,
        );
        assert_eq!(r.hits[0].local_path.as_deref(), Some("mirror/docs/a.md"));
        assert_eq!(
            r.hits[1].local_path, None,
            "stat-miss renders `not mirrored`, never a path"
        );
        assert_eq!(
            r.hits[2].local_path, None,
            "an untracked hit renders `not mirrored`"
        );
    }

    #[test]
    fn an_attachment_hit_renders_its_marker_and_a_relocated_marker_renders_the_control_path() {
        let mut f = fx();
        track(
            &mut f.state,
            5,
            TrackedKind::Attachment,
            "docs/pic.png",
            Some("docs/pic.png.docli"),
        );
        std::fs::write(f.project.root.join("mirror/docs/pic.png.docli"), "m").unwrap();
        // A RELOCATED marker (control-file collision): lives under .docli/markers/, and the
        // rendered address must be that path — a `{mount}/.docli/…` spelling names a file the
        // geometry rules guarantee cannot exist (round-1 §4.2).
        let reloc = format!(
            ".docli/markers/{}/{}.docli",
            f.mount.workspace,
            Uuid::from_u128(6)
        );
        track(
            &mut f.state,
            6,
            TrackedKind::Attachment,
            "MOUNT",
            Some(&reloc),
        );
        let markers = ControlRoot::new(&f.project.root)
            .markers_dir()
            .join(f.mount.workspace.to_string());
        std::fs::create_dir_all(&markers).unwrap();
        std::fs::write(markers.join(format!("{}.docli", Uuid::from_u128(6))), "m").unwrap();

        let o = outcome(vec![], vec![att(5, "docs/pic.png"), att(6, "MOUNT")]);
        let mut any = false;
        let r = render_workspace(
            &f.project,
            &f.project.root,
            &f.mount,
            &as_local(Some(&f.state)),
            &o,
            &mut any,
        );
        assert_eq!(
            r.attachments[0].local_path.as_deref(),
            Some("mirror/docs/pic.png.docli")
        );
        assert!(r.attachments[0].marker);
        assert_eq!(r.attachments[1].local_path.as_deref(), Some(reloc.as_str()));
    }

    #[test]
    fn an_unsynced_mount_renders_not_mirrored_for_everything() {
        let f = fx();
        let o = outcome(vec![hit(2, "a.md")], vec![att(3, "b.png")]);
        let mut any = false;
        let r = render_workspace(
            &f.project,
            &f.project.root,
            &f.mount,
            &as_local(None),
            &o,
            &mut any,
        );
        assert!(r.hits[0].local_path.is_none());
        assert!(r.attachments[0].local_path.is_none());
        assert!(
            any,
            "hits still count as hits - absence of a PATH is not absence of the note"
        );
    }

    /// The «токен» ban covers THIS refusal surface too (round-1 M4): the search branch reuses
    /// sync_cmd's single copy builder, and this pins the reuse.
    #[test]
    fn the_search_refusal_copy_never_says_token() {
        let msg = crate::sync_cmd::no_access_message("книга продаж");
        assert!(!msg.to_lowercase().contains("token"), "{msg}");
        assert!(msg.contains("ask the workspace owner"), "{msg}");
    }

    // ---- v0.29.0: the ask decision and the frozen `--json` projection ------------------------

    use crate::state::{Park, ParkClass};

    /// A state that is a COMPLETE, current projection — the only shape that asks.
    fn ready_state(scope: Option<&str>, now: i64) -> WsState {
        let mut st = WsState::fresh(scope.map(str::to_string));
        st.from_zero = false;
        st.at_head = true;
        st.head_reached_at = Some(now);
        st.cursor = docli_sync_wire::WireCursor {
            rev: 7,
            id: Uuid::from_u128(9),
        };
        st.epoch = 3;
        st.ledger.insert(Uuid::from_u128(2));
        st.ledger.insert(Uuid::from_u128(3));
        st
    }

    fn save(f: &Fx, st: &WsState) {
        ControlRoot::new(&f.project.root)
            .save_state(f.mount.workspace, st)
            .unwrap();
    }

    const NOW: i64 = 1_800_000_000;

    #[test]
    fn a_complete_projection_asks_and_carries_its_ledger_count() {
        let f = fx();
        save(&f, &ready_state(None, NOW));
        let l = read_local(
            &f.project,
            &ControlRoot::new(&f.project.root),
            &f.mount,
            NOW,
        );
        assert_eq!(
            l.decision,
            AskDecision::Ask(MirrorPosition {
                cursor: docli_sync_wire::WireCursor {
                    rev: 7,
                    id: Uuid::from_u128(9)
                },
                epoch: 3,
                // The LEDGER, not `nodes`: out-of-scope, parked and unknown-kind ids count too,
                // which is what makes the server-side comparison well-defined.
                ledger_count: 2,
            })
        );
        assert!(l.state.is_some(), "and it renders local paths");
    }

    /// A mount that has NEVER synced is SILENT — searching without a cache is an explicit
    /// contract, and a line on every search forever is the «cannot stop firing» rule.
    #[test]
    fn a_never_synced_mount_asks_nothing_and_says_nothing() {
        let f = fx();
        let l = read_local(
            &f.project,
            &ControlRoot::new(&f.project.root),
            &f.mount,
            NOW,
        );
        assert_eq!(l.decision, AskDecision::NeverSynced);
        assert!(l.state.is_none());
        let r = rendered_with(&f, &l, None);
        assert_eq!(r.mirror, Some("never_synced"));
        assert_eq!(r.delta, None);
    }

    /// A state file that will not PARSE is not «never synced» — it must not be silent.
    #[test]
    fn an_unreadable_state_is_reported_not_silenced() {
        let f = fx();
        let control = ControlRoot::new(&f.project.root);
        let p = control.state_path(f.mount.workspace);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, "{ not json").unwrap();
        let l = read_local(&f.project, &control, &f.mount, NOW);
        assert!(matches!(l.decision, AskDecision::StateUnreadable(_)));
        assert!(l.state.is_none());
        assert_eq!(rendered_with(&f, &l, None).mirror, Some("state_unreadable"));
    }

    /// Every D4 readiness term routes the SAME way: no position, and a local line. The broader
    /// set is the point — the first draft used only three terms, so a parked or
    /// pending-removal mount would have asked, received `none`, and printed nothing over a
    /// mirror the `CACHE_INCOMPLETE.docli` contract already calls incomplete.
    #[test]
    fn every_readiness_term_withholds_the_position() {
        type Mutate = Box<dyn Fn(&mut WsState)>;
        let cases: Vec<(&str, Mutate)> = vec![
            (
                "from_zero",
                Box::new(|st: &mut WsState| st.from_zero = true),
            ),
            (
                "scope",
                Box::new(|st: &mut WsState| st.scope_key = Some("other".into())),
            ),
            ("at_head", Box::new(|st: &mut WsState| st.at_head = false)),
            (
                "transient park",
                Box::new(|st: &mut WsState| {
                    st.parks.insert(
                        Uuid::from_u128(4),
                        Park {
                            class: ParkClass::Transient,
                            reason: "occupied".into(),
                            server_path: "a.md".into(),
                        },
                    );
                }),
            ),
            (
                "pending removal",
                Box::new(|st: &mut WsState| {
                    st.pending_removals.insert("docs".into());
                }),
            ),
            (
                "over-age head",
                Box::new(|st: &mut WsState| {
                    st.head_reached_at = Some(NOW - crate::state::MAX_HEAD_AGE_SECS - 1);
                }),
            ),
            (
                "never reached head",
                Box::new(|st: &mut WsState| st.head_reached_at = None),
            ),
        ];
        for (name, mutate) in cases {
            let f = fx();
            let mut st = ready_state(None, NOW);
            mutate(&mut st);
            save(&f, &st);
            let l = read_local(
                &f.project,
                &ControlRoot::new(&f.project.root),
                &f.mount,
                NOW,
            );
            assert!(
                matches!(l.decision, AskDecision::Unusable(_)),
                "{name} must withhold the position, got {:?}",
                l.decision
            );
            assert_eq!(
                rendered_with(&f, &l, None).mirror,
                Some("unusable"),
                "{name}"
            );
        }
        // …and a STRUCTURAL park is deliberately NOT a term: it is durable by nature, so a
        // mount holding one would fire the line forever over a single unmaterializable path.
        let f = fx();
        let mut st = ready_state(None, NOW);
        st.parks.insert(
            Uuid::from_u128(5),
            Park {
                class: ParkClass::Structural,
                reason: "docli-namespace".into(),
                server_path: "x.docli".into(),
            },
        );
        save(&f, &st);
        let l = read_local(
            &f.project,
            &ControlRoot::new(&f.project.root),
            &f.mount,
            NOW,
        );
        assert!(matches!(l.decision, AskDecision::Ask(_)));
    }

    /// A directory that no longer carries THIS workspace's `MOUNT.docli` is not this
    /// workspace's mirror, whatever the state file says.
    #[test]
    fn a_failed_identity_check_withholds_the_position() {
        let f = fx();
        save(&f, &ready_state(None, NOW));
        std::fs::remove_file(f.project.root.join("mirror/MOUNT.docli")).unwrap();
        let l = read_local(
            &f.project,
            &ControlRoot::new(&f.project.root),
            &f.mount,
            NOW,
        );
        assert!(matches!(l.decision, AskDecision::Unusable(_)));
    }

    /// D6's three cases, frozen. The middle one — asked, no answer — is the one an earlier
    /// draft omitted, and it covers FOUR causes at once: a derivation failure, a refusal or
    /// INTERNAL outcome, an api older than v0.29.0, and an unknown value from a newer one.
    #[test]
    fn the_json_projection_freezes_three_cases() {
        let f = fx();
        save(&f, &ready_state(None, NOW));
        let control = ControlRoot::new(&f.project.root);
        let asked = read_local(&f.project, &control, &f.mount, NOW);

        // (1) asked and answered.
        let r = rendered_with(&f, &asked, Some("pending"));
        let v: serde_json::Value = serde_json::to_value(&r).unwrap();
        assert_eq!(v["delta"], "pending");
        assert!(
            v.get("mirror").is_none(),
            "no local diagnosis beside an answer"
        );

        // (2) asked, unanswered — a derivation failure / older api / unknown value all arrive
        //     as the same absent field, and each renders `null` with NO `mirror`.
        for unanswered in [None, Some("reindex_required")] {
            let mut o = outcome(vec![], vec![]);
            o.delta = unanswered.map(str::to_string);
            let mut any = false;
            let r = render_workspace(&f.project, &f.project.root, &f.mount, &asked, &o, &mut any);
            let v: serde_json::Value = serde_json::to_value(&r).unwrap();
            // The `--json` field is CLOSED over the four known values: an unknown one from a
            // newer server folds to null here, even though the wire preserved it verbatim.
            assert_eq!(v["delta"], serde_json::Value::Null, "{unanswered:?}");
            assert!(v.get("mirror").is_none(), "{unanswered:?}");
        }
        // …and a refusal, which is what `refused` distinguishes from the other three causes.
        let mut o = outcome(vec![], vec![]);
        o.refused = Some("FORBIDDEN".into());
        let mut any = false;
        let r = render_workspace(&f.project, &f.project.root, &f.mount, &asked, &o, &mut any);
        let v: serde_json::Value = serde_json::to_value(&r).unwrap();
        assert_eq!(v["delta"], serde_json::Value::Null);
        assert_eq!(v["refused"], "FORBIDDEN");
        assert!(v.get("mirror").is_none());

        // (3) not asked.
        let f2 = fx();
        let never = read_local(
            &f2.project,
            &ControlRoot::new(&f2.project.root),
            &f2.mount,
            NOW,
        );
        let v: serde_json::Value = serde_json::to_value(rendered_with(&f2, &never, None)).unwrap();
        assert_eq!(v["delta"], serde_json::Value::Null);
        assert_eq!(v["mirror"], "never_synced");
    }

    /// A server answer this build does not know is treated as NO answer — the whole reason the
    /// wire carries a plain string rather than an enum. `none` is equally silent: silence is
    /// the default, and the field is not a freshness verdict to be announced.
    #[test]
    fn an_unknown_value_and_none_both_render_no_sentence() {
        let f = fx();
        save(&f, &ready_state(None, NOW));
        let asked = read_local(
            &f.project,
            &ControlRoot::new(&f.project.root),
            &f.mount,
            NOW,
        );
        for value in [Some("none"), Some("reindex_required"), None] {
            let r = rendered_with(&f, &asked, value);
            assert!(
                mirror_sentence(&r).is_none(),
                "{value:?} must print nothing"
            );
        }
        assert!(mirror_sentence(&rendered_with(&f, &asked, Some("pending"))).is_some());
    }

    /// Codex round 2: `Unusable` is one JSON token over several causes, and the notice must not
    /// offer one remedy for all of them. A directory that is no longer this workspace's mirror
    /// cannot be brought to head by `docli sync`, and an over-age head is not `incomplete` in
    /// this codebase's own sense of the word — an editorial pass had made the line claim both.
    #[test]
    fn the_unusable_line_never_offers_a_remedy_that_cannot_work() {
        let f = fx();
        save(&f, &ready_state(None, NOW));
        std::fs::remove_file(f.project.root.join("mirror/MOUNT.docli")).unwrap();
        let l = read_local(
            &f.project,
            &ControlRoot::new(&f.project.root),
            &f.mount,
            NOW,
        );
        let line = mirror_sentence(&rendered_with(&f, &l, None)).expect("a line");
        assert!(line.contains(NOT_THIS_MIRROR), "{line}");
        assert!(
            !line.contains("brings it to head"),
            "`docli sync` cannot bring a directory that is not the mirror to head: {line}"
        );

        // …while a sync-fixable cause still names `docli sync`, and never calls an over-age
        // mirror «incomplete» — a term `WsState::incomplete` and the marker already own.
        let f = fx();
        let mut st = ready_state(None, NOW);
        st.head_reached_at = Some(NOW - crate::state::MAX_HEAD_AGE_SECS - 1);
        assert!(
            !st.incomplete(),
            "an over-age head is NOT the incomplete predicate"
        );
        save(&f, &st);
        let l = read_local(
            &f.project,
            &ControlRoot::new(&f.project.root),
            &f.mount,
            NOW,
        );
        let line = mirror_sentence(&rendered_with(&f, &l, None)).expect("a line");
        assert!(!line.contains("incomplete"), "{line}");

        // …and a mount blocked by a transient park must NOT be told plain `docli sync` fixes it:
        // that never replays a parked delivery (the occupant must go, then `sync --full`). The
        // line redirects to `sync --check`, which renders the exact remedy for every cause.
        let f = fx();
        let mut st = ready_state(None, NOW);
        st.parks.insert(
            Uuid::from_u128(7),
            Park {
                class: ParkClass::Transient,
                reason: "occupied".into(),
                server_path: "a.md".into(),
            },
        );
        save(&f, &st);
        let l = read_local(
            &f.project,
            &ControlRoot::new(&f.project.root),
            &f.mount,
            NOW,
        );
        let line = mirror_sentence(&rendered_with(&f, &l, None)).expect("a line");
        assert!(line.contains("`docli sync --check`"), "{line}");
        assert!(
            !line.contains("`docli sync` brings"),
            "plain `docli sync` does not replay a parked delivery: {line}"
        );
    }

    fn rendered_with(f: &Fx, l: &MountLocal, delta: Option<&str>) -> RenderedWorkspace {
        let mut o = outcome(vec![], vec![]);
        o.delta = delta.map(str::to_string);
        let mut any = false;
        render_workspace(&f.project, &f.project.root, &f.mount, l, &o, &mut any)
    }
}
