// SPDX-FileCopyrightText: 2026 OOO Agitek
// SPDX-License-Identifier: MIT

//! `docli init` (v0.28.0 goal 5 + D12) — create/extend `docli.toml`, offer the gitignore
//! lines, drop the bundled SKILL.md teaching agents the contract (at the Agent Skills open
//! standard path `.agents/skills/`, read natively by Claude Code, Codex, Gemini, Cursor, Zed,
//! both Copilots, OpenCode and Amp), and — opt-in only — wire agent MCP configs (`agents.rs`).
//! Workspace enumeration uses
//! `viewer.workspaces` (deliberately exempt from `deny_scoped_pat_via_graphql`, filtered to the
//! PAT's pin set).

use std::fs;
use std::path::Path;

use anyhow::{bail, Context, Result};
use uuid::Uuid;

use crate::config::{load_project, validate_geometry, DocliToml, Mount, CONFIG_NAME};
use crate::http::Api;

pub const SKILL_MD: &str = include_str!("../assets/SKILL.md");

#[derive(Clone)]
pub struct InitArgs {
    pub workspace: Option<Uuid>,
    pub dir: Option<String>,
    pub folder: Option<String>,
    pub name: Option<String>,
    pub server: Option<String>,
    /// D12.4 — `--mcp auto` (detected), `--mcp none`, or a comma list of agent keys.
    /// `None` = not requested: prompt on a TTY, print a hint otherwise. Opt-in only.
    pub mcp: Option<String>,
    /// Explicit connection label (validated verbatim against the shared grammar — user-supplied
    /// labels are refused off-grammar, never truncated). Default: the label persisted in
    /// docli.toml, else derived from the dir name (and then persisted — see `DocliToml::mcp_label`).
    pub mcp_label: Option<String>,
    /// Write the BARE `…/api/mcp` URL instead of a labeled connection — the escape hatch for
    /// clients that omit RFC 8707 `resource` (the labeled route's byte-exact audience refuses
    /// their bare-audience token).
    pub mcp_bare: bool,
    /// May `run` ask on a TTY when `--mcp` is absent? `main` passes true; unit tests pass
    /// false so a test run never consults ambient terminal state (or blocks on stdin).
    pub allow_prompt: bool,
    /// Clear a recorded folder scope. Absent `--folder` means «leave what is recorded» (a
    /// re-run must not silently widen the mirror to the whole workspace), so WIDENING needs a
    /// word of its own — this is it, and the wizard sets it when the reader empties the field.
    pub clear_folder: bool,
    /// Append the missing `.gitignore` entries instead of only naming them — the scriptable
    /// half of the wizard's confirmation, so a CI setup can reach the same end state without
    /// a terminal. Absent, `.gitignore` is never touched (it is the user's file).
    pub write_gitignore: bool,
}

