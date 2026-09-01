// SPDX-FileCopyrightText: 2026 OOO Agitek
// SPDX-License-Identifier: MIT

//! Agent HOOKS (v0.28.6 D2/D3/D6) — render, merge and remove the two hook entries `docli init`
//! writes, on the two agents this slice covers.
//!
//! Why hooks at all: a skill is context, a hook is configuration. The rule whose violation is
//! silent data loss («never edit the mirror») cannot be carried by a document the model must
//! first decide to open, and «run `docli sync --check` at session start» has no session-start
//! trigger inside a skill at all. Both vendors say so themselves — *"To block an action
//! regardless of what Claude decides, use a PreToolUse hook instead."*
//!
//! **This module is the ONLY writer of hooks**, and `docli init` is its only caller.
//!
//! # The vendor facts this file is built on (D10 — dated, from vendor documentation)
//!
//! Verified 2026-09-01 against `code.claude.com/docs/en/hooks` and
//! `learn.chatgpt.com/docs/hooks.md`:
//!
//! * Both agents take the same config SHAPE — `hooks.<Event>` is an array of matcher objects,
//!   each holding a `hooks` array of `{"type": "command", "command": …}`. They differ only in
//!   the FILE: Claude Code merges ours into `.claude/settings.json` (where the user's own
//!   settings live), Codex into `.codex/hooks.json` (a file whose whole purpose is hooks).
//! * Both accept the SAME response envelope — `hookSpecificOutput` carrying
//!   `permissionDecision`/`permissionDecisionReason` for `PreToolUse`, and `additionalContext`
//!   for `SessionStart`. That convergence is an OBSERVED fact about two products, not a
//!   guarantee, which is why `docli guard` still takes an explicit `--agent` (D2a): the day one
//!   of them moves, the fork is a match arm rather than a redesign.
//! * Matchers are load-bearing and differ. Claude Code's file-editing tools are
//!   `Write|Edit|MultiEdit|NotebookEdit`. Codex's `PreToolUse` also fires for Bash and MCP
//!   calls, so an unmatched Codex entry would spawn `docli guard` on every shell command — the
//!   opposite of D2's «has to be cheap». Codex documents `apply_patch`, `Edit` and `Write` as
//!   accepted matcher spellings for the edit tool, so that is the set we name.
//! * `SessionStart` matchers are five (`startup`, `resume`, `clear`, `compact`, `fork`). We
//!   take **`startup|resume`** deliberately: freshness belongs where a session BEGINS, `clear`
//!   and `compact` are context operations rather than new work, and `fork` inherits an
//!   already-checked parent. Firing on all five would multiply the ephemeral pulls and the
//!   `read_audit` rows per session for no new information.
//! * Project-local hooks on BOTH agents run only after the user has trusted the project layer.
//!   `docli init` writing a hook therefore cannot smuggle execution onto a machine — and we ask
//!   anyway, unticked (D6), because a config entry names a server while a hook runs a program.
//!
//! # The guarded command (D2)
//!
//! The entry is never a bare `docli …`. `docli uninstall` deliberately leaves agent
//! configurations in place, and a teammate cloning the repository has the hook entry without the
//! binary — both routine states. So the rendered command resolves `docli` first and exits 0 when
//! it is absent. The trade is stated rather than hidden: this converts a visible per-call notice
//! into silence, and the counterweight is `docli status`, which reports whether the entries are
//! present and whether the binary they name resolves.

use std::path::Path;

use anyhow::{Context, Result};
use serde_json::{json, Value};

/// Which agent a hook payload or hook entry belongs to.
///
/// Kept as a type rather than a string because three commands take it (`guard`, the freshness
/// emission, and the renderer here) and «which agent» must never be guessed from a payload
/// shape: we render the entry ourselves, so the discriminator is free.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HookAgent {
    Claude,
    Codex,
}

impl HookAgent {
    pub fn parse(s: &str) -> Result<Self> {
        match s.trim() {
            "claude" => Ok(HookAgent::Claude),
            "codex" => Ok(HookAgent::Codex),
            other => anyhow::bail!("unknown agent {other:?} - use `claude` or `codex`"),
        }
    }

    pub fn key(self) -> &'static str {
        match self {
            HookAgent::Claude => "claude",
            HookAgent::Codex => "codex",
        }
    }

    pub fn display(self) -> &'static str {
        match self {
            HookAgent::Claude => "Claude Code",
            HookAgent::Codex => "Codex CLI",
        }
    }

    /// The project-relative file this agent's hooks live in.
    pub fn config_path(self) -> &'static str {
        match self {
            // The user's own settings file. Ours is a splice into `hooks`; everything else in
            // it is somebody's careful configuration and is preserved key for key.
            HookAgent::Claude => ".claude/settings.json",
            // A file whose entire purpose is hooks (Codex's MCP config is elsewhere, in
            // `.codex/config.toml` — see `agents.rs`).
            HookAgent::Codex => ".codex/hooks.json",
        }
    }

    /// The `PreToolUse` matcher — the FILE-EDITING tools and nothing else.
    fn pre_tool_use_matcher(self) -> &'static str {
        match self {
            HookAgent::Claude => "Write|Edit|MultiEdit|NotebookEdit",
            // Codex's PreToolUse also intercepts Bash and MCP calls; naming the edit tool
            // explicitly is what keeps `docli guard` off every shell command.
            HookAgent::Codex => "apply_patch|Edit|Write",
        }
    }

    pub fn all() -> [HookAgent; 2] {
        [HookAgent::Claude, HookAgent::Codex]
    }
}

/// The two hook events this slice installs. Ordered as they are written.
const EVENTS: [&str; 2] = ["PreToolUse", "SessionStart"];

/// The rendered command for one event, guarded so a missing binary is silence rather than a
/// broken tool call on every edit (D2).
///
/// **Platform-INDEPENDENT, and that is a correction.** The first version rendered this from
/// `cfg!(windows)` — the platform of whoever ran `docli init`. But `.claude/settings.json` and
/// `.codex/hooks.json` are PROJECT files, usually committed; this module's own reasoning turns
/// on a teammate cloning the repository. So a macOS-initialized repo handed every Windows
/// teammate a `command -v … >/dev/null` line, which their shell fails and `|| exit 0` swallows:
/// exit 0, no output, **no gate**, while `docli status` reported the entries present and the
/// binary resolving. That is the exact silence D2 built the guarded command to avoid, reached
/// from the other side — and it is worse than the case D2 was guarding against, because nothing
/// can see it.
///
/// The two agents each document a way out, and they are different ways (verified 2026-09-01):
///
/// * **Codex** documents `commandWindows`, *"Windows-only command override"*, on the handler.
///   Both forms ship in every entry, so the file is correct on every machine that reads it.
/// * **Claude Code** documents no per-platform override, but it does document `shell`
///   (`"bash"` | `"powershell"`), and its own default is *"bash, or powershell on Windows when
///   Git Bash isn't installed"*. So the POSIX form plus an explicit `shell: "bash"` is right on
///   every Unix machine and on Windows-with-Git-Bash — and on Windows WITHOUT Git Bash the hook
///   fails to start, which their docs put in the non-blocking bucket: a visible notice, and the
///   tool call proceeds. That is the honest degradation. A gate that announces it cannot run is
///   strictly better than one that silently is not there, which is the whole lesson of the
///   defect this slice exists to fix.
fn command_for(agent: HookAgent, event: &str) -> String {
    // `if …; then …; fi`, NOT `A && B || exit 0`. The trailing `|| exit 0` swallowed every
    // non-zero exit, so it hid two very different things: «docli is not installed» (which must
    // be silent — a teammate's clone, or after `docli uninstall`) and «docli is installed and
    // FAILED» (a broken shebang, a corrupt binary, a half-finished self-update), which must
    // not be. This form swallows only the first: the `if` is simply false and nothing runs.
    //
    // Nothing is lost by letting the second surface, because both of our commands exit 0 by
    // contract — `guard` always, and `sync --check --agent` always (a stale mirror is reported
    // in the payload, never in the exit code). So a non-zero exit from either genuinely means
    // something broke, and a visible hook error is the right answer to that.
    format!(
        "if command -v docli >/dev/null 2>&1; then {}; fi",
        inner_command(agent, event)
    )
}

