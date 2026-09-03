// SPDX-FileCopyrightText: 2026 OOO Agitek
// SPDX-License-Identifier: MIT

//! `docli search` (v0.28.0 D5, client half) — server BM25 across all mounts by default.
//!
//! Results carry the SERVER path and the node id, and **no local mirror address** (v0.29.1 D1).
//! Until this slice they carried a per-note local path, which is what made the CLI the finder and
//! something else the reader — the split grep, false absence and every hide-the-mirror question
//! descended from. `docli read` is the other half of closing it: search finds, read opens, and
//! both addresses are the one the server speaks.
//!
//! The split-brain rule, degraded-aware: only a NON-degraded server search may conclude a note
//! does not exist; a degraded answer is INCONCLUSIVE about absence, and the CLI says so rather
//! than printing a bare empty result.

use anyhow::Result;
use docli_sync_wire::{
    MirrorPosition, SearchRequest, SearchWorkspaceOutcome, SEARCH_WORKSPACE_CAP,
};
use uuid::Uuid;

use crate::config::{validate_config, Mount, Project};
use crate::http::Api;
use crate::state::ControlRoot;

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
    let control = project.control_root();
    let mounts: Vec<&Mount> = project.config.mounts.iter().collect();

    // The local read happens BEFORE the request, because the position rides the request
    // (v0.29.0 D2d) and the answer must describe the position actually sent.
    //
    // Since v0.29.1 D1 that is ALL it is for. It used to double as the id → local-path map the
    // hits were rendered off, which made the freshness verdict and the addresses beneath it two
    // snapshots of one instant — and gave the whole window (a concurrent rename, or the narrow
    // swap case) something to be wrong about. With no address to render, the read decides one
    // thing: whether this mount's position is worth sending.
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
        rendered.push(render_workspace(mount, l, o, &mut any_hit));
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
        // The `[mount]` tag is what `docli read --mount` accepts, so it earns its line whenever
        // there is a choice to make; with one mount there is none.
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
                    "no hits - but the note index was incomplete for at least one workspace, \
                     so this is INCONCLUSIVE about absence; retry shortly",
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