/// The validated `--mcp` intent, resolved BEFORE anything touches the disk.
enum McpPlan {
    Skip,
    Hint,
    Prompt,
    Auto,
    Keys(Vec<&'static str>),
}

pub fn run(cwd: &Path, api: Option<&Api>, args: &InitArgs) -> Result<i32> {
    // Never SHADOW an ancestor project (Codex round 24): every other verb discovers
    // docli.toml git-style up the tree, so a nested config written here would silently take
    // over — with a default server — for everything below it. Extend the discovered project
    // from its own root instead.
    if let Some(ancestor) = crate::config::find_project(cwd) {
        if ancestor != cwd {
            bail!(
                "this directory is inside the docli project at {} - run `docli init` from \
                 there (a nested docli.toml would take precedence for every command run in \
                 its subtree)",
                ancestor.display()
            );
        }
    }
    let config_path = cwd.join(CONFIG_NAME);
    // Through the SHARED door (`config::parse_config`), never a bare `toml::from_str`: it is
    // what refuses control characters, and the very next thing this function does is render the
    // existing directory back to the terminal in the re-point warning.
    let mut config: DocliToml = if config_path.exists() {
        crate::config::parse_config(&fs::read_to_string(&config_path)?)?
    } else {
        toml::from_str("").expect("empty config parses")
    };
    // An EXPLICIT `--server` applies whether or not the config already exists. Applying it only
    // to new files meant the wizard could list workspace of server B, record one of their ids,
    // and leave the file naming server A — every later command then asked A about B's workspace.
    if let Some(s) = &args.server {
        let requested = s.trim_end_matches('/').to_string();
        // A workspace id is SERVER-SCOPED. Rewriting the origin under mounts recorded against
        // the old one produces a config that asks the new server about the old server's
        // workspaces — every later sync then fails on ids that server has never heard of.
        // Both sides NORMALIZED: `server = "https://docli.ru/"` in the file against the same
        // origin on the command line is not a cross-server switch, and refusing it was a
        // refusal of the exact command the wizard writes.
        let stored = config.server.trim_end_matches('/');
        if requested != stored && !config.mounts.is_empty() {
            bail!(
                "docli.toml records server {} and mounts that belong to it - switching to \
                 {requested} would leave another server's workspace ids behind. Remove the \
                 mounts, or start a separate project.",
                stored
            );
        }
        config.server = requested;
    }

    match (&args.workspace, &args.dir) {
        (Some(ws), Some(dir)) => {
            // Idempotent: re-running init for a workspace RE-POINTS its existing mount instead
            // of adding a second one (which validate_geometry would then refuse) — re-running
            // the same command after fixing .gitignore is the most ordinary thing a user does.
            // Absent --folder/--name keep what is already recorded: dropping a folder scope on a
            // re-run would silently WIDEN the mirror to the whole workspace.
            match config.mounts.iter_mut().find(|m| m.workspace == *ws) {
                Some(existing) => {
                    if existing.dir != *dir {
                        crate::ui::warn(&format!(
                            "workspace {ws} was mounted at `{}` - re-pointing it to `{dir}`; \
                             the old directory is no longer managed (remove it yourself)",
                            existing.dir
                        ));
                    }
                    existing.dir = dir.clone();
                    if args.folder.is_some() {
                        existing.folder = args.folder.clone();
                    } else if args.clear_folder {
                        existing.folder = None;
                    }
                    if args.name.is_some() {
                        existing.name = args.name.clone();
                    }
                }
                None => config.mounts.push(Mount {
                    workspace: *ws,
                    dir: dir.clone(),
                    folder: args.folder.clone(),
                    name: args.name.clone(),
                }),
            }
            // The geometry rules gate the ADD, not just the later sync — MINUS the ignore
            // rule, which `--gitignore` is about to satisfy. Everything else refuses here,
            // before anything at all is written.
            crate::config::validate_geometry_paths_only(cwd, &config)?;
        }
        (None, None) => {
            // `--clear-folder` needs to know WHICH mount to widen; on its own it silently did
            // nothing and exited 0.
            // Every mount-shaping flag needs to know WHICH mount. On their own they were
            // silently discarded and the command exited 0, which reads as «done».
            // Each flag carries the SPELLING it needs in the suggestion: a value-taking flag
            // printed bare produced a suggested command that clap immediately rejects.
            let stray = [
                ("--folder", "--folder <folder>", args.folder.is_some()),
                ("--name", "--name <name>", args.name.is_some()),
                ("--clear-folder", "--clear-folder", args.clear_folder),
            ]
            .into_iter()
            .filter(|(_, _, given)| *given)
            .collect::<Vec<_>>();
            if !stray.is_empty() {
                bail!(
                    "{} applies to a specific mount - name it: docli init --workspace <id> \
                     --dir <dir> {}",
                    stray
                        .iter()
                        .map(|(f, _, _)| *f)
                        .collect::<Vec<_>>()
                        .join(", "),
                    stray
                        .iter()
                        .map(|(_, usage, _)| *usage)
                        .collect::<Vec<_>>()
                        .join(" ")
                );
            }
        }
        _ => bail!("--workspace and --dir must be used together"),
    }

    // Validate + resolve the whole --mcp intent BEFORE the first write: a bad flag must
    // refuse over an untouched tree, not over a half-initialized one. The TTY prompt also
    // runs here — Ctrl-C at the prompt leaves nothing behind.
    let plan = plan_mcp(args)?;
    let hint_mcp = matches!(plan, McpPlan::Hint);
    let mcp_selection: Vec<&'static str> = match plan {
        McpPlan::Skip => Vec::new(),
        // The hint is PRINTED at the end, not here: `plan_mcp` runs before the first write so a
        // bad flag refuses over an untouched tree, and a suggestion that appears above «записан
        // …» reads as something that already happened.
        McpPlan::Hint => Vec::new(),
        McpPlan::Prompt => {
            prompt_selection(&crate::agents::detect(cwd, std::env::home_dir().as_deref()))?
                .unwrap_or_default()
        }
        McpPlan::Auto => {
            let detected = crate::agents::detect(cwd, std::env::home_dir().as_deref());
            if detected.is_empty() {
                crate::ui::detail("--mcp auto: no coding agents detected here - nothing was wired");
            }
            detected
        }
        McpPlan::Keys(keys) => keys,
    };
    if mcp_selection.is_empty() && (args.mcp_label.is_some() || args.mcp_bare) {
        crate::ui::detail("--mcp-label/--mcp-bare do not apply: no agents were selected");
    }
    if !mcp_selection.is_empty() && config.server.chars().any(|c| c.is_control()) {
        // Codex round 1: the committed server value is printed raw in the wiring output — a
        // control character in it is terminal-escape injection (\u{1b}[2J + forged lines),
        // and no legitimate origin contains one. Refuse before anything is echoed.
        bail!("docli.toml's server contains control characters - fix it before wiring agents");
    }
    let mcp_url = if mcp_selection.is_empty() {
        None
    } else if args.mcp_bare {
        Some((crate::agents::connection_url_bare(&config.server), false))
    } else {
        let label = match &args.mcp_label {
            // Flag labels were validated in plan_mcp; the PERSISTED label is validated below —
            // docli.toml is committed and teammate-editable, i.e. untrusted input (round-4
            // F-A, the F0 premise applied to the second field).
            Some(l) => l.clone(),
            // The persisted label wins over dir-name derivation: renaming the project
            // directory must not silently fork the connection (grant/persona/pin).
            None => match &config.mcp_label {
                Some(l) => {
                    if !docli_rules::valid_label(l) {
                        bail!(
                            "docli.toml's mcp_label {l:?} is invalid: use at most 64 bytes \
                             containing only lowercase a-z, 0-9 and '-' - fix it in docli.toml \
                             or override it with --mcp-label"
                        );
                    }
                    l.clone()
                }
                None => crate::agents::sanitize_label(
                    &cwd.file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                ),
            },
        };
        config.mcp_label = Some(label.clone());
        Some((crate::agents::connection_url(&config.server, &label), true))
    };

