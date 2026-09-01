// SPDX-FileCopyrightText: 2026 OOO Agitek
// SPDX-License-Identifier: MIT

//! `docli guard` (v0.28.6 D2/D2a) — the `PreToolUse` hook's decision, and the ONE place the
//! mirror-write rule is written down.
//!
//! The mirror's read-only bit is advisory (the contract says so itself: editors that replace a
//! file by deleting and recreating it walk straight through it), and a skill only speaks once
//! the model has decided to open it. This is the layer that answers at the moment of the
//! mistake, which is also the sharpest teaching moment there is — a denial arrives exactly when
//! the agent is about to lose a note.
//!
//! **Hidden from `--help`** (`#[command(hide = true)]`): its entire output is platform-specific
//! JSON on stdout, which is outside `ui.rs`'s vocabulary and meaningless to a person. `-q`,
//! `--no-color` and `--json` do not apply, because the hook schema dictates the format.
//!
//! # What it decides
//!
//! Deny when a path the tool is about to write resolves inside a mirror directory declared in
//! the governing `docli.toml`. Allow otherwise. No network, no server call, no credential read:
//! this runs on every matching tool call and has to be cheap and offline.
//!
//! **Path comparison takes BOTH of `config.rs`'s rules.** `physicalize` resolves symlinks;
//! `geometry_key` folds case and NFC on the platforms whose filesystems do. A guard that skipped
//! the fold would let `Docli-Mirror/x.md` through on APFS, and one that skipped `physicalize`
//! would miss a symlink pointing into the mirror. Either alone makes the deny decorative.
//!
//! # What it deliberately does NOT decide
//!
//! * **Shell.** Claude Code's `Bash` and Codex's `tool_input.command` hand us a command STRING.
//!   Deciding whether `sed -i … path` targets a mirror means parsing shell, which is a different
//!   slice and a worse trade — so the matchers name the structured file-editing tools and the
//!   guarantee on BOTH agents is the same: structured file-edit tools are guarded, shell writes
//!   are not. The one exception is not shell at all: Codex delivers `apply_patch` as a command
//!   string carrying a FIXED envelope grammar (`*** Update File: <path>`), and reading four
//!   literal markers out of it is parsing a patch format, not a shell.
//! * **Anything outside the governing config.** Mounts may be absolute and live outside the
//!   project root, so a mirror declared in project B's `docli.toml` is unguarded while the agent
//!   is rooted in project A. Fail-open covers it safely; it is named here so this is not read as
//!   a global mirror firewall.
//!
//! # Failure direction: ALLOW
//!
//! A missing or unparseable `docli.toml` means no mirror is KNOWN to protect, and denying under
//! uncertainty turns a docli defect into «this agent cannot write files anywhere» — worse, and
//! far more common, than what it would prevent. The one failure that is not ours to choose is a
//! MISSING BINARY: `guard` then never runs at all, and what happens is the agent's own
//! failed-hook handling. That is why the rendered hook entry is a guarded command (`hooks.rs`)
//! and why `docli status` reports whether the gate resolves.

use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde_json::{json, Value};

use crate::hooks::HookAgent;

/// The mirror-write rule, in ONE place.
///
/// D1a: the guard's deny reason and the CLI's own message for the same act are one string, so an
/// agent that hits the wall from either side learns the same rule — and every refusal names the
/// correct alternative rather than only the problem. `apply.rs` uses it when a sync destroys a
/// hand edit; this module uses it to refuse the edit in the first place.
pub const MIRROR_RULE: &str = "the docli mirror is a read-only cache: an edit made there is \
                               never synced and is destroyed with no conflict copy the next time \
                               that note changes on the server. To change a note, write through \
                               your docli MCP connection with `edit_note`, then run `docli sync`.";

/// The refusal for one path — the deny reason, and the same sentence a person sees.
pub fn mirror_write_refusal(local: &str, mount_name: &str) -> String {
    format!("{local} is inside the docli mirror `{mount_name}` - {MIRROR_RULE}")
}

/// The `PreToolUse` verdict.
#[derive(Debug, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Deny(String),
}