/// The `cmd.exe` spelling, for the one agent that documents a slot to put it in.
fn command_windows_for(agent: HookAgent, event: &str) -> String {
    // `where` is the `command -v` of cmd.exe, and `|| exit /b 0` keeps a stale entry inert.
    //
    // RESIDUAL, stated rather than hidden: unlike the POSIX form above, this one still swallows
    // a FAILED docli as well as a missing one — `cmd.exe` has no `if …; then …; fi` that fits
    // cleanly on one line, and this project has no Windows runner to verify a cleverer spelling
    // against. It joins the named manual checks in `/release-cli` rather than shipping an
    // untested construct into somebody's config.
    format!(
        "where docli >nul 2>nul && {} || exit /b 0",
        inner_command(agent, event)
    )
}

fn inner_command(agent: HookAgent, event: &str) -> String {
    match event {
        "PreToolUse" => format!("docli guard --agent {} --tool-input -", agent.key()),
        "SessionStart" => format!("docli sync --check --agent {}", agent.key()),
        other => unreachable!("unknown hook event {other}"),
    }
}

/// One matcher object.
fn entry_for(agent: HookAgent, event: &str) -> Value {
    let matcher = match event {
        "PreToolUse" => agent.pre_tool_use_matcher().to_string(),
        // Freshness belongs where a session BEGINS — see the module docs for why the other
        // three documented sources are deliberately absent.
        "SessionStart" => "startup|resume".to_string(),
        other => unreachable!("unknown hook event {other}"),
    };
    let mut hook = serde_json::Map::new();
    hook.insert("type".into(), json!("command"));
    hook.insert("command".into(), json!(command_for(agent, event)));
    match agent {
        // The documented Windows override — so a repository initialized on one platform is
        // still correct for a teammate on the other.
        HookAgent::Codex => {
            hook.insert(
                "commandWindows".into(),
                json!(command_windows_for(agent, event)),
            );
        }
        // No per-platform field exists here, so the shell is NAMED instead of inherited from a
        // default that varies by machine.
        HookAgent::Claude => {
            hook.insert("shell".into(), json!("bash"));
        }
    }
    // A bound the hook itself also enforces in-process (D3): this key is the harness's outer
    // backstop, never the thing that makes the budget true.
    hook.insert("timeout".into(), json!(10));
    // DOCUMENTED FIELDS ONLY — see `ours_commands` for why there is no identity key here.
    json!({
        "matcher": matcher,
        "hooks": [Value::Object(hook)],
    })
}

/// The exact commands we generate — identity is EQUALITY against this closed set, never a
/// substring of it.
///
/// # Why not a marker key
///
/// `agents.rs`'s `merge_json` gets identity for free: it keys ONE named entry (`"docli"`) under a
/// top-level object. Hook configuration is an ARRAY of matcher objects, so there is no name to
/// key on — and both an idempotent re-run and `docli uninstall`'s removal are unimplementable
/// without some way to say «this element is ours».
///
/// The obvious answer, an extra `"docliManaged"` key on the matcher object, was **written and
/// then withdrawn**. Claude Code documents two failure tiers for `settings.json`: a *Settings
/// Warning*, where only the offending entry is skipped, and a **Settings Error** — *"a value the
/// schema rejects"* — where the session starts *"without the broken settings"*, i.e. the user
/// loses their whole permissions/model/status-line configuration. Nothing in the vendor
/// documentation says an unknown key inside a matcher object lands in the first tier rather than
/// the second. That is exactly the class of unverified vendor claim D10 was minted to stop, and
/// this slice mints D10 — so betting somebody's settings file on it was not available.
///
/// # Why EQUALITY, and why it tightened twice
///
/// `merge` REPLACES what it thinks is ours and `remove` DELETES it, so every loosening of this
/// predicate is a way to destroy somebody's configuration. A `contains` test looked precise
/// enough while the needle was long, but `printf 'docli guard --agent claude --tool-input -' >>
/// audit.log` is a hook a person could plausibly write, and it contains the needle. Equality
/// against the closed set of strings we actually generate cannot be tricked that way.
///
/// The set is every command we have ever rendered for a live entry: each agent's invocation
/// bare, and wrapped in either guard prefix. The bare form is kept so an entry written before
/// the guarded form, or hand-copied without it, still converges.
///
/// The cost is stated rather than hidden: an entry of ours that somebody has EDITED — appended
/// a redirect, changed the flags — stops being recognised, so a re-run adds a clean one beside
/// it instead of replacing it. That is the harmless direction. Nothing of theirs is ever eaten.
fn ours_commands(agent: HookAgent, event: &str) -> [String; 3] {
    [
        inner_command(agent, event),
        command_for(agent, event),
        command_windows_for(agent, event),
    ]
}

/// Would this element actually FIRE, and run our command? The health question, as against the
/// ownership question [`is_ours`] answers.
fn effectively_ours(v: &Value, agent: HookAgent, event: &str) -> bool {
    let desired = entry_for(agent, event);
    if v.get("matcher") != desired.get("matcher") {
        return false;
    }
    let Some(handler) = v
        .get("hooks")
        .and_then(|h| h.as_array())
        .and_then(|h| h.first())
    else {
        return false;
    };
    // EVERY field we write, except `timeout`. Enumerating them one at a time kept missing one
    // — first `type` (a handler that is not a command cannot run one), then `shell` (a Claude
    // entry switched to `powershell` cannot parse `if command -v …; then …; fi`, so neither
    // hook runs) — and each miss was `status` reporting a gate that does not guard. So the rule
    // is stated once, as a rule: what we set is what has to match.
    //
    // `timeout` is the single deliberate exception: an entry with `20` instead of `10` fires
    // exactly as ours does, and calling it absent would send a reader to fix a working gate.
    let (Some(want), Some(got)) = (desired["hooks"][0].as_object(), handler.as_object()) else {
        return false;
    };
    // No EXTRA fields either, and that half matters as much as the matching half: `async: true`
    // is a documented Claude Code option that runs the hook WITHOUT waiting, so its denial
    // cannot block the write. A predicate that checked only OUR fields would call that gate
    // healthy. We cannot know what a future field does, so the honest health answer for a
    // handler carrying one we did not write is «I cannot vouch for this» — a false negative,
    // which costs a nudge, rather than a false positive, which costs a note.
    if got.keys().any(|k| k != "timeout" && !want.contains_key(k)) {
        return false;
    }
    want.iter()
        .filter(|(k, _)| k.as_str() != "timeout")
        .all(|(k, v)| got.get(k) == Some(v))
}

/// Is this array element one of ours? See [`OURS`] for what the answer rests on and why.
fn is_ours(v: &Value, agent: HookAgent, event: &str) -> bool {
    let Some(hooks) = v.get("hooks").and_then(|h| h.as_array()) else {
        return false;
    };
    // EXACTLY ONE handler, because that is what we write. A matcher object of ours that has
    // grown a SECOND handler is no longer ours to rewrite: `merge` replaces the whole matcher
    // and `remove` deletes it wholesale, so claiming it would silently take the handler the
    // user added beside ours with it. Leaving it alone costs a duplicate entry on the next
    // re-run; claiming it costs somebody their hook.
    let [handler] = hooks.as_slice() else {
        return false;
    };
    handler
        .get("command")
        .and_then(|c| c.as_str())
        .is_some_and(|c| ours_commands(agent, event).iter().any(|o| o == c))
}

/// What a merge decided, mirroring `agents::MergeOutcome` so `init` renders both the same way.
#[derive(Debug, PartialEq, Eq)]
pub enum HookOutcome {
    /// The full new file text.
    Write(String),
    /// Our entries are already exactly what we would write.
    AlreadyInstalled,
    /// The file exists in a shape we will not rewrite; `init` prints the snippet instead.
    Occupied(String),
}

