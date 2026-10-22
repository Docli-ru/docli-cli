// SPDX-FileCopyrightText: 2026 OOO Agitek
// SPDX-License-Identifier: MIT

//! `docli agent-setup` (v0.46.0 D8/D9) — install, or bring up to this binary's bundled version,
//! the three agent artifacts: the skill, the hooks and the `AGENTS.md` section.
//!
//! Those three reach disk in ONE place, the two-phase unit below — [`resolve`] the selection
//! before anything is written, then [`apply`] it — and `docli init` calls the same unit, so the
//! two commands cannot write the artifacts differently. MCP wiring is not here: it stays with
//! `docli init --mcp`.
//!
//! Why a command of its own: every CLI release that changes a bundled asset leaves every project
//! holding the old one, and `init` re-asks the mount questions to regenerate it. This regenerates
//! without asking anything when there is no terminal — it refreshes what is installed and adds
//! nothing — which is what an agent following `docli status`'s remedy needs.

use std::path::Path;

use anyhow::Result;
use dialoguer::Confirm;

use crate::agents::{self, AgentDef};
use crate::config::DocliToml;
use crate::hooks::{self, HookAgent};
use crate::{init_cmd, instructions, ui, wizard};

/// The Agent Skills open-standard path — every standard-following harness reads it.
pub const STANDARD_SKILL_DIR: &str = ".agents/skills/docli-mirror";

/// One artifact's state against what this binary would write.
#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    /// Not there (or, for `AGENTS.md`, no docli section in it).
    Absent,
    Current,
    /// There, and different from what this binary writes — hand-edited included.
    Outdated,
    /// Ours or about us, but docli will not edit it: a hook file the locator cannot splice, a
    /// hand-written `AGENTS.md` description, a file that cannot be read.
    Occupied,
}

/// Equal up to line endings. Every comparison of a USER file goes through this: the skill copies,
/// `AGENTS.md` and the hook files are committed in repositories we cannot pin, so an autocrlf
/// checkout would otherwise read «outdated» forever.
pub fn same_text(a: &str, b: &str) -> bool {
    a.replace("\r\n", "\n") == b.replace("\r\n", "\n")
}

/// What [`apply`] writes.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Selection {
    /// The open-standard copy of the skill.
    pub skill: bool,
    /// Agents that get the skill in their own skills directory.
    pub skill_agents: Vec<&'static str>,
    pub hook_agents: Vec<HookAgent>,
    /// The `AGENTS.md` section.
    pub agents_md: bool,
    /// …and a `CLAUDE.md` importing it when none exists. False on a bare refresh, which installs
    /// nothing new (D8).
    pub create_claude_md: bool,
}

/// The flag half, resolved BEFORE the first write so a bad value refuses over an untouched tree.
/// `skills` absent means `auto`; `hooks` absent means none.
pub fn resolve(
    root: &Path,
    skills: Option<&str>,
    hooks_flag: Option<&str>,
    agents_md: bool,
) -> Result<Selection> {
    let detected = || agents::detect(root, std::env::home_dir().as_deref());
    let skill_agents = match skills.map(str::trim) {
        Some("none") | Some("") => Vec::new(),
        None | Some("auto") => detected(),
        Some(list) => parse_agent_list(list, "--skills")?,
    };
    let hook_agents = match hooks_flag.map(str::trim) {
        None | Some("none") | Some("") => Vec::new(),
        Some("auto") => {
            let d = detected();
            HookAgent::all()
                .into_iter()
                .filter(|a| d.contains(&a.key()))
                .collect()
        }
        Some(list) => {
            let mut out = Vec::new();
            for k in list.split(',').map(str::trim).filter(|k| !k.is_empty()) {
                let a = HookAgent::parse(k).map_err(|e| {
                    anyhow::anyhow!("--hooks: {e} (only these two agents have a hook mechanism)")
                })?;
                if !out.contains(&a) {
                    out.push(a);
                }
            }
            out
        }
    };
    Ok(Selection {
        skill: true,
        skill_agents,
        hook_agents,
        agents_md,
        create_claude_md: agents_md,
    })
}

