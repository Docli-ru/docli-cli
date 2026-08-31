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

use crate::config::{load_project, mount_abs, validate_geometry, DocliToml, Mount, CONFIG_NAME};
use crate::http::Api;

pub const SKILL_MD: &str = include_str!("../assets/SKILL.md");

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
                "this directory is inside the docli project at {} — run `docli init` from \
                 there (a nested docli.toml would take precedence for every command run in \
                 its subtree)",
                ancestor.display()
            );
        }
    }
    let config_path = cwd.join(CONFIG_NAME);
    let mut config: DocliToml = if config_path.exists() {
        toml::from_str(&fs::read_to_string(&config_path)?).context("parsing docli.toml")?
    } else {
        let mut c: DocliToml = toml::from_str("").expect("empty config parses");
        if let Some(s) = &args.server {
            c.server = s.trim_end_matches('/').to_string();
        }
        c
    };

    match (&args.workspace, &args.dir) {
        (Some(ws), Some(dir)) => {
            config.mounts.push(Mount {
                workspace: *ws,
                dir: dir.clone(),
                folder: args.folder.clone(),
                name: args.name.clone(),
            });
            // The geometry rules gate the ADD, not just the later sync.
            validate_geometry(cwd, &config)?;
        }
        (None, None) => {}
        _ => bail!("--workspace and --dir must be used together"),
    }

    // Validate + resolve the whole --mcp intent BEFORE the first write: a bad flag must
    // refuse over an untouched tree, not over a half-initialized one. The TTY prompt also
    // runs here — Ctrl-C at the prompt leaves nothing behind.
    let mcp_selection: Vec<&'static str> = match plan_mcp(args)? {
        McpPlan::Skip => Vec::new(),
        McpPlan::Hint => {
            println!(
                "\nTo point coding agents at this project's docli MCP connection:\n  \
                 docli init --mcp auto   (or a comma-separated list: claude,codex,gemini,...)"
            );
            Vec::new()
        }
        McpPlan::Prompt => {
            prompt_selection(&crate::agents::detect(cwd, std::env::home_dir().as_deref()))?
                .unwrap_or_default()
        }
        McpPlan::Auto => {
            let detected = crate::agents::detect(cwd, std::env::home_dir().as_deref());
            if detected.is_empty() {
                println!("\n--mcp auto: no coding agents detected here — nothing wired");
            }
            detected
        }
        McpPlan::Keys(keys) => keys,
    };
    if mcp_selection.is_empty() && (args.mcp_label.is_some() || args.mcp_bare) {
        println!("(--mcp-label/--mcp-bare ignored — no agents selected for MCP wiring)");
    }
    if !mcp_selection.is_empty() && config.server.chars().any(|c| c.is_control()) {
        // Codex round 1: the committed server value is printed raw in the wiring output — a
        // control character in it is terminal-escape injection (\u{1b}[2J + forged lines),
        // and no legitimate origin contains one. Refuse before anything is echoed.
        bail!("docli.toml's server contains control characters — fix it before wiring agents");
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
                             containing only lowercase a-z, 0-9 and '-' — fix it in docli.toml \
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

    let body = toml::to_string_pretty(&config).context("serializing docli.toml")?;
    fs::write(&config_path, format!("{}{body}", config_header()))?;
    println!("wrote {}", config_path.display());

    // The agent contract, at the Agent Skills open-standard path (D12.2) — one drop, read
    // lazily by every standard-following harness; off-standard agents get copies via the
    // picker below.
    let skill_dir = cwd.join(".agents/skills/docli-mirror");
    fs::create_dir_all(&skill_dir)?;
    fs::write(skill_dir.join("SKILL.md"), SKILL_MD)?;
    println!("wrote {}", skill_dir.join("SKILL.md").display());

    // Offer the ignore lines (never write .gitignore ourselves — it is the user's file).
    if config.mounts.is_empty() {
        println!(
            "\nadd a mount:\n  docli init --workspace <id> --dir <dir> [--folder <server folder>]"
        );
        if let Some(api) = api {
            match api.workspaces() {
                Ok(list) if !list.is_empty() => {
                    println!("\nyour workspaces:");
                    for w in list {
                        println!("  {}  @{}  {}", w.id, w.handle, w.name);
                    }
                }
                Ok(_) => {}
                Err(e) => println!("\n(could not list workspaces: {e} — run `docli login`)"),
            }
        } else {
            println!("\nrun `docli login` first to list your workspaces here");
        }
    } else {
        println!(
            "\nmake sure .gitignore contains these entries (docli sync refuses to run otherwise):"
        );
        println!("  .docli/");
        for m in &config.mounts {
            let abs = mount_abs(cwd, m);
            if let Ok(rel) = abs.strip_prefix(cwd) {
                println!("  {}/", rel.display());
            }
        }
        println!("\nthen: docli sync");
    }
    if let Some((url, labeled)) = mcp_url {
        let selected: Vec<&crate::agents::AgentDef> = mcp_selection
            .iter()
            .filter_map(|k| crate::agents::agent(k))
            .collect();
        crate::agents::wire(cwd, &selected, &url, labeled, SKILL_MD);
        if config.mounts.len() > 1 {
            println!(
                "(this project mounts {} workspaces but uses one MCP connection; the \
                 workspaces the agent can access are selected during browser consent)",
                config.mounts.len()
            );
        }
    }

    // Re-load to prove the round-trip.
    let _ = load_project(cwd)?;
    Ok(0)
}