/// Merge our two entries into `existing`.
///
/// **A SPLICE, not a rewrite** — the same discipline `agents.rs` applies to `.mcp.json`, and for
/// the same reason, only more so: `.claude/settings.json` is where a person keeps their
/// permission rules, their status line and their model choice, hand-formatted. Re-serializing it
/// would re-order every key and flatten every deliberate blank line, which is a hostile thing to
/// do to somebody's configuration on the way to installing a convenience.
///
/// The structure is harder than `agents.rs`'s: there, identity comes free from a named entry
/// under a named object. Here `hooks.<Event>` is an ARRAY, so there is no name to key on — which
/// is what [`ours_commands`] exists for — and the splice has to locate one ELEMENT inside it. It
/// does that by walking the array's value extents (the same tiny JSON walk `agents.rs` uses) and
/// parsing each element to ask [`is_ours`].
///
/// Anything it cannot locate unambiguously falls to the print branch. A file we author from
/// scratch is pretty-printed, because there is no formatting to preserve.
pub fn merge(agent: HookAgent, existing: Option<&str>) -> HookOutcome {
    let text = existing.map(str::trim).filter(|t| !t.is_empty());
    let Some(text) = text else {
        let mut hooks = serde_json::Map::new();
        for event in EVENTS {
            hooks.insert(event.to_string(), json!([entry_for(agent, event)]));
        }
        let doc = json!({ "hooks": Value::Object(hooks) });
        let mut out = serde_json::to_string_pretty(&doc).expect("a literal serializes");
        out.push('\n');
        return HookOutcome::Write(out);
    };
    let mut out = text.to_string();
    let mut changed = false;
    // A file with NO `hooks` key at all gets the whole subtree in ONE splice — both events, in
    // order, pretty-printed. Letting the per-event loop below build it instead produced the
    // events in reverse (each one splices to the front of what the previous one wrote) and
    // crammed 400 characters onto the opening brace's line.
    match whole_block(&out, agent) {
        Ok(Some(next)) => {
            out = next;
            changed = true;
        }
        Ok(None) => {}
        Err(reason) => return HookOutcome::Occupied(reason),
    }
    // Everything else is spliced per event, with the text re-scanned between them: offsets
    // shift under an edit, and re-locating is both simpler and safer than tracking deltas.
    //
    // Each call makes at most ONE edit and reports whether anything is left to do, so the loop
    // is what makes the merge CONVERGENT. It matters for the case where more than one element
    // is already ours — residue from an older shape, or from a hand-installed copy: replacing
    // the first and leaving the rest would mean `init` reported «wrote hooks» on every re-run
    // forever, and two `docli guard` processes per tool call. The bound is a runaway guard, not
    // an expected count.
    for event in EVENTS {
        let mut settled = false;
        for _ in 0..16 {
            match splice_event(&out, agent, event) {
                Ok(Some(next)) => {
                    out = next;
                    changed = true;
                }
                Ok(None) => {
                    settled = true;
                    break;
                }
                Err(reason) => return HookOutcome::Occupied(reason),
            }
        }
        if !settled {
            return HookOutcome::Occupied(format!(
                "\"hooks.{event}\" did not converge after 16 edits"
            ));
        }
    }
    if !changed {
        return HookOutcome::AlreadyInstalled;
    }
    ensure_trailing_newline(&mut out);
    HookOutcome::Write(out)
}

/// The one-splice path for a file that carries no `hooks` key yet. `Ok(None)` = it has one.
fn whole_block(text: &str, agent: HookAgent) -> Result<Option<String>, String> {
    let root: Value = serde_json::from_str(text)
        .map_err(|_| "does not parse as strict JSON (it may contain comments)".to_string())?;
    let Some(obj) = root.as_object() else {
        return Err("root is not a JSON object".to_string());
    };
    if obj.contains_key("hooks") {
        return Ok(None);
    }
    let mut hooks = serde_json::Map::new();
    for event in EVENTS {
        hooks.insert(event.to_string(), json!([entry_for(agent, event)]));
    }
    let pretty = serde_json::to_string_pretty(&Value::Object(hooks))
        .expect("a literal serializes")
        .replace('\n', "\n  ");
    let brace = text.find('{').ok_or("no root object")?;
    let insert = format!(
        "\n  \"hooks\": {pretty}{}",
        if obj.is_empty() { "" } else { "," }
    );
    Ok(Some(splice(text, brace + 1, brace + 1, &insert)))
}

fn ensure_trailing_newline(s: &mut String) {
    if !s.ends_with('\n') {
        s.push('\n');
    }
}

/// Splice ONE event's entry into `text`. `Ok(None)` = already exactly ours.
fn splice_event(text: &str, agent: HookAgent, event: &str) -> Result<Option<String>, String> {
    let desired = entry_for(agent, event);
    let rendered = desired.to_string();
    // A full parse first, purely to REFUSE the shapes we will not touch. The splice below then
    // works on the text, so the user's own bytes are what survives.
    let root: Value = serde_json::from_str(text)
        .map_err(|_| "does not parse as strict JSON (it may contain comments)".to_string())?;
    let Some(obj) = root.as_object() else {
        return Err("root is not a JSON object".to_string());
    };
    let hooks = match obj.get("hooks") {
        // Unreachable through `merge`, which lays the whole block down first; kept so this
        // function is correct on its own terms rather than only in that caller's order.
        None => {
            let brace = text.find('{').ok_or("no root object")?;
            let insert = if obj.is_empty() {
                format!("\"hooks\": {{\"{event}\": [{rendered}]}}")
            } else {
                format!("\"hooks\": {{\"{event}\": [{rendered}]}}, ")
            };
            return Ok(Some(splice(text, brace + 1, brace + 1, &insert)));
        }
        Some(Value::Object(h)) => h,
        Some(_) => return Err("\"hooks\" is not an object".to_string()),
    };
    // The locate refuses on anything it is not certain of — a duplicated depth-1 key, or ANY
    // escaped string at that depth (an escaped alias of a key is a duplicate the parser
    // resolves last-wins, and telling aliases apart would mean reimplementing JSON unescaping).
    // A Windows path value is enough to trip it, and the file looks perfectly fine to its
    // owner, so the refusal has to say what it could not do rather than only that it failed.
    let hooks_open = crate::agents::top_level_value_brace(text, "hooks").ok_or(
        "its \"hooks\" object could not be located unambiguously (a duplicated key, or a \
         backslash-escaped string at the top level - a Windows path is enough); docli will not \
         guess which one to edit",
    )?;
    let Some(existing_event) = hooks.get(event) else {
        // The object is there but this event is not: splice the key after its `{`.
        let insert = if hooks.is_empty() {
            format!("\"{event}\": [{rendered}]")
        } else {
            format!("\"{event}\": [{rendered}], ")
        };
        return Ok(Some(splice(text, hooks_open + 1, hooks_open + 1, &insert)));
    };
    let Some(entries) = existing_event.as_array() else {
        return Err(format!("\"hooks.{event}\" is not an array"));
    };
    let (val_start, val_end) = crate::agents::value_of_key_in_object(text, hooks_open, event)
        .ok_or_else(|| format!("could not locate \"hooks.{event}\" unambiguously"))?;
    if text.as_bytes().get(val_start) != Some(&b'[') {
        return Err(format!("\"hooks.{event}\" is not an array"));
    }
    let spans = array_element_spans(text, val_start, val_end)
        .ok_or_else(|| format!("could not read the elements of \"hooks.{event}\""))?;
    if spans.len() != entries.len() {
        // The walk and the parser disagree about how many elements there are — any doubt goes
        // to the print branch rather than to a guess about which bytes to replace.
        return Err(format!("could not read the elements of \"hooks.{event}\""));
    }
    let mine: Vec<usize> = entries
        .iter()
        .enumerate()
        .filter(|(_, v)| is_ours(v, agent, event))
        .map(|(i, _)| i)
        .collect();
    match mine.first() {
        None => {
            // Insert at the FRONT: ours is the gate, and a reader scanning the array meets it
            // before the entries it does not need to think about.
            let at = val_start + 1;
            let insert = if entries.is_empty() {
                rendered
            } else {
                format!("{rendered}, ")
            };
            Ok(Some(splice(text, at, at, &insert)))
        }
        Some(&i) => {
            // A SECOND element of ours is residue: drop the last one, and the caller's loop
            // comes back for any more. Dropping before replacing means the surviving element
            // is always the first, which is where a fresh install would have put it.
            if let Some(&last) = mine.last().filter(|&&last| last != i) {
                return Ok(Some(drop_element(text, val_start, val_end, &spans, last)));
            }
            if entries[i] == desired {
                return Ok(None);
            }
            let (a, b) = spans[i];
            Ok(Some(splice(text, a, b, &rendered)))
        }
    }
}