/// `tool_input` keys that carry a filesystem path the tool is about to write.
///
/// A FIXED set, not a recursive hunt for anything path-shaped: `path` also names a SERVER path in
/// docli's own MCP tools, and a guard that went looking would eventually rule on a call that
/// touches no filesystem at all.
const PATH_KEYS: [&str; 5] = [
    "file_path",
    "filePath",
    "notebook_path",
    "notebookPath",
    "path",
];

/// The `apply_patch` envelope markers (Codex). A fixed grammar with four spellings — reading
/// them is not shell parsing, which is the thing this slice refuses to do.
const PATCH_MARKERS: [&str; 4] = [
    "*** Add File: ",
    "*** Update File: ",
    "*** Delete File: ",
    "*** Move to: ",
];

/// Every path this tool call is about to write, in the order they appear.
pub fn candidate_paths(payload: &Value) -> Vec<String> {
    // An MCP call never writes to the local filesystem, and docli's own MCP tools take a
    // `path` that names a SERVER note. Ruling on one would be a confident wrong answer.
    if payload
        .get("tool_name")
        .and_then(|t| t.as_str())
        .is_some_and(|t| t.starts_with("mcp__"))
    {
        return Vec::new();
    }
    let Some(input) = payload.get("tool_input") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut push = |v: Option<&Value>| {
        if let Some(s) = v.and_then(|v| v.as_str()) {
            if !s.trim().is_empty() {
                out.push(s.to_string());
            }
        }
    };
    for key in PATH_KEYS {
        push(input.get(key));
    }
    // MultiEdit's per-edit paths, and any array-of-paths spelling.
    if let Some(edits) = input.get("edits").and_then(|e| e.as_array()) {
        for e in edits {
            for key in PATH_KEYS {
                push(e.get(key));
            }
        }
    }
    for key in ["file_paths", "paths"] {
        if let Some(arr) = input.get(key).and_then(|a| a.as_array()) {
            for v in arr {
                push(Some(v));
            }
        }
    }
    if let Some(cmd) = input.get("command").and_then(|c| c.as_str()) {
        out.extend(patch_envelope_paths(cmd));
    }
    out
}

/// Follow a symlink chain by hand, so a DANGLING link cannot smuggle a write into the mirror.
///
/// `physicalize` resolves symlinks by canonicalizing the nearest EXISTING ancestor — and
/// `exists()` FOLLOWS links, so `shortcut -> mirror/new.md` whose target does not exist yet reads
/// as «nothing here», canonicalizes the parent, and comes back as `<project>/shortcut`.
/// Containment then says no and the guard allows — while the agent's `Write` follows the link and
/// creates `mirror/new.md`, which is the one thing this module exists to prevent. Creating a file
/// through a symlink is ordinary behaviour, not a trick.
///
/// Bounded, and it degrades to the literal path on a cycle — the same shape `agents.rs` uses for
/// config destinations, for the same reason. A relative target resolves against the LINK's own
/// directory, which is what the kernel does.
fn follow_links(path: &Path) -> PathBuf {
    let mut p = path.to_path_buf();
    for _ in 0..8 {
        match std::fs::read_link(&p) {
            Ok(next) => {
                p = if next.is_absolute() {
                    next
                } else {
                    p.parent().map(|d| d.join(&next)).unwrap_or(next)
                };
            }
            Err(_) => break,
        }
    }
    p
}

/// The paths an `apply_patch` envelope names, and nothing else. A command string that carries no
/// marker contributes nothing — we do not read shell.
fn patch_envelope_paths(command: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in command.lines() {
        // At COLUMN ZERO, never after a trim. Inside a hunk, apply_patch prefixes context lines
        // with a space and removed lines with `-`, so a patch to some unrelated file that
        // happens to CONTAIN the text `*** Update File: mirror/x.md` carries it as ` *** Update
        // File: …` — and trimming turned that quoted content into a header naming a mirror path,
        // denying a write that was never going near the mirror.
        let line = line.trim_end();
        for marker in PATCH_MARKERS {
            if let Some(rest) = line.strip_prefix(marker) {
                let p = rest.trim();
                if !p.is_empty() {
                    out.push(p.to_string());
                }
            }
        }
    }
    out
}