/// A comma list of agent keys, refused (never silently dropped) on an unknown one.
pub fn parse_agent_list(list: &str, flag: &str) -> Result<Vec<&'static str>> {
    let mut keys = Vec::new();
    for k in list.split(',').map(str::trim).filter(|k| !k.is_empty()) {
        match agents::agent(k) {
            Some(def) => {
                if !keys.contains(&def.key) {
                    keys.push(def.key);
                }
            }
            None => anyhow::bail!(
                "unknown agent {k:?} in {flag} - valid: {}",
                agents::AGENTS
                    .iter()
                    .map(|a| a.key)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }
    Ok(keys)
}

fn hook_label(config: &DocliToml) -> &str {
    // The PostToolUse matcher covers the `docli` key `init --mcp` writes AND this project's label
    // (Desktop's gateway name) — see `hooks::post_tool_use_matcher`.
    config
        .mcp_label
        .as_deref()
        .unwrap_or(hooks::DEFAULT_MCP_LABEL)
}

/// The write half. A file that already says what this binary would write is left untouched, so a
/// second run changes nothing.
pub fn apply(root: &Path, config: &DocliToml, sel: &Selection) -> Result<()> {
    if sel.skill {
        let written = agents::copy_skill(root, STANDARD_SKILL_DIR, init_cmd::SKILL_MD)?;
        ui::ok(&if written {
            format!("wrote {STANDARD_SKILL_DIR}/SKILL.md")
        } else {
            format!("{STANDARD_SKILL_DIR}/SKILL.md is up to date")
        });
    }
    // The contract into each selected agent's OWN skills directory — independent of whether any
    // MCP config was written (D4). The activation globs come from the mount table, so this is a
    // template rather than a copy.
    let skill_agents: Vec<&AgentDef> = sel
        .skill_agents
        .iter()
        .filter_map(|k| agents::agent(k))
        .collect();
    if !skill_agents.is_empty() {
        agents::install_skills(
            root,
            &skill_agents,
            init_cmd::SKILL_MD,
            &agents::skill_globs(root, &config.mounts),
        );
    }
    if !sel.hook_agents.is_empty() {
        ui::heading("Hooks");
        // The hook files go through the SAME atomic writer as the MCP configs, so a crashed
        // earlier run leaves the same `.docli-cfg-*.tmp` residue beside them. An empty
        // selection sweeps exactly the hook directories.
        agents::sweep_cfg_temps(root, &[]);
        for line in hooks::consent_summary(&sel.hook_agents) {
            ui::detail(&line);
        }
        for agent in &sel.hook_agents {
            // A file that already holds this binary's entries is not handed to the text locator:
            // it can refuse a file that is current, or tidy one the screen just called up to date.
            if hooks::file_state(root, *agent, hook_label(config)) == State::Current {
                ui::ok(&format!(
                    "{}: the docli hooks are up to date",
                    agent.config_path()
                ));
                continue;
            }
            hooks::install(root, *agent, hook_label(config))?;
        }
        // D9: enforcement language appears ONLY for agents that got a hook, and it states the
        // limit in the same breath.
        ui::detail(
            "docli will now refuse writes into the mirror from these agents' file-editing \
             tools. Shell writes (`sed -i`, `>`) are NOT covered, on either agent, and no other \
             agent gets enforcement at all - they get the contract as advice.",
        );
    }
    if sel.agents_md {
        ui::heading("Instructions");
        instructions::install(root, sel.create_claude_md)?;
    }
    Ok(())
}

/// A file [`apply`] would write, by what it is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Item {
    /// The skill: the open-standard copy (`None`) or an agent's own, by key.
    Skill(Option<&'static str>),
    Hooks(HookAgent),
    AgentsMd,
}

impl Item {
    pub fn path(self) -> String {
        match self {
            Item::Skill(None) => format!("{STANDARD_SKILL_DIR}/SKILL.md"),
            Item::Skill(Some(key)) => format!(
                "{}/SKILL.md",
                agents::agent(key)
                    .and_then(|d| d.skill_copy_dir)
                    .unwrap_or_default()
            ),
            Item::Hooks(a) => a.config_path().to_string(),
            Item::AgentsMd => "AGENTS.md".to_string(),
        }
    }
}

/// A file's state, as `docli status --json` reports it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct FileState {
    pub path: String,
    pub state: State,
}