/// The byte spans of an array's elements, given the `[` and the one-past-`]` offsets.
fn array_element_spans(text: &str, open: usize, end: usize) -> Option<Vec<(usize, usize)>> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = open + 1;
    loop {
        while i < end && (bytes[i].is_ascii_whitespace() || bytes[i] == b',') {
            i += 1;
        }
        if i >= end || bytes[i] == b']' {
            return Some(out);
        }
        let stop = crate::agents::json_value_extent(bytes, i)?;
        if stop > end {
            return None;
        }
        out.push((i, stop));
        i = stop;
    }
}

/// Delete array element `i`, taking exactly one adjacent comma with it so the array is left
/// well-formed. ONE implementation, shared by the merge's duplicate-dropping arm and by
/// `remove` — two comma rules that drifted apart would each be a way to emit broken JSON into
/// somebody's settings file.
fn drop_element(
    text: &str,
    val_start: usize,
    val_end: usize,
    spans: &[(usize, usize)],
    i: usize,
) -> String {
    let (a, mut b) = spans[i];
    let bytes = text.as_bytes();
    let mut a2 = a;
    let mut j = b;
    while j < val_end && bytes[j].is_ascii_whitespace() {
        j += 1;
    }
    if j < val_end && bytes[j] == b',' {
        b = j + 1;
    } else {
        // The LAST element: take the comma BEFORE it instead.
        let mut k = a;
        while k > val_start + 1 && bytes[k - 1].is_ascii_whitespace() {
            k -= 1;
        }
        if k > val_start + 1 && bytes[k - 1] == b',' {
            a2 = k - 1;
        }
    }
    splice(text, a2, b, "")
}

fn splice(text: &str, from: usize, to: usize, with: &str) -> String {
    let mut out = String::with_capacity(text.len() + with.len());
    out.push_str(&text[..from]);
    out.push_str(with);
    out.push_str(&text[to..]);
    out
}

/// What a removal decided. THREE outcomes: «could not read it safely» is not «nothing of ours»,
/// and collapsing them let `docli uninstall` delete the binary while leaving live hook entries
/// behind, silently — the entries then name a command that no longer exists, and nobody was
/// told. The textual locator refuses on shapes it cannot be sure of (a duplicated key, any
/// backslash-escaped top-level string — a Windows path in `statusLine` is enough), so this is
/// reachable on a perfectly ordinary settings file.
#[derive(Debug, PartialEq, Eq)]
pub enum RemoveOutcome {
    Removed(String),
    NothingOfOurs,
    Refused,
}

/// Remove every element we wrote, leaving the user's own hooks and the rest of the file byte for
/// byte.
///
/// A textual removal, for the same reason the merge is textual — and it deliberately leaves an
/// array our departure emptied (`"PreToolUse": []`) rather than reaching further into the user's
/// file to tidy it. An empty array is valid, inert, and a re-run of `docli init` splices straight
/// back into it; removing the key would mean guessing at the span of a `"key":` we did not write.
pub fn remove(agent: HookAgent, existing: &str) -> RemoveOutcome {
    let Ok(root) = serde_json::from_str::<Value>(existing) else {
        return RemoveOutcome::Refused;
    };
    let Some(obj) = root.as_object() else {
        return RemoveOutcome::Refused;
    };
    // No `hooks` at all, or one we cannot read as an object: nothing of ours can be in there.
    let Some(hooks) = obj.get("hooks").and_then(|h| h.as_object()) else {
        return RemoveOutcome::NothingOfOurs;
    };
    // Is there anything of ours to remove? Asked BEFORE the locator runs, so «we found nothing»
    // and «we could not look» stay distinguishable.
    let any_ours = EVENTS.iter().any(|e| {
        hooks
            .get(*e)
            .and_then(|a| a.as_array())
            .is_some_and(|a| a.iter().any(|v| is_ours(v, agent, e)))
    });
    if !any_ours {
        return RemoveOutcome::NothingOfOurs;
    }
    let mut out = existing.to_string();
    let mut removed = false;
    // Re-locate from scratch after every deletion: offsets shift, and one removal per pass is
    // simpler to be sure of than an offset ledger.
    'again: loop {
        let Ok(root) = serde_json::from_str::<Value>(&out) else {
            return RemoveOutcome::Refused;
        };
        let Some(hooks_now) = root.as_object().and_then(|o| o.get("hooks")?.as_object()) else {
            break;
        };
        let Some(hooks_open) = crate::agents::top_level_value_brace(&out, "hooks") else {
            return RemoveOutcome::Refused;
        };
        for event in EVENTS {
            let Some(entries) = hooks_now.get(event).and_then(|v| v.as_array()) else {
                continue;
            };
            let Some(i) = entries.iter().position(|v| is_ours(v, agent, event)) else {
                continue;
            };
            let Some((val_start, val_end)) =
                crate::agents::value_of_key_in_object(&out, hooks_open, event)
            else {
                return RemoveOutcome::Refused;
            };
            let Some(spans) = array_element_spans(&out, val_start, val_end) else {
                return RemoveOutcome::Refused;
            };
            if spans.len() != entries.len() {
                return RemoveOutcome::Refused;
            }
            out = drop_element(&out, val_start, val_end, &spans, i);
            removed = true;
            continue 'again;
        }
        break;
    }
    let _ = hooks;
    if !removed {
        // We saw something of ours before the loop, so failing to remove it means the locator
        // refused — never «there was nothing there».
        return RemoveOutcome::Refused;
    }
    ensure_trailing_newline(&mut out);
    RemoveOutcome::Removed(out)
}

/// Are our entries present in this project for `agent`, and does the binary they name resolve?
///
/// D2's counterweight: choosing silence for a missing binary means the disabled gate has to be
/// visible SOMEWHERE, and this is where. Reported by `docli status`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct HookStatus {
    pub agent: &'static str,
    pub installed: bool,
    /// False when the entries are present but `docli` is not on `PATH` — the silently-disabled
    /// gate. `None` when nothing is installed, so there is nothing to resolve.
    pub binary_resolves: Option<bool>,
}

pub fn status(project_root: &Path, agent: HookAgent) -> HookStatus {
    let installed = std::fs::read_to_string(project_root.join(agent.config_path()))
        .ok()
        .and_then(|body| serde_json::from_str::<Value>(&body).ok())
        .and_then(|v| v.get("hooks")?.as_object().cloned())
        .is_some_and(|hooks| {
            EVENTS.iter().all(|e| {
                hooks
                    .get(*e)
                    .and_then(|a| a.as_array())
                    // The LOAD-BEARING fields, not whole-entry equality. Identity and health
                    // are different questions and this is the health one — «would this entry
                    // actually fire, and run our command?» — which turns on exactly two things:
                    // the matcher, and the command.
                    //
                    // Whole-entry equality answered it in both wrong directions. An entry of
                    // ours whose matcher somebody changed to `Bash` is still OURS to rewrite
                    // but guards no Write or Edit, so «writes into the mirror are refused»
                    // over it is the lie this report exists to prevent. And an entry with
                    // `timeout: 20` instead of `10` works perfectly, so calling it absent
                    // would send a reader to fix a gate that is not broken.
                    .is_some_and(|a| a.iter().any(|v| effectively_ours(v, agent, e)))
            })
        });
    HookStatus {
        agent: agent.key(),
        installed,
        binary_resolves: installed.then(binary_on_path),
    }
}