    // EVERY validation has passed by now (geometry, the `--mcp` intent, any prompt), so this is
    // the first point at which writing is safe. `--gitignore` writes here rather than beside the
    // mount edit: appending to the user's file and then refusing the command left them with a
    // change they did not get anything for.
    // Whatever arm brought us here, an EXISTING mount can be illegal too (a hand-edited path
    // with a control character, an overlap): validating only inside the `--workspace --dir` arm
    // let `docli init --gitignore` append patterns derived from that path — including one that
    // hid real source — and refuse afterwards.
    if !config.mounts.is_empty() {
        crate::config::validate_geometry_paths_only(cwd, &config)?;
    }
    if args.write_gitignore {
        // Computed and applied HERE for both shapes — with mounts and without. The no-mount
        // case used to run after `docli.toml` and `SKILL.md` were already on disk, so a
        // repository where git cannot answer left a failed command with files written.
        let mut want: Vec<crate::wizard::IgnoreFix> = Vec::new();
        if config.mounts.is_empty() {
            if let Some(fix) = crate::wizard::control_ignore(cwd)? {
                want.push(fix);
            }
        } else {
            for m in &config.mounts {
                for fix in crate::wizard::missing_ignores(cwd, &m.dir)? {
                    if !want.contains(&fix) {
                        want.push(fix);
                    }
                }
            }
        }
        if !want.is_empty() {
            let labels: Vec<String> = want.iter().map(|f| f.label(cwd)).collect();
            crate::wizard::append_ignores(&want)?;
            crate::ui::ok(&format!("appended to .gitignore: {}", labels.join(", ")));
        }
    }
    // The full gate, ignore rule included — now satisfiable, and still a refusal if it is not.
    // Only with mounts: a config that has none is a legal intermediate state (`docli init` with
    // no arguments writes exactly that), and `validate_config` rightly refuses an empty table.
    if !config.mounts.is_empty() {
        validate_geometry(cwd, &config)?;
    }