/// Parse + validate the `--mcp` intent — PURE, called before any write. The prompt itself is
/// deferred to the caller (it needs the detection scan), but whether prompting is allowed at
/// all is decided here: `allow_prompt` gates it, and a non-TTY stdin downgrades to the hint.
fn plan_mcp(args: &InitArgs) -> Result<McpPlan> {
    use std::io::IsTerminal;
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
                        "unknown agent {k:?} in --mcp — valid: {}",
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
            if args.allow_prompt && std::io::stdin().is_terminal() {
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
    use std::io::Write;
    println!(
        "\nAdd this project's docli MCP connection to coding-agent configurations? (No \
         changes are made unless you confirm.)"
    );
    if detected.is_empty() {
        println!("  detected here: none");
    } else {
        println!("  detected here: {}", detected.join(", "));
    }
    println!(
        "  [Enter = detected agents / n = skip / comma-separated list of: {}]",
        crate::agents::AGENTS
            .iter()
            .map(|a| a.key)
            .collect::<Vec<_>>()
            .join(", ")
    );
    print!("> ");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    let line = line.trim();
    if line.eq_ignore_ascii_case("n") || line.eq_ignore_ascii_case("no") {
        return Ok(None);
    }
    if line.is_empty() {
        return Ok(Some(detected.to_vec()));
    }
    let mut keys = Vec::new();
    for k in line.split(',').map(str::trim).filter(|k| !k.is_empty()) {
        match crate::agents::agent(k) {
            Some(def) => keys.push(def.key),
            None => println!("  (skipping unknown agent {k:?})"),
        }
    }
    Ok(Some(keys))
}

fn config_header() -> &'static str {
    "# docli.toml — the mount table for the docli CLI (committed; names workspaces, grants nothing).\n\
     # Mounts are keyed by stable workspace IDs, never by @handles (which can be renamed).\n\
     # The mirror dirs and .docli/ must be git-ignored; only this file is committed.\n\n"
}

#[cfg(test)]
mod tests {
    use super::*;

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
            dir: Some(".".into()), // would contain docli.toml — the control-plane rule
            folder: None,
            name: None,
            server: None,
            mcp: None,
            mcp_label: None,
            mcp_bare: false,
            allow_prompt: false,
        };
        let err = run(tmp.path(), None, &args).unwrap_err().to_string();
        assert!(err.contains("docli.toml or .docli"), "{err}");
    }
}