/// Does `docli` resolve on `PATH`? Asked the way the rendered hook command asks it — by name,
/// through `PATH` — and NOT by `current_exe()`, which is exactly the case that lies: a binary
/// invoked as `./target/debug/docli`, or one still running after `uninstall` removed it, is not
/// on anybody's `PATH`.
/// Does `docli` RESOLVE as a command — for this user, on this machine, right now?
///
/// It asks by RUNNING the resolution the rendered hook command runs, rather than reasoning about
/// it. Two earlier attempts got this wrong in the same direction: `is_file` accepted a `docli`
/// sitting on `PATH` at mode 0644, and `mode & 0o111` accepted a root-owned `0700` binary the
/// invoking user cannot execute. Both reported a working gate over a hook that exits silently —
/// which is the precise lie this report exists to prevent, and each fix would have been another
/// approximation of what the shell already knows. Simulating a permission check needs the
/// effective uid, the gid and every supplementary group; `command -v` needs none of that,
/// because it IS the check.
///
/// A spawn is affordable here: `docli status` runs once, at a person's request, and only when
/// entries are actually installed.
fn binary_on_path() -> bool {
    let (shell, flag, probe) = if cfg!(windows) {
        ("cmd", "/C", "where docli >nul 2>nul")
    } else {
        ("/bin/sh", "-c", "command -v docli >/dev/null 2>&1")
    };
    let child = std::process::Command::new(shell)
        .arg(flag)
        .arg(probe)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    // No shell at all: we cannot answer, and the honest answer to «is the gate live» is then
    // not «yes».
    let Ok(mut child) = child else {
        return false;
    };
    // BOUNDED. `status` is offline-first by design — it answers on a plane and behind a captive
    // portal — and a `PATH` entry on an unresponsive network mount makes the lookup itself
    // block. A status command that can hang is a status command people stop running, so a probe
    // that has not answered in this long is reported as «cannot tell», not waited on.
    let deadline = std::time::Instant::now() + PATH_PROBE_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(st)) => return st.success(),
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    // Reaped OFF this path. `kill` only QUEUES the signal, and a process stuck
                    // in uninterruptible filesystem I/O — the unresponsive network mount this
                    // timeout exists for — does not die until that I/O returns, so waiting for
                    // it here would reintroduce exactly the hang. A detached thread collects
                    // the child whenever it finally goes; if the CLI exits first, init reaps it.
                    std::thread::spawn(move || {
                        let _ = child.wait();
                    });
                    return false;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(_) => return false,
        }
    }
}

/// Long enough for a shell to consult a healthy `PATH` many times over, short enough that an
/// unresponsive mount on it cannot hold the screen.
const PATH_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(1500);

/// Write our entries into `agent`'s config under `project_root`. Reports through `ui`, and — like
/// `agents::wire` — is best-effort per agent: a config it cannot merge is named, never fatal.
pub fn install(project_root: &Path, agent: HookAgent) -> Result<()> {
    let rel = agent.config_path();
    let abs = project_root.join(rel);
    let existing = match std::fs::read_to_string(&abs) {
        Ok(s) => Some(s),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            crate::ui::warn(&format!(
                "{}: could not read {rel} ({e}) - hooks were not installed; add them by hand:\n    {}",
                agent.display(),
                snippet(agent)
            ));
            return Ok(());
        }
    };
    match merge(agent, existing.as_deref()) {
        HookOutcome::Write(content) => {
            if let Some(parent) = abs.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("creating {}", parent.display()))?;
            }
            crate::agents::write_user_config(&abs, content.as_bytes())?;
            crate::ui::ok(&format!("{}: wrote hooks into {rel}", agent.display()));
            crate::ui::detail(
                "The agent will ask you to trust this project before any project-local hook \
                 runs.",
            );
        }
        HookOutcome::AlreadyInstalled => {
            crate::ui::ok(&format!(
                "{}: hooks already installed in {rel}",
                agent.display()
            ));
        }
        HookOutcome::Occupied(reason) => {
            crate::ui::warn(&format!(
                "{}: {rel} - {reason}; add them by hand:\n    {}",
                agent.display(),
                snippet(agent)
            ));
        }
    }
    Ok(())
}

/// The copy-paste form for the occupied branch.
pub fn snippet(agent: HookAgent) -> String {
    let mut doc = serde_json::Map::new();
    let mut hooks = serde_json::Map::new();
    for event in EVENTS {
        hooks.insert(event.to_string(), json!([entry_for(agent, event)]));
    }
    doc.insert("hooks".into(), Value::Object(hooks));
    format!(
        "{}:\n{}",
        agent.config_path(),
        serde_json::to_string_pretty(&Value::Object(doc)).unwrap_or_default()
    )
}