/// A mount's physicalized directory and the AUTHOR's name for it.
type Mirror = (PathBuf, String);

/// What the governing `docli.toml` says: its root, the parsed config, and each mount's
/// physicalized directory paired with its display name. `None` on ANY uncertainty — no project,
/// an unreadable file, a config that will not parse — which is the allow direction.
fn project_mirrors(cwd: &Path) -> Option<(PathBuf, crate::config::DocliToml, Vec<Mirror>)> {
    let root = crate::config::find_project(cwd)?;
    let raw = std::fs::read_to_string(root.join(crate::config::CONFIG_NAME)).ok()?;
    let config = crate::config::parse_config(&raw).ok()?;
    let mirrors = config
        .mounts
        .iter()
        .map(|m| {
            (
                crate::config::physicalize(&crate::config::mount_abs(&root, m)),
                // Sanitized: `docli.toml` is committed and teammate-editable, and this name
                // travels into a reason string an agent renders. (`parse_config` already
                // refuses control characters; this is the second reader of the same rule.)
                crate::ui::sanitize(m.display_name()),
            )
        })
        .collect();
    Some((root, config, mirrors))
}

/// The decision, given the hook's `cwd` and the payload.
pub fn decide(cwd: &Path, payload: &Value) -> Decision {
    let Some((root, config, mirrors)) = project_mirrors(cwd) else {
        return Decision::Allow;
    };
    if mirrors.is_empty() {
        return Decision::Allow;
    }
    for candidate in candidate_paths(payload) {
        let raw = Path::new(&candidate);
        let abs = if raw.is_absolute() {
            raw.to_path_buf()
        } else {
            cwd.join(raw)
        };
        let target = crate::config::physicalize(&follow_links(&abs));
        for (mirror, name) in &mirrors {
            if crate::config::is_ancestor_or_self(mirror, &target) {
                // Containment says DENY — so now, and ONLY now, ask whether this configuration
                // is one anybody could sync with. A config that PARSES is not a config that is
                // USABLE: `dir = "."` parses fine and every other command refuses it (it
                // contains `docli.toml` and `.docli/`), but to a guard that only parsed, the
                // whole project is «inside the mirror» and every structured write is denied —
                // `docli.toml` itself included, so the agent could not even fix the file that
                // broke it. That is exactly «denying under uncertainty turns a docli defect into
                // *this agent cannot write files anywhere*».
                //
                // The ORDER is deliberate, and it is a cost decision as much as a clarity one.
                // The overlap rule is quadratic in the mount count and this runs on every
                // matched tool call, so validating up front would make a config with many
                // mounts pay it on every ALLOW — the overwhelmingly common answer. Here the
                // common path is one physicalize and a comparison per mount, and the quadratic
                // work happens only on the rare call that is about to be refused anyway.
                //
                // The git-ignore half is deliberately excluded: it shells out to `git`, and an
                // unignored mirror is a sync-safety question, not a containment one.
                if crate::config::validate_geometry_paths_only(&root, &config).is_err() {
                    return Decision::Allow;
                }
                return Decision::Deny(mirror_write_refusal(&candidate, name));
            }
        }
    }
    Decision::Allow
}

/// The response body for a decision, in the agent's own schema.
///
/// Both vendors document the SAME envelope for `PreToolUse` (verified 2026-09-01 against
/// `code.claude.com/docs/en/hooks` and `learn.chatgpt.com/docs/hooks.md`), so today the two arms
/// render identically. That is an observed fact about two products, not a promise either makes,
/// which is why the discriminator exists at all: the day one of them moves, this is a match arm
/// rather than a redesign.
///
/// ALLOW prints nothing. On Claude Code an empty stdout with exit 0 is «no decision, normal
/// permission flow applies», which is exactly right — the guard has an opinion about the mirror
/// and no opinion about anything else, and printing an `allow` would override the user's own
/// permission rules for every edit in the repository.
pub fn response(agent: HookAgent, decision: &Decision) -> Option<String> {
    match decision {
        Decision::Allow => None,
        Decision::Deny(reason) => {
            let body = match agent {
                HookAgent::Claude | HookAgent::Codex => json!({
                    "hookSpecificOutput": {
                        "hookEventName": "PreToolUse",
                        "permissionDecision": "deny",
                        "permissionDecisionReason": reason,
                    }
                }),
            };
            Some(body.to_string())
        }
    }
}

