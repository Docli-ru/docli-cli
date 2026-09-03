// SPDX-FileCopyrightText: 2026 OOO Agitek
// SPDX-License-Identifier: MIT

//! The interactive `docli init` journey (0.1.1).
//!
//! `docli init` used to be a flag-driven command that assumed you already knew a workspace
//! UUID, already knew which agent configurations you wanted rewritten, and already had the
//! right `.gitignore` lines. Every one of those is something the CLI can find out or offer.
//!
//! Three rules the flow keeps:
//!
//! * **Nothing is written before the last question is answered.** Each step only collects; the
//!   plan is applied at the end. Ctrl-C at any prompt leaves the tree untouched.
//! * **Detection pre-selects, it never decides.** The agent step arrives with the detected
//!   configurations ticked, and the reader unticks what they don't want — the 0.1.0 flow wrote
//!   all five detected configs on a bare Enter, which is what the complaint was about.
//! * **No UUIDs.** Workspaces are chosen from a list of `@handle — Название`; the id is what
//!   gets persisted, never what gets typed.
//!
//! Non-interactive callers are unaffected: the wizard runs only when `docli init` is given no
//! mount/MCP intent AND both ends of the terminal are attended (`ui::interactive`), so a
//! scripted `docli init --workspace … --dir …` and an agent's piped invocation both keep the
//! flag path.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use console::style;
use dialoguer::theme::ColorfulTheme;
use dialoguer::{Confirm, Input, MultiSelect, Select};

use crate::config::{self, DocliToml, Mount};
use crate::http::{Api, WorkspaceInfo};
use crate::{creds, init_cmd, login, ui};

const STEPS: usize = 7;

/// What the wizard decided, ready to apply.
pub struct Plan {
    pub server: String,
    pub workspace: WorkspaceInfo,
    pub dir: String,
    pub folder: Option<String>,
    /// The reader agreed to the `.gitignore` lines. The WRITE happens inside `init_cmd`, after
    /// the whole configuration validates — writing here covered only the selected mount and
    /// left a partial edit behind when another mount then failed the gate.
    pub gitignore: bool,
    /// The reader emptied a scope that was set: `init_cmd` must be told to CLEAR it, since an
    /// absent `--folder` there means «leave what is recorded».
    pub clear_folder: bool,
    pub agents: Vec<&'static str>,
    /// Which of the two hook-capable agents the reader agreed to install hooks for (v0.28.6 D6).
    /// Offered UNTICKED, unlike the detected agent configurations: a config entry names a
    /// server, a hook runs a program.
    pub hooks: Vec<crate::hooks::HookAgent>,
    /// The reader agreed to the `AGENTS.md` section (and a `CLAUDE.md` importing it, when there
    /// is none to damage — D5).
    pub instructions: bool,
    pub sync_now: bool,
}

/// The prompt theme, degraded to ASCII where the terminal cannot render the default symbols.
///
/// `ColorfulTheme::default()` is Unicode throughout — `✔`, `✗`, `❯`, `☑`, `☐` — so a terminal
/// without UTF-8 sees the questions themselves in mojibake, which is worse than seeing them
/// plain. The gate is the same [`ui::unicode`] the rest of the output uses.
pub fn prompt_theme() -> ColorfulTheme {
    let t = ColorfulTheme::default();
    if ui::unicode() {
        return t;
    }
    ColorfulTheme {
        prompt_prefix: style("?".to_string()).for_stderr().yellow(),
        success_prefix: style("+".to_string()).for_stderr().green(),
        error_prefix: style("x".to_string()).for_stderr().red(),
        active_item_prefix: style(">".to_string()).for_stderr().cyan(),
        inactive_item_prefix: style(" ".to_string()).for_stderr(),
        checked_item_prefix: style("[x]".to_string()).for_stderr().green(),
        unchecked_item_prefix: style("[ ]".to_string()).for_stderr(),
        picked_item_prefix: style(">".to_string()).for_stderr().green(),
        unpicked_item_prefix: style(" ".to_string()).for_stderr(),
        // `..t` would carry ColorfulTheme's own Unicode `›` and `·` through, which is exactly
        // the leak this function exists to close.
        prompt_suffix: style(":".to_string()).for_stderr().black().bright(),
        success_suffix: style(":".to_string()).for_stderr().black().bright(),
        ..t
    }
}

/// The mirror directory offered for a workspace: `docli-mirror/<handle>`.
///
/// One parent for every mount means ONE `.gitignore` entry no matter how many workspaces get
/// mounted later — the mirror cannot live under `.docli/` (the control plane refuses the
/// inverse containment), so a shared visible parent is the next best thing. Visible, not
/// dotted: coding agents have to be able to list it.
/// The one directory the CLI proposes for mirrors. Named, because the `.gitignore` helper
/// hoists to it and must hoist to nothing else.
/// The mirror lives INSIDE the control plane (`.docli/mirror/<name>`), not beside it.
///
/// One gitignored directory instead of two, one thing `uninstall --purge` removes, and the
/// cache stops sitting in the project root where `ls`, a file tree or an IDE sidebar puts it in
/// front of anyone who never asked. v0.29.1 D1 already removed the reason anyone would be
/// HANDED a path into it; this removes the reason they would trip over one.
pub const MIRROR_PARENT: &str = ".docli/mirror";

/// The per-MACHINE cache for a workspace: `~/.docli/mirror/<workspace-id>`.
///
/// Keyed on the ID, not the handle — a project links several workspaces and several projects
/// link the same workspace, so the cache belongs to the WORKSPACE, and handles rename while ids
/// do not. Shown to the user as the default; accepting it stores NOTHING in `docli.toml`, which
/// is what keeps a committed file free of this machine's home directory.
pub fn machine_cache_dir(ws: uuid::Uuid) -> Option<String> {
    crate::creds::cli_home().ok().map(|h| {
        h.join("mirror")
            .join(ws.to_string())
            .to_string_lossy()
            .into_owned()
    })
}