/// What `docli init` prints before writing anything (D6): plainly what will be written, where,
/// and that the agent holds its own gate in front of it.
pub fn consent_summary(agents: &[HookAgent]) -> Vec<String> {
    agents
        .iter()
        .map(|a| {
            format!(
                "{}: {} - a PreToolUse hook that refuses writes into the mirror, and a \
                 SessionStart hook that reports mirror freshness. Both run `docli`; neither \
                 runs until you trust this project in {}.",
                a.display(),
                a.config_path(),
                a.display()
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_file_gets_both_events_with_the_documented_shape() {
        let HookOutcome::Write(out) = merge(HookAgent::Claude, None) else {
            panic!("a fresh file must write");
        };
        let v: Value = serde_json::from_str(&out).unwrap();
        let pre = &v["hooks"]["PreToolUse"][0];
        assert_eq!(pre["matcher"], "Write|Edit|MultiEdit|NotebookEdit");
        assert_eq!(pre["hooks"][0]["type"], "command");
        assert!(pre["hooks"][0]["command"]
            .as_str()
            .unwrap()
            .contains("docli guard --agent claude"));
        let start = &v["hooks"]["SessionStart"][0];
        // Deliberately NOT all five documented sources: `clear`/`compact` are context
        // operations and `fork` inherits a checked parent.
        assert_eq!(start["matcher"], "startup|resume");
        assert!(start["hooks"][0]["command"]
            .as_str()
            .unwrap()
            .contains("docli sync --check --agent claude"));
    }

    #[test]
    fn the_codex_matcher_names_the_edit_tool_and_not_bash() {
        // Codex's PreToolUse fires for Bash and MCP calls too; an unmatched entry would spawn
        // `docli guard` on every shell command, which is the opposite of «has to be cheap».
        let HookOutcome::Write(out) = merge(HookAgent::Codex, None) else {
            panic!("write");
        };
        let v: Value = serde_json::from_str(&out).unwrap();
        let m = v["hooks"]["PreToolUse"][0]["matcher"].as_str().unwrap();
        assert_eq!(m, "apply_patch|Edit|Write");
        assert!(!m.contains("Bash"), "{m}");
    }

    #[test]
    fn the_rendered_command_is_inert_without_the_binary() {
        // D2's pin, run for real: `docli uninstall` leaves agent configs in place and a
        // teammate's clone has the entry without the binary — both must be silence, not a
        // broken tool call on every edit.
        for agent in HookAgent::all() {
            for event in EVENTS {
                let cmd = command_for(agent, event);
                // `/bin/sh` by absolute path: the child's PATH is emptied below, and resolving
                // the SHELL itself through it would fail before the line under test ran.
                let out = std::process::Command::new("/bin/sh")
                    .arg("-c")
                    .arg(&cmd)
                    // An EMPTY PATH is the honest spelling of «docli is not installed».
                    .env("PATH", "")
                    .output()
                    .expect("sh runs");
                assert_eq!(
                    out.status.code(),
                    Some(0),
                    "{cmd:?} must exit 0 with docli off PATH: {out:?}"
                );
                assert!(
                    out.stdout.is_empty(),
                    "{cmd:?} must print NOTHING when inert: {:?}",
                    String::from_utf8_lossy(&out.stdout)
                );
            }
        }
    }

    #[test]
    fn the_entry_is_the_same_on_every_platform_that_reads_it() {
        // The round-2 correction. These are PROJECT files, usually committed, and the module's
        // own reasoning turns on a teammate cloning the repository — so rendering from
        // `cfg!(windows)` handed every teammate on the other platform a command their shell
        // fails and `|| exit 0` swallows: exit 0, no output, NO GATE, with `docli status`
        // reporting the entries present and the binary resolving.
        for agent in HookAgent::all() {
            for event in EVENTS {
                let cmd = command_for(agent, event);
                assert!(cmd.starts_with("if command -v docli"), "{cmd}");
                assert!(!cmd.contains("where docli"), "{cmd}");
                // The swallow covers the RESOLUTION only. `|| exit 0` also hid «docli is
                // installed and failed», which must stay visible.
                assert!(!cmd.contains("|| exit"), "{cmd}");
            }
        }
        // Codex documents a slot for the other spelling; Claude Code documents none, so it
        // NAMES the shell instead of inheriting a default that varies by machine.
        let HookOutcome::Write(codex) = merge(HookAgent::Codex, None) else {
            panic!("write");
        };
        let v: Value = serde_json::from_str(&codex).unwrap();
        let h = &v["hooks"]["PreToolUse"][0]["hooks"][0];
        assert!(h["commandWindows"]
            .as_str()
            .unwrap()
            .starts_with("where docli"));
        assert!(h.get("shell").is_none(), "Codex documents no shell field");

        let HookOutcome::Write(claude) = merge(HookAgent::Claude, None) else {
            panic!("write");
        };
        let v: Value = serde_json::from_str(&claude).unwrap();
        let h = &v["hooks"]["PreToolUse"][0]["hooks"][0];
        assert_eq!(h["shell"], "bash");
        assert!(
            h.get("commandWindows").is_none(),
            "Claude Code documents no such field - an undocumented key here is the same bet \
             the identity marker was withdrawn for"
        );
    }

    #[test]
    fn a_users_own_hooks_survive_the_merge_and_the_removal() {
        let mine = r#"{
  "permissions": {"allow": ["Bash(ls:*)"]},
  "hooks": {
    "PreToolUse": [
      {"matcher": "Bash", "hooks": [{"type": "command", "command": "my-audit.sh"}]}
    ]
  }
}"#;
        let HookOutcome::Write(out) = merge(HookAgent::Claude, Some(mine)) else {
            panic!("must merge");
        };
        let v: Value = serde_json::from_str(&out).unwrap();
        // Ours goes at the FRONT — it is the gate, and a reader scanning the array meets it
        // before the entries they do not need to think about. Theirs is still there, untouched.
        assert!(is_ours(
            &v["hooks"]["PreToolUse"][0],
            HookAgent::Claude,
            "PreToolUse"
        ));
        assert_eq!(v["hooks"]["PreToolUse"][1]["matcher"], "Bash");
        assert_eq!(
            v["hooks"]["PreToolUse"][1]["hooks"][0]["command"],
            "my-audit.sh"
        );
        // Everything outside `hooks` survives — as DATA…
        assert_eq!(v["permissions"]["allow"][0], "Bash(ls:*)");
        // …and as BYTES. This is the whole reason the merge is a splice rather than a
        // parse-and-reserialize: `.claude/settings.json` is where somebody keeps their
        // permission rules and their status line, hand-formatted, and re-ordering every key on
        // the way to installing a convenience is a hostile thing to do to it.
        assert!(
            out.contains(r#"  "permissions": {"allow": ["Bash(ls:*)"]},"#),
            "the user's own text is untouched: {out}"
        );
        assert!(
            out.contains(
                r#"{"matcher": "Bash", "hooks": [{"type": "command", "command": "my-audit.sh"}]}"#
            ),
            "including inside the array we spliced into: {out}"
        );

        // …and uninstall takes back exactly ours, byte-preservingly too.
        let RemoveOutcome::Removed(back) = remove(HookAgent::Claude, &out) else {
            panic!("something of ours to remove");
        };
        let v: Value = serde_json::from_str(&back).unwrap();
        assert_eq!(v["hooks"]["PreToolUse"].as_array().unwrap().len(), 1);
        assert_eq!(v["hooks"]["PreToolUse"][0]["matcher"], "Bash");
        // An array OUR departure emptied is left in place, valid and inert: removing the key
        // would mean guessing at the span of a `"key":` we did not write, and a re-run of
        // `docli init` splices straight back into it.
        assert_eq!(v["hooks"]["SessionStart"].as_array().unwrap().len(), 0);
        assert_eq!(v["permissions"]["allow"][0], "Bash(ls:*)");
        assert!(
            back.contains(r#""permissions": {"allow": ["Bash(ls:*)"]},"#),
            "{back}"
        );
        assert!(back.contains("my-audit.sh"), "{back}");
        // Nothing of ours survives anywhere in the text.
        assert!(!back.contains("docli"), "{back}");

        // Re-installing into the emptied arrays converges rather than accumulating.
        let HookOutcome::Write(again) = merge(HookAgent::Claude, Some(&back)) else {
            panic!("must merge back in");
        };
        let v: Value = serde_json::from_str(&again).unwrap();
        assert_eq!(v["hooks"]["SessionStart"].as_array().unwrap().len(), 1);
        assert_eq!(v["hooks"]["PreToolUse"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn merging_twice_is_a_no_op() {
        // «Run twice, diff» — the identity marker is what makes this answerable at all.
        let HookOutcome::Write(first) = merge(HookAgent::Codex, None) else {
            panic!("write");
        };
        assert_eq!(
            merge(HookAgent::Codex, Some(&first)),
            HookOutcome::AlreadyInstalled
        );
    }

    #[test]
    fn an_older_shape_converges_instead_of_accumulating() {
        // A previous-version entry of OURS is replaced, never duplicated: same invocation,
        // older matcher, no `shell`, no guard prefix.
        //
        // Note what «ours» now requires — the WHOLE invocation, `--tool-input -` included. A
        // command that merely STARTS like ours is deliberately not claimed: `merge` replaces
        // what it thinks is ours and `remove` deletes it, so the predicate has to be precise
        // enough that a line somebody else wrote can never match. The cost is that a
        // hand-TRUNCATED copy of ours stops converging; the benefit is that nothing of theirs
        // is ever eaten. That is the right side of the trade.
        let stale = r#"{"hooks": {"PreToolUse": [{"matcher": "Write", "hooks": [{"type": "command", "command": "docli guard --agent claude --tool-input -"}]}]}}"#;
        let HookOutcome::Write(out) = merge(HookAgent::Claude, Some(stale)) else {
            panic!("must rewrite");
        };
        let v: Value = serde_json::from_str(&out).unwrap();
        let arr = v["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(arr.len(), 1, "converged, not accumulated: {out}");
        assert_eq!(arr[0]["matcher"], "Write|Edit|MultiEdit|NotebookEdit");

        // …and the other half of that trade, pinned so it cannot be loosened by accident: a
        // command that only shares a PREFIX with ours is somebody else's line.
        let theirs = r#"{"hooks": {"PreToolUse": [{"matcher": "Write", "hooks": [{"type": "command", "command": "printf 'docli guard --agent ' >> audit.log"}]}]}}"#;
        let HookOutcome::Write(out) = merge(HookAgent::Claude, Some(theirs)) else {
            panic!("must merge beside it");
        };
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(
            v["hooks"]["PreToolUse"].as_array().unwrap().len(),
            2,
            "theirs survives beside ours: {out}"
        );
        assert!(out.contains("audit.log"), "{out}");
    }

    #[test]
    fn more_than_one_of_ours_converges_to_exactly_one() {
        // Residue — from an older shape, or from a hand-installed copy beside an installed one.
        // Replacing the first and leaving the rest would mean `init` reported «wrote hooks» on
        // every re-run forever, and TWO `docli guard` processes per tool call.
        let dupes = r#"{"hooks": {"PreToolUse": [
            {"matcher": "Write", "hooks": [{"type": "command", "command": "docli guard --agent claude --tool-input -"}]},
            {"matcher": "Bash", "hooks": [{"type": "command", "command": "keep-me.sh"}]},
            {"matcher": "Edit", "hooks": [{"type": "command", "command": "docli guard --agent claude --tool-input -"}]}
        ]}}"#;
        let HookOutcome::Write(out) = merge(HookAgent::Claude, Some(dupes)) else {
            panic!("must converge");
        };
        let v: Value = serde_json::from_str(&out).unwrap();
        let arr = v["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(arr.len(), 2, "one of ours + the user's: {out}");
        assert!(is_ours(&arr[0], HookAgent::Claude, "PreToolUse"));
        assert_eq!(arr[1]["hooks"][0]["command"], "keep-me.sh");
        // …and the result is a FIXED POINT: the whole point of converging.
        assert_eq!(
            merge(HookAgent::Claude, Some(&out)),
            HookOutcome::AlreadyInstalled
        );
    }

    #[test]
    fn we_write_only_documented_fields_into_the_users_settings() {
        // An extra identity key was written and then WITHDRAWN: Claude Code documents a
        // «Settings Error» tier — *"a value the schema rejects"* — where the session starts
        // *"without the broken settings"*, i.e. the user loses their whole configuration, and
        // nothing in the vendor docs says an unknown key inside a matcher object lands in the
        // milder tier. D10 is minted by this very slice; betting a settings file on an
        // unverified vendor claim was not available. Identity lives in `command` instead.
        let HookOutcome::Write(out) = merge(HookAgent::Claude, None) else {
            panic!("write");
        };
        let v: Value = serde_json::from_str(&out).unwrap();
        assert!(!out.contains("docliManaged"), "{out}");
        for event in EVENTS {
            let entry = &v["hooks"][event][0];
            let mut keys: Vec<&String> = entry.as_object().unwrap().keys().collect();
            keys.sort();
            assert_eq!(keys, vec!["hooks", "matcher"], "{event}: {entry}");
            let hook = &entry["hooks"][0];
            let mut hk: Vec<&String> = hook.as_object().unwrap().keys().collect();
            hk.sort();
            // Every one of these is in the vendor's own handler table (verified 2026-09-01).
            assert_eq!(
                hk,
                vec!["command", "shell", "timeout", "type"],
                "{event}: {hook}"
            );
        }
    }

    #[test]
    fn unmergeable_shapes_fall_to_the_print_branch_not_an_error() {
        for bad in [
            "// a comment\n{}",
            "[1,2,3]",
            r#"{"hooks": []}"#,
            r#"{"hooks": {"PreToolUse": {}}}"#,
            "not json",
        ] {
            assert!(
                matches!(
                    merge(HookAgent::Claude, Some(bad)),
                    HookOutcome::Occupied(_)
                ),
                "{bad:?} must fall to the print branch"
            );
        }
        // …and the snippet it prints is itself valid JSON the reader can paste.
        let s = snippet(HookAgent::Claude);
        let body = s.split_once(":\n").expect("path prefix").1;
        serde_json::from_str::<Value>(body).expect("a pasteable snippet");
    }

    #[test]
    fn a_users_own_docli_hook_is_not_mistaken_for_ours() {
        // The direction that matters: `merge` REPLACES what it thinks is ours and `remove`
        // DELETES it. Writing your own `docli sync --check` hook is a reasonable thing to do —
        // the obvious thing to try before discovering `--hooks` — and eating it would be far
        // worse than failing to converge on a hand-mangled entry of ours.
        let theirs = r#"{"hooks": {"SessionStart": [
            {"matcher": "startup", "hooks": [{"type": "command", "command": "docli sync --check | tee /tmp/log"}]}
        ]}}"#;
        let HookOutcome::Write(out) = merge(HookAgent::Claude, Some(theirs)) else {
            panic!("must merge");
        };
        let v: Value = serde_json::from_str(&out).unwrap();
        let arr = v["hooks"]["SessionStart"].as_array().unwrap();
        assert_eq!(arr.len(), 2, "theirs survives beside ours: {out}");
        assert!(out.contains("tee /tmp/log"), "byte-preserved: {out}");
        // …and uninstall takes back only ours.
        let RemoveOutcome::Removed(back) = remove(HookAgent::Claude, &out) else {
            panic!("ours to remove");
        };
        let v: Value = serde_json::from_str(&back).unwrap();
        assert_eq!(v["hooks"]["SessionStart"].as_array().unwrap().len(), 1);
        assert!(back.contains("docli sync --check | tee"), "{back}");
    }

    #[test]
    fn a_matcher_of_ours_that_grew_a_second_handler_is_no_longer_ours() {
        // `merge` replaces the whole MATCHER and `remove` deletes it wholesale, so claiming an
        // element because ONE of its handlers is ours would take the handler the user added
        // beside ours with it. Leaving it alone costs a duplicate entry; claiming it costs
        // somebody their hook.
        let HookOutcome::Write(installed) = merge(HookAgent::Claude, None) else {
            panic!("write");
        };
        let mut v: Value = serde_json::from_str(&installed).unwrap();
        v["hooks"]["PreToolUse"][0]["hooks"]
            .as_array_mut()
            .unwrap()
            .push(json!({"type": "command", "command": "my-audit.sh"}));
        let theirs = serde_json::to_string_pretty(&v).unwrap();
        assert!(!is_ours(
            &v["hooks"]["PreToolUse"][0],
            HookAgent::Claude,
            "PreToolUse"
        ));

        let HookOutcome::Write(out) = merge(HookAgent::Claude, Some(&theirs)) else {
            panic!("must add beside it");
        };
        assert!(out.contains("my-audit.sh"), "their handler survives: {out}");
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["hooks"]["PreToolUse"].as_array().unwrap().len(), 2);
        // …and uninstall leaves the whole modified matcher alone.
        assert!(
            matches!(remove(HookAgent::Claude, &out), RemoveOutcome::Removed(back) if back.contains("my-audit.sh"))
        );
    }

    #[test]
    fn identity_is_equality_so_a_command_that_merely_quotes_ours_is_not_ours() {
        // A `contains` test looked precise enough while the needle was long — but this is a
        // hook somebody could plausibly write, and it carries the whole invocation.
        let theirs = format!(
            r#"{{"hooks": {{"PreToolUse": [{{"matcher": "Write", "hooks": [{{"type": "command", "command": "printf '{}' >> audit.log"}}]}}]}}}}"#,
            inner_command(HookAgent::Claude, "PreToolUse")
        );
        let v: Value = serde_json::from_str(&theirs).unwrap();
        assert!(!is_ours(
            &v["hooks"]["PreToolUse"][0],
            HookAgent::Claude,
            "PreToolUse"
        ));
        let HookOutcome::Write(out) = merge(HookAgent::Claude, Some(&theirs)) else {
            panic!("must merge beside it");
        };
        assert!(out.contains("audit.log"), "{out}");
        assert_eq!(
            serde_json::from_str::<Value>(&out).unwrap()["hooks"]["PreToolUse"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn ownership_is_per_agent_and_per_event_not_a_global_set() {
        // A command we generate for ONE agent/event is not ours everywhere. This entry — a
        // Codex SessionStart invocation, sitting under Claude's PreToolUse — was never emitted
        // for that slot, so it belongs to whoever put it there. A global set claimed it, and
        // `merge` would have replaced it while `uninstall` deleted it.
        let theirs = format!(
            r#"{{"hooks": {{"PreToolUse": [{{"matcher": "Bash", "hooks": [{{"type": "command", "command": "{}"}}]}}]}}}}"#,
            inner_command(HookAgent::Codex, "SessionStart")
        );
        let v: Value = serde_json::from_str(&theirs).unwrap();
        assert!(!is_ours(
            &v["hooks"]["PreToolUse"][0],
            HookAgent::Claude,
            "PreToolUse"
        ));
        // …and it survives both operations.
        let HookOutcome::Write(out) = merge(HookAgent::Claude, Some(&theirs)) else {
            panic!("must merge beside it");
        };
        assert_eq!(
            serde_json::from_str::<Value>(&out).unwrap()["hooks"]["PreToolUse"]
                .as_array()
                .unwrap()
                .len(),
            2,
            "{out}"
        );
        let RemoveOutcome::Removed(back) = remove(HookAgent::Claude, &out) else {
            panic!("ours to remove");
        };
        assert!(
            back.contains(&inner_command(HookAgent::Codex, "SessionStart")),
            "theirs survives uninstall: {back}"
        );
    }

    #[test]
    fn removal_reports_nothing_when_nothing_is_ours() {
        // «Nothing of ours» and «could not look» are DIFFERENT answers, because uninstall acts
        // on them differently: the first is silence, the second is a warning naming what to
        // delete by hand.
        assert_eq!(
            remove(HookAgent::Claude, r#"{"hooks": {"PreToolUse": []}}"#),
            RemoveOutcome::NothingOfOurs
        );
        assert_eq!(
            remove(HookAgent::Claude, "{}"),
            RemoveOutcome::NothingOfOurs
        );
        assert_eq!(
            remove(HookAgent::Claude, "not json"),
            RemoveOutcome::Refused
        );
        // The reachable case, on a perfectly ordinary settings file: a backslash-escaped
        // top-level string (a Windows path in `statusLine`) makes the textual locator refuse.
        // Reporting that as «nothing of ours» deleted the binary and left the entries live.
        let HookOutcome::Write(installed) = merge(HookAgent::Claude, None) else {
            panic!("write");
        };
        let with_escape =
            installed.replacen('{', r#"{"statusLine": "C:\\Users\\me\\bin\\line.exe","#, 1);
        serde_json::from_str::<Value>(&with_escape).expect("still valid JSON");
        assert_eq!(
            remove(HookAgent::Claude, &with_escape),
            RemoveOutcome::Refused
        );
    }

    #[test]
    fn status_reports_a_present_gate_and_notices_a_missing_binary() {
        let tmp = tempfile::tempdir().unwrap();
        let s = status(tmp.path(), HookAgent::Claude);
        assert!(!s.installed);
        assert_eq!(s.binary_resolves, None, "nothing to resolve");

        let HookOutcome::Write(out) = merge(HookAgent::Claude, None) else {
            panic!("write");
        };
        std::fs::create_dir_all(tmp.path().join(".claude")).unwrap();
        std::fs::write(tmp.path().join(".claude/settings.json"), out).unwrap();
        let s = status(tmp.path(), HookAgent::Claude);
        assert!(s.installed);
        // Installed ⇒ the question is asked. WHAT it answers depends on the machine running
        // the test, so the answer itself is pinned on the pure half below.
        assert!(s.binary_resolves.is_some());
    }

    #[test]
    fn a_disabled_gate_is_detectable_because_the_shell_is_what_is_consulted() {
        // The silently-disabled gate D2 chose to accept — visible HERE and nowhere else.
        //
        // Asked by RUNNING the resolution the rendered command runs, rather than reasoning
        // about it. Two earlier attempts approximated it and were wrong in the same direction:
        // `is_file` accepted a `docli` at mode 0644, and `mode & 0o111` accepted a root-owned
        // `0700` binary this user cannot execute. Both reported a working gate over a hook that
        // exits silently. `command -v` needs no uid, gid or group list, because it IS the
        // check — so what is pinned here is that we ask it with an EMPTY PATH and get «no».
        let out = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg("command -v docli >/dev/null 2>&1")
            .env("PATH", "")
            .status()
            .expect("sh runs");
        assert!(
            !out.success(),
            "an empty PATH is the honest spelling of «docli is not installed»"
        );
    }

    #[test]
    fn a_broken_docli_is_visible_while_a_missing_one_stays_silent() {
        // Two different things that `|| exit 0` used to hide behind one exit code. A teammate
        // without the binary must see nothing; a binary that is present and FAILS — a broken
        // shebang, a half-finished self-update — must not be swallowed into a silently absent
        // gate, which is the failure mode this whole slice exists to remove.
        if cfg!(windows) {
            return; // the cmd.exe form is a named manual check (no Windows runner here)
        }
        let tmp = tempfile::tempdir().unwrap();
        let bin = tmp.path().join("docli");
        std::fs::write(&bin, "#!/nonexistent/interpreter\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let run = |path: &str| {
            std::process::Command::new("/bin/sh")
                .arg("-c")
                .arg(command_for(HookAgent::Claude, "PreToolUse"))
                .env("PATH", path)
                .output()
                .expect("sh runs")
        };
        // Missing: silent, exit 0.
        let out = run("");
        assert_eq!(out.status.code(), Some(0));
        assert!(out.stdout.is_empty());
        // Present but unrunnable: NOT silent.
        let out = run(tmp.path().to_str().unwrap());
        assert_ne!(
            out.status.code(),
            Some(0),
            "a docli that cannot run must surface, not be swallowed"
        );
    }

    #[test]
    fn a_working_gate_is_not_reported_as_absent() {
        // The other direction of the same question. `timeout` is a documented field somebody
        // may well tune; an entry with `20` instead of `10` fires exactly as ours does, so
        // calling it absent would send a reader to fix a gate that is not broken. Health turns
        // on the matcher and the command, and nothing else.
        let tmp = tempfile::tempdir().unwrap();
        let HookOutcome::Write(good) = merge(HookAgent::Claude, None) else {
            panic!("write");
        };
        let mut v: Value = serde_json::from_str(&good).unwrap();
        v["hooks"]["PreToolUse"][0]["hooks"][0]["timeout"] = json!(20);
        // …but `type` IS load-bearing: a handler that is not a command cannot run one.
        let mut broken = v.clone();
        broken["hooks"]["PreToolUse"][0]["hooks"][0]["type"] = json!("prompt");
        assert!(!effectively_ours(
            &broken["hooks"]["PreToolUse"][0],
            HookAgent::Claude,
            "PreToolUse"
        ));
        // …and so is `shell`: PowerShell cannot parse `if command -v …; then …; fi`, so a
        // Claude entry switched to it runs neither hook. The rule is «every field we write
        // except timeout», precisely because enumerating them by hand kept missing one.
        let mut broken = v.clone();
        broken["hooks"]["PreToolUse"][0]["hooks"][0]["shell"] = json!("powershell");
        assert!(!effectively_ours(
            &broken["hooks"]["PreToolUse"][0],
            HookAgent::Claude,
            "PreToolUse"
        ));
        // An EXTRA field can disable the gate just as thoroughly as a changed one: `async: true`
        // is documented, and Claude Code runs an async hook without waiting, so its denial
        // cannot block the write.
        let mut broken = v.clone();
        broken["hooks"]["PreToolUse"][0]["hooks"][0]["async"] = json!(true);
        assert!(!effectively_ours(
            &broken["hooks"]["PreToolUse"][0],
            HookAgent::Claude,
            "PreToolUse"
        ));
        // Codex's Windows override is ours to check too.
        let HookOutcome::Write(cx) = merge(HookAgent::Codex, None) else {
            panic!("write");
        };
        let mut cv: Value = serde_json::from_str(&cx).unwrap();
        assert!(effectively_ours(
            &cv["hooks"]["PreToolUse"][0],
            HookAgent::Codex,
            "PreToolUse"
        ));
        cv["hooks"]["PreToolUse"][0]["hooks"][0]["commandWindows"] = json!("echo nope");
        assert!(!effectively_ours(
            &cv["hooks"]["PreToolUse"][0],
            HookAgent::Codex,
            "PreToolUse"
        ));
        std::fs::create_dir_all(tmp.path().join(".claude")).unwrap();
        std::fs::write(
            tmp.path().join(".claude/settings.json"),
            serde_json::to_string_pretty(&v).unwrap(),
        )
        .unwrap();
        assert!(status(tmp.path(), HookAgent::Claude).installed);
    }

    #[test]
    fn status_reports_health_not_merely_ownership() {
        // Identity and health are different questions. An entry of ours whose matcher somebody
        // changed to `Bash` is still OURS to rewrite — but it does not guard a single Write or
        // Edit, and «writes into the mirror are refused» over it would be exactly the lie this
        // report exists to prevent.
        let tmp = tempfile::tempdir().unwrap();
        let HookOutcome::Write(good) = merge(HookAgent::Claude, None) else {
            panic!("write");
        };
        let mut v: Value = serde_json::from_str(&good).unwrap();
        v["hooks"]["PreToolUse"][0]["matcher"] = json!("Bash");
        std::fs::create_dir_all(tmp.path().join(".claude")).unwrap();
        std::fs::write(
            tmp.path().join(".claude/settings.json"),
            serde_json::to_string_pretty(&v).unwrap(),
        )
        .unwrap();
        // Still ours…
        assert!(is_ours(
            &v["hooks"]["PreToolUse"][0],
            HookAgent::Claude,
            "PreToolUse"
        ));
        // …and NOT reported as a working gate.
        assert!(!status(tmp.path(), HookAgent::Claude).installed);
    }
}