/// One mount's local half. Since v0.29.1 D1 it is the ask decision and nothing else: the state
/// snapshot it used to carry existed solely to render per-note local addresses, which `search`
/// no longer publishes.
pub struct MountLocal {
    pub decision: AskDecision,
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
            };
        }
    };
    let Some(st) = loaded else {
        return MountLocal {
            decision: AskDecision::NeverSynced,
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
    MountLocal { decision }
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
    /// The address, and the only one: what the server calls this node. `docli read` takes it,
    /// wikilinks resolve to it, and the MCP tools speak it — one address space (v0.29.1 D2).
    pub server_path: String,
    /// The node id — `docli read --id`'s producer. Stable across renames, unlike the path.
    pub id: Uuid,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snippet: Option<String>,
    /// True for the attachment arm: what the mirror holds for this node is a metadata MARKER,
    /// never the file's bytes, so `docli read` prints the marker's fields and `read_attachment`
    /// over MCP fetches the bytes.
    pub marker: bool,
}

fn render_workspace(
    mount: &Mount,
    local: &MountLocal,
    o: &SearchWorkspaceOutcome,
    any_hit: &mut bool,
) -> RenderedWorkspace {
    let hits: Vec<RenderedHit> = o
        .hits
        .iter()
        .map(|h| {
            *any_hit = true;
            RenderedHit {
                server_path: h.path.clone(),
                id: h.id,
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
                id: a.id,
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
            "the local mirror state could not be read ({}), so `docli read` cannot serve from \
             this mount",
            reason()
        ),
        // The directory is not this mirror: `docli sync` cannot bring it to head, so the generic
        // remedy below would be false. Its own sentence, with the remedy that does apply.
        (Some("unusable"), _) if r.mirror_reason.as_deref() == Some(NOT_THIS_MIRROR) => {
            format!("{NOT_THIS_MIRROR} - `docli init` re-points it")
        }
        // Two deliberate word choices here, each a defect an earlier draft actually shipped:
        //
        // NOT «incomplete» — that is a DEFINED term in this codebase (`WsState::incomplete`,
        // `CACHE_INCOMPLETE.docli`) and an over-age head is not one of its terms, so a mirror can
        // be complete and still too old to trust. The first replacement was «not a usable
        // projection», which avoided the collision by reaching for our own internal vocabulary;
        // v0.29.1's editorial pass replaced it with a sentence that says the same thing without
        // asking the reader to know what a projection is. Both constraints still bind.
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
            "the local mirror cannot be vouched for right now - {}; `docli sync --check` either \
             clears the condition or names the fix",
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

/// `show_mount` gates the `[mount]` prefix: it is what `docli read --mount` accepts, so it is
/// worth a line whenever there is a choice to make — and pure repetition when there is one mount.
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
             nothing; retry shortly",
            r.mount
        ));
    }
    for h in &r.hits {
        // The SERVER PATH is the answer — bold and on its own line, because it is what you hand
        // to `docli read`. The id rides beside it, dimmed, so `--id` has a producer in the human
        // output too and not only in `--json`. The snippet is context: dimmed and clipped to one
        // terminal line, so a screenful of hits stays a list rather than a wall of prose.
        crate::ui::line(&format!(
            "{tag}{} {}",
            console::style(&h.server_path).bold(),
            crate::ui::dim(&h.id.to_string())
        ));
        if let Some(s) = &h.snippet {
            crate::ui::line(&format!(
                "    {}",
                crate::ui::dim(&clip(&s.replace('\n', " ")))
            ));
        }
    }
    for a in &r.attachments {
        crate::ui::line(&format!(
            "{tag}{} {} {}",
            console::style(&a.server_path).bold(),
            crate::ui::dim(&a.id.to_string()),
            crate::ui::dim("(a file - docli read prints its marker; the bytes stay on the server)")
        ));
    }
    if r.attachments_truncated {
        crate::ui::detail(&format!(
            "[{}] file matches truncated - more files match than are shown",
            r.mount
        ));
    }
    if r.attachments_query_truncated {
        crate::ui::detail(&format!(
            "[{}] file matches may include extras - file names were matched on only some of \
             the query's terms",
            r.mount
        ));
    }
    mirror_notice(r);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DocliToml;
    use crate::state::WsState;
    use docli_sync_wire::{SearchAttachmentWire, SearchHitWire};

    struct Fx {
        _tmp: tempfile::TempDir,
        project: Project,
        mount: Mount,
    }

    fn fx() -> Fx {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let control_dir = root.join(".docli");
        std::fs::create_dir_all(root.join("mirror/docs")).unwrap();
        let project = Project {
            root: root.clone(),
            config: DocliToml {
                server: "https://docli.ru".into(),
                mounts: vec![],
                mcp_label: None,
            },
            control: control_dir.clone(),
        };
        let mount = Mount {
            workspace: Uuid::from_u128(1),
            dir: "mirror".into(),
            folder: None,
            name: Some("заметки".into()),
            derived_dir: false,
            workspace_label: String::new(),
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
        }
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

    /// The render input, as a `MountLocal`. Since v0.29.1 D1 it is the ask decision alone —
    /// there is no local address to render, so there is no state snapshot to render it off.
    fn as_local(asking: bool) -> MountLocal {
        MountLocal {
            decision: if asking {
                AskDecision::Ask(MirrorPosition {
                    cursor: docli_sync_wire::WireCursor {
                        rev: 0,
                        id: Uuid::nil(),
                    },
                    epoch: 0,
                    ledger_count: 0,
                })
            } else {
                AskDecision::NeverSynced
            },
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

    /// D1's spine, and the one verification item stated narrowly on purpose: **no per-NOTE
    /// mirror address in `search`'s output**, human or `--json`. Not "no code path prints a
    /// local path" — `doctor` must publish one on every discrepancy row and `guard`'s refusal
    /// echoes the path it refused; those are mount-level and deliberate. What is retired is the
    /// per-note handout an agent read for itself off.
    #[test]
    fn no_hit_carries_a_per_note_mirror_address() {
        // Run it against BOTH mount shapes, because the one that ships is the second: `docli
        // init` writes no `name`, so `display_name()` falls back to the DIRECTORY. A pin written
        // only against a named mount would pass while never exercising the default.
        for name in [Some("заметки".to_string()), None] {
            let mut f = fx();
            f.mount.name = name.clone();
            // A note and a file that ARE on this disk, at addresses the old build would have
            // printed. The mirror's state is irrelevant now, which is itself the point.
            std::fs::write(f.project.root.join("mirror/docs/a.md"), "x").unwrap();
            std::fs::write(f.project.root.join("mirror/docs/pic.png.docli"), "m").unwrap();
            let o = outcome(vec![hit(2, "docs/a.md")], vec![att(5, "docs/pic.png")]);
            let mut any = false;
            let r = render_workspace(&f.mount, &as_local(true), &o, &mut any);
            let text = serde_json::to_string(&r).unwrap();
            assert!(
                !text.contains("localPath") && !text.contains("local_path"),
                "{name:?} {text}"
            );
            assert!(!text.contains("mirror/docs/a.md"), "{name:?} {text}");
            assert!(
                !text.contains("mirror/docs/pic.png.docli"),
                "{name:?} {text}"
            );
            assert!(!text.contains(".docli/markers"), "{name:?} {text}");
            // The SERVER path is what remains, and it is what `docli read` takes.
            assert_eq!(r.hits[0].server_path, "docs/a.md");
            assert_eq!(r.attachments[0].server_path, "docs/pic.png");
            assert!(r.attachments[0].marker, "a file is still flagged as a file");
        }
    }

    /// The limit of the pin above, asserted rather than left to be discovered: with no `name`,
    /// the mount TAG is the mount DIRECTORY, so a reader who joins the tag to the server path
    /// reconstructs the file. That is the plan's accepted position — D1 is «affordance removal,
    /// not impossibility», the mirror's LOCATION stays public because `docli.toml` is committed
    /// and `doctor`/`guard`/`status`/`init` must print directories — but it is the reason the
    /// documents say the per-note HANDOUT is retired rather than claiming the address is
    /// unobtainable. This test exists so that sentence cannot quietly become the stronger one.
    #[test]
    fn a_nameless_mount_tags_hits_with_its_directory_and_the_docs_must_not_deny_it() {
        let mut f = fx();
        f.mount.name = None;
        assert_eq!(f.mount.display_name(), "mirror");
        let o = outcome(vec![hit(2, "docs/a.md")], vec![]);
        let mut any = false;
        let r = render_workspace(&f.mount, &as_local(true), &o, &mut any);
        assert_eq!(r.mount, "mirror");
        // The two halves are separate values on separate fields; nothing emits the join.
        assert_eq!(r.hits[0].server_path, "docs/a.md");
    }

    /// `--id` needs a producer (step 1), and `search` is it — in BOTH surfaces, so an agent
    /// reading the human output is not left with only the address that can be renamed.
    #[test]
    fn every_hit_carries_the_node_id() {
        let f = fx();
        let o = outcome(vec![hit(2, "docs/a.md")], vec![att(5, "docs/pic.png")]);
        let mut any = false;
        let r = render_workspace(&f.mount, &as_local(true), &o, &mut any);
        assert_eq!(r.hits[0].id, Uuid::from_u128(2));
        assert_eq!(r.attachments[0].id, Uuid::from_u128(5));
        let v: serde_json::Value = serde_json::to_value(&r).unwrap();
        assert_eq!(v["hits"][0]["id"], Uuid::from_u128(2).to_string());
        assert_eq!(v["attachments"][0]["id"], Uuid::from_u128(5).to_string());
    }

    /// An unsynced mount still reports its HITS. Absence of a local copy was never absence of
    /// the note, and now there is not even an address to be absent.
    #[test]
    fn an_unsynced_mount_still_reports_its_hits() {
        let f = fx();
        let o = outcome(vec![hit(2, "a.md")], vec![att(3, "b.png")]);
        let mut any = false;
        let r = render_workspace(&f.mount, &as_local(false), &o, &mut any);
        assert_eq!(r.hits.len(), 1);
        assert_eq!(r.attachments.len(), 1);
        assert!(any);
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
            let r = render_workspace(&f.mount, &asked, &o, &mut any);
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
        let r = render_workspace(&f.mount, &asked, &o, &mut any);
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
        render_workspace(&f.mount, l, &o, &mut any)
    }
}