pub fn default_dir(handle: &str) -> String {
    let h = handle.trim_start_matches('@').trim();
    let safe: String = h
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let safe = safe.trim_matches('-');
    if safe.is_empty() {
        format!("{MIRROR_PARENT}/workspace")
    } else {
        format!("{MIRROR_PARENT}/{safe}")
    }
}

/// Which of the two entries a project still needs in `.gitignore`.
/// One `.gitignore` line that is still missing, and the file it belongs in.
///
/// The FILE matters: a mount inside a vendored repository (`vendor/repo/cache`, with
/// `vendor/repo/.git`) is governed by that repository's `.gitignore`, and an entry appended to
/// the outer project's file ignores nothing — the guardrail would keep refusing right after the
/// "fix" was applied, having edited the wrong file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IgnoreFix {
    /// Work tree whose `.gitignore` must carry the entry.
    pub worktree: PathBuf,
    /// The pattern, relative to that work tree.
    pub entry: String,
}

impl IgnoreFix {
    /// What to SHOW: the pattern alone in the ordinary case, qualified by its file when the
    /// entry belongs to a different repository than the project root (`vendor/repo`).
    pub fn label(&self, project_root: &Path) -> String {
        if config::physicalize(&self.worktree) == config::physicalize(project_root) {
            self.entry.clone()
        } else {
            format!("{} ({}/.gitignore)", self.entry, self.worktree.display())
        }
    }
}

/// Which `.gitignore` lines this project still needs for `dir` (plus its control directory).
/// The entry for `.docli/`, when the control directory still needs one. Applies with or without
/// a mount — `docli init --gitignore` in a project that has no mount yet would otherwise be a
/// silent no-op, which is the worst possible answer to an explicit request.
pub fn control_ignore(project_root: &Path) -> Result<Option<IgnoreFix>> {
    let control = project_root.join(".docli");
    match config::ignore_state(project_root, &control) {
        config::IgnoreState::NotInRepo | config::IgnoreState::Covered => return Ok(None),
        // PROPAGATED, never swallowed: a caller that cannot tell must say so rather than render
        // «всё в порядке» over a question git refused to answer.
        config::IgnoreState::Unknown(why) => anyhow::bail!(why),
        config::IgnoreState::Missing => {}
    }
    let Some(wt) = config::find_git_worktree(project_root) else {
        return Ok(None);
    };
    Ok(relative_entry(&wt, &control, false).map(|entry| IgnoreFix {
        worktree: wt,
        entry,
    }))
}

pub fn missing_ignores(project_root: &Path, dir: &str) -> Result<Vec<IgnoreFix>> {
    let mut out = Vec::new();
    if let Some(fix) = control_ignore(project_root)? {
        out.push(fix);
    }
    let abs_dir = config::mount_abs(
        project_root,
        &config::Mount {
            workspace: uuid::Uuid::nil(),
            dir: dir.to_string(),
            folder: None,
            name: None,
            derived_dir: false,
            workspace_label: String::new(),
        },
    );
    // PHYSICAL from the first step, like the geometry rules. With `/outside/link -> /repo/sub`
    // and a mount at `/outside/link/cache`, the LEXICAL ancestor walk never meets `/repo/.git`,
    // so both the «does it need an entry» question and the work-tree lookup answered about the
    // wrong tree — the fix vanished while geometry (which physicalizes) went on refusing.
    let phys = config::physicalize(&abs_dir);
    // A mount INSIDE `.docli/` is already covered by the `/.docli/` entry proposed above — which
    // is the default since the mirror moved there. Asking git is the wrong question at this
    // moment: the entry that will cover it has not been WRITTEN yet, so `check-ignore` says
    // «missing» and we would offer a second, redundant line for a path the first one subsumes.
    let control = config::physicalize(&project_root.join(".docli"));
    if config::is_ancestor_or_self(&control, &phys) {
        return Ok(out);
    }
    match config::ignore_state(&phys, &phys) {
        config::IgnoreState::NotInRepo | config::IgnoreState::Covered => {}
        config::IgnoreState::Unknown(why) => anyhow::bail!(why),
        config::IgnoreState::Missing => {
            if let Some(wt) = config::find_git_worktree(&phys) {
                if let Some(entry) = relative_entry(&wt, &phys, true) {
                    out.push(IgnoreFix {
                        worktree: wt,
                        entry,
                    });
                }
            }
        }
    }
    Ok(out)
}

/// The pattern for `path` inside work tree `wt`. Git reads a leading slash as «from the
/// repository root», so an absolute spelling like `/repo/cache/` matches nothing — the entry is
/// always work-tree-relative.
///
/// `hoist` allows the parent to stand in for the child, but ONLY when the parent is the
/// directory this CLI itself proposes (`docli-mirror/<handle>`): one line then covers every
/// workspace mounted later, which is why the default nests. Hoisting anything else —
/// `src/docli-mirror` to `src/` — would ignore the project's whole source tree, a far bigger
/// promise than the one being made.
fn relative_entry(worktree: &Path, path: &Path, hoist: bool) -> Option<String> {
    let wt = config::physicalize(worktree);
    let p = config::physicalize(path);
    let rel = p.strip_prefix(&wt).ok()?;
    // Separator normalization is a WINDOWS concern: on unix `\` is an ordinary character in a
    // file name, and rewriting it to `/` produced a pattern for a directory that does not exist.
    let rel = rel.to_string_lossy().to_string();
    let rel = if cfg!(windows) {
        rel.replace('\\', "/")
    } else {
        rel
    };
    let rel = rel.trim_matches('/');
    if rel.is_empty() {
        return None;
    }
    let target = match rel.split_once('/') {
        Some((MIRROR_PARENT, _)) if hoist => MIRROR_PARENT,
        _ => rel,
    };
    Some(format!("/{}/", escape_pattern(target)))
}