/// `docli guard --agent <a> --tool-input -`.
///
/// Always exits 0. On Claude Code a deny is carried by the JSON, not by the exit code (exit 2
/// would block too, but unconditionally and without our reason surviving as a decision); on
/// Codex the same envelope decides. A non-zero exit here would only ever be read as a broken
/// hook.
pub fn run(agent: HookAgent, tool_input: &str) -> Result<i32> {
    let raw = if tool_input == "-" {
        let mut buf = String::new();
        // A read failure is the allow direction like every other uncertainty here.
        if std::io::stdin().read_to_string(&mut buf).is_err() {
            return Ok(0);
        }
        buf
    } else {
        match std::fs::read_to_string(tool_input) {
            Ok(s) => s,
            Err(_) => return Ok(0),
        }
    };
    let Ok(payload) = serde_json::from_str::<Value>(&raw) else {
        return Ok(0);
    };
    // The hook's own `cwd` is the authority on which project governs — the guard process
    // inherits a working directory that need not be the agent's.
    let cwd = payload
        .get("cwd")
        .and_then(|c| c.as_str())
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."));
    if let Some(body) = response(agent, &decide(&cwd, &payload)) {
        println!("{body}");
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A project with one mirror at `<tmp>/mirror`.
    fn project() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("docli.toml"),
            "server = \"https://docli.ru\"\n\n[[mount]]\nworkspace = \
             \"00000000-0000-0000-0000-000000000001\"\ndir = \"mirror\"\nname = \"заметки\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(tmp.path().join("mirror/sub")).unwrap();
        tmp
    }

    fn edit(path: &str) -> Value {
        json!({"tool_name": "Edit", "tool_input": {"file_path": path}})
    }

    #[test]
    fn a_write_inside_the_mirror_is_denied_and_the_reason_teaches() {
        let tmp = project();
        let d = decide(tmp.path(), &edit("mirror/sub/note.md"));
        let Decision::Deny(reason) = d else {
            panic!("must deny: {d:?}");
        };
        // Every refusal names the correct ALTERNATIVE, not just the problem (D1a).
        assert!(reason.contains("edit_note"), "{reason}");
        assert!(reason.contains("docli sync"), "{reason}");
        // …and the AUTHOR's mount name, whatever language it is in.
        assert!(reason.contains("заметки"), "{reason}");
        // Absolute spellings too.
        assert!(matches!(
            decide(
                tmp.path(),
                &edit(&tmp.path().join("mirror/x.md").display().to_string())
            ),
            Decision::Deny(_)
        ));
        // The mirror ROOT itself, and a not-yet-existing path inside it.
        assert!(matches!(
            decide(tmp.path(), &edit("mirror/brand/new/file.md")),
            Decision::Deny(_)
        ));
    }

    #[test]
    fn writes_outside_the_mirror_are_allowed() {
        let tmp = project();
        for p in [
            "src/main.rs",
            "docli.toml",
            "../elsewhere/x.md",
            "mirrorless/x.md",
            "mirror-adjacent/x.md",
        ] {
            assert_eq!(decide(tmp.path(), &edit(p)), Decision::Allow, "{p}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_dangling_symlink_into_the_mirror_is_denied_too() {
        // The one `physicalize` alone cannot see: `exists()` FOLLOWS the link, so a link whose
        // target does not exist YET reads as «nothing here» and resolves back to the link's own
        // path. The guard then allowed — while the agent's `Write` followed the link and created
        // the file inside the mirror. Writing a NEW file through a symlink is ordinary.
        let tmp = project();
        std::os::unix::fs::symlink(
            tmp.path().join("mirror/brand-new.md"),
            tmp.path().join("shortcut.md"),
        )
        .unwrap();
        assert!(!tmp.path().join("mirror/brand-new.md").exists());
        assert!(matches!(
            decide(tmp.path(), &edit("shortcut.md")),
            Decision::Deny(_)
        ));
        // …and a RELATIVE link resolves against the link's own directory, not the cwd.
        std::fs::create_dir_all(tmp.path().join("sub")).unwrap();
        std::os::unix::fs::symlink("../mirror/other.md", tmp.path().join("sub/rel.md")).unwrap();
        assert!(matches!(
            decide(tmp.path(), &edit("sub/rel.md")),
            Decision::Deny(_)
        ));
        // A dangling link pointing OUTSIDE the mirror is still allowed — the fix must not turn
        // «follow the link» into «deny anything symbolic».
        std::os::unix::fs::symlink("../elsewhere/x.md", tmp.path().join("sub/out.md")).unwrap();
        assert_eq!(decide(tmp.path(), &edit("sub/out.md")), Decision::Allow);
        // A CYCLE degrades to the literal path rather than looping.
        std::os::unix::fs::symlink("loop-b.md", tmp.path().join("loop-a.md")).unwrap();
        std::os::unix::fs::symlink("loop-a.md", tmp.path().join("loop-b.md")).unwrap();
        assert_eq!(decide(tmp.path(), &edit("loop-a.md")), Decision::Allow);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_into_the_mirror_is_still_denied() {
        // `physicalize`'s half: a lexical comparison never meets the mirror at all.
        let tmp = project();
        let link = tmp.path().join("shortcut");
        std::os::unix::fs::symlink(tmp.path().join("mirror"), &link).unwrap();
        assert!(matches!(
            decide(tmp.path(), &edit("shortcut/note.md")),
            Decision::Deny(_)
        ));
    }

    #[cfg(any(target_os = "macos", target_os = "windows"))]
    #[test]
    fn case_varied_and_decomposed_spellings_are_denied_where_the_filesystem_folds() {
        // `geometry_key`'s half: `Mirror/x.md` IS `mirror/x.md` on APFS, and a guard without
        // the fold is decorative there. On a case-SENSITIVE filesystem they are honestly
        // different files, which is why this test is platform-gated rather than universal.
        let tmp = project();
        assert!(matches!(
            decide(tmp.path(), &edit("MIRROR/note.md")),
            Decision::Deny(_)
        ));
        assert!(matches!(
            decide(tmp.path(), &edit("Mirror/note.md")),
            Decision::Deny(_)
        ));
    }

    #[test]
    fn uncertainty_allows_rather_than_bricking_the_agent() {
        // No docli.toml at all…
        let empty = tempfile::tempdir().unwrap();
        assert_eq!(decide(empty.path(), &edit("mirror/x.md")), Decision::Allow);
        // …and one that will not parse. Denying under uncertainty would turn a docli defect
        // into «this agent cannot write files anywhere».
        let broken = tempfile::tempdir().unwrap();
        std::fs::write(broken.path().join("docli.toml"), "server = [[[").unwrap();
        assert_eq!(decide(broken.path(), &edit("mirror/x.md")), Decision::Allow);
        // A config with no mounts guards nothing.
        let nomounts = tempfile::tempdir().unwrap();
        std::fs::write(
            nomounts.path().join("docli.toml"),
            "server = \"https://docli.ru\"\n",
        )
        .unwrap();
        assert_eq!(
            decide(nomounts.path(), &edit("mirror/x.md")),
            Decision::Allow
        );
    }

    #[test]
    fn the_multi_edit_and_notebook_shapes_are_seen() {
        let tmp = project();
        let multi = json!({
            "tool_name": "MultiEdit",
            "tool_input": {"edits": [
                {"file_path": "src/ok.rs"},
                {"file_path": "mirror/note.md"}
            ]}
        });
        assert!(matches!(decide(tmp.path(), &multi), Decision::Deny(_)));
        let nb =
            json!({"tool_name": "NotebookEdit", "tool_input": {"notebook_path": "mirror/n.ipynb"}});
        assert!(matches!(decide(tmp.path(), &nb), Decision::Deny(_)));
    }

    #[test]
    fn codexs_apply_patch_envelope_is_read_but_shell_is_not() {
        let tmp = project();
        // The envelope is a FIXED grammar, and reading it is not parsing shell.
        let patch = json!({
            "tool_name": "apply_patch",
            "tool_input": {"command": "apply_patch <<'EOF'\n*** Begin Patch\n*** Update File: mirror/note.md\n@@\n-a\n+b\n*** End Patch\nEOF"}
        });
        assert!(matches!(decide(tmp.path(), &patch), Decision::Deny(_)));
        for marker in ["*** Add File: ", "*** Delete File: ", "*** Move to: "] {
            let v = json!({"tool_name": "apply_patch",
                           "tool_input": {"command": format!("{marker}mirror/x.md")}});
            assert!(
                matches!(decide(tmp.path(), &v), Decision::Deny(_)),
                "{marker}"
            );
        }
        // A patch to an UNRELATED file that merely QUOTES a header must not be read as one:
        // inside a hunk, context lines carry a leading space and changed lines `-`/`+`, so
        // trimming before matching turned quoted content into a target.
        for prefix in [" ", "-", "+"] {
            let v = json!({"tool_name": "apply_patch", "tool_input": {"command":
                format!("*** Begin Patch\n*** Update File: docs/howto.md\n@@\n{prefix}*** Update File: mirror/note.md\n*** End Patch")}});
            assert_eq!(
                decide(tmp.path(), &v),
                Decision::Allow,
                "a {prefix:?}-prefixed line is patch CONTENT, not a header"
            );
        }
        // …and a SHELL write into the mirror is NOT covered, on either agent. This is the
        // stated limit of the guarantee, pinned so nobody later reads the guard as complete.
        let shell = json!({
            "tool_name": "Bash",
            "tool_input": {"command": "sed -i '' s/a/b/ mirror/note.md"}
        });
        assert_eq!(decide(tmp.path(), &shell), Decision::Allow);
    }

    #[test]
    fn an_mcp_call_is_never_ruled_on() {
        // docli's own MCP tools take a `path` naming a SERVER note; the guard has no opinion.
        let tmp = project();
        let v =
            json!({"tool_name": "mcp__docli__edit_note", "tool_input": {"path": "mirror/x.md"}});
        assert_eq!(decide(tmp.path(), &v), Decision::Allow);
    }

    #[test]
    fn the_response_is_the_documented_envelope_on_both_agents() {
        let deny = Decision::Deny("because".into());
        for agent in HookAgent::all() {
            let body = response(agent, &deny).expect("a deny has a body");
            let v: Value = serde_json::from_str(&body).unwrap();
            assert_eq!(v["hookSpecificOutput"]["hookEventName"], "PreToolUse");
            assert_eq!(v["hookSpecificOutput"]["permissionDecision"], "deny");
            assert_eq!(
                v["hookSpecificOutput"]["permissionDecisionReason"],
                "because"
            );
            // ALLOW prints nothing: an explicit `allow` would override the user's own
            // permission rules for every edit in the repository.
            assert_eq!(response(agent, &Decision::Allow), None);
        }
        // Today the two arms are byte-identical — an OBSERVED fact about two vendors
        // (2026-09-01), recorded here so a future divergence is a visible test change.
        assert_eq!(
            response(HookAgent::Claude, &deny),
            response(HookAgent::Codex, &deny)
        );
    }

    #[test]
    fn a_malformed_payload_allows_silently() {
        let tmp = project();
        // No tool_input, no cwd, wrong types — every one of them allows.
        for v in [
            json!({}),
            json!({"tool_input": 42}),
            json!({"tool_input": {"file_path": 7}}),
            json!({"tool_input": {"file_path": "   "}}),
        ] {
            assert_eq!(decide(tmp.path(), &v), Decision::Allow, "{v}");
        }
    }

    #[test]
    fn the_rule_is_one_constant_shared_with_the_cli_s_own_message() {
        // D1a's pin: the guard's deny reason and the CLI's own message for the same act are
        // ONE string, so an agent that hits the wall from either side learns the same rule.
        let reason = mirror_write_refusal("mirror/n.md", "notes");
        assert!(reason.contains(MIRROR_RULE));
        assert!(crate::apply::hand_edit_overwritten("n.md").contains(MIRROR_RULE));
    }
}
