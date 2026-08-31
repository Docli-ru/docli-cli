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
use docli_sync_wire::{SearchRequest, SearchWorkspaceOutcome, SEARCH_WORKSPACE_CAP};
use uuid::Uuid;

use crate::config::{validate_config, Mount, Project};
use crate::http::Api;
use crate::state::{ControlRoot, TrackedKind, WsState};

pub fn run(project: &Project, api: &Api, query: &str, json: bool) -> Result<i32> {
    // Request-level validation only (Codex round 24): search works without a cache, so the
    // mirror-write geometry rules must not block a server query.
    validate_config(&project.config)?;
    let cwd = std::env::current_dir().unwrap_or_else(|_| project.root.clone());
    let control = ControlRoot::new(&project.root);
    let mounts: Vec<&Mount> = project.config.mounts.iter().collect();

    // Batch: a CLI with more than 16 mounts never hard-fails "search all mounts by default".
    let mut outcomes: Vec<SearchWorkspaceOutcome> = Vec::new();
    for chunk in mounts.chunks(SEARCH_WORKSPACE_CAP) {
        let req = SearchRequest {
            workspace_ids: chunk.iter().map(|m| m.workspace).collect(),
            query: query.to_string(),
            limit: None,
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
                "docli: the server answered for an unrequested workspace {} — skipping",
                o.workspace_id
            );
            continue;
        };
        // A corrupt cache is NO cache (Codex round 25): the hits already arrived from the
        // server, and the disposable `.docli` state must not veto them — degrade to
        // «not mirrored» with a warning.
        let state = match control.load_state(o.workspace_id) {
            // A cache built for a DIFFERENT folder scope maps server paths to stale local
            // spellings (Codex round 26): until sync reprojects it, it is no cache at all —
            // and a MID-REPAIR cache (`from_zero` still set, round 27) or one whose cursor
            // never reached head (round 30 — an interrupted incremental page) is the same:
            // local paths render only off a COMPLETE projection.
            Ok(st) => st.filter(|st| st.scope_key == mount.folder && !st.from_zero && st.at_head),
            Err(e) => {
                eprintln!(
                    "docli: не удалось прочитать локальный кэш ({e:#}) — показываю результаты \
                     без локальных путей"
                );
                None
            }
        };
        rendered.push(render_workspace(
            project,
            &cwd,
            mount,
            state.as_ref(),
            o,
            &mut any_hit,
        ));
        any_degraded |= o.degraded;
        any_refused |= o.refused.is_some();
    }

    if json {
        println!("{}", serde_json::to_string_pretty(&rendered)?);
    } else {
        for r in &rendered {
            print_workspace(r);
        }
        if !any_hit {
            // A REFUSED workspace was not searched at all — strictly MORE inconclusive about
            // absence than a degraded one, so it must never fold into a bare «no hits» either
            // (the split-brain rule's summary half).
            if any_refused {
                println!(
                    "no hits — but at least one workspace was NOT searched (see the refusal \
                     above), so this is INCONCLUSIVE about absence"
                );
            } else if any_degraded {
                // Never a bare empty result on a degraded index.
                println!(
                    "no hits — but the note index was DEGRADED for at least one workspace, so \
                     this is INCONCLUSIVE about absence; retry shortly"
                );
            } else {
                println!("no hits");
            }
        }
    }
    Ok(0)
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
    state: Option<&WsState>,
    o: &SearchWorkspaceOutcome,
    any_hit: &mut bool,
) -> RenderedWorkspace {
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
    // root (`.docli` lives inside the project by construction).
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
    }
}

fn print_workspace(r: &RenderedWorkspace) {
    if let Some(code) = &r.refused {
        // Per-code guidance, same split as sync's arms: «попросите доступ» is ONLY for the
        // no-access class — telling a user to ask a colleague about their own entitlement
        // (402) or about a server fault (INTERNAL) is wrong guidance. The no-access copy
        // itself comes from sync_cmd's single builder (one reader of one message).
        let line = match code.as_str() {
            "UPGRADE_REQUIRED" => {
                "синхронизация не включена для вашего аккаунта — пространство пропущено".to_string()
            }
            "INTERNAL" => "временная ошибка на сервере — повторите поиск".to_string(),
            _ => crate::sync_cmd::no_access_message(&r.mount),
        };
        println!("[{}] {code}: {line}", r.mount);
        return;
    }
    if r.degraded {
        println!(
            "[{}] ВНИМАНИЕ: индекс заметок был неполон для этого запроса — отсутствие \
             результата здесь ничего не доказывает",
            r.mount
        );
    }
    for h in &r.hits {
        match &h.local_path {
            Some(l) => println!("[{}] {}", r.mount, l),
            None => println!(
                "[{}] {} — not mirrored (run `docli sync`, then `docli doctor` if it persists)",
                r.mount, h.server_path
            ),
        }
        if let Some(s) = &h.snippet {
            println!("    {}", s.replace('\n', " "));
        }
    }
    for a in &r.attachments {
        match &a.local_path {
            Some(l) => println!("[{}] {} (marker — bytes live on the server)", r.mount, l),
            None => println!("[{}] {} — file, not mirrored", r.mount, a.server_path),
        }
    }
    if r.attachments_truncated {
        println!(
            "[{}] file matches truncated — more files match than shown",
            r.mount
        );
    }
    if r.attachments_query_truncated {
        println!(
            "[{}] file matches may be a SUPERSET (the query had more terms than the file arm \
             applies)",
            r.mount
        );
    }
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
        }
    }

    fn hit(id: u128, path: &str) -> SearchHitWire {
        SearchHitWire {
            id: Uuid::from_u128(id),
            name: path.rsplit('/').next().unwrap().into(),
            path: path.into(),
            snippet: "…".into(),
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
            Some(&f.state),
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
            Some(&f.state),
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
            Some(&f.state),
            &o,
            &mut any,
        );
        assert_eq!(r.hits[0].local_path.as_deref(), Some("mirror/docs/a.md"));
        assert_eq!(
            r.hits[1].local_path, None,
            "stat-miss renders «not mirrored», never a path"
        );
        assert_eq!(
            r.hits[2].local_path, None,
            "an untracked hit renders «not mirrored»"
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
            Some(&f.state),
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
        let r = render_workspace(&f.project, &f.project.root, &f.mount, None, &o, &mut any);
        assert!(r.hits[0].local_path.is_none());
        assert!(r.attachments[0].local_path.is_none());
        assert!(
            any,
            "hits still count as hits — absence of a PATH is not absence of the note"
        );
    }

    /// The «токен» ban covers THIS refusal surface too (round-1 M4): the search branch reuses
    /// sync_cmd's single copy builder, and this pins the reuse.
    #[test]
    fn the_search_refusal_copy_never_says_token() {
        let msg = crate::sync_cmd::no_access_message("книга продаж");
        assert!(!msg.to_lowercase().contains("токен"), "{msg}");
        assert!(msg.contains("попросите доступ"), "{msg}");
    }
}
