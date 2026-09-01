// SPDX-FileCopyrightText: 2026 OOO Agitek
// SPDX-License-Identifier: MIT

//! `AGENTS.md` for the many, `CLAUDE.md` importing it for Claude Code (v0.28.6 D5).
//!
//! `AGENTS.md` is read by roughly twenty tools — Codex, Cursor, Gemini CLI, Zed, VS Code,
//! Copilot's coding agent, Windsurf, Amp, opencode, Junie among them. Claude Code deliberately
//! is not one of them and says so in its own documentation; its documented bridge is a
//! `CLAUDE.md` whose first line imports the other file.
//!
//! # The rule that shapes everything here: an existing `CLAUDE.md` is NEVER edited
//!
//! That file is dense, hand-tuned and load-bearing for the user's own work. Appending to it
//! silently is the `.gitignore` mistake with higher stakes. So when one exists, the import line
//! is PRINTED for a person to paste, not written.
//!
//! The consequence is worth stating plainly rather than leaving it to be discovered, because it
//! is the COMMON case — this repository included, and most serious Claude Code projects: D5
//! delivers automatically for Codex (and the other `AGENTS.md` readers) and BY INSTRUCTION for
//! Claude Code. The copy must not imply otherwise.
//!
//! `AGENTS.md` is a different matter: we own a delimited section inside it, replace that section
//! in place on a re-run, and never touch a byte outside it.

use std::path::Path;

use anyhow::Result;

/// The docli section of `AGENTS.md`, between markers so a re-run replaces rather than appends.
pub const AGENTS_FRAGMENT: &str = include_str!("../assets/AGENTS-fragment.md");

const BEGIN: &str = "<!-- docli:begin -->";
const END: &str = "<!-- docli:end -->";

/// The documented Claude Code bridge — its first line, and the whole file when we create one.
pub const CLAUDE_IMPORT: &str = "@AGENTS.md";

/// The delimited block, exactly as it is written to disk.
pub fn block() -> String {
    format!("{BEGIN}\n{}\n{END}\n", AGENTS_FRAGMENT.trim_end())
}

/// What to do about `AGENTS.md`.
///
/// THREE outcomes, not two, and the third is why: an `Option` collapsed «our block is already
/// exactly right» together with «somebody wrote their own docli section», and the caller then
/// treated an ordinary idempotent re-run as a hand-written file — warning about a problem that
/// did not exist and, worse, withholding the `CLAUDE.md` bridge from a project whose `AGENTS.md`
/// was perfect.
#[derive(Debug, PartialEq, Eq)]
pub enum AgentsMd {
    /// Write this text.
    Write(String),
    /// Our block is already present and current.
    Current,
    /// The file describes docli in somebody else's words; we will not add a second description.
    HandWritten,
}

/// The section is replaced IN PLACE when it is already there, so a re-run is idempotent and the
/// reader's own ordering survives; otherwise it is appended, because the end of the file is the
/// only position we can claim without moving somebody's prose.
pub fn merged_agents_md(existing: Option<&str>) -> AgentsMd {
    let block = block();
    let Some(text) = existing else {
        return AgentsMd::Write(block);
    };
    if let (Some(start), Some(end)) = (text.find(BEGIN), text.find(END)) {
        if end > start {
            let end = end + END.len();
            // Keep whatever line ending followed the old block.
            let tail = &text[end..];
            let rebuilt = format!("{}{}{}", &text[..start], block.trim_end(), tail);
            return if rebuilt == text {
                AgentsMd::Current
            } else {
                AgentsMd::Write(rebuilt)
            };
        }
    }
    // A file that mentions docli but carries no markers is a HAND-written section (or an older
    // spelling of ours). Appending a second one would be noise the reader then has to
    // reconcile, so it is left alone and reported instead.
    if text.contains("docli") {
        return AgentsMd::HandWritten;
    }
    let mut out = text.to_string();
    if !out.ends_with('\n') {
        out.push('\n');
    }
    if !out.trim().is_empty() {
        out.push('\n');
    }
    out.push_str(&block);
    AgentsMd::Write(out)
}