    let body = toml::to_string_pretty(&config).context("serializing docli.toml")?;
    fs::write(&config_path, format!("{}{body}", config_header()))?;
    crate::ui::ok(&format!("wrote {}", config_path.display()));

    // The agent contract, at the Agent Skills open-standard path (D12.2) — one drop, read
    // lazily by every standard-following harness; off-standard agents get copies via the
    // picker below.
    let skill_dir = cwd.join(".agents/skills/docli-mirror");
    fs::create_dir_all(&skill_dir)?;
    fs::write(skill_dir.join("SKILL.md"), SKILL_MD)?;
    crate::ui::ok(&format!("wrote {}", skill_dir.join("SKILL.md").display()));

    // Offer the ignore lines (never write .gitignore ourselves — it is the user's file).
    if config.mounts.is_empty() {
        crate::ui::next(&format!(
            "Add a mount: {} (or run {} with no flags for the guided setup)",
            crate::ui::cmd("docli init --workspace <id> --dir <dir>"),
            crate::ui::cmd("docli init")
        ));
        if let Some(api) = api {
            // ONE renderer for the workspace list (`docli list`), so the columns and the
            // mounted-here marking cannot drift between the two places it appears.
            // Reported, not swallowed: with valid credentials and an unreachable server, a
            // silent success printed no list and no reason, which reads as «you have none».
            if let Err(e) = crate::list_cmd::render_rows(cwd, api, &config.server) {
                crate::ui::warn(&format!("could not list your workspaces: {e:#}"));
            }
        } else {
            crate::ui::next(&format!(
                "Sign in first: {} - your workspaces are then listed here",
                crate::ui::cmd("docli login")
            ));
        }
    } else {
        // Only name what git does NOT already ignore — a repo that is fully set up should not
        // be told to add entries it already has. ONE producer of these entries
        // (`wizard::missing_ignores`), so the hoist rule cannot differ between the line this
        // prints and the line `--gitignore` writes.
        let mut want: Vec<crate::wizard::IgnoreFix> = Vec::new();
        for m in &config.mounts {
            for fix in crate::wizard::missing_ignores(cwd, &m.dir)? {
                if !want.contains(&fix) {
                    want.push(fix);
                }
            }
        }
        if !want.is_empty() {
            // Anything still missing here was NOT written above (that path covers every mount
            // under `--gitignore`), so it is named, never written behind the reader's back.
            let labels: Vec<String> = want.iter().map(|f| f.label(cwd)).collect();
            crate::ui::warn("add these lines to .gitignore - otherwise docli sync refuses to run:");
            for line in &labels {
                crate::ui::detail(line);
            }
            crate::ui::next(&format!(
                "Or let docli append them: {}",
                crate::ui::cmd("docli init --gitignore")
            ));
        }
        crate::ui::next(&format!("Sync: {}", crate::ui::cmd("docli sync")));
    }
    if let Some((url, labeled)) = mcp_url {
        let selected: Vec<&crate::agents::AgentDef> = mcp_selection
            .iter()
            .filter_map(|k| crate::agents::agent(k))
            .collect();
        crate::agents::wire(cwd, &selected, &url, labeled, SKILL_MD);
        if config.mounts.len() > 1 {
            crate::ui::detail(&format!(
                "This project mounts {} but uses ONE MCP connection: which of them the agent \
                 can reach is chosen during the browser consent.",
                crate::ui::plural(config.mounts.len(), "workspace", "workspaces")
            ));
        }
    }
    if hint_mcp {
        crate::ui::next(&format!(
            "Wire this project's MCP connection into agents: {} (or a list: \
             claude,codex,gemini,...)",
            crate::ui::cmd("docli init --mcp auto")
        ));
    }

    // Re-load to prove the round-trip.
    let _ = load_project(cwd)?;
    Ok(0)
}