/// The state of every file `sel` covers, against what this binary would write.
pub fn states(root: &Path, config: &DocliToml, sel: &Selection) -> Vec<(Item, State)> {
    let read = |rel: &str| std::fs::read_to_string(root.join(rel));
    let file = |rel: &str, want: &str| match read(rel) {
        Ok(t) if same_text(&t, want) => State::Current,
        Ok(_) => State::Outdated,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => State::Absent,
        Err(_) => State::Occupied,
    };
    let globs = agents::skill_globs(root, &config.mounts);
    let mut out = Vec::new();
    if sel.skill {
        let item = Item::Skill(None);
        out.push((item, file(&item.path(), init_cmd::SKILL_MD)));
    }
    for def in sel.skill_agents.iter().filter_map(|k| agents::agent(k)) {
        if def.skill_copy_dir.is_none() {
            continue;
        }
        let item = Item::Skill(Some(def.key));
        let want = agents::skill_copy_body(def, init_cmd::SKILL_MD, &globs);
        out.push((item, file(&item.path(), &want)));
    }
    for a in &sel.hook_agents {
        out.push((
            Item::Hooks(*a),
            hooks::file_state(root, *a, hook_label(config)),
        ));
    }
    if sel.agents_md {
        let state = match read("AGENTS.md") {
            Ok(t) => instructions::section_state(Some(&t)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => State::Absent,
            Err(_) => State::Occupied,
        };
        out.push((Item::AgentsMd, state));
    }
    out
}

/// Everything docli could have installed here: the standard skill, every agent's skill copy,
/// both hook files and the `AGENTS.md` section.
fn everything() -> Selection {
    Selection {
        skill: true,
        skill_agents: agents::AGENTS
            .iter()
            .filter(|a| a.skill_copy_dir.is_some())
            .map(|a| a.key)
            .collect(),
        hook_agents: HookAgent::all().to_vec(),
        agents_md: true,
        create_claude_md: false,
    }
}

/// The skill copies and the `AGENTS.md` section that ARE installed here, with their state (D9).
/// Hooks are reported by `hooks::status`, which also says whether the gate can run.
pub fn installed_files(root: &Path, config: &DocliToml) -> Vec<FileState> {
    states(root, config, &everything())
        .into_iter()
        .filter(|(item, s)| {
            !matches!(item, Item::Hooks(_)) && matches!(s, State::Current | State::Outdated)
        })
        .map(|(item, state)| FileState {
            path: item.path(),
            state,
        })
        .collect()
}

/// What a bare non-interactive run refreshes: what is installed, and nothing else. A hook file
/// counts when any entry is docli's — including one the locator refuses, so the refresh prints
/// the entries to add by hand rather than staying silent.
pub(crate) fn installed_selection(root: &Path, config: &DocliToml) -> Selection {
    let mut sel = Selection::default();
    for (item, state) in states(root, config, &everything()) {
        match (item, state) {
            (Item::Hooks(a), State::Current | State::Outdated | State::Occupied) => {
                sel.hook_agents.push(a)
            }
            (_, State::Current | State::Outdated) => match item {
                Item::Skill(None) => sel.skill = true,
                Item::Skill(Some(key)) => sel.skill_agents.push(key),
                Item::AgentsMd => sel.agents_md = true,
                Item::Hooks(_) => {}
            },
            _ => {}
        }
    }
    sel
}

fn row(item: Item, state: State) -> String {
    let what = match state {
        State::Absent => "new",
        State::Current => "up to date",
        State::Outdated => "will be replaced",
        State::Occupied => "cannot be updated automatically - update it by hand",
    };
    format!("{}  {}", item.path(), ui::dim(what))
}

/// The attended path: init's consent step, then every file it would write, then one confirm.
/// Nothing is written before the confirm. `None` = nothing to do, or the reader declined.
fn ask(root: &Path, config: &DocliToml) -> Result<Option<Selection>> {
    ui::heading("docli agent setup");
    let detected = agents::detect(root, std::env::home_dir().as_deref());
    let (hook_agents, agents_md) = wizard::pick_enforcement(root, &detected)?;
    let sel = Selection {
        skill: true,
        skill_agents: detected,
        hook_agents,
        agents_md,
        create_claude_md: agents_md,
    };
    let planned = states(root, config, &sel);
    // The `CLAUDE.md` bridge `instructions::install` creates beside a section of ours, when there
    // is no file to damage — listed, so the confirm covers every file written.
    let bridge = agents_md
        && root.join("CLAUDE.md").symlink_metadata().is_err()
        && !planned.contains(&(Item::AgentsMd, State::Occupied));
    let writes = bridge
        || planned
            .iter()
            .any(|(_, s)| matches!(s, State::Absent | State::Outdated));
    let manual = planned.iter().any(|(_, s)| *s == State::Occupied);
    ui::heading("Files");
    for (item, state) in &planned {
        ui::detail(&row(*item, *state));
    }
    if bridge {
        ui::detail(&format!(
            "CLAUDE.md  {}",
            ui::dim("new - it imports AGENTS.md")
        ));
    }
    if !writes && !manual {
        ui::ok("Everything here is up to date.");
        return Ok(None);
    }
    if writes
        && !Confirm::with_theme(&wizard::prompt_theme())
            .with_prompt("Write these files?")
            .default(true)
            .interact()?
    {
        return Ok(None);
    }
    Ok(Some(sel))
}

/// `docli agent-setup`. With a flag it installs what the flags name, as `docli init` does; at a
/// terminal it asks what to install, then confirms; otherwise it refreshes what is installed and
/// adds nothing.
pub fn run(
    cwd: &Path,
    skills: Option<&str>,
    hooks_flag: Option<&str>,
    agents_md: bool,
) -> Result<i32> {
    // The hooks take the project's label, so there has to be a project.
    let project = crate::config::load_project(cwd)?;
    let (root, config) = (&project.root, &project.config);
    let sel = if skills.is_some() || hooks_flag.is_some() || agents_md {
        resolve(root, skills, hooks_flag, agents_md)?
    } else if ui::interactive() {
        match ask(root, config)? {
            Some(sel) => sel,
            None => return Ok(0),
        }
    } else {
        let sel = installed_selection(root, config);
        if sel == Selection::default() {
            ui::detail("No docli agent files are installed here, so there is nothing to refresh.");
            ui::next(&format!(
                "Install them: {}",
                ui::cmd("docli agent-setup --hooks auto --agents-md")
            ));
            return Ok(0);
        }
        sel
    };
    apply(root, config, &sel)?;
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> DocliToml {
        toml::from_str(
            "server = \"https://docli.ru\"\n[[mount]]\nworkspace = \"00000000-0000-0000-0000-000000000029\"\ndir = \"docli-mirror/notes\"\n",
        )
        .unwrap()
    }

    /// Every artifact, for an agent pair detected by their config directories.
    fn full(root: &Path) -> Selection {
        std::fs::create_dir_all(root.join(".claude")).unwrap();
        std::fs::create_dir_all(root.join(".codex")).unwrap();
        Selection {
            skill: true,
            skill_agents: vec!["claude", "qwen"],
            hook_agents: HookAgent::all().to_vec(),
            agents_md: true,
            create_claude_md: true,
        }
    }

    fn snapshot(root: &Path) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for rel in [
            ".agents/skills/docli-mirror/SKILL.md",
            ".claude/skills/docli-mirror/SKILL.md",
            ".qwen/skills/docli-mirror/SKILL.md",
            ".claude/settings.json",
            ".codex/hooks.json",
            "AGENTS.md",
            "CLAUDE.md",
        ] {
            out.push((
                rel.to_string(),
                std::fs::read_to_string(root.join(rel)).unwrap_or_default(),
            ));
        }
        out
    }

    #[test]
    fn a_second_run_changes_nothing_on_a_crlf_checkout_too() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let cfg = config();
        let sel = full(root);
        apply(root, &cfg, &sel).unwrap();
        assert!(states(root, &cfg, &sel)
            .iter()
            .all(|(_, s)| *s == State::Current));
        // An autocrlf checkout of every file docli wrote: still current, and not rewritten.
        for (rel, body) in snapshot(root) {
            if !body.is_empty() {
                std::fs::write(root.join(&rel), body.replace('\n', "\r\n")).unwrap();
            }
        }
        let before = snapshot(root);
        assert!(
            states(root, &cfg, &sel)
                .iter()
                .all(|(_, s)| *s == State::Current),
            "{:?}",
            states(root, &cfg, &sel)
        );
        apply(root, &cfg, &sel).unwrap();
        assert_eq!(before, snapshot(root), "a second run must change nothing");
    }

    #[test]
    fn an_outdated_and_a_hand_edited_skill_both_read_as_will_be_replaced() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let cfg = config();
        let sel = full(root);
        apply(root, &cfg, &sel).unwrap();
        // Older bundled text, and a person's edit: the same answer for both.
        std::fs::write(
            root.join(".agents/skills/docli-mirror/SKILL.md"),
            "---\nname: docli-mirror\n---\nan older contract\n",
        )
        .unwrap();
        let claude = root.join(".claude/skills/docli-mirror/SKILL.md");
        let edited = format!(
            "{}\nmy own note\n",
            std::fs::read_to_string(&claude).unwrap()
        );
        std::fs::write(&claude, edited).unwrap();
        let got = states(root, &cfg, &sel);
        for item in [Item::Skill(None), Item::Skill(Some("claude"))] {
            let (_, s) = got.iter().find(|(i, _)| *i == item).unwrap();
            assert_eq!(*s, State::Outdated, "{}", item.path());
            assert!(row(item, *s).contains("will be replaced"));
        }
        assert_eq!(
            installed_files(root, &cfg)
                .iter()
                .filter(|f| f.state == State::Outdated)
                .count(),
            2
        );
    }

    #[test]
    fn a_bare_refresh_brings_old_hook_installs_to_the_full_set_and_adds_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let mut cfg = config();
        cfg.mcp_label = Some("berkut".into());
        std::fs::create_dir_all(root.join(".claude")).unwrap();
        std::fs::create_dir_all(root.join(".codex")).unwrap();
        // Claude: a TWO-event install from before 2026-09-05, with a label-only matcher from
        // before 2026-10-02 — exactly the installs a health predicate called «no hooks».
        let hooks::HookOutcome::Write(fresh) = hooks::merge(HookAgent::Claude, None, "berkut")
        else {
            panic!("write")
        };
        let mut v: serde_json::Value = serde_json::from_str(&fresh).unwrap();
        v["hooks"].as_object_mut().unwrap().remove("PostToolUse");
        std::fs::write(
            root.join(".claude/settings.json"),
            serde_json::to_string_pretty(&v).unwrap(),
        )
        .unwrap();
        let label_only = fresh.replace("mcp__(docli|berkut)__.*", "mcp__berkut__.*");
        assert_eq!(
            hooks::state(HookAgent::Claude, Some(&label_only), "berkut"),
            State::Outdated
        );
        // Codex: user hooks, none of them docli's.
        std::fs::write(
            root.join(".codex/hooks.json"),
            r#"{"hooks": {"PreToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": "echo hi"}]}]}}"#,
        )
        .unwrap();

        let sel = installed_selection(root, &cfg);
        assert_eq!(
            sel,
            Selection {
                skill: false,
                skill_agents: vec![],
                hook_agents: vec![HookAgent::Claude],
                agents_md: false,
                create_claude_md: false,
            },
            "install nothing new"
        );
        let codex_before = std::fs::read_to_string(root.join(".codex/hooks.json")).unwrap();
        apply(root, &cfg, &sel).unwrap();
        assert_eq!(
            hooks::status(root, HookAgent::Claude, "berkut").state,
            State::Current,
            "the full current set"
        );
        assert_eq!(
            std::fs::read_to_string(root.join(".codex/hooks.json")).unwrap(),
            codex_before,
            "an agent without docli entries gets none"
        );
        for absent in [
            ".agents/skills/docli-mirror/SKILL.md",
            ".claude/skills/docli-mirror/SKILL.md",
            "AGENTS.md",
        ] {
            assert!(!root.join(absent).exists(), "{absent}");
        }

        // The label-only matcher alone, refreshed the same way.
        std::fs::write(root.join(".claude/settings.json"), &label_only).unwrap();
        apply(root, &cfg, &installed_selection(root, &cfg)).unwrap();
        assert_eq!(
            hooks::status(root, HookAgent::Claude, "berkut").state,
            State::Current
        );
    }

    #[test]
    fn a_current_hook_file_is_never_rewritten() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let cfg = config();
        std::fs::create_dir_all(root.join(".claude")).unwrap();
        let hooks::HookOutcome::Write(fresh) =
            hooks::merge(HookAgent::Claude, None, hook_label(&cfg))
        else {
            panic!("write")
        };
        // Our entries plus a second copy of one of them: equality says current, and the locator
        // would have tidied the copy away.
        let mut v: serde_json::Value = serde_json::from_str(&fresh).unwrap();
        let first = v["hooks"]["PreToolUse"][0].clone();
        v["hooks"]["PreToolUse"].as_array_mut().unwrap().push(first);
        let body = serde_json::to_string_pretty(&v).unwrap();
        std::fs::write(root.join(".claude/settings.json"), &body).unwrap();
        assert_eq!(
            hooks::state(HookAgent::Claude, Some(&body), hook_label(&cfg)),
            State::Current
        );
        let sel = Selection {
            hook_agents: vec![HookAgent::Claude],
            ..Selection::default()
        };
        apply(root, &cfg, &sel).unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join(".claude/settings.json")).unwrap(),
            body
        );
    }

    #[test]
    fn a_bare_refresh_of_agents_md_creates_no_claude_md() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let cfg = config();
        std::fs::write(root.join("AGENTS.md"), "# Project\n").unwrap();
        crate::instructions::install(root, false).unwrap();
        // An older section, so the refresh has something to do.
        let agents = std::fs::read_to_string(root.join("AGENTS.md")).unwrap();
        std::fs::write(
            root.join("AGENTS.md"),
            agents.replacen("docli sync", "docli  sync", 1),
        )
        .unwrap();
        let sel = installed_selection(root, &cfg);
        assert!(sel.agents_md && !sel.create_claude_md, "{sel:?}");
        apply(root, &cfg, &sel).unwrap();
        assert!(
            !root.join("CLAUDE.md").exists(),
            "a refresh installs nothing new"
        );
        assert!(std::fs::read_to_string(root.join("AGENTS.md"))
            .unwrap()
            .contains(&crate::instructions::block()));
    }

    #[test]
    fn the_flags_refuse_an_unknown_agent_before_anything_is_written() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(resolve(tmp.path(), Some("nope"), None, false).is_err());
        let err = resolve(tmp.path(), Some("none"), Some("cursor"), false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("--hooks"), "{err}");
        assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 0);
    }

    #[test]
    fn without_a_project_it_says_to_run_init() {
        let tmp = tempfile::tempdir().unwrap();
        let err = run(tmp.path(), None, None, false).unwrap_err().to_string();
        assert!(err.contains("docli init"), "{err}");
    }
}