/// Write the instruction files, reporting through `ui`. Best-effort per file, like every other
/// write `init` performs: one unwritable file names itself and the rest proceed.
pub fn install(project_root: &Path) -> Result<()> {
    let agents = project_root.join("AGENTS.md");
    let existing = match std::fs::read_to_string(&agents) {
        Ok(s) => Some(s),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            crate::ui::warn(&format!(
                "AGENTS.md could not be read ({e}) - add the docli section by hand:\n{}",
                block()
            ));
            return Ok(());
        }
    };
    let had = existing.is_some();
    // BEST-EFFORT, like every other write `init` performs. An earlier version propagated these
    // errors, so an unwritable `AGENTS.md` failed the whole `docli init` at its LAST step —
    // after docli.toml, the agent configs, the skills and the hooks had all landed.
    let mut have_section = true;
    match merged_agents_md(existing.as_deref()) {
        AgentsMd::Write(body) => match std::fs::write(&agents, body) {
            Ok(()) => crate::ui::ok(if had {
                "AGENTS.md: the docli section is up to date"
            } else {
                "wrote AGENTS.md"
            }),
            Err(e) => {
                have_section = false;
                crate::ui::warn(&format!(
                    "AGENTS.md could not be written ({e}) - add this section by hand:\n{}",
                    block()
                ));
            }
        },
        // Already exactly right — the ordinary re-run. The section IS there, so the bridge
        // below is still owed.
        AgentsMd::Current => crate::ui::ok("AGENTS.md: the docli section is up to date"),
        AgentsMd::HandWritten => {
            have_section = false;
            // HONEST about what was actually detected. The test is «the file mentions docli
            // and carries no markers of ours», which a single passing reference to
            // `docli-mirror/` satisfies — so claiming it «already describes docli» can be
            // simply false, and the reader is left with no way to know the section is absent.
            crate::ui::warn(
                "AGENTS.md mentions docli already, so no section was added - docli will not \
                 write a second description beside a hand-written one. If the file does not \
                 actually carry the mirror contract, add it yourself:",
            );
            crate::ui::detail(&block());
        }
    }

    // Claude Code reads CLAUDE.md, not AGENTS.md. We create the bridge only when there is no
    // file to damage.
    let claude = project_root.join("CLAUDE.md");
    // `symlink_metadata`, not `exists()`: a DANGLING `CLAUDE.md` symlink makes `exists()` false
    // (it follows the link), and the create below would then write through it — silently
    // producing the referent, in whatever directory the link points at.
    if claude.symlink_metadata().is_ok() {
        crate::ui::warn(
            "CLAUDE.md already exists and docli will not edit it - Claude Code does not read \
             AGENTS.md, so add this line at the top yourself:",
        );
        crate::ui::detail(CLAUDE_IMPORT);
    } else if !have_section {
        // A bridge to a file that may carry no contract is worse than no bridge: it looks like
        // the job is done. Say what is missing instead of writing the import over a gap.
        crate::ui::detail(
            "CLAUDE.md was not created: it would import an AGENTS.md that does not carry the \
             docli section. Add the section, then add `@AGENTS.md` at the top of a CLAUDE.md.",
        );
    } else {
        // CREATE_NEW, never `write`: «an existing CLAUDE.md is never edited» is the hardest
        // guarantee in this module, and a check-then-write cannot make it. Between the check
        // above and the write, another process — a second agent, a teammate's editor, a
        // template generator — can create the file, and `fs::write` would TRUNCATE it to one
        // line. The kernel settles it atomically instead, and an `AlreadyExists` here is the
        // race actually happening.
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&claude)
        {
            Ok(mut f) => {
                use std::io::Write as _;
                match f.write_all(format!("{CLAUDE_IMPORT}\n").as_bytes()) {
                    Ok(()) => crate::ui::ok("wrote CLAUDE.md (it imports AGENTS.md)"),
                    Err(e) => crate::ui::warn(&format!(
                        "CLAUDE.md could not be written ({e}) - create it with one line: \
                         {CLAUDE_IMPORT}"
                    )),
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                crate::ui::warn(
                    "CLAUDE.md appeared while docli was working and will not be edited - add \
                     this line at the top yourself:",
                );
                crate::ui::detail(CLAUDE_IMPORT);
            }
            Err(e) => crate::ui::warn(&format!(
                "CLAUDE.md could not be created ({e}) - create it with one line: {CLAUDE_IMPORT}"
            )),
        }
    }
    Ok(())
}

