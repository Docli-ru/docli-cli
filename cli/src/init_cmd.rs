// SPDX-FileCopyrightText: 2026 OOO Agitek
// SPDX-License-Identifier: MIT

//! `docli init` (v0.28.0 goal 5 + D12) — create/extend `docli.toml`, offer the gitignore
//! lines, drop the bundled SKILL.md teaching agents the contract, and — opt-in only — wire
//! agent MCP configs (`agents.rs`). The SKILL.md goes to the Agent Skills open-standard path
//! `.agents/skills/` unconditionally, and additionally to any per-agent path in the picker
//! table. Which agents actually READ the standard path is a per-vendor fact that has to be
//! verified, not assumed: Claude Code does not (corrected 2026-09-01 — it reads
//! `.claude/skills/`), and the rest of the table's claims are unverified.
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

/// The frontmatter `description:` value, and NOTHING after it.
///
/// v0.28.6 D12: the first version of this took «everything between `description:` and the end of
/// frontmatter», which is a slice that silently GROWS as keys are added below it — a
/// `when_to_use:` block would have sat inside it and kept `description.contains("докли")` green
/// while the trigger had actually been moved out of the field a skill fires on. A pin that
/// measures more than it claims to is worse than no pin, so the extractor stops at the next
/// frontmatter key.
///
/// YAML folded/indented continuation lines belong to the value and are kept.
pub fn skill_description(skill_md: &str) -> Option<String> {
    let body = skill_md.strip_prefix("---\n")?;
    let front = &body[..body.find("\n---")?];
    let mut lines = front.lines();
    let first = lines
        .by_ref()
        .find_map(|l| l.strip_prefix("description:"))?;
    let mut out = first.trim().to_string();
    for line in lines {
        // A new key at column zero ends the value; an indented line continues it.
        let is_key = line
            .split_once(':')
            .is_some_and(|(k, _)| !k.is_empty() && !k.starts_with(char::is_whitespace));
        if is_key || line.trim().is_empty() {
            break;
        }
        out.push(' ');
        out.push_str(line.trim());
    }
    Some(out)
}

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
    /// Which agents get the mirror contract in their OWN skills directory (v0.28.6 D4):
    /// `auto` (detected here — the default), `none`, or a comma list of agent keys.
    ///
    /// **Deliberately not tied to `--mcp`.** The skill used to ride along with the MCP wiring,
    /// which meant `--mcp none` delivered the contract to `.agents/skills/` ONLY — the one path
    /// Claude Code does not read, which is the entire defect this slice exists to fix. It is
    /// also a lighter act than the others here: `init` already drops the same file at the
    /// open-standard path unconditionally, so a copy into an agent's own skills directory is
    /// the same class of write, which is why this one defaults to `auto` while hooks and
    /// instruction files do not.
    pub skills: Option<String>,
    /// Which of the two hook-capable agents get hooks (v0.28.6 D6): `none` (the default),
    /// `auto` (detected here), or a comma list of `claude`/`codex`.
    ///
    /// Offered UNTICKED interactively and never written under `--no-input` without this flag: a
    /// config entry names a server, a hook runs a program. Different acts, different defaults.
    pub hooks: Option<String>,
    /// Write the `AGENTS.md` section, and a `CLAUDE.md` importing it when none exists (D5).
    /// Never edits an existing `CLAUDE.md`.
    pub instructions: bool,
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

    // `--workspace` without `--dir` means the machine cache — the normal case now, and the
    // reason `--dir` stopped being mandatory: there is a correct default and it is not the
    // project's business. An empty `dir` is what `resolve_mount_dirs` fills in at load.
    let effective_dir = args
        .dir
        .clone()
        .or_else(|| args.workspace.map(|_| String::new()));
    match (&args.workspace, &effective_dir) {
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
                    derived_dir: false,
                    workspace_label: String::new(),
                }),
            }
            // The MOUNT-TABLE rules gate the ADD too, not only the commands that later read the
            // table. `init` is the command that WRITES mounts, so a table it authors and every
            // other verb then refuses is the worst arrangement available: the refusal arrives
            // from a different command than the one that caused it, and names none. Cheapest
            // example: `--name <another mount's workspace uuid>`, which a scripted setup pasting
            // ids around produces naturally.
            // Resolve BEFORE validating: an omitted `dir` is not yet a path, and every rule
            // below — the display name, the geometry, the ignore gate — asks about one.
            crate::config::resolve_mount_dirs(&mut config)?;
            crate::config::validate_config(&config)?;
            // …then the geometry rules, MINUS the ignore rule, which `--gitignore` is about to
            // satisfy. Everything else refuses here, before anything at all is written.
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
        _ => bail!("--dir needs --workspace to say which mount it is for"),
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
    // Resolved here, with the rest of the intent, so a bad value refuses over an UNTOUCHED
    // tree rather than a half-initialized one (the F9 pin, extended to the new flags).
    let detected = || crate::agents::detect(cwd, std::env::home_dir().as_deref());
    let skills_selection: Vec<&'static str> = match args.skills.as_deref().map(str::trim) {
        Some("none") | Some("") => Vec::new(),
        // Absent means AUTO — see `InitArgs::skills` for why this one default differs from the
        // heavier writes beside it.
        None | Some("auto") => detected(),
        Some(list) => parse_agent_list(list, "--skills")?,
    };
    let hook_agents: Vec<crate::hooks::HookAgent> = match args.hooks.as_deref().map(str::trim) {
        None | Some("none") | Some("") => Vec::new(),
        Some("auto") => {
            let d = detected();
            crate::hooks::HookAgent::all()
                .into_iter()
                .filter(|a| d.contains(&a.key()))
                .collect()
        }
        Some(list) => {
            let mut out = Vec::new();
            for k in list.split(',').map(str::trim).filter(|k| !k.is_empty()) {
                let a = crate::hooks::HookAgent::parse(k).map_err(|e| {
                    anyhow::anyhow!("--hooks: {e} (only these two agents have a hook mechanism)")
                })?;
                if !out.contains(&a) {
                    out.push(a);
                }
            }
            out
        }
    };
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

    // A DERIVED dir is never written back: `docli.toml` is committed and shared, so baking this
    // machine's home directory into it would make the file wrong on every other machine — and
    // the whole reason the default exists is that the path is not the project's business.
    //
    // SCOPED, deliberately. An earlier draft bound this copy to `config`, shadowing it for the
    // rest of the function — so every later step (`missing_ignores`, the gitignore report) saw
    // blanked dirs, resolved a mount to the PROJECT ROOT, and asked git to check-ignore the
    // repository against itself. The suppression is a serialization detail and must not outlive
    // the serialization.
    let body = {
        let mut for_disk = config.clone();
        for m in &mut for_disk.mounts {
            if m.derived_dir {
                m.dir = String::new();
            }
        }
        toml::to_string_pretty(&for_disk).context("serializing docli.toml")?
    };
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
    }
    if let Some((url, labeled)) = mcp_url {
        let selected: Vec<&crate::agents::AgentDef> = mcp_selection
            .iter()
            .filter_map(|k| crate::agents::agent(k))
            .collect();
        crate::agents::wire(cwd, &selected, &url, labeled);
        if config.mounts.len() > 1 {
            crate::ui::detail(&format!(
                "This project mounts {} but uses ONE MCP connection: which of them the agent \
                 can reach is chosen during the browser consent.",
                crate::ui::plural(config.mounts.len(), "workspace", "workspaces")
            ));
        }
    }

    // The contract into each selected agent's OWN skills directory — INDEPENDENT of whether any
    // MCP config was written (D4). The activation globs come from the mount table, so this is a
    // template rather than a copy.
    let skill_agents: Vec<&crate::agents::AgentDef> = skills_selection
        .iter()
        .filter_map(|k| crate::agents::agent(k))
        .collect();
    if !skill_agents.is_empty() {
        crate::agents::install_skills(
            cwd,
            &skill_agents,
            SKILL_MD,
            &crate::agents::skill_globs(cwd, &config.mounts),
        );
    }

    // Hooks last, and only where they were explicitly asked for (D6).
    if !hook_agents.is_empty() {
        crate::ui::heading("Hooks");
        // The hook files go through the SAME atomic writer as the MCP configs, so a crashed
        // earlier run leaves the same `.docli-cfg-*.tmp` residue beside them. `wire`'s sweep
        // covers it only when MCP wiring happened at all — `docli init --mcp none --hooks
        // claude` writes into `.claude/` with nothing to clean it. An empty selection sweeps
        // exactly the hook directories.
        crate::agents::sweep_cfg_temps(cwd, &[]);
        for line in crate::hooks::consent_summary(&hook_agents) {
            crate::ui::detail(&line);
        }
        for agent in &hook_agents {
            crate::hooks::install(cwd, *agent)?;
        }
        // D9: enforcement language appears ONLY for agents that got a hook, and it states the
        // limit in the same breath. Every other agent is advisory, and a user who wired Cursor
        // and read «the mirror is protected» would be wrong in exactly the way that costs a note.
        crate::ui::detail(
            "docli will now refuse writes into the mirror from these agents' file-editing \
             tools. Shell writes (`sed -i`, `>`) are NOT covered, on either agent, and no other \
             agent gets enforcement at all - they get the contract as advice.",
        );
    }
    if args.instructions {
        crate::ui::heading("Instructions");
        crate::instructions::install(cwd)?;
    }

    // The next command, LAST — v0.28.6 Step 1. It used to be printed before the agent wiring,
    // so «→ Sync: docli sync» arrived in the middle of the screen with three more sections of
    // writing after it, which reads as «and then some other stuff happened».
    if !config.mounts.is_empty() {
        crate::ui::next(&format!("Sync: {}", crate::ui::cmd("docli sync")));
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

/// A comma list of agent keys, refused (never silently dropped) on an unknown one.
fn parse_agent_list(list: &str, flag: &str) -> Result<Vec<&'static str>> {
    let mut keys = Vec::new();
    for k in list.split(',').map(str::trim).filter(|k| !k.is_empty()) {
        match crate::agents::agent(k) {
            Some(def) => {
                if !keys.contains(&def.key) {
                    keys.push(def.key);
                }
            }
            None => bail!(
                "unknown agent {k:?} in {flag} - valid: {}",
                crate::agents::AGENTS
                    .iter()
                    .map(|a| a.key)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }
    Ok(keys)
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
    "# docli.toml - the mount table for docli-cli (committed; names workspaces, grants nothing).\n\
     # Mounts are keyed by stable workspace IDs, never by @handles (which can be renamed).\n\
     # The mirror dirs and .docli/ must be git-ignored; only this file is committed.\n\n"
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `init` is the command that WRITES mounts, so it must ask the same door every reader
    /// asks. A table it authors and every other verb then refuses is the worst arrangement
    /// available: the refusal arrives from a different command than the one that caused it.
    /// `docli init` in a GIT REPOSITORY with the default (derived) mount, writing the agent
    /// files — the ordinary first run, and the one no test covered.
    ///
    /// 0.1.6 shipped broken here: the copy that blanks a derived `dir` before serializing was
    /// bound to `config`, shadowing it for the rest of the function, so the later gitignore
    /// report resolved a mount to the PROJECT ROOT and asked git to check-ignore the repository
    /// against itself — exit 128, «cannot verify», fail closed. Every existing init test used a
    /// non-git tempdir, so the whole ignore path was untested.
    #[test]
    fn init_in_a_git_repository_with_the_default_mount_succeeds() {
        let tmp = tempfile::tempdir().unwrap();
        // The fake home is its OWN tempdir, outside the repository — a real `~/.docli` never
        // sits inside the project, and putting it there would test the wrong thing.
        let home = tempfile::tempdir().unwrap();
        let _lock = crate::creds::home_env_lock();
        std::env::set_var("DOCLI_HOME", home.path().join(".docli"));
        struct Restore;
        impl Drop for Restore {
            fn drop(&mut self) {
                std::env::remove_var("DOCLI_HOME");
            }
        }
        let _restore = Restore;
        std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(tmp.path())
            .status()
            .unwrap();
        let args = InitArgs {
            workspace: Some(Uuid::from_u128(7)),
            // No `--dir`: the machine cache, which is the shipped default.
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
            skills: Some("none".into()),
            hooks: None,
            instructions: true,
        };
        run(tmp.path(), None, &args).expect("init must succeed in a git repository");
        // Run twice: the second pass re-points an existing mount, which is the path that
        // reached the blanked config first.
        run(tmp.path(), None, &args).expect("a re-run must succeed too");
        // …and the committed file still carries no machine path.
        let body = std::fs::read_to_string(tmp.path().join("docli.toml")).unwrap();
        assert!(
            !body.contains("dir ="),
            "a derived dir must not be written: {body}"
        );
        assert!(body.contains(&Uuid::from_u128(7).to_string()), "{body}");
    }

    #[test]
    fn init_refuses_a_mount_table_the_readers_would_refuse() {
        let tmp = tempfile::tempdir().unwrap();
        let a = Uuid::from_u128(1);
        let b = Uuid::from_u128(2);
        let base = |ws: Uuid, dir: &str, name: Option<&str>| InitArgs {
            workspace: Some(ws),
            dir: Some(dir.into()),
            folder: None,
            name: name.map(str::to_string),
            server: Some("https://docli.ru".into()),
            mcp: None,
            mcp_label: None,
            mcp_bare: false,
            allow_prompt: false,
            clear_folder: false,
            write_gitignore: false,
            skills: Some("none".into()),
            hooks: None,
            instructions: false,
        };
        run(tmp.path(), None, &base(a, "m1", None)).unwrap();
        // Naming a mount with ANOTHER mount's workspace id: one string, two mounts, across every
        // surface that prints a mount tag.
        let e = run(tmp.path(), None, &base(b, "m2", Some(&a.to_string())))
            .expect_err("the collision must be refused where it is authored");
        assert!(format!("{e:#}").contains("workspace id"), "{e:#}");
        // …and a blank name, which would be printed as the tag and accepted by nothing.
        let e = run(tmp.path(), None, &base(b, "m2", Some("   ")))
            .expect_err("a blank name must be refused too");
        assert!(format!("{e:#}").contains("blank display name"), "{e:#}");
        // The refused mounts are NOT written: the door runs before anything lands.
        let p = load_project(tmp.path()).unwrap();
        assert_eq!(p.config.mounts.len(), 1);
        assert_eq!(p.config.mounts[0].workspace, a);
    }

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
            skills: Some("none".into()),
            hooks: None,
            instructions: false,
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
            skills: Some("none".into()),
            hooks: None,
            instructions: false,
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
            skills: Some("none".into()),
            hooks: None,
            instructions: false,
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
            skills: Some("none".into()),
            hooks: None,
            instructions: false,
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
        // Moved with the wording in v0.29.1's editorial pass — see the pin list below for why.
        assert!(SKILL_MD.contains("only a server search that does not report an incomplete"));
        assert!(SKILL_MD.contains("sync --check"));
        // D12.3 — the write-discipline paragraph ships in the contract.
        assert!(SKILL_MD.contains("prefer `edit_note`"));
        assert!(SKILL_MD.contains("conflictSiblingId"));
        // 2026-09-01 — a skill fires on its `description` (and, on Claude Code, on the `paths`
        // globs injected at copy time — D4, which is why the description is now the FALLBACK
        // rather than the only door). Two things still have to be IN the description.
        let description = skill_description(SKILL_MD).expect("frontmatter description");
        let description = description.as_str();
        // (a) The Russian spelling. «докли» is the product's PRIMARY name, so a request is at
        // least as likely to say it as `docli`; an all-Latin description matches neither the
        // user's words nor the brand rule.
        assert!(
            description.contains("докли"),
            "the description must match the Russian spelling: {description}"
        );
        // (b) The surfaces that identify this project as a mirror. A description that only
        // names the CONTRACT makes the model leap from \"find my note about X\" to \"docli
        // read-only mirror\" unaided.
        for marker in ["docli.toml", ".docli/mirror/", ".docli"] {
            assert!(
                description.contains(marker),
                "the description must name {marker}: {description}"
            );
        }
        // …and the extractor measures what it claims to: a key added BELOW `description` must
        // not join the slice and keep the pins above green while the trigger moved out of it.
        let with_extra = SKILL_MD.replacen(
            "\nallowed-tools:",
            "\nwhen_to_use: |\n  найди в докли docli.toml docli-mirror/ .docli\nallowed-tools:",
            1,
        );
        let d = skill_description(&with_extra).expect("still parses");
        assert!(
            !d.contains("when_to_use") && !d.contains("найди"),
            "the description slice must stop at the next key: {d}"
        );
    }

    #[test]
    fn the_shared_asset_carries_no_qualified_tool_name() {
        // D12: an earlier draft directed every mention to `docli:edit_note`, citing
        // cross-product guidance. That would have shipped a WRONG contract into the one asset
        // this slice exists to fix — Claude Code's MCP namespace is `mcp__<server>__<tool>`
        // (observed: the live tools are literally `mcp__docli__edit_note`), so `docli:edit_note`
        // matches neither the bare name nor the real qualified one, and the qualified form is
        // platform-specific besides, while this asset must read correctly on Codex too. Bare
        // names with the server named in prose is what demonstrably resolves.
        assert!(
            !SKILL_MD.contains("docli:"),
            "no `docli:`-qualified tool names in the shared asset"
        );
        assert!(
            SKILL_MD.contains("docli MCP connection"),
            "the server is named in prose"
        );
    }

    #[test]
    fn the_asset_frontmatter_stays_inside_the_agent_skills_six() {
        // `.agents/skills/` is the open-standard path, where an out-of-set key is a HARD
        // packaging error, not an ignored field. Claude-Code-only keys are injected per
        // destination by `agents::copy_skill` (D4) and must never be written into the asset.
        const SPEC_FIELDS: [&str; 6] = [
            "name",
            "description",
            "license",
            "compatibility",
            "metadata",
            "allowed-tools",
        ];
        let body = SKILL_MD.strip_prefix("---\n").expect("frontmatter");
        let front = &body[..body.find("\n---").expect("frontmatter ends")];
        for line in front.lines() {
            let Some((key, _)) = line.split_once(':') else {
                continue;
            };
            if key.starts_with(char::is_whitespace) || key.is_empty() {
                continue; // a continuation line, not a key
            }
            assert!(
                SPEC_FIELDS.contains(&key),
                "`{key}` is outside the Agent Skills six and would fail packaging: {line}"
            );
        }
    }

    #[test]
    fn the_body_is_written_as_standing_rules_not_a_session_start_procedure() {
        // D12: Claude Code does not re-read the skill file on later turns, so a document that
        // opens with «At session start: 1. Run …» is a one-time procedure living in a file
        // loaded once, mid-task. The rewrite states rules that hold for the whole task — and
        // the freshness PROCEDURE now has a mechanism that can actually run it (the
        // SessionStart hook, D3).
        assert!(!SKILL_MD.contains("At session start"), "the wrong mood");
        // One term throughout: the body called the same thing a «mirror» and a «cache for
        // reading», and inconsistent terminology is called out by the authoring guidance.
        assert!(!SKILL_MD.contains("cache for reading"));
        // The five body pins, preserved VERBATIM through the rewrite rather than deleted to go
        // green (SPEC §7's failing-test rule).
        for pin in [
            "never synced and never protected",
            // v0.29.1's editorial pass moved this pin DELIBERATELY, from «only a non-degraded
            // server search». «Non-degraded» is our word for the condition; the human output
            // says the note index was «incomplete» and only `--json` says `degraded`, so the
            // old phrasing asked the reader to translate an adjective they never see. The RULE
            // is unchanged and still pinned — only its wording names the observable now.
            "only a server search that does not report an incomplete",
            "sync --check",
            "prefer `edit_note`",
            "conflictSiblingId",
        ] {
            assert!(SKILL_MD.contains(pin), "the rewrite must preserve: {pin}");
        }
        // …and v0.29.1's three, pinned the same way. Each is a rule whose ABSENCE is the defect:
        // an agent that learns the mirror is grep-able looks for a path we no longer publish; one
        // that reads exit 3 as absence draws the conclusion only `docli search` may draw; and one
        // that reads an empty graph field as an empty graph gets D5's false negative inside the
        // verb built to replace grep.
        for pin in [
            "publishes **no local mirror path** for its results",
            "exits 3 when this mirror does not hold the requested note or file",
            "never an empty list",
        ] {
            assert!(SKILL_MD.contains(pin), "the contract must carry: {pin}");
        }
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
            skills: Some("none".into()),
            hooks: None,
            instructions: false,
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
            skills: Some("none".into()),
            hooks: None,
            instructions: false,
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
            skills: Some("none".into()),
            hooks: None,
            instructions: false,
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
            skills: Some("none".into()),
            hooks: None,
            instructions: false,
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
            skills: Some("none".into()),
            hooks: None,
            instructions: false,
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
            skills: Some("none".into()),
            hooks: None,
            instructions: false,
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
            skills: Some("none".into()),
            hooks: None,
            instructions: false,
        };
        run(tmp.path(), None, &args).unwrap();
        for f in [".mcp.json", ".codex/config.toml", ".gemini/settings.json"] {
            assert!(!tmp.path().join(f).exists(), "{f} must not exist");
        }
    }

    /// The v0.28.6 baseline: a mounted project, no MCP wiring at all.
    fn mounted(skills: Option<&str>, hooks: Option<&str>, instructions: bool) -> InitArgs {
        InitArgs {
            workspace: Some(Uuid::from_u128(41)),
            dir: Some("docli-mirror/notes".into()),
            folder: None,
            name: None,
            server: Some("https://docli.ru".into()),
            mcp: Some("none".into()),
            mcp_label: None,
            mcp_bare: false,
            allow_prompt: false,
            clear_folder: false,
            write_gitignore: false,
            skills: skills.map(str::to_string),
            hooks: hooks.map(str::to_string),
            instructions,
        }
    }

    #[test]
    fn the_skill_reaches_claude_code_even_when_no_mcp_config_is_written() {
        // THE defect this slice exists to fix (D4). `agents::wire` was the only writer of
        // `.claude/skills/` and it ran only inside `if let Some(url) = mcp_url` — so
        // `docli init --mcp none`, or a guided run where the user ticks no agent, delivered the
        // contract to `.agents/skills/` ONLY: the one path Claude Code does not read.
        let tmp = tempfile::tempdir().unwrap();
        run(tmp.path(), None, &mounted(Some("claude"), None, false)).unwrap();
        let skill = tmp.path().join(".claude/skills/docli-mirror/SKILL.md");
        assert!(
            skill.exists(),
            "the contract must not be hostage to the MCP offer"
        );
        // …and it carries the ACTIVATION globs, derived from the mount table rather than
        // guessed, so a `--dir` the user chose is not silently inert.
        let body = fs::read_to_string(&skill).unwrap();
        assert!(
            body.contains(r#"paths: ["docli-mirror/notes/**"]"#),
            "{body}"
        );
        // No MCP config was written at all.
        assert!(!tmp.path().join(".mcp.json").exists());
    }

    #[test]
    fn no_input_writes_no_hooks_unless_the_flag_names_agents() {
        // D6: a hook runs a program, so it never rides along with anything else.
        let tmp = tempfile::tempdir().unwrap();
        run(tmp.path(), None, &mounted(Some("none"), None, false)).unwrap();
        for f in [".claude/settings.json", ".codex/hooks.json"] {
            assert!(!tmp.path().join(f).exists(), "{f} must not exist");
        }
        // …and `--hooks none` is the same.
        run(
            tmp.path(),
            None,
            &mounted(Some("none"), Some("none"), false),
        )
        .unwrap();
        assert!(!tmp.path().join(".claude/settings.json").exists());
        // An unknown key REFUSES rather than being silently dropped, and only the two agents
        // with a hook mechanism are nameable.
        let err = run(
            tmp.path(),
            None,
            &mounted(Some("none"), Some("cursor"), false),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("--hooks"), "{err}");
        assert!(!tmp.path().join(".claude/settings.json").exists());
    }

    #[test]
    fn hooks_and_instructions_are_written_on_request_and_are_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let args = mounted(Some("none"), Some("claude,codex"), true);
        run(tmp.path(), None, &args).unwrap();
        let settings = tmp.path().join(".claude/settings.json");
        let codex = tmp.path().join(".codex/hooks.json");
        assert!(settings.exists() && codex.exists());
        let before: Vec<String> = [&settings, &codex, &tmp.path().join("AGENTS.md")]
            .iter()
            .map(|p| fs::read_to_string(p).unwrap())
            .collect();

        // «Run twice, diff» — no duplicate hook entries, no duplicated AGENTS.md section.
        run(tmp.path(), None, &args).unwrap();
        let after: Vec<String> = [&settings, &codex, &tmp.path().join("AGENTS.md")]
            .iter()
            .map(|p| fs::read_to_string(p).unwrap())
            .collect();
        assert_eq!(
            before, after,
            "init must be idempotent across the new surfaces"
        );
        let v: serde_json::Value = serde_json::from_str(&after[0]).unwrap();
        assert_eq!(v["hooks"]["PreToolUse"].as_array().unwrap().len(), 1);
        assert_eq!(v["hooks"]["SessionStart"].as_array().unwrap().len(), 1);
        // The CLAUDE.md bridge was created because there was nothing to damage.
        assert_eq!(
            fs::read_to_string(tmp.path().join("CLAUDE.md")).unwrap(),
            "@AGENTS.md\n"
        );
    }

    #[test]
    fn an_existing_claude_md_is_never_touched_by_init() {
        // The COMMON case, this repository included: D5 delivers automatically for the
        // AGENTS.md readers and BY INSTRUCTION for Claude Code.
        let tmp = tempfile::tempdir().unwrap();
        let mine = "# Careful instructions\n";
        fs::write(tmp.path().join("CLAUDE.md"), mine).unwrap();
        run(tmp.path(), None, &mounted(Some("none"), None, true)).unwrap();
        assert_eq!(
            fs::read_to_string(tmp.path().join("CLAUDE.md")).unwrap(),
            mine
        );
        assert!(tmp.path().join("AGENTS.md").exists());
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
            skills: Some("none".into()),
            hooks: None,
            instructions: false,
        };
        let err = run(tmp.path(), None, &args).unwrap_err().to_string();
        assert!(err.contains("docli.toml or .docli/ directory"), "{err}");
    }
}