/// Parse + validate the `--mcp` intent — PURE, called before any write. The prompt itself is
/// deferred to the caller (it needs the detection scan), but whether prompting is allowed at
/// all is decided here: `allow_prompt` gates it, and a non-TTY stdin downgrades to the hint.
fn plan_mcp(args: &InitArgs) -> Result<McpPlan> {
    // A user-supplied label is validated UNCONDITIONALLY (round-2 R9) — `--mcp none
    // --mcp-label "МОЙ"` must refuse loudly, not accept-and-ignore an off-grammar label.
    if let Some(l) = &args.mcp_label {
        if !docli_rules::valid_label(l) {
            bail!(
                "--mcp-label {l:?} is invalid: use at most 64 bytes containing only \
                 lowercase a-z, 0-9 and '-'; invalid labels are rejected rather than truncated"
            );
        }
    }
    match args.mcp.as_deref().map(str::trim) {
        Some("none") | Some("") => Ok(McpPlan::Skip),
        Some("auto") => Ok(McpPlan::Auto),
        Some(list) => {
            let mut keys = Vec::new();
            for k in list.split(',').map(str::trim).filter(|k| !k.is_empty()) {
                match crate::agents::agent(k) {
                    Some(def) => keys.push(def.key),
                    None => bail!(
                        "unknown agent {k:?} in --mcp - valid: {}",
                        crate::agents::AGENTS
                            .iter()
                            .map(|a| a.key)
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                }
            }
            if keys.is_empty() {
                return Ok(McpPlan::Skip);
            }
            Ok(McpPlan::Keys(keys))
        }
        None => {
            // `ui::interactive`, not a bare stdin check: it also honours `--no-input`, which
            // otherwise reached a terminal run and prompted anyway.
            if args.allow_prompt && crate::ui::interactive() {
                Ok(McpPlan::Prompt)
            } else {
                Ok(McpPlan::Hint)
            }
        }
    }
}

/// One TTY prompt: Enter = detected set, `n` = skip, else a comma list. Unknown keys are
/// reported and dropped (interactive forgiveness; the FLAG path refuses instead).
fn prompt_selection(detected: &[&'static str]) -> Result<Option<Vec<&'static str>>> {
    crate::ui::detail(
        "Space toggles, Enter confirms. The configurations found here are ticked; only what \
         stays ticked is written.",
    );
    let picked = crate::wizard::pick_agents(detected)?;
    // An empty selection is a deliberate «ничего не трогать», not an error.
    Ok(Some(picked))
}

fn config_header() -> &'static str {
    "# docli.toml - the mount table for the docli CLI (committed; names workspaces, grants nothing).\n\
     # Mounts are keyed by stable workspace IDs, never by @handles (which can be renamed).\n\
     # The mirror dirs and .docli/ must be git-ignored; only this file is committed.\n\n"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn re_running_init_repoints_the_mount_instead_of_duplicating_it() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = Uuid::from_u128(11);
        let base = InitArgs {
            workspace: Some(ws),
            dir: Some("cache".into()),
            folder: Some("docs".into()),
            name: None,
            server: Some("https://docli.ru".into()),
            mcp: None,
            mcp_label: None,
            mcp_bare: false,
            allow_prompt: false,
            clear_folder: false,
            write_gitignore: false,
        };
        run(tmp.path(), None, &base).unwrap();
        // Same command again: idempotent, not a duplicate mount (which geometry would refuse).
        run(tmp.path(), None, &base).unwrap();
        let p = load_project(tmp.path()).unwrap();
        assert_eq!(p.config.mounts.len(), 1);

        // Re-pointing keeps the folder scope when --folder is absent — dropping it would
        // silently widen the mirror to the whole workspace.
        let moved = InitArgs {
            dir: Some("other".into()),
            folder: None,
            ..base.clone()
        };
        run(tmp.path(), None, &moved).unwrap();
        let p = load_project(tmp.path()).unwrap();
        assert_eq!(p.config.mounts.len(), 1);
        assert_eq!(p.config.mounts[0].dir, "other");
        assert_eq!(p.config.mounts[0].folder.as_deref(), Some("docs"));

        // A second workspace still ADDS.
        let other = InitArgs {
            workspace: Some(Uuid::from_u128(12)),
            dir: Some("second".into()),
            folder: None,
            ..base.clone()
        };
        run(tmp.path(), None, &other).unwrap();
        assert_eq!(load_project(tmp.path()).unwrap().config.mounts.len(), 2);
    }

    #[test]
    fn a_folder_scope_survives_a_re_run_and_clears_only_when_asked() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = Uuid::from_u128(21);
        let scoped = InitArgs {
            workspace: Some(ws),
            dir: Some("cache".into()),
            folder: Some("docs".into()),
            name: None,
            server: Some("https://docli.ru".into()),
            mcp: None,
            mcp_label: None,
            mcp_bare: false,
            allow_prompt: false,
            clear_folder: false,
            write_gitignore: false,
        };
        run(tmp.path(), None, &scoped).unwrap();

        // A re-run without --folder KEEPS the scope: dropping it would silently widen the
        // mirror to the whole workspace.
        let bare = InitArgs {
            folder: None,
            ..scoped.clone()
        };
        run(tmp.path(), None, &bare).unwrap();
        let p = load_project(tmp.path()).unwrap();
        assert_eq!(p.config.mounts[0].folder.as_deref(), Some("docs"));

        // Widening is available, but it takes a word of its own.
        let cleared = InitArgs {
            folder: None,
            clear_folder: true,
            ..scoped.clone()
        };
        run(tmp.path(), None, &cleared).unwrap();
        let p = load_project(tmp.path()).unwrap();
        assert_eq!(p.config.mounts[0].folder, None);
    }

    #[test]
    fn an_explicit_server_applies_but_never_strands_existing_mounts() {
        // Applying `--server` only when CREATING the file let the wizard record a workspace id
        // from server B into a config still naming server A. Applying it UNCONDITIONALLY is the
        // other half of the same bug: a workspace id is server-scoped, so moving the origin
        // under existing mounts asks the new server about the old one's workspaces.
        let tmp = tempfile::tempdir().unwrap();
        let bare = InitArgs {
            workspace: None,
            dir: None,
            folder: None,
            name: None,
            server: Some("https://a.example".into()),
            mcp: Some("none".into()),
            mcp_label: None,
            mcp_bare: false,
            allow_prompt: false,
            clear_folder: false,
            write_gitignore: false,
        };
        run(tmp.path(), None, &bare).unwrap();
        assert_eq!(
            load_project(tmp.path()).unwrap().config.server,
            "https://a.example"
        );

        // No mounts yet: the origin is free to move (trailing slash normalized at this seam).
        run(
            tmp.path(),
            None,
            &InitArgs {
                server: Some("https://b.example/".into()),
                ..bare.clone()
            },
        )
        .unwrap();
        assert_eq!(
            load_project(tmp.path()).unwrap().config.server,
            "https://b.example"
        );

        // Now record a mount, and the same change REFUSES rather than stranding its id.
        let mounted = InitArgs {
            workspace: Some(Uuid::from_u128(31)),
            dir: Some("cache".into()),
            server: None,
            ..bare.clone()
        };
        run(tmp.path(), None, &mounted).unwrap();
        let err = run(
            tmp.path(),
            None,
            &InitArgs {
                server: Some("https://c.example".into()),
                ..mounted.clone()
            },
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("another server's workspace ids"), "{err}");
        assert_eq!(
            load_project(tmp.path()).unwrap().config.server,
            "https://b.example",
            "a refusal must leave the file untouched"
        );
    }

    #[test]
    fn init_writes_config_and_skill_and_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let args = InitArgs {
            workspace: Some(Uuid::from_u128(7)),
            dir: Some("mirror/notes".into()),
            folder: Some("docs".into()),
            name: Some("заметки".into()),
            server: Some("https://docli.ru".into()),
            mcp: None,
            mcp_label: None,
            mcp_bare: false,
            allow_prompt: false,
            clear_folder: false,
            write_gitignore: false,
        };
        run(tmp.path(), None, &args).unwrap();
        let p = load_project(tmp.path()).unwrap();
        assert_eq!(p.config.mounts.len(), 1);
        assert_eq!(p.config.mounts[0].workspace, Uuid::from_u128(7));
        assert_eq!(p.config.mounts[0].folder.as_deref(), Some("docs"));
        assert!(tmp
            .path()
            .join(".agents/skills/docli-mirror/SKILL.md")
            .exists());
        // The bundled contract carries the load-bearing sentences.
        assert!(SKILL_MD.contains("never synced and never protected"));
        assert!(SKILL_MD.contains("only a non-degraded server search"));
        assert!(SKILL_MD.contains("sync --check"));
        // D12.3 — the write-discipline paragraph ships in the contract.
        assert!(SKILL_MD.contains("prefer `edit_note`"));
        assert!(SKILL_MD.contains("conflictSiblingId"));
    }

    #[test]
    fn mcp_flag_wires_the_selected_agent_with_a_validated_label() {
        let tmp = tempfile::tempdir().unwrap();
        let args = InitArgs {
            workspace: None,
            dir: None,
            folder: None,
            name: None,
            server: Some("https://docli.ru".into()),
            mcp: Some("claude".into()),
            mcp_label: Some("myproj".into()),
            mcp_bare: false,
            allow_prompt: false,
            clear_folder: false,
            write_gitignore: false,
        };
        run(tmp.path(), None, &args).unwrap();
        let mcp: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(tmp.path().join(".mcp.json")).unwrap())
                .unwrap();
        assert_eq!(
            mcp["mcpServers"]["docli"]["url"],
            "https://docli.ru/api/mcp/c/myproj"
        );
        // Re-run is idempotent — the merge reports already-configured, never duplicates.
        run(tmp.path(), None, &args).unwrap();
    }

    #[test]
    fn mcp_flag_refuses_unknown_agents_and_off_grammar_labels() {
        let tmp = tempfile::tempdir().unwrap();
        let mut args = InitArgs {
            workspace: None,
            dir: None,
            folder: None,
            name: None,
            server: Some("https://docli.ru".into()),
            mcp: Some("claude,nonsense".into()),
            mcp_label: None,
            mcp_bare: false,
            allow_prompt: false,
            clear_folder: false,
            write_gitignore: false,
        };
        let err = run(tmp.path(), None, &args).unwrap_err().to_string();
        assert!(err.contains("unknown agent"), "{err}");
        // F9 pin: validation runs BEFORE the first write — a bad flag refuses over an
        // untouched tree, not a half-initialized one.
        assert!(!tmp.path().join(".mcp.json").exists(), "nothing written");
        assert!(
            !tmp.path().join(CONFIG_NAME).exists(),
            "docli.toml not written"
        );
        assert!(
            !tmp.path()
                .join(".agents/skills/docli-mirror/SKILL.md")
                .exists(),
            "skill not dropped"
        );

        args.mcp = Some("claude".into());
        // User-SUPPLIED labels are validated verbatim: refused, never truncated/sanitized.
        args.mcp_label = Some("Мой Проект".into());
        let err = run(tmp.path(), None, &args).unwrap_err().to_string();
        assert!(err.contains("is invalid"), "{err}");
        assert!(!tmp.path().join(".mcp.json").exists());
    }

    #[test]
    fn label_is_persisted_and_wins_over_dir_derivation() {
        let tmp = tempfile::tempdir().unwrap();
        let mut args = InitArgs {
            workspace: None,
            dir: None,
            folder: None,
            name: None,
            server: Some("https://docli.ru".into()),
            mcp: Some("cursor".into()),
            mcp_label: Some("stable-label".into()),
            mcp_bare: false,
            allow_prompt: false,
            clear_folder: false,
            write_gitignore: false,
        };
        run(tmp.path(), None, &args).unwrap();
        // The chosen label lands in docli.toml…
        let cfg = fs::read_to_string(tmp.path().join(CONFIG_NAME)).unwrap();
        assert!(cfg.contains("mcp_label = \"stable-label\""), "{cfg}");
        // …and a re-run WITHOUT --mcp-label reuses it (a directory rename must not fork the
        // connection), wiring a second agent at the SAME url.
        args.mcp = Some("gemini".into());
        args.mcp_label = None;
        run(tmp.path(), None, &args).unwrap();
        let gem: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(tmp.path().join(".gemini/settings.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            gem["mcpServers"]["docli"]["httpUrl"],
            "https://docli.ru/api/mcp/c/stable-label"
        );
    }

    #[test]
    fn bare_flag_writes_the_unlabeled_url_and_persists_no_label() {
        let tmp = tempfile::tempdir().unwrap();
        let args = InitArgs {
            workspace: None,
            dir: None,
            folder: None,
            name: None,
            server: Some("https://docli.ru".into()),
            mcp: Some("claude".into()),
            mcp_label: None,
            mcp_bare: true,
            allow_prompt: false,
            clear_folder: false,
            write_gitignore: false,
        };
        run(tmp.path(), None, &args).unwrap();
        let mcp: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(tmp.path().join(".mcp.json")).unwrap())
                .unwrap();
        assert_eq!(
            mcp["mcpServers"]["docli"]["url"],
            "https://docli.ru/api/mcp"
        );
        let cfg = fs::read_to_string(tmp.path().join(CONFIG_NAME)).unwrap();
        assert!(
            !cfg.contains("mcp_label"),
            "bare wiring persists no label: {cfg}"
        );
    }

    #[test]
    fn a_persisted_off_grammar_label_is_refused_not_laundered() {
        // Round-4 F-A: docli.toml is committed and teammate-editable — a hand-edited
        // `mcp_label` must refuse like a flag label, never flow into agent configs.
        let tmp = tempfile::tempdir().unwrap();
        fs::write(
            tmp.path().join(CONFIG_NAME),
            "server = \"https://docli.ru\"\nmcp_label = \"Мой Проект\"\n",
        )
        .unwrap();
        let args = InitArgs {
            workspace: None,
            dir: None,
            folder: None,
            name: None,
            server: None,
            mcp: Some("claude".into()),
            mcp_label: None,
            mcp_bare: false,
            allow_prompt: false,
            clear_folder: false,
            write_gitignore: false,
        };
        let err = run(tmp.path(), None, &args).unwrap_err().to_string();
        assert!(err.contains("is invalid"), "{err}");
        assert!(!tmp.path().join(".mcp.json").exists());
    }

    #[test]
    fn a_control_character_server_is_refused_before_any_echo() {
        // Codex round 1 (finding 5): the committed server value is printed raw in the
        // wiring output — a control character is terminal-escape injection. Constructed at
        // runtime (char 27 = ESC) so nothing decodes it out of the source.
        let tmp = tempfile::tempdir().unwrap();
        let args = InitArgs {
            workspace: None,
            dir: None,
            folder: None,
            name: None,
            server: Some(format!("https://docli.ru{}[2JFORGED", char::from(27u8))),
            mcp: Some("claude".into()),
            mcp_label: Some("x".into()),
            mcp_bare: false,
            allow_prompt: false,
            clear_folder: false,
            write_gitignore: false,
        };
        let err = run(tmp.path(), None, &args).unwrap_err().to_string();
        assert!(err.contains("control characters"), "{err}");
        assert!(!tmp.path().join(".mcp.json").exists());
    }

    #[test]
    fn without_the_flag_nothing_is_wired() {
        // Opt-in only (D12.4): a plain non-TTY init never touches agent configs.
        let tmp = tempfile::tempdir().unwrap();
        let args = InitArgs {
            workspace: None,
            dir: None,
            folder: None,
            name: None,
            server: Some("https://docli.ru".into()),
            mcp: None,
            mcp_label: None,
            mcp_bare: false,
            allow_prompt: false,
            clear_folder: false,
            write_gitignore: false,
        };
        run(tmp.path(), None, &args).unwrap();
        for f in [".mcp.json", ".codex/config.toml", ".gemini/settings.json"] {
            assert!(!tmp.path().join(f).exists(), "{f} must not exist");
        }
    }

    #[test]
    fn init_refuses_a_geometry_breaking_mount() {
        let tmp = tempfile::tempdir().unwrap();
        let args = InitArgs {
            workspace: Some(Uuid::from_u128(7)),
            dir: Some(".".into()), // would contain docli.toml - the control-plane rule
            folder: None,
            name: None,
            server: None,
            mcp: None,
            mcp_label: None,
            mcp_bare: false,
            allow_prompt: false,
            clear_folder: false,
            write_gitignore: false,
        };
        let err = run(tmp.path(), None, &args).unwrap_err().to_string();
        assert!(err.contains("docli.toml or .docli/ directory"), "{err}");
    }
}