/// A `.gitignore` line for a LITERAL path, anchored and escaped.
///
/// Both halves are load-bearing. **Anchored**: a bare `cache/` matches `cache` at ANY depth, so
/// ignoring a top-level mirror silently hid every unrelated `src/cache/` in the project — a
/// leading `/` makes the pattern mean this directory and no other. **Escaped**: git reads `#` as
/// a comment and `!` as a negation at the start of a line, and `*`, `?`, `[` as globs anywhere,
/// so a mount called `#notes` produced a line git ignored entirely and the gate kept refusing
/// after the fix. (The leading `/` already defuses `#`/`!`; they are escaped anyway, because the
/// pattern's correctness should not depend on another part of the string.)
fn escape_pattern(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for c in path.chars() {
        if matches!(c, '#' | '!' | '*' | '?' | '[' | ']' | '\\' | ' ') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Append the missing entries to `.gitignore`, creating it if absent. Only ever called after
/// an explicit confirmation — `.gitignore` is the user's file, and 0.1.0's rule was to never
/// touch it. Consent changes that, nothing else does.
pub fn append_ignores(fixes: &[IgnoreFix]) -> Result<()> {
    use std::io::Write;
    // Group by the file each entry belongs to: a project can legitimately need lines in two
    // different `.gitignore`s at once (its own, and a vendored repository's).
    let mut by_worktree: std::collections::BTreeMap<PathBuf, Vec<&str>> =
        std::collections::BTreeMap::new();
    for f in fixes {
        by_worktree
            .entry(f.worktree.clone())
            .or_default()
            .push(&f.entry);
    }
    for (worktree, entries) in by_worktree {
        let p = worktree.join(".gitignore");
        let existing = std::fs::read_to_string(&p).unwrap_or_default();
        let mut block = String::new();
        if !existing.is_empty() && !existing.ends_with('\n') {
            block.push('\n');
        }
        block.push_str(
            "\n# docli - the mirror and its control directory (only docli.toml is committed)\n",
        );
        for e in entries {
            block.push_str(e);
            block.push('\n');
        }
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&p)
            .with_context(|| format!("opening {}", p.display()))?;
        f.write_all(block.as_bytes())
            .with_context(|| format!("appending to {}", p.display()))?;
    }
    Ok(())
}

/// The guardrail's ERGONOMIC half: when the only thing standing between the reader and a sync
/// is two lines in `.gitignore`, offer to write them right here instead of refusing and making
/// them go edit a file and re-run.
///
/// The refusal itself stays — a mirror of someone else's notes inside a git work tree is one
/// `git add -A` away from a remote, and that is not a warning-class harm. But `.gitignore` is
/// the USER's file, so it is only ever written with consent (the established rule: modifying
/// configuration that is not your program's requires asking and saying exactly what you do),
/// and consent is only asked at a terminal. Piped, in CI, or under `--no-input`, nothing is
/// asked and nothing is written: the refusal names `docli init --gitignore` instead, because a
/// guardrail with no scriptable remedy is a wall.
///
/// Returns true when it wrote something (the caller re-validates).
pub fn offer_missing_ignores(project_root: &Path, config: &DocliToml) -> Result<bool> {
    if !ui::interactive() {
        return Ok(false);
    }
    // Offer ONLY when the ignore rule is the sole complaint. With an overlapping mount or a
    // duplicate workspace the geometry fails either way, and writing to the user's `.gitignore`
    // to fix nothing is a change they did not need and did not benefit from.
    if config::validate_geometry_paths_only(project_root, config).is_err() {
        return Ok(false);
    }
    let mut want: Vec<IgnoreFix> = Vec::new();
    for m in &config.mounts {
        for fix in missing_ignores(project_root, &m.dir)? {
            if !want.contains(&fix) {
                want.push(fix);
            }
        }
    }
    if want.is_empty() {
        return Ok(false);
    }
    ui::warn("The mirror is not hidden from git - docli will not update it until it is:");
    for f in &want {
        ui::detail(&f.label(project_root));
    }
    // The interrupt PROPAGATES: reading Ctrl-C as «нет» turns a deliberate abort into the
    // original geometry error and exit 2, instead of the silent 130 `main` is built to give.
    if !Confirm::with_theme(&prompt_theme())
        .with_prompt("Append these lines to .gitignore and carry on?")
        .default(true)
        .interact()?
    {
        return Ok(false);
    }
    match append_ignores(&want) {
        Ok(()) => {
            ui::ok(&format!(
                "appended to .gitignore: {}",
                want.iter()
                    .map(|f| f.label(project_root))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
            Ok(true)
        }
        Err(e) => {
            // Every refusal names the alternative (v0.28.6 D1a): the reader still has to get
            // these lines in, and the command that refuses is not the only way to do it.
            ui::refuse(&format!(
                "could not write .gitignore ({e:#}) - add these lines to it yourself:\n    {}",
                want.iter()
                    .map(|f| f.label(project_root))
                    .collect::<Vec<_>>()
                    .join("\n    ")
            ));
            Ok(false)
        }
    }
}

/// Should `docli init` run the wizard? Only with no intent expressed on the command line and
/// a terminal on both ends.
pub fn should_run(args: &init_cmd::InitArgs) -> bool {
    !has_intent(args) && ui::interactive()
}

/// Did the command line already say what to do? Kept apart from the terminal check so it can be
/// tested: a test run has no TTY, so a `should_run` assertion is satisfied by the TTY half alone
/// and proves nothing about the intent half.
pub fn has_intent(args: &init_cmd::InitArgs) -> bool {
    args.workspace.is_some()
        || args.dir.is_some()
        || args.folder.is_some()
        || args.name.is_some()
        || args.mcp.is_some()
        || args.mcp_label.is_some()
        || args.mcp_bare
        // `--gitignore` asks for one specific fix — appending the lines to the config that
        // already exists. Starting the whole wizard instead would be answering a different
        // question. `--clear-folder` is the same kind of instruction.
        || args.write_gitignore
        || args.clear_folder
        // The v0.28.6 flags are intent too: `docli init --hooks claude` asks for one specific
        // thing, and starting the whole guided journey instead would be answering a different
        // question — the same reasoning `--gitignore` already carries.
        || args.hooks.is_some()
        || args.skills.is_some()
        || args.instructions
}

/// The ONE agent picker: every configuration on one screen, the ones detected here already
/// ticked, and writing limited to what stays ticked. Shared with the flag path's prompt so
/// there is a single answer to «which configs am I about to change» — 0.1.0 had a free-text
/// list where a bare Enter meant «all five detected», which is how five configs got written
/// by someone who meant to look at the list first.
pub fn pick_agents(detected: &[&'static str]) -> Result<Vec<&'static str>> {
    let all: Vec<&crate::agents::AgentDef> = crate::agents::AGENTS.iter().collect();
    let items: Vec<String> = all
        .iter()
        .map(|a| {
            if detected.contains(&a.key) {
                format!("{}  {}", a.display, ui::dim("(found here)"))
            } else {
                a.display.to_string()
            }
        })
        .collect();
    let checked: Vec<bool> = all.iter().map(|a| detected.contains(&a.key)).collect();
    let chosen = MultiSelect::with_theme(&prompt_theme())
        .with_prompt("Where to wire this project's MCP connection")
        .items(&items)
        .defaults(&checked)
        .interact()?;
    Ok(chosen.into_iter().map(|i| all[i].key).collect())
}

/// Step 6 — the two heavier writes, offered UNTICKED (v0.28.6 D6).
///
/// Both agents refuse to run project-local hooks until the user accepts a trust dialog, so
/// `docli init` writing one cannot smuggle execution onto a machine: the platform holds the
/// gate. We ask anyway, and harder than for MCP, because a config entry names a server while a
/// hook runs a program — different acts, different defaults. The offer states plainly what will
/// be written, to which file, and that the agent will ask before any of it runs.
///
/// Only the two agents that HAVE a hook mechanism appear here. D9's asymmetry is stated rather
/// than left to inference: a user who wired Cursor and read «the mirror is protected» would be
/// wrong in exactly the way that costs a note.
fn pick_enforcement(
    cwd: &Path,
    detected: &[&'static str],
) -> Result<(Vec<crate::hooks::HookAgent>, bool)> {
    let candidates: Vec<crate::hooks::HookAgent> = crate::hooks::HookAgent::all()
        .into_iter()
        .filter(|a| detected.contains(&a.key()))
        .collect();
    let mut chosen = Vec::new();
    if candidates.is_empty() {
        ui::detail(
            "No agent with a hook mechanism was found here (only Claude Code and Codex have \
             one). The mirror stays marked read-only and the contract asks agents not to edit \
             it, but nothing refuses a write.",
        );
    } else {
        ui::detail("Hooks make the mirror unwritable in fact, not only in prose:");
        for line in crate::hooks::consent_summary(&candidates) {
            ui::detail(&format!("  {line}"));
        }
        ui::detail(
            "  Shell writes (`sed -i`, `>`) are NOT covered, and no other agent gets \
             enforcement at all.",
        );
        let items: Vec<String> = candidates.iter().map(|a| a.display().to_string()).collect();
        // UNTICKED. This is the one selection in the whole wizard that starts empty.
        let picked = MultiSelect::with_theme(&prompt_theme())
            .with_prompt("Install docli's hooks (nothing is ticked - space to opt in)")
            .items(&items)
            .defaults(&vec![false; items.len()])
            .interact()?;
        chosen = picked.into_iter().map(|i| candidates[i]).collect();
    }

    // DISCLOSED, because it is a write the reader did not tick. Step 5 governs MCP configs;
    // the contract file follows DETECTION instead (D4 — declining a config edit must not cost
    // you the contract), and `init` already drops the same file at the open-standard path
    // unconditionally. That makes it the lightest write here, not an invisible one.
    let skill_dirs: Vec<&str> = crate::agents::AGENTS
        .iter()
        .filter(|a| detected.contains(&a.key))
        .filter_map(|a| a.skill_copy_dir)
        .collect();
    if !skill_dirs.is_empty() {
        ui::detail(&format!(
            "The mirror contract will also be copied into {} (a document, nothing executable). \
             Skip it with `docli init --skills none`.",
            skill_dirs.join(", ")
        ));
    }

    ui::detail("Instruction files put the contract where each agent already looks:");
    for line in crate::instructions::consent_summary(cwd) {
        ui::detail(&format!("  {line}"));
    }
    let instructions = Confirm::with_theme(&prompt_theme())
        .with_prompt("Write these instruction files?")
        .default(false)
        .interact()?;
    Ok((chosen, instructions))
}

pub fn run(cwd: &Path, server_flag: Option<&str>) -> Result<i32> {
    // Refuse to shadow an ancestor project before asking anything (the same rule `init_cmd`
    // enforces — a nested docli.toml silently takes over its whole subtree).
    if let Some(ancestor) = config::find_project(cwd) {
        if ancestor != cwd {
            anyhow::bail!(
                "this directory is inside the docli project at {} - run `docli init` from \
                 there (a nested docli.toml would take precedence for every command in its \
                 subtree)",
                ancestor.display()
            );
        }
    }

    println!();
    ui::heading("docli setup");
    ui::detail("A workspace mirror for coding agents. Nothing is written until the last answer.");

    // ── 1. Сервер ────────────────────────────────────────────────────────────────────────
    ui::step(1, STEPS, "Server");
    // An existing config that will not parse is a REFUSAL, not «no config»: treating it as
    // absent walked the reader through every prompt and could write `.gitignore` before
    // `init_cmd` finally read the file and failed, leaving a half-applied setup behind.
    let existing: Option<DocliToml> = match std::fs::read_to_string(cwd.join("docli.toml")) {
        // The shared door again: the wizard renders the recorded directory as a prompt DEFAULT.
        Ok(raw) => Some(config::parse_config(&raw)?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(anyhow::Error::new(e).context("reading docli.toml")),
    };
    let default_server = server_flag
        .map(|s| s.to_string())
        .or_else(|| existing.as_ref().map(|c| c.server.clone()))
        .unwrap_or_else(|| "https://docli.ru".to_string());
    let server: String = Input::with_theme(&prompt_theme())
        .with_prompt("Server URL")
        .default(default_server)
        .interact_text()?;
    let server = server.trim().trim_end_matches('/').to_string();

    // ── 2. Вход ──────────────────────────────────────────────────────────────────────────
    ui::step(2, STEPS, "Sign-in");
    let store = creds::CredsStore::open_default()?;
    if store.get(&server)?.is_none() {
        ui::detail(&format!("This device is not connected to {server} yet."));
        if !Confirm::with_theme(&prompt_theme())
            .with_prompt("Sign in now? (opens a browser)")
            .default(true)
            .interact()?
        {
            ui::warn("Without signing in there is no workspace list to choose from.");
            // The origin was just chosen in step 1 and there is no docli.toml yet, so a bare
            // `docli login` here would sign into production instead.
            let login = if server == "https://docli.ru" {
                "docli login".to_string()
            } else {
                format!("docli login --server {server}")
            };
            ui::next(&format!(
                "Sign in later with {}, then run {} again",
                ui::cmd(&login),
                ui::cmd("docli init")
            ));
            return Ok(1);
        }
        login::run_login(&server, &store)?;
    } else {
        ui::ok(&format!("This device is already connected to {server}."));
    }
    let api = Api::new(&server, creds::CredsStore::open_default()?)?;

    // ── 3. Пространство ──────────────────────────────────────────────────────────────────
    ui::step(3, STEPS, "Workspace");
    let spaces = api.workspaces().context("listing your workspaces")?;
    if spaces.is_empty() {
        ui::warn("This account has no workspaces you can reach.");
        ui::next(&format!(
            "Create one at {server}, then run the setup again."
        ));
        return Ok(1);
    }
    let labels: Vec<String> = spaces
        .iter()
        .map(|w| format!("@{}  {}", w.handle, ui::dim(&w.name)))
        .collect();
    let picked = Select::with_theme(&prompt_theme())
        .with_prompt("Which workspace should be mirrored?")
        .items(&labels)
        .default(0)
        .interact()?;
    let workspace = spaces[picked].clone();

    // ── 4. Каталог ───────────────────────────────────────────────────────────────────────
    ui::step(4, STEPS, "Mirror directory");
    // Prefilled with the mount this workspace ALREADY has, if any: pressing Enter on a re-run
    // must not silently abandon an existing mirror for the CLI's own default.
    let current_dir = existing
        .as_ref()
        .and_then(|c| c.mounts.iter().find(|m| m.workspace == workspace.id))
        .map(|m| m.dir.clone());
    // The DEFAULT is this machine's shared cache for that workspace. Accepting it must store
    // nothing: `docli.toml` is committed, and writing an absolute home path into it would make
    // the file wrong on every other machine.
    // The default is EMPTY and described in words, not shown as a path. Two reasons: the
    // absolute cache path is not something the reader acts on (they do not manage that
    // directory), and printing it here is the most prominent place the CLI could hand an agent
    // the mirror root — the leak the live-agent gate measured on `docli status`.
    //
    // An explicit path still works: type one and it wins, and it is written to `docli.toml`.
    let dir: String = Input::with_theme(&prompt_theme())
        .with_prompt("Where to put the mirror (Enter for this machine's docli cache)")
        .allow_empty(true)
        .default(current_dir.clone().unwrap_or_default())
        .interact_text()?;
    let dir = dir.trim().trim_end_matches('/').to_string();
    // Prefilled with what this workspace is ALREADY scoped to, so Enter means «leave it» rather
    // than silently widening the mirror; clearing the field is then an explicit act, and the
    // flag path is told to clear (absent `--folder` means «leave alone» there).
    let current_folder = existing
        .as_ref()
        .and_then(|c| c.mounts.iter().find(|m| m.workspace == workspace.id))
        .and_then(|m| m.folder.clone());
    let prompt = match &current_folder {
        Some(_) => "Mirror only this folder (clear the field for the whole workspace)",
        None => "Mirror only one folder (Enter for the whole workspace)",
    };
    let folder: String = Input::with_theme(&prompt_theme())
        .with_prompt(prompt)
        .allow_empty(true)
        .default(current_folder.clone().unwrap_or_default())
        .interact_text()?;
    let folder = {
        let f = folder.trim().trim_matches('/').to_string();
        if f.is_empty() {
            None
        } else {
            Some(f)
        }
    };
    let clear_folder = folder.is_none() && current_folder.is_some();

    // Validate the geometry NOW, while the answer is still changeable and nothing is written —
    // against the config `init_cmd` will actually PRODUCE, existing mounts included. A probe
    // holding only the new mount cannot see an overlap with a mount already in the file, so the
    // reader would answer every remaining prompt (and possibly have `.gitignore` written) before
    // the refusal arrived.
    let mut mounts: Vec<Mount> = existing
        .as_ref()
        .map(|c| c.mounts.clone())
        .unwrap_or_default();
    let chosen = Mount {
        workspace: workspace.id,
        dir: dir.clone(),
        folder: folder.clone(),
        name: None,
        derived_dir: false,
        workspace_label: String::new(),
    };
    match mounts.iter_mut().find(|m| m.workspace == workspace.id) {
        // The same upsert `init_cmd` performs — re-point, never add a second mount.
        Some(slot) => *slot = chosen,
        None => mounts.push(chosen),
    }
    let mut probe = DocliToml {
        server: server.clone(),
        mcp_label: existing.as_ref().and_then(|c| c.mcp_label.clone()),
        mounts,
    };
    // An empty `dir` — pressing Enter at the directory step — is not yet a path, and every rule
    // below asks about one. Resolving here is what `init`'s flag path already does; without it
    // the wizard validated a mount whose dir was «», whose display name was therefore blank, and
    // refused its own default answer.
    if let Err(e) = config::resolve_mount_dirs(&mut probe) {
        ui::refuse(&format!("{e:#}"));
        return Ok(1);
    }
    // The gitignore half is answered by step 6, so a missing entry must not fail the probe:
    // only the geometry rules (overlap, control plane, vault, containment) are checked here.
    if let Err(e) = config::validate_geometry_paths_only(cwd, &probe) {
        ui::refuse(&format!("{e:#}"));
        // Nothing was written, and the answer that caused this is one prompt back.
        ui::next(&format!(
            "Run {} again and choose a different mirror directory",
            ui::cmd("docli init")
        ));
        return Ok(1);
    }

    // ── 5. Агенты ────────────────────────────────────────────────────────────────────────
    ui::step(5, STEPS, "Coding agents");
    let detected = crate::agents::detect(cwd, std::env::home_dir().as_deref());
    ui::detail(
        "Space toggles, Enter confirms. The configurations found here are ticked; only what \
         stays ticked is written.",
    );
    let agents = pick_agents(&detected)?;

    // ── 6. Правила и хуки ────────────────────────────────────────────────────────────────
    ui::step(6, STEPS, "Rules and enforcement");
    let (hook_agents, instructions) = pick_enforcement(cwd, &detected)?;

    // ── 7. Git и первая синхронизация ────────────────────────────────────────────────────
    ui::step(7, STEPS, "Git and the first sync");
    // Every line the consent will actually cause to be written — `init_cmd` writes for EVERY
    // mount, so showing only the selected one asked for agreement to a smaller change than the
    // one performed (and could touch a nested repository's file the reader never saw named).
    let mut missing: Vec<IgnoreFix> = Vec::new();
    for m in &probe.mounts {
        for fix in missing_ignores(cwd, &m.dir)? {
            if !missing.contains(&fix) {
                missing.push(fix);
            }
        }
    }
    let mut gitignore = false;
    if missing.is_empty() {
        ui::ok("`.gitignore` already covers the mirror and `.docli/`.");
    } else {
        ui::detail("The mirror must not reach git: it is somebody's notes, not source code.");
        for f in &missing {
            ui::detail(&format!("  {}", f.label(cwd)));
        }
        gitignore = Confirm::with_theme(&prompt_theme())
            .with_prompt("Append these lines to .gitignore?")
            .default(true)
            .interact()?;
    }
    let sync_now = Confirm::with_theme(&prompt_theme())
        .with_prompt("Sync straight after setup?")
        .default(true)
        .interact()?;

    let plan = Plan {
        server,
        workspace,
        dir,
        folder,
        clear_folder,
        agents,
        hooks: hook_agents,
        instructions,
        gitignore,
        sync_now,
    };
    apply(cwd, &api, plan)
}

/// Everything the wizard collected, written in one pass.
fn apply(cwd: &Path, api: &Api, plan: Plan) -> Result<i32> {
    ui::heading("Done");
    // The flag path does the actual writing — one implementation of «create docli.toml, drop
    // SKILL.md, wire the agents», never a second copy that can drift from it.
    let code = init_cmd::run(
        cwd,
        Some(api),
        &init_cmd::InitArgs {
            workspace: Some(plan.workspace.id),
            dir: Some(plan.dir.clone()),
            folder: plan.folder.clone(),
            clear_folder: plan.clear_folder,
            name: None,
            server: Some(plan.server.clone()),
            mcp: Some(if plan.agents.is_empty() {
                "none".to_string()
            } else {
                plan.agents.join(",")
            }),
            mcp_label: None,
            mcp_bare: false,
            allow_prompt: false,
            // The skill goes to every DETECTED agent's own directory regardless of what the
            // reader ticked for MCP (D4): declining a config write must not silently cost them
            // the contract too, and `init` already drops the same file at the open-standard
            // path unconditionally.
            skills: Some("auto".to_string()),
            hooks: (!plan.hooks.is_empty()).then(|| {
                plan.hooks
                    .iter()
                    .map(|a| a.key())
                    .collect::<Vec<_>>()
                    .join(",")
            }),
            instructions: plan.instructions,
            // Consent was collected in step 6; the WRITE belongs to `init_cmd`, which does it
            // once the whole configuration has passed the gate.
            write_gitignore: plan.gitignore,
        },
    )?;
    if code != 0 {
        return Ok(code);
    }
    if plan.sync_now {
        ui::heading("Sync");
        let project = config::load_project(cwd)?;
        return crate::sync_cmd::run(
            &project,
            api,
            &crate::sync_cmd::SyncOptions {
                check: false,
                full: false,
            },
        );
    }
    ui::next(&format!("First sync: {}", ui::cmd("docli sync")));
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The patterns alone — the tests are about WHICH lines are produced, and the work tree
    /// they belong to is asserted separately where it matters.
    fn entries(fixes: &[IgnoreFix]) -> Vec<String> {
        fixes.iter().map(|f| f.entry.clone()).collect()
    }

    #[test]
    fn the_default_dir_is_derived_from_the_handle_and_shares_one_parent() {
        assert_eq!(default_dir("agitek"), ".docli/mirror/agitek");
        assert_eq!(default_dir("@agitek"), ".docli/mirror/agitek");
        // A handle that is not a legal path component still yields a usable directory.
        assert_eq!(default_dir("../etc"), ".docli/mirror/etc");
        assert_eq!(default_dir("///"), ".docli/mirror/workspace");
        // Every mount lands under ONE parent, which is what makes a single ignore line enough.
        assert!(default_dir("a").starts_with(".docli/mirror/"));
        assert!(default_dir("b").starts_with(".docli/mirror/"));
    }

    #[test]
    fn missing_ignores_names_the_parent_and_clears_once_covered() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(root)
            .status()
            .unwrap();
        // The DEFAULT mount lives inside `.docli/`, so a single line covers the control plane
        // and the mirror both — the old two-line answer (`/.docli/` + `/docli-mirror/`) is one
        // line now, which is the point of the move.
        let missing = missing_ignores(root, ".docli/mirror/agitek").unwrap();
        assert_eq!(entries(&missing), vec!["/.docli/".to_string()]);
        // A mount the user places OUTSIDE `.docli/` still needs its own entry — and it is the
        // EXACT path now, not a hoisted parent: `docli-mirror/` stopped being our default, so
        // ignoring the whole of it would be ignoring a directory the user chose, not ours.
        let outside = missing_ignores(root, "docli-mirror/agitek").unwrap();
        assert_eq!(
            entries(&outside),
            vec!["/.docli/".to_string(), "/docli-mirror/agitek/".to_string()]
        );

        // The round trip is per-MOUNT: writing what a mount asked for must satisfy the same
        // check for THAT mount. Writing `/.docli/` covers the default mount and the control
        // plane; it says nothing about a mount the user put elsewhere.
        append_ignores(&missing).unwrap();
        assert!(
            missing_ignores(root, ".docli/mirror/agitek")
                .unwrap()
                .is_empty(),
            "the written entries must satisfy the same check that produced them"
        );
        append_ignores(&outside).unwrap();
        assert!(
            missing_ignores(root, "docli-mirror/agitek")
                .unwrap()
                .is_empty(),
            "…and the same holds for a mount outside .docli/"
        );
        // The user's own content survives an append.
        std::fs::write(root.join(".gitignore"), "target/\n").unwrap();
        append_ignores(&[IgnoreFix {
            worktree: root.to_path_buf(),
            entry: "/.docli/".to_string(),
        }])
        .unwrap();
        let body = std::fs::read_to_string(root.join(".gitignore")).unwrap();
        assert!(body.starts_with("target/\n"), "{body}");
        assert!(body.contains(".docli/"), "{body}");
    }

    #[test]
    fn an_absolute_mount_path_yields_a_repository_relative_entry() {
        // git reads `/repo/cache/` as «cache under the repo root», so an absolute --dir used to
        // produce a pattern matching nothing — and the guardrail kept refusing after the fix.
        let tmp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&root)
            .status()
            .unwrap();
        let abs = root.join("cache");
        let m = entries(&missing_ignores(&root, &abs.to_string_lossy()).unwrap());
        assert!(m.contains(&"/cache/".to_string()), "{m:?}");
        assert!(m.iter().all(|e| e.starts_with('/')), "anchored: {m:?}");
        // A mount outside the project contributes no MOUNT entry to this .gitignore (`.docli/`
        // still belongs to it — that one is the project's own control directory).
        let outside = tmp.path().parent().unwrap().join("elsewhere");
        let m = entries(&missing_ignores(&root, &outside.to_string_lossy()).unwrap());
        assert_eq!(m, vec!["/.docli/".to_string()], "{m:?}");
    }

    #[test]
    fn patterns_are_anchored_and_escaped_so_git_actually_honours_them() {
        let tmp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&root)
            .status()
            .unwrap();
        // A directory name git would otherwise read as a comment line.
        let fixes = missing_ignores(&root, "#notes").unwrap();
        let mount = fixes.iter().find(|f| f.entry.contains("notes")).unwrap();
        assert_eq!(mount.entry, "/\\#notes/", "{fixes:?}");
        append_ignores(&fixes).unwrap();
        // The real proof: git itself now ignores it, so the gate is satisfied.
        std::fs::create_dir_all(root.join("#notes")).unwrap();
        assert!(
            missing_ignores(&root, "#notes").unwrap().is_empty(),
            "git did not honour the written pattern"
        );

        // Anchoring: ignoring a top-level `cache` must not hide `src/cache`.
        let fixes = missing_ignores(&root, "cache").unwrap();
        append_ignores(&fixes).unwrap();
        std::fs::create_dir_all(root.join("src/cache")).unwrap();
        std::fs::write(root.join("src/cache/keep.rs"), "fn main() {}").unwrap();
        let out = std::process::Command::new("git")
            .args(["check-ignore", "src/cache/keep.rs"])
            .current_dir(&root)
            .output()
            .unwrap();
        assert!(
            !out.status.success(),
            "an unanchored pattern hid unrelated project files: {}",
            String::from_utf8_lossy(&out.stdout)
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_mount_reached_through_a_symlink_still_finds_its_repository() {
        // The lexical walk from `/outside/link/cache` never meets `/repo/.git`; geometry
        // physicalizes and refuses the mount, so the offer must physicalize too or the fix is
        // silently absent while the gate keeps refusing.
        let tmp = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(tmp.path()).unwrap();
        let repo = base.join("repo");
        std::fs::create_dir_all(repo.join("sub")).unwrap();
        std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&repo)
            .status()
            .unwrap();
        let outside = base.join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(repo.join("sub"), outside.join("link")).unwrap();

        let fixes =
            missing_ignores(&outside, &outside.join("link/cache").to_string_lossy()).unwrap();
        let mount = fixes
            .iter()
            .find(|f| f.entry.contains("cache"))
            .unwrap_or_else(|| panic!("no mount fix for a symlinked path: {fixes:?}"));
        assert_eq!(config::physicalize(&mount.worktree), repo);
        assert_eq!(mount.entry, "/sub/cache/");
    }

    #[test]
    fn a_mount_in_a_nested_repository_targets_that_repositorys_gitignore() {
        // `vendor/repo` is its own work tree: an entry appended to the OUTER project's
        // .gitignore ignores nothing there, so the guardrail would keep refusing after the fix.
        let tmp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let inner = root.join("vendor/repo");
        std::fs::create_dir_all(&inner).unwrap();
        for dir in [&root, &inner] {
            std::process::Command::new("git")
                .args(["init", "-q"])
                .current_dir(dir)
                .status()
                .unwrap();
        }
        let fixes = missing_ignores(&root, "vendor/repo/cache").unwrap();
        let mount = fixes
            .iter()
            .find(|f| f.entry == "/cache/")
            .unwrap_or_else(|| panic!("expected a `cache/` entry in {fixes:?}"));
        assert_eq!(
            config::physicalize(&mount.worktree),
            inner,
            "the entry must go to the INNER repository"
        );
        // …and the control directory still belongs to the outer project.
        let control = fixes.iter().find(|f| f.entry == "/.docli/").unwrap();
        assert_eq!(config::physicalize(&control.worktree), root);

        append_ignores(&fixes).unwrap();
        assert!(std::fs::read_to_string(inner.join(".gitignore"))
            .unwrap()
            .contains("/cache/"));
        assert!(missing_ignores(&root, "vendor/repo/cache")
            .unwrap()
            .is_empty());
    }

    #[test]
    fn a_flat_mount_dir_is_ignored_by_its_own_name() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(root)
            .status()
            .unwrap();
        // No parent to hoist to: the entry must be the directory itself, not a stray "/".
        let missing = entries(&missing_ignores(root, "cache").unwrap());
        assert!(missing.contains(&"/cache/".to_string()), "{missing:?}");
    }

    #[test]
    fn outside_a_git_worktree_nothing_is_missing() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(missing_ignores(tmp.path(), "docli-mirror/x")
            .unwrap()
            .is_empty());
    }

    /// Pressing Enter at the directory step leaves `dir` EMPTY, and the wizard then validated a
    /// mount that had no path yet — whose `display_name()` was therefore blank, so it refused its
    /// own default answer with «a mount has a blank display name». Reported from a live run.
    ///
    /// The probe must be resolved before it is validated, exactly as the flag path does.
    #[test]
    fn the_wizards_probe_resolves_an_empty_dir_before_it_is_validated() {
        let tmp = tempfile::tempdir().unwrap();
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

        let mut probe = config::DocliToml {
            server: "https://docli.ru".into(),
            mcp_label: None,
            mounts: vec![config::Mount {
                workspace: uuid::Uuid::from_u128(9),
                dir: String::new(), // ← what Enter produces
                folder: None,
                name: None,
                derived_dir: false,
                workspace_label: String::new(),
            }],
        };
        // Unresolved, the mount has no name to print and geometry refuses it.
        assert!(
            config::validate_config(&probe).is_err(),
            "an unresolved empty dir must not validate"
        );
        config::resolve_mount_dirs(&mut probe).unwrap();
        config::validate_geometry_paths_only(tmp.path(), &probe)
            .expect("the wizard's own default answer must validate");
        // …and it is named by its workspace, never by the cache path.
        let name = probe.mounts[0].display_name();
        assert!(
            name.contains(&uuid::Uuid::from_u128(9).to_string()),
            "{name}"
        );
        assert!(
            !name.contains(".docli"),
            "the name must not be a path: {name}"
        );
    }

    #[test]
    fn the_default_parent_is_the_only_thing_hoisted() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(root)
            .status()
            .unwrap();
        // The default is inside `.docli/`, which is already required — nothing extra to add…
        assert_eq!(
            entries(&missing_ignores(root, ".docli/mirror/agitek").unwrap()),
            vec!["/.docli/".to_string()]
        );
        // …but a mount the user placed under their source tree is ignored by its OWN path.
        // Offering `src/` would ignore every new file anywhere under `src`.
        let m = entries(&missing_ignores(root, "src/docli-mirror").unwrap());
        assert!(m.contains(&"/src/docli-mirror/".to_string()), "{m:?}");
        assert!(!m.contains(&"/src/".to_string()), "{m:?}");
    }

    #[test]
    fn any_expressed_intent_keeps_the_flag_path() {
        let bare = init_cmd::InitArgs {
            workspace: None,
            dir: None,
            folder: None,
            name: None,
            server: None,
            mcp: None,
            mcp_label: None,
            mcp_bare: false,
            allow_prompt: true,
            clear_folder: false,
            write_gitignore: false,
            skills: None,
            hooks: None,
            instructions: false,
        };
        // `has_intent`, not `should_run`: a test run has no TTY, so `should_run` is false for
        // every variant and would pass even if the intent check were deleted.
        assert!(!has_intent(&bare));
        assert!(has_intent(&init_cmd::InitArgs {
            workspace: Some(uuid::Uuid::from_u128(1)),
            ..bare.clone()
        }));
        assert!(has_intent(&init_cmd::InitArgs {
            mcp: Some("auto".into()),
            ..bare.clone()
        }));
        assert!(has_intent(&init_cmd::InitArgs {
            mcp_bare: true,
            ..bare.clone()
        }));
        assert!(has_intent(&init_cmd::InitArgs {
            folder: Some("docs".into()),
            ..bare.clone()
        }));
        // The v0.28.6 flags are intent too — each asks for one specific thing, and starting
        // the whole guided journey instead would answer a different question.
        assert!(has_intent(&init_cmd::InitArgs {
            hooks: Some("claude".into()),
            ..bare.clone()
        }));
        assert!(has_intent(&init_cmd::InitArgs {
            skills: Some("none".into()),
            ..bare.clone()
        }));
        assert!(has_intent(&init_cmd::InitArgs {
            instructions: true,
            ..bare.clone()
        }));
        // `docli init --gitignore` must perform the advertised fix, not open the wizard.
        assert!(has_intent(&init_cmd::InitArgs {
            write_gitignore: true,
            skills: Some("none".into()),
            hooks: None,
            instructions: false,
            ..bare.clone()
        }));
    }
}