/// What `docli init` shows before writing anything (D6).
///
/// It runs the SAME decisions `install` will, rather than describing the usual case. A summary
/// that promises something the write then declines to do is worse than no summary: the reader
/// consents to one thing and gets another, and both of the branches that withhold a file here
/// are perfectly ordinary states to be in.
pub fn consent_summary(project_root: &Path) -> Vec<String> {
    // The same THREE-way read `install` performs. `.ok()` collapsed «absent» together with
    // «unreadable / not UTF-8», so the screen promised a section and a bridge for a file
    // `install` would then decline to touch at all.
    let read = std::fs::read_to_string(project_root.join("AGENTS.md"));
    let unreadable = matches!(&read, Err(e) if e.kind() != std::io::ErrorKind::NotFound);
    if unreadable {
        return vec![
            "AGENTS.md: NOT touched - it cannot be read, so the docli section is printed for \
             you to add by hand"
                .to_string(),
            "CLAUDE.md: NOT created - it would import an AGENTS.md docli could not read"
                .to_string(),
        ];
    }
    let agents_state = merged_agents_md(read.ok().as_deref());
    let mut out = vec![match agents_state {
        AgentsMd::HandWritten => "AGENTS.md: NOT touched - it already describes docli in its \
                                  own words, and docli will not add a second description"
            .to_string(),
        AgentsMd::Current => {
            "AGENTS.md: already carries the docli section - nothing to change".to_string()
        }
        AgentsMd::Write(_) => "AGENTS.md: a short docli section (read by Codex, Cursor, Gemini \
                               CLI, Zed, Copilot and about fifteen other tools)"
            .to_string(),
    }];
    // The SAME test `install` uses (`symlink_metadata`, not `exists()`), or the screen promises
    // to create a file the write then correctly refuses to touch — the dangling-symlink case.
    // A consent summary that disagrees with the action is worse than none.
    out.push(
        if project_root.join("CLAUDE.md").symlink_metadata().is_ok() {
            "CLAUDE.md: NOT touched - it already exists, so the one line to add is printed for \
             you to paste"
                .to_string()
        } else if matches!(agents_state, AgentsMd::HandWritten) {
            // The OTHER reason `install` withholds the bridge: importing an AGENTS.md that
            // carries no docli section would look like the job is done.
            "CLAUDE.md: NOT created - it would import an AGENTS.md that carries no docli section"
                .to_string()
        } else {
            format!(
                "CLAUDE.md: created, containing `{CLAUDE_IMPORT}` (Claude Code does not read \
                 AGENTS.md)"
            )
        },
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_project_gets_both_files() {
        let tmp = tempfile::tempdir().unwrap();
        install(tmp.path()).unwrap();
        let agents = std::fs::read_to_string(tmp.path().join("AGENTS.md")).unwrap();
        assert!(agents.contains(BEGIN) && agents.contains(END), "{agents}");
        assert!(agents.contains("docli sync"), "{agents}");
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("CLAUDE.md")).unwrap(),
            "@AGENTS.md\n"
        );
    }

    #[test]
    fn an_existing_claude_md_is_never_modified() {
        // D5's hard rule, and the case that matters: this repository's own CLAUDE.md is dense,
        // hand-tuned and load-bearing. The bridge is PRINTED, never written.
        let tmp = tempfile::tempdir().unwrap();
        let mine = "# My project\n\nCarefully tuned instructions.\n";
        std::fs::write(tmp.path().join("CLAUDE.md"), mine).unwrap();
        install(tmp.path()).unwrap();
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("CLAUDE.md")).unwrap(),
            mine,
            "not one byte"
        );
        // …and AGENTS.md was still written, so Codex and the rest are served.
        assert!(tmp.path().join("AGENTS.md").exists());
    }

    fn written(a: AgentsMd) -> String {
        match a {
            AgentsMd::Write(s) => s,
            other => panic!("expected a write, got {other:?}"),
        }
    }

    #[test]
    fn the_agents_section_is_replaced_in_place_not_appended() {
        let mine = "# Agents\n\nRun the tests before you commit.\n";
        let first = written(merged_agents_md(Some(mine)));
        assert!(first.starts_with(mine), "the user's prose comes first");
        assert_eq!(first.matches(BEGIN).count(), 1);
        // Re-running with the same content changes nothing at all — and says so as CURRENT,
        // which is a different answer from «somebody wrote their own».
        assert_eq!(merged_agents_md(Some(&first)), AgentsMd::Current);
        // …and an OLD block is replaced, never doubled.
        let stale = format!("{mine}\n{BEGIN}\nold text\n{END}\n\nAfterwards.\n");
        let out = written(merged_agents_md(Some(&stale)));
        assert_eq!(out.matches(BEGIN).count(), 1, "{out}");
        assert!(!out.contains("old text"), "{out}");
        assert!(
            out.contains("Run the tests"),
            "prose before survives: {out}"
        );
        assert!(out.contains("Afterwards."), "prose after survives: {out}");
    }

    #[test]
    fn a_hand_written_docli_section_is_left_alone() {
        // No markers and the file already talks about docli: appending a second section would
        // hand the reader two descriptions to reconcile.
        let mine = "# Agents\n\nThe docli mirror is read-only; ask me before touching it.\n";
        assert_eq!(merged_agents_md(Some(mine)), AgentsMd::HandWritten);
    }

    #[cfg(unix)]
    #[test]
    fn a_dangling_claude_md_symlink_is_not_a_missing_file() {
        // `exists()` FOLLOWS the link, so a dangling `CLAUDE.md` symlink reads as absent — and
        // the create would then write through it, silently producing the referent in whatever
        // directory the link points at. That is «an existing CLAUDE.md is never edited» losing
        // to a link the user made on purpose.
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("elsewhere/REAL.md");
        std::os::unix::fs::symlink(&target, tmp.path().join("CLAUDE.md")).unwrap();
        install(tmp.path()).unwrap();
        assert!(!target.exists(), "the referent must not be conjured up");
        assert!(
            tmp.path().join("CLAUDE.md").symlink_metadata().is_ok(),
            "the link itself is untouched"
        );
    }

    #[test]
    fn an_already_current_agents_md_still_gets_the_claude_bridge() {
        // The ambiguity that made this three-valued. `Current` and `HandWritten` both used to
        // come back as `None`, so an ordinary SECOND run over a perfectly good AGENTS.md was
        // read as «somebody wrote their own»: the wrong warning, and — because the bridge is
        // withheld when the section is missing — no CLAUDE.md at all for a project that
        // deserved one.
        let tmp = tempfile::tempdir().unwrap();
        install(tmp.path()).unwrap();
        std::fs::remove_file(tmp.path().join("CLAUDE.md")).unwrap();
        install(tmp.path()).unwrap();
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("CLAUDE.md")).unwrap(),
            "@AGENTS.md\n",
            "a current AGENTS.md is not a hand-written one"
        );
    }

    #[test]
    fn the_fragment_states_the_rule_and_names_the_alternative() {
        // The fragment is delivered to agents with no hook and no CLI, so it states the rule
        // and the consequence rather than promising a refusal that may not exist where it is
        // read (D9), and — like every other message — it names what to do instead.
        assert!(AGENTS_FRAGMENT.contains("read-only"));
        assert!(AGENTS_FRAGMENT.contains("edit_note"));
        assert!(AGENTS_FRAGMENT.contains("docli sync --check"));
        assert!(
            !AGENTS_FRAGMENT.contains("docli will refuse"),
            "enforcement language belongs only where a hook was actually written"
        );
    }
}
