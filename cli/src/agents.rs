// SPDX-FileCopyrightText: 2026 OOO Agitek
// SPDX-License-Identifier: MIT

//! `docli init` agent wiring (v0.28.0 D12) — the opt-in MCP picker + per-agent config adapters.
//!
//! Design pins (D12.4, research: `docs/research/2026-08-30-coding-agent-skills-mcp-matrix.md`):
//! - **Opt-in only** — `--mcp` or an interactive yes; never automatic, never on plain re-runs.
//! - **No credential is ever written.** The configs carry a URL; the agent runs its own browser
//!   OAuth on first connect, and the v0.25.7 per-connection persona pin binds the workspace.
//!   Labels carry no authority, so these files are safe to commit.
//! - **Write policy** (every adapter): create if absent; merge ONLY our key if the file exists;
//!   leave an existing `docli` entry untouched; print the snippet if the file is occupied in a
//!   way we can't merge (JSONC comments, non-object roots, exotic spellings).
//! - **Merges are splices, not rewrites**: an existing file is never re-serialized wholesale
//!   (that would re-sort keys and destroy the user's formatting) — our entry is spliced into the
//!   located top-level object, and anything the splicer can't locate falls to the print branch.
//! - Print-only agents (Qwen/Cline/Trae + everything unlisted) still get the skill copy where
//!   they have an off-standard skills dir; their MCP snippet is printed, never written —
//!   Cline's config is global-only, and Trae's remote-MCP OAuth is unconfirmed (a PAT-in-header
//!   config would resurrect the pasted-token UX the OAuth server exists to kill).

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};

/// The labeled MCP resource path (doc-twin of docli-core's `MCP_LABEL_PATH_PREFIX` — the CLI
/// cannot depend on core; the grammar itself is shared via `docli_rules::valid_label`).
const MCP_LABEL_PATH_PREFIX: &str = "/api/mcp/c/";

/// The unlabeled MCP resource path (`--mcp-bare`, and what a client that omits RFC 8707
/// `resource` connects to).
const MCP_BARE_PATH: &str = "/api/mcp";

pub fn connection_url(server: &str, label: &str) -> String {
    format!(
        "{}{MCP_LABEL_PATH_PREFIX}{label}",
        server.trim_end_matches('/')
    )
}

/// The BARE (unlabeled) connection — the `--mcp-bare` escape hatch. Labeled URLs are the
/// default (the v0.25.7 per-connection pin is the whole point of wiring per project), but the
/// audience fence is byte-exact and a client that omits RFC 8707 `resource` gets a bare-audience
/// token the labeled route refuses — and the labeled live-client gate is still open for most
/// wired clients. The bare connection is the proven-everywhere fallback.
pub fn connection_url_bare(server: &str) -> String {
    format!("{}{MCP_BARE_PATH}", server.trim_end_matches('/'))
}

/// Derive a grammar-valid connection label from a free-form name (the project dir name).
/// Lowercase, separators → `-`, everything outside `[a-z0-9-]` dropped, runs collapsed,
/// byte-capped WITH re-trim (the grammar rejects over-long, so the derivation may shorten —
/// unlike a user-SUPPLIED label, which is validated verbatim and refused off-grammar).
pub fn sanitize_label(name: &str) -> String {
    let mut out = String::new();
    for c in name.to_lowercase().chars() {
        let mapped = match c {
            'a'..='z' | '0'..='9' => Some(c),
            ' ' | '_' | '.' | '-' => Some('-'),
            _ => None,
        };
        if let Some(m) = mapped {
            if m == '-' && out.ends_with('-') {
                continue;
            }
            out.push(m);
        }
    }
    out.truncate(docli_rules::CONNECTION_LABEL_MAX_BYTES);
    let out = out.trim_matches('-').to_string();
    if out.is_empty() {
        // Nothing survived the ASCII grammar — the COMMON case for RU-named directories
        // («докли», «заметки»), not an edge. A constant fallback would wire every such
        // project to ONE connection (one grant, one persona pin); a stable hash of the
        // original name keeps differently-named projects on different connections in
        // practice (collision-RESISTANT at personal-project scale, not guaranteed distinct —
        // round-4 F-D; `--mcp-label` is the deterministic escape), and stays deterministic
        // per name.
        use sha2::{Digest, Sha256};
        let h = Sha256::digest(name.as_bytes());
        format!("project-{}", hex::encode(&h[..4]))
    } else {
        out
    }
}

/// How one agent gets its MCP entry.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum McpAdapter {
    /// Splice `"docli": <entry>` under a top-level JSON key in `path`.
    Json {
        path: &'static str,
        top_key: &'static str,
        entry_shape: JsonShape,
    },
    /// `[mcp_servers.docli] url = …` in `path`, format-preserving (toml_edit).
    CodexToml { path: &'static str },
    /// Print the snippet; never write (global-only or OAuth-unconfirmed config).
    Print,
}

/// The three JSON entry spellings the tier-1 agents use. `httpUrl`-vs-`url` is load-bearing on
/// Gemini (and Qwen): `url` means SSE there — the classic misconfig the research pass flagged.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum JsonShape {
    TypeHttpUrl,    // {"type": "http", "url": ...}   - .mcp.json, .vscode/mcp.json
    UrlOnly,        // {"url": ...}                   - .cursor/mcp.json
    HttpUrl,        // {"httpUrl": ...}               - .gemini/settings.json
    OpencodeRemote, // {"type": "remote", "url": ..., "enabled": true}
}

impl JsonShape {
    /// Entries are built by the JSON SERIALIZER, never by `format!` interpolation (round-3
    /// F0): `config.server` comes from a COMMITTED docli.toml nobody validates as a URL, so a
    /// quote in it must become an escaped character inside the `url` string — not a panic,
    /// and never an extra sibling key written into a teammate's agent config.
    fn entry_value(self, url: &str) -> serde_json::Value {
        match self {
            JsonShape::TypeHttpUrl => serde_json::json!({"type": "http", "url": url}),
            JsonShape::UrlOnly => serde_json::json!({"url": url}),
            JsonShape::HttpUrl => serde_json::json!({"httpUrl": url}),
            JsonShape::OpencodeRemote => {
                serde_json::json!({"type": "remote", "url": url, "enabled": true})
            }
        }
    }

    fn entry(self, url: &str) -> String {
        self.entry_value(url).to_string()
    }
}

pub struct AgentDef {
    pub key: &'static str,
    pub display: &'static str,
    /// Project-relative markers whose presence pre-selects this agent.
    project_markers: &'static [&'static str],
    /// $HOME-relative markers meaning "installed on this machine".
    home_markers: &'static [&'static str],
    adapter: McpAdapter,
    /// Where to copy `SKILL.md` (project-relative), for any agent that does not pick it up from
    /// `.agents/skills/`. `None` means the agent is BELIEVED to read the standard path — and that
    /// belief is only as good as the last verification (see the Claude Code note below).
    pub skill_copy_dir: Option<&'static str>,
    /// Does this agent's skill frontmatter accept Claude Code's own keys (v0.28.6 D4)?
    ///
    /// `.agents/skills/` is the open-standard path, where a key outside the spec's six is a hard
    /// packaging error rather than an ignored field — which is why the shared asset stays
    /// spec-clean and `copy_skill` injects per destination instead. Only `.claude/skills/` takes
    /// the extension today; every other row is `false` because nothing verifies otherwise, which
    /// is exactly the posture D10 mints.
    pub accepts_claude_frontmatter: bool,
}

/// The picker table. Tier-1 adapters write; the rest print. `.mcp.json` serves Claude Code AND
/// Copilot CLI with one write (same path, same format — the research pass's best accident).
pub const AGENTS: &[AgentDef] = &[
    AgentDef {
        key: "claude",
        display: "Claude Code + Copilot CLI (.mcp.json)",
        project_markers: &[".claude", ".mcp.json"],
        home_markers: &[".claude"],
        adapter: McpAdapter::Json {
            path: ".mcp.json",
            top_key: "mcpServers",
            entry_shape: JsonShape::TypeHttpUrl,
        },
        // Claude Code does NOT read `.agents/skills/` (verified 2026-09-01 against its own docs
        // and by observation: the file was in place, `/reload-skills` reported "no changes", the
        // skill never appeared). Its documented project path is `.claude/skills/`, which Copilot
        // in VS Code also reads. Getting this wrong was silent in the worst way: `docli init`
        // reported Claude Code configured, and for MCP it truly was.
        skill_copy_dir: Some(".claude/skills/docli-mirror"),
        // `.claude/skills/` is Claude Code's own path, so it takes Claude Code's own frontmatter
        // — including `paths` glob activation (verified 2026-09-01,
        // `code.claude.com/docs/en/skills`, frontmatter reference table).
        accepts_claude_frontmatter: true,
    },
    AgentDef {
        key: "codex",
        display: "Codex CLI (.codex/config.toml)",
        project_markers: &[".codex"],
        home_markers: &[".codex"],
        adapter: McpAdapter::CodexToml {
            path: ".codex/config.toml",
        },
        // Codex DOES read `.agents/skills/` — the one `None` in this table that carries a dated
        // vendor-doc verification (2026-09-01, `learn.chatgpt.com/docs/build-skills.md`, which
        // lists the scan order verbatim: `$CWD/.agents/skills`, `$CWD/../.agents/skills`,
        // `$REPO_ROOT/.agents/skills`, `$HOME/.agents/skills`, `/etc/codex/skills`). The
        // unconditional drop `init_cmd` makes was correct for Codex and wrong only about Claude
        // Code. Its frontmatter documents `name` and `description` only, so no injection.
        skill_copy_dir: None,
        accepts_claude_frontmatter: false,
    },
    AgentDef {
        key: "gemini",
        display: "Gemini CLI (.gemini/settings.json)",
        project_markers: &[".gemini"],
        home_markers: &[".gemini"],
        adapter: McpAdapter::Json {
            path: ".gemini/settings.json",
            top_key: "mcpServers",
            entry_shape: JsonShape::HttpUrl,
        },
        skill_copy_dir: None,
        accepts_claude_frontmatter: false,
    },
    AgentDef {
        key: "cursor",
        display: "Cursor (.cursor/mcp.json)",
        project_markers: &[".cursor"],
        home_markers: &[".cursor"],
        adapter: McpAdapter::Json {
            path: ".cursor/mcp.json",
            top_key: "mcpServers",
            entry_shape: JsonShape::UrlOnly,
        },
        skill_copy_dir: None,
        accepts_claude_frontmatter: false,
    },
    AgentDef {
        key: "vscode",
        display: "VS Code / Copilot (.vscode/mcp.json)",
        project_markers: &[".vscode"],
        home_markers: &[],
        adapter: McpAdapter::Json {
            path: ".vscode/mcp.json",
            top_key: "servers",
            entry_shape: JsonShape::TypeHttpUrl,
        },
        skill_copy_dir: None,
        accepts_claude_frontmatter: false,
    },
    AgentDef {
        key: "opencode",
        display: "OpenCode (opencode.json)",
        project_markers: &["opencode.json", ".opencode"],
        home_markers: &[".config/opencode"],
        adapter: McpAdapter::Json {
            path: "opencode.json",
            top_key: "mcp",
            entry_shape: JsonShape::OpencodeRemote,
        },
        skill_copy_dir: None,
        accepts_claude_frontmatter: false,
    },
    AgentDef {
        key: "qwen",
        display: "Qwen Code (snippet + skill copy)",
        project_markers: &[".qwen"],
        home_markers: &[".qwen"],
        adapter: McpAdapter::Print,
        skill_copy_dir: Some(".qwen/skills/docli-mirror"),
        accepts_claude_frontmatter: false,
    },
    AgentDef {
        key: "cline",
        display: "Cline (snippet + skill copy)",
        project_markers: &[".clinerules", ".cline"],
        home_markers: &[".cline"],
        adapter: McpAdapter::Print,
        skill_copy_dir: Some(".cline/skills/docli-mirror"),
        accepts_claude_frontmatter: false,
    },
    AgentDef {
        key: "trae",
        display: "Trae (snippet + skill copy)",
        project_markers: &[".trae"],
        home_markers: &[".trae"],
        adapter: McpAdapter::Print,
        skill_copy_dir: Some(".trae/skills/docli-mirror"),
        accepts_claude_frontmatter: false,
    },
    // The remaining print-only set (D12.4): reachable by key so every named agent's user can
    // get their snippet — Zed's project-level MCP placement is undocumented, Windsurf's config
    // is global-only, SourceCraft's and Junie's remote-MCP OAuth is unconfirmed. Zed and
    // Windsurf read `.agents/skills/` natively, so no skill copy; SourceCraft and Junie have
    // no skills mechanism at all. NOTE (2026-09-01): the "reads `.agents/skills/` natively"
    // claim below is UNVERIFIED — the same claim was false for Claude Code.
    AgentDef {
        key: "zed",
        display: "Zed (snippet)",
        project_markers: &[".zed"],
        home_markers: &[".config/zed"],
        adapter: McpAdapter::Print,
        skill_copy_dir: None,
        accepts_claude_frontmatter: false,
    },
    AgentDef {
        key: "windsurf",
        display: "Windsurf (snippet)",
        project_markers: &[".windsurf"],
        home_markers: &[".codeium/windsurf"],
        adapter: McpAdapter::Print,
        skill_copy_dir: None,
        accepts_claude_frontmatter: false,
    },
    AgentDef {
        key: "sourcecraft",
        display: "SourceCraft Code Assistant (snippet)",
        project_markers: &[".codeassistant"],
        home_markers: &[],
        adapter: McpAdapter::Print,
        skill_copy_dir: None,
        accepts_claude_frontmatter: false,
    },
    AgentDef {
        key: "junie",
        display: "JetBrains Junie (snippet)",
        project_markers: &[".junie"],
        home_markers: &[".junie"],
        adapter: McpAdapter::Print,
        skill_copy_dir: None,
        accepts_claude_frontmatter: false,
    },
    // Tier-2 (research doc): documented project config + confirmed OAuth, smaller audience —
    // snippet-only until demand shows. The "reads `.agents/skills/` natively" claim behind this
    // `None` is UNVERIFIED (2026-09-01) — it was false for Claude Code.
    AgentDef {
        key: "amp",
        display: "Amp (snippet)",
        project_markers: &[".amp"],
        home_markers: &[".config/amp"],
        adapter: McpAdapter::Print,
        skill_copy_dir: None,
        accepts_claude_frontmatter: false,
    },
];

pub fn agent(key: &str) -> Option<&'static AgentDef> {
    AGENTS.iter().find(|a| a.key == key)
}

/// The keys pre-selected by detection: a project marker OR a home marker exists.
pub fn detect(project: &Path, home: Option<&Path>) -> Vec<&'static str> {
    // `symlink_metadata`, not `exists()` (Codex round 3): a DANGLING `.mcp.json` symlink is
    // still a marker — `exists()` follows the link and would hide the agent whose config the
    // writer is specifically able to heal (it creates the referent).
    fn present(p: &Path) -> bool {
        p.symlink_metadata().is_ok()
    }
    AGENTS
        .iter()
        .filter(|a| {
            a.project_markers.iter().any(|m| present(&project.join(m)))
                || home
                    .map(|h| a.home_markers.iter().any(|m| present(&h.join(m))))
                    .unwrap_or(false)
        })
        .map(|a| a.key)
        .collect()
}

/// What a merge decided. `Write` carries the FULL new file content (the splice already applied).
#[derive(Debug, PartialEq, Eq)]
pub enum MergeOutcome {
    Write(String),
    /// Our entry already exists; `same` = it matches what we would write today.
    AlreadyConfigured {
        same: bool,
    },
    /// Can't merge safely — the caller prints the snippet instead. Never an error.
    Occupied(String),
}

/// Splice `"docli": <entry>` under `top_key` in a JSON config, preserving the user's text.
fn merge_json(existing: Option<&str>, top_key: &str, entry: &str) -> MergeOutcome {
    let desired: serde_json::Value = serde_json::from_str(entry)
        .expect("the entry is serializer output (JsonShape::entry) - always valid JSON");
    let text = existing.map(str::trim).filter(|t| !t.is_empty());
    let Some(text) = text else {
        return MergeOutcome::Write(format!(
            "{{\n  \"{top_key}\": {{\n    \"docli\": {entry}\n  }}\n}}\n"
        ));
    };
    let root: serde_json::Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(_) => {
            return MergeOutcome::Occupied(
                "does not parse as strict JSON (it may contain comments)".to_string(),
            )
        }
    };
    let Some(obj) = root.as_object() else {
        return MergeOutcome::Occupied("root is not a JSON object".to_string());
    };
    match obj.get(top_key) {
        Some(serde_json::Value::Object(servers)) => {
            if let Some(ours) = servers.get("docli") {
                if *ours == desired {
                    return MergeOutcome::AlreadyConfigured { same: true };
                }
                // An explicit re-run with a different URL (a label change, --mcp-bare)
                // UPDATES our own entry in place — ours and only ours; everything around it
                // stays byte-identical. If the entry can't be located unambiguously, fall
                // back to the left-untouched report.
                return match replace_docli_entry(text, top_key, entry) {
                    Some(out) => MergeOutcome::Write(out),
                    None => MergeOutcome::AlreadyConfigured { same: false },
                };
            }
            let Some(brace) = top_level_value_brace(text, top_key) else {
                return MergeOutcome::Occupied(format!(
                    "could not locate the \"{top_key}\" object to merge into"
                ));
            };
            let insert = if servers.is_empty() {
                format!("\"docli\": {entry}")
            } else {
                format!("\"docli\": {entry}, ")
            };
            let mut out = String::with_capacity(text.len() + insert.len());
            out.push_str(&text[..=brace]);
            out.push_str(&insert);
            out.push_str(&text[brace + 1..]);
            ensure_trailing_newline(&mut out);
            MergeOutcome::Write(out)
        }
        Some(_) => MergeOutcome::Occupied(format!("\"{top_key}\" is not an object")),
        None => {
            // No top_key yet — splice the whole block right after the root `{`.
            let Some(brace) = text.find('{') else {
                return MergeOutcome::Occupied("no root object".to_string());
            };
            let insert = if obj.is_empty() {
                format!("\"{top_key}\": {{\"docli\": {entry}}}")
            } else {
                format!("\"{top_key}\": {{\"docli\": {entry}}}, ")
            };
            let mut out = String::with_capacity(text.len() + insert.len());
            out.push_str(&text[..=brace]);
            out.push_str(&insert);
            out.push_str(&text[brace + 1..]);
            ensure_trailing_newline(&mut out);
            MergeOutcome::Write(out)
        }
    }
}

fn ensure_trailing_newline(s: &mut String) {
    if !s.ends_with('\n') {
        s.push('\n');
    }
}

/// Byte offset of the `{` opening `key`'s value, where `key` is a DEPTH-1 member of the root
/// object — a tiny JSON walk (strings + escapes + depth over `{}`/`[]`). Returns `None` for any
/// shape it isn't sure about; the caller then falls to the print branch rather than guessing.
/// «Isn't sure» includes a DUPLICATED depth-1 key: serde_json keeps the LAST duplicate while a
/// first-match splice would edit the object the agent ignores — so any second candidate match
/// makes the whole locate refuse.
pub(crate) fn top_level_value_brace(text: &str, key: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut depth = 0usize;
    let mut i = 0usize;
    let mut found: Option<usize> = None;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => {
                let start = i + 1;
                let end = scan_string(bytes, start)?;
                // A candidate KEY: depth 1, plain spelling, followed by `:`.
                // ANY escaped string at this depth refuses the whole locate (Codex round
                // 1): an escaped alias of our key is a duplicate serde resolves last-wins
                // while a plain-spelling matcher would edit the ineffective entry — and
                // telling aliases apart would mean reimplementing JSON unescaping. Any
                // doubt → the print branch.
                if depth == 1 && text[start..end].contains('\\') {
                    return None;
                }
                if depth == 1 && &text[start..end] == key {
                    let mut j = end + 1;
                    while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                        j += 1;
                    }
                    if j < bytes.len() && bytes[j] == b':' {
                        if found.is_some() {
                            return None; // duplicated key - refuse rather than guess
                        }
                        j += 1;
                        while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                            j += 1;
                        }
                        if j >= bytes.len() || bytes[j] != b'{' {
                            return None;
                        }
                        found = Some(j);
                    }
                }
                i = end + 1;
            }
            b'{' | b'[' => {
                depth += 1;
                i += 1;
            }
            b'}' | b']' => {
                depth = depth.checked_sub(1)?;
                i += 1;
            }
            _ => i += 1,
        }
    }
    found
}

/// Index just past the closing quote's content: `bytes[start..ret]` is the raw string body.
pub(crate) fn scan_string(bytes: &[u8], start: usize) -> Option<usize> {
    let mut i = start;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b'"' => return Some(i),
            _ => i += 1,
        }
    }
    None
}

/// Replace the VALUE of the (unique) `"docli"` member inside `top_key`'s object with `entry`,
/// splicing over the user's text like everything else here. `None` = couldn't locate it
/// unambiguously; the caller reports left-untouched instead of guessing.
fn replace_docli_entry(text: &str, top_key: &str, entry: &str) -> Option<String> {
    let obj_open = top_level_value_brace(text, top_key)?;
    let (val_start, val_end) = value_of_key_in_object(text, obj_open, "docli")?;
    let mut out = String::with_capacity(text.len() + entry.len());
    out.push_str(&text[..val_start]);
    out.push_str(entry);
    out.push_str(&text[val_end..]);
    ensure_trailing_newline(&mut out);
    Some(out)
}

/// Locate the value span of `key` as a DIRECT member of the object opening at `obj_open`
/// (byte index of its `{`). Same discipline as [`top_level_value_brace`]: strings and escapes
/// respected, a duplicated key refuses, any doubt returns `None`.
pub(crate) fn value_of_key_in_object(
    text: &str,
    obj_open: usize,
    key: &str,
) -> Option<(usize, usize)> {
    let bytes = text.as_bytes();
    let mut depth = 0usize;
    let mut i = obj_open;
    let mut found: Option<(usize, usize)> = None;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => {
                let start = i + 1;
                let end = scan_string(bytes, start)?;
                // ANY escaped string at this depth refuses the whole locate (Codex round
                // 1): an escaped alias of our key is a duplicate serde resolves last-wins
                // while a plain-spelling matcher would edit the ineffective entry — and
                // telling aliases apart would mean reimplementing JSON unescaping. Any
                // doubt → the print branch.
                if depth == 1 && text[start..end].contains('\\') {
                    return None;
                }
                if depth == 1 && &text[start..end] == key {
                    let mut j = end + 1;
                    while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                        j += 1;
                    }
                    if j < bytes.len() && bytes[j] == b':' {
                        if found.is_some() {
                            return None; // duplicated key - refuse rather than guess
                        }
                        j += 1;
                        while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                            j += 1;
                        }
                        let val_end = json_value_extent(bytes, j)?;
                        found = Some((j, val_end));
                        i = val_end;
                        continue;
                    }
                }
                i = end + 1;
            }
            b'{' | b'[' => {
                depth += 1;
                i += 1;
            }
            b'}' | b']' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return found; // the object we were scanning closed
                }
                i += 1;
            }
            _ => i += 1,
        }
    }
    None
}

/// End (exclusive) of the JSON value starting at `start` (first byte of the value).
pub(crate) fn json_value_extent(bytes: &[u8], start: usize) -> Option<usize> {
    match *bytes.get(start)? {
        b'"' => scan_string(bytes, start + 1).map(|e| e + 1),
        b'{' | b'[' => {
            // One depth walk handles both bracket kinds; the root parse already guaranteed
            // they are balanced and correctly paired.
            let mut depth = 0usize;
            let mut i = start;
            while i < bytes.len() {
                match bytes[i] {
                    b'"' => i = scan_string(bytes, i + 1)? + 1,
                    b'{' | b'[' => {
                        depth += 1;
                        i += 1;
                    }
                    b'}' | b']' => {
                        depth = depth.checked_sub(1)?;
                        i += 1;
                        if depth == 0 {
                            return Some(i);
                        }
                    }
                    _ => i += 1,
                }
            }
            None
        }
        _ => {
            // number / true / false / null — runs to the next structural delimiter.
            let mut i = start;
            while i < bytes.len()
                && !matches!(bytes[i], b',' | b'}' | b']')
                && !bytes[i].is_ascii_whitespace()
            {
                i += 1;
            }
            (i > start).then_some(i)
        }
    }
}

/// `[mcp_servers.docli] url = …`, format-preserving via toml_edit.
fn merge_codex_toml(existing: Option<&str>, url: &str) -> MergeOutcome {
    let text = existing.map(str::trim).filter(|t| !t.is_empty());
    let mut doc = match text {
        None => toml_edit::DocumentMut::new(),
        Some(t) => match t.parse::<toml_edit::DocumentMut>() {
            Ok(d) => d,
            Err(_) => return MergeOutcome::Occupied("does not parse as TOML".to_string()),
        },
    };
    // Guard BEFORE any IndexMut: toml_edit's `doc[..]` panics when asked to index INTO a
    // non-table value (round-3 F1) — a `docli = "…"` string or a `[[mcp_servers.docli]]`
    // array must be handled, not unwound through `docli init`.
    if doc.get("mcp_servers").is_some() && doc["mcp_servers"].as_table_like().is_none() {
        return MergeOutcome::Occupied("mcp_servers is not a table".to_string());
    }
    if let Some(existing_entry) = doc.get("mcp_servers").and_then(|t| t.get("docli")) {
        // «Same» under the wholesale-convergence ruling (round-3 F2): exactly the entry we
        // would write — one `url` key with our URL. Anything else converges below.
        let same = existing_entry
            .get("url")
            .and_then(|v| v.as_str())
            .map(|u| u == url)
            .unwrap_or(false)
            && existing_entry.as_table_like().map(|t| t.len() == 1) == Some(true);
        if same {
            return MergeOutcome::AlreadyConfigured { same: true };
        }
    } else if doc.get("mcp_servers").is_none() {
        let mut t = toml_edit::Table::new();
        t.set_implicit(true);
        doc["mcp_servers"] = toml_edit::Item::Table(t);
    }
    // Create or wholesale-REPLACE our entry (both adapters share this semantics — the plan's
    // round-3 ruling). A regular table is cleared and refilled IN PLACE so its position and
    // any comment on its `[mcp_servers.docli]` header survive; every other prior shape
    // (string, number, array-of-tables, inline value) is replaced by assignment — which never
    // indexes INTO the old value, so nothing here can hit toml_edit's IndexMut panic.
    let existing_table = doc
        .get_mut("mcp_servers")
        .and_then(|t| t.get_mut("docli"))
        .and_then(|i| i.as_table_mut());
    if let Some(tbl) = existing_table {
        tbl.clear();
        tbl.insert("url", toml_edit::value(url));
    } else {
        let mut entry = toml_edit::Table::new();
        entry["url"] = toml_edit::value(url);
        doc["mcp_servers"]["docli"] = toml_edit::Item::Table(entry);
    }
    MergeOutcome::Write(doc.to_string())
}

fn merge_for(def: &AgentDef, existing: Option<&str>, url: &str) -> Option<(String, MergeOutcome)> {
    match def.adapter {
        McpAdapter::Json {
            path,
            top_key,
            entry_shape,
        } => {
            let mut outcome = merge_json(existing, top_key, &entry_shape.entry(url));
            // opencode.json is schema-anchored; give a FRESH file the $schema line so the
            // user's editor validates it (never touch an existing file's schema).
            if def.key == "opencode" && existing.map(str::trim).filter(|t| !t.is_empty()).is_none()
            {
                if let MergeOutcome::Write(_) = outcome {
                    outcome = MergeOutcome::Write(format!(
                        "{{\n  \"$schema\": \"https://opencode.ai/config.json\",\n  \"mcp\": {{\n    \"docli\": {}\n  }}\n}}\n",
                        entry_shape.entry(url)
                    ));
                }
            }
            Some((path.to_string(), outcome))
        }
        McpAdapter::CodexToml { path } => Some((path.to_string(), merge_codex_toml(existing, url))),
        McpAdapter::Print => None,
    }
}

/// The copy-paste snippet for one agent (the print branch and the occupied fallback).
/// Every embedded URL goes through the JSON string serializer (round-4 F-C — same reasoning
/// as F0: `config.server` is untrusted committed input, and a quote in it must yield a
/// snippet that is malformed-LOOKING at worst, never one that parses into extra keys when
/// pasted); the Codex shell line single-quotes it.
pub fn snippet(def: &AgentDef, url: &str) -> String {
    // A JSON string literal (quotes included) with `url` correctly escaped.
    let jurl = serde_json::Value::String(url.to_string()).to_string();
    match def.adapter {
        McpAdapter::Json {
            path,
            top_key,
            entry_shape,
        } => format!(
            "{path}:\n  {{ \"{top_key}\": {{ \"docli\": {} }} }}",
            entry_shape.entry(url)
        ),
        McpAdapter::CodexToml { path } => {
            // TOML basic strings share JSON's escape rules for the characters that matter
            // here (`"` and `\`), so the JSON-escaped literal is a valid TOML string too.
            format!(
                "{path} (or `codex mcp add docli --url '{}'` in the global config):\n  [mcp_servers.docli]\n  url = {jurl}",
                url.replace('\'', "'\\''")
            )
        }
        McpAdapter::Print => match def.key {
            "qwen" => format!(
                ".qwen/settings.json:\n  {{ \"mcpServers\": {{ \"docli\": {{ \"httpUrl\": {jurl} }} }} }}\n  (httpUrl, not url - url means SSE there)"
            ),
            "cline" => format!(
                "Cline's MCP config is global (the extension's MCP panel / ~/.cline/mcp.json):\n  {{ \"mcpServers\": {{ \"docli\": {{ \"type\": \"streamableHttp\", \"url\": {jurl} }} }} }}"
            ),
            "zed" => format!(
                "Zed settings.json (project-level placement of context_servers is undocumented - use \
                 `zed: open settings`):\n  {{ \"context_servers\": {{ \"docli\": {{ \"source\": \"custom\", \"url\": {jurl} }} }} }}"
            ),
            "windsurf" => format!(
                "Windsurf's MCP config is global (~/.codeium/windsurf/mcp_config.json):\n  {{ \"mcpServers\": {{ \"docli\": {{ \"serverUrl\": {jurl} }} }} }}"
            ),
            "sourcecraft" => format!(
                ".codeassistant/mcp.json:\n  {{ \"mcpServers\": {{ \"docli\": {{ \"url\": {jurl} }} }} }}\n  (docli MCP requires a browser OAuth flow; support for it is unverified in this client - if sign-in fails, use a verified OAuth-capable client)"
            ),
            "junie" => format!(
                ".junie/mcp/mcp.json:\n  {{ \"mcpServers\": {{ \"docli\": {{ \"url\": {jurl} }} }} }}\n  (docli MCP requires a browser OAuth flow; support for it is unverified in this client - if sign-in fails, use a verified OAuth-capable client)"
            ),
            "trae" => format!(
                ".trae/mcp.json:\n  {{ \"mcpServers\": {{ \"docli\": {{ \"url\": {jurl} }} }} }}\n  (docli MCP requires a browser OAuth flow; support for it is unverified in this client - if sign-in fails, use a verified OAuth-capable client)"
            ),
            "amp" => format!(
                ".amp/settings.json:\n  {{ \"amp.mcpServers\": {{ \"docli\": {{ \"url\": {jurl} }} }} }}"
            ),
            _ => format!(
                "add a remote MCP server named \"docli\" at {url} in the agent's MCP settings"
            ),
        },
    }
}

/// Apply the wiring for `selected` agent keys — genuinely BEST-EFFORT per agent: a failure on
/// one agent (an unwritable config, an unreadable dir) is reported with its snippet and the
/// rest proceed; `wire` itself never errors (the partial-success discipline). `labeled` gates
/// the bare-URL fallback note: the audience fence is byte-exact, so an agent that omits RFC
/// 8707 `resource` gets a bare-audience token the labeled route refuses — the note tells the
/// user the escape hatch instead of leaving them at an opaque `invalid_token`.
pub fn wire(project_root: &Path, selected: &[&AgentDef], url: &str, labeled: bool) {
    crate::ui::heading("This project's MCP connection");
    crate::ui::line(&format!("  {}", crate::ui::path(url)));
    crate::ui::detail(
        "Each agent authorizes itself through browser OAuth on first connection; docli writes \
         no credential into project files.",
    );
    if labeled {
        crate::ui::detail(
            "If an agent rejects this labeled URL with invalid_token, re-run with --mcp-bare.",
        );
    }
    sweep_cfg_temps(project_root, selected);
    for def in selected {
        if let Err(e) = wire_one(project_root, def, url) {
            crate::ui::refuse(&format!(
                "{}: failed ({e:#}). Add it by hand:\n    {}",
                def.display,
                snippet(def, url)
            ));
        }
    }
}

fn wire_one(project_root: &Path, def: &AgentDef, url: &str) -> Result<()> {
    let existing = read_existing(project_root, def);
    // An unreadable/undecodable existing file is the Occupied shape, not an abort: fold it in.
    let (existing, read_problem) = match existing {
        Ok(v) => (v, None),
        Err(reason) => (None, Some(reason)),
    };
    let merged = merge_for(def, existing.as_deref(), url);
    match merged {
        Some((rel, outcome)) => {
            let outcome = match read_problem {
                Some(reason) => MergeOutcome::Occupied(reason),
                None => outcome,
            };
            let abs = project_root.join(&rel);
            match outcome {
                MergeOutcome::Write(content) => {
                    if let Some(parent) = abs.parent() {
                        fs::create_dir_all(parent)?;
                    }
                    write_user_config(&abs, content.as_bytes())?;
                    crate::ui::ok(&format!("{}: wrote {}", def.display, rel));
                    if def.key == "codex" {
                        crate::ui::detail(
                            "Codex reads project configuration only in trusted repositories \
                             - approve the repository on first run.",
                        );
                    }
                }
                MergeOutcome::AlreadyConfigured { same: true } => {
                    crate::ui::ok(&format!("{}: {} already configured", def.display, rel));
                }
                MergeOutcome::AlreadyConfigured { same: false } => {
                    crate::ui::warn(&format!(
                        "{}: {} already has a \"docli\" entry; it was left unchanged. To \
                         update it by hand:\n    {}",
                        def.display,
                        rel,
                        snippet(def, url)
                    ));
                }
                MergeOutcome::Occupied(reason) => {
                    crate::ui::warn(&format!(
                        "{}: {} - {}; add it by hand:\n    {}",
                        def.display,
                        rel,
                        reason,
                        snippet(def, url)
                    ));
                }
            }
        }
        None => {
            crate::ui::line(&format!("  {}\n    {}", def.display, snippet(def, url)));
        }
    }
    Ok(())
}

/// Atomic write for a USER-OWNED config (round-4 F-B): same temp+rename shape as the mirror's
/// `write_atomic`, minus the read-only bit (these are the user's files — marking them
/// read-only would be hostile). Truncate-in-place on a `.mcp.json` would risk exactly the
/// data the whole splice discipline exists to preserve, with no re-sync and no doctor class
/// behind it.
///
/// Round-5 F1 — the rename must not change what the file IS: the write goes through the
/// RESOLVED target (a symlinked `.mcp.json` keeps its link identity and updates the shared
/// file behind it), and an existing target's permissions ride the temp through the swap (a
/// `0600` config holding another server's env secret must not come back umask-default).
/// Ownership is untouched by construction — everything here runs as the invoking user.
/// The ONE resolution both the writer and the sweep apply (Codex round 2, finding 4 — two
/// resolvers of one namespace drift): an existing target canonicalizes; a DANGLING symlink
/// still is one (`exists()` follows the link), so a temporarily-absent referent (an unmounted
/// dotfiles checkout) is reached by following `read_link` by hand — the link keeps its
/// identity and the referent is created. Bounded walk; a cycle degrades to the literal path.
/// Which agent configurations in this project already carry a docli entry — what
/// `docli status` reports.
///
/// The test is deliberately textual rather than a per-shape parse: status must never refuse to
/// render because a teammate's config has a trailing comma, and the wiring itself is idempotent
/// so a miss costs only a re-run of `--mcp`. But it looks for OUR ENTRY, not merely the origin:
/// a config naming this server under some other MCP entry (or in a comment) is not this project
/// wired to docli, and reporting it as such is a confident wrong answer.
///
/// Only agents whose configuration is a PROJECT file can be checked at all. The print-only
/// adapters keep their configuration globally (Cline's panel, Windsurf's home directory), which
/// is why they are absent here rather than reported as unwired — this answers «what does this
/// project carry», not «what is installed on this machine».
pub fn wired_here(project_root: &Path, server: &str) -> Vec<String> {
    let needle = server.trim_end_matches('/');
    let mut out = Vec::new();
    for def in AGENTS {
        let Some(rel) = def.config_path() else {
            continue;
        };
        let Ok(body) = fs::read_to_string(project_root.join(rel)) else {
            continue;
        };
        if entry_points_at(def, &body, needle) {
            // `display` already names the file it writes; appending `rel` printed it twice.
            out.push(def.display.to_string());
        }
    }
    out
}

/// Does this config's OWN `docli` entry point at `server`?
///
/// Parsed, not grepped. The first attempt matched the origin anywhere in the file, which called
/// a project wired because some OTHER MCP server happened to share the host; the second required
/// the two to share a line, which fails on a pretty-printed `.mcp.json` where `"docli": {` and
/// `"url": …` are two lines apart — reporting a config we wrote ourselves as unwired.
///
/// An unparseable file (JSONC comments, a trailing comma, a half-finished edit) is not an error
/// here: status must render, and the honest answer for a file we cannot read is «no entry
/// found». The wiring itself is idempotent, so a miss costs one re-run of `--mcp`.
fn entry_points_at(def: &AgentDef, body: &str, server: &str) -> bool {
    // A BOUNDED origin test: `starts_with` alone accepts `https://docli.ru.evil/api/mcp` for
    // the origin `https://docli.ru`, and status would then report an agent as wired to this
    // project when it is pointed at somebody else's host entirely.
    // …and the destination must be the MCP ROUTE. The origin alone accepts `…/api/other`, the
    // site root, or `…?x`: configurations that cannot reach MCP at all, which `status` would
    // then vouch for while the agent fails to connect.
    let is_ours = |url: &str| {
        let Some(rest) = url.strip_prefix(server) else {
            return false;
        };
        // EXACT: the API serves `/api/mcp` and `/api/mcp/c/<label>` and nothing else, so a
        // query string, a fragment or a trailing slash is a URL it refuses. Accepting the
        // lexical variants would vouch for a configuration that cannot connect.
        let path = rest;
        if path == MCP_BARE_PATH {
            return true;
        }
        // A LABELED route is only a route the server will serve: the label has to satisfy the
        // shared grammar and be the last segment. `/api/mcp/c/Blog` (uppercase),
        // `/api/mcp/c/` (empty) and `/api/mcp/c/blog/extra` are all refused by the API, so
        // reporting them as wired points the reader away from the actual problem.
        match path.strip_prefix(MCP_LABEL_PATH_PREFIX) {
            Some(label) => !label.is_empty() && docli_rules::valid_label(label),
            None => false,
        }
    };
    match def.adapter {
        McpAdapter::Json {
            top_key,
            entry_shape,
            ..
        } => serde_json::from_str::<serde_json::Value>(body)
            .ok()
            .and_then(|v| v.get(top_key)?.get("docli").cloned())
            .is_some_and(|entry| {
                // The key THIS agent requires, not any URL-ish key: Gemini reads `url` as SSE
                // and needs `httpUrl`, so an entry with `url` is a misconfiguration, and
                // reporting it as wired sends the reader looking for a problem elsewhere.
                let key = match entry_shape {
                    JsonShape::HttpUrl => "httpUrl",
                    JsonShape::TypeHttpUrl | JsonShape::UrlOnly | JsonShape::OpencodeRemote => {
                        "url"
                    }
                };
                // OpenCode's own switch: `enabled: false` is a configured-but-off server.
                let enabled = !matches!(entry_shape, JsonShape::OpencodeRemote)
                    || entry.get("enabled").and_then(|e| e.as_bool()) != Some(false);
                // …and the TRANSPORT the shape declares: an entry carrying an MCP url under
                // `type: "stdio"` is not a working remote server, whatever the url says.
                let transport_ok = match entry_shape {
                    JsonShape::TypeHttpUrl => {
                        entry.get("type").and_then(|t| t.as_str()) == Some("http")
                    }
                    JsonShape::OpencodeRemote => {
                        entry.get("type").and_then(|t| t.as_str()) == Some("remote")
                    }
                    // These shapes carry no transport field — the KEY is the transport.
                    JsonShape::UrlOnly | JsonShape::HttpUrl => true,
                };
                enabled
                    && transport_ok
                    && entry.get(key).and_then(|u| u.as_str()).is_some_and(is_ours)
            }),
        McpAdapter::CodexToml { .. } => body
            .parse::<toml_edit::DocumentMut>()
            .ok()
            .and_then(|doc| {
                Some(
                    doc.get("mcp_servers")?
                        .get("docli")?
                        .get("url")?
                        .as_str()?
                        .to_string(),
                )
            })
            .is_some_and(|u| is_ours(&u)),
        // Global configuration, not a project file: this answers «what does THIS PROJECT
        // carry», and there is nothing here to read.
        McpAdapter::Print => false,
    }
}

impl AgentDef {
    /// The PROJECT-relative config this adapter writes, or None for print-only agents.
    fn config_path(&self) -> Option<&'static str> {
        match self.adapter {
            McpAdapter::Json { path, .. } => Some(path),
            McpAdapter::CodexToml { path } => Some(path),
            McpAdapter::Print => None,
        }
    }
}

fn resolve_config_dest(target: &Path) -> std::path::PathBuf {
    if target.exists() {
        return fs::canonicalize(target).unwrap_or_else(|_| target.to_path_buf());
    }
    let mut p = target.to_path_buf();
    for _ in 0..8 {
        match fs::read_link(&p) {
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

pub fn write_user_config(target: &Path, bytes: &[u8]) -> Result<()> {
    use rand::RngCore;
    let dest = resolve_config_dest(target);
    let existing_perms = fs::metadata(&dest).ok().map(|m| m.permissions());
    let dir = dest
        .parent()
        .with_context(|| format!("no parent dir for {}", dest.display()))?;
    let mut suffix = [0u8; 8];
    rand::thread_rng().fill_bytes(&mut suffix);
    let tmp = dir.join(format!(".docli-cfg-{}.tmp", hex::encode(suffix)));
    debug_assert!(is_cfg_temp(tmp.file_name().unwrap().to_str().unwrap()));
    let write = (|| -> Result<()> {
        // The temp is BORN restrictive on unix (Codex round 2, finding 1): the bytes may
        // include another server's env secret copied from a 0600 config, and creating at
        // umask default would expose them for the write→chmod window (permanently, if the
        // process dies inside it). The copied perms then widen it to the target's own mode.
        #[cfg(unix)]
        {
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            let mut f = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&tmp)
                .with_context(|| format!("creating {}", tmp.display()))?;
            f.write_all(bytes)
                .with_context(|| format!("writing {}", tmp.display()))?;
        }
        #[cfg(not(unix))]
        fs::write(&tmp, bytes).with_context(|| format!("writing {}", tmp.display()))?;
        if let Some(perms) = existing_perms.clone() {
            let _ = fs::set_permissions(&tmp, perms);
        }
        // Ownership must survive the inode swap too (Codex round 3): a 0640 file whose GROUP
        // carries the read grant would silently re-group to the directory default through a
        // rename. Reproduce uid/gid on the temp; when that is not permitted (a group the
        // invoking user is not a member of), fall back to the inode-preserving direct write.
        #[cfg(unix)]
        if let Ok(md) = fs::metadata(&dest) {
            use std::os::unix::fs::MetadataExt;
            if std::os::unix::fs::chown(&tmp, Some(md.uid()), Some(md.gid())).is_err() {
                fs::write(&dest, bytes).with_context(|| format!("writing {}", dest.display()))?;
                let _ = crate::mountfs::remove_owned_file(&tmp);
                return Ok(());
            }
        }
        if fs::rename(&tmp, &dest).is_err() {
            // The Windows share-blocked-rename shape; fall back to a direct write (which
            // keeps the inode, so metadata is preserved trivially on this branch). The
            // fallback IS truncate-in-place — the recorded trade (Codex round 1): when the
            // OS forbids the swap there is no atomic option left, and this is exactly the
            // pre-D12 semantics, reachable only on that arm. Failing the wire instead would
            // trade a narrow crash window for a guaranteed no-write.
            fs::write(&dest, bytes).with_context(|| format!("writing {}", dest.display()))?;
            // The temp may CARRY the target's read-only bit (we just copied it) — removal
            // must lift it first, the same owned-removal shape as the mirror's (round-6).
            let _ = crate::mountfs::remove_owned_file(&tmp);
        }
        Ok(())
    })();
    if write.is_err() {
        let _ = crate::mountfs::remove_owned_file(&tmp);
    }
    write
}

/// Exactly the writer's temp shape (Codex round 1: the sweep DELETES matches, so the
/// recognizer must never be looser than the generator — the same rule the mirror's
/// `is_write_temp` follows; a user's own `.docli-cfg-manual-backup.tmp` must survive).
fn is_cfg_temp(name: &str) -> bool {
    // LOWERCASE hex only (Codex round 2): `hex::encode` never emits A-F, and the sweep
    // deletes matches — an uppercase spelling is a name the writer cannot have generated.
    name.strip_prefix(".docli-cfg-")
        .and_then(|r| r.strip_suffix(".tmp"))
        .is_some_and(|h| h.len() == 16 && h.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')))
}

/// Best-effort cleanup of `.docli-cfg-*.tmp` strays a crashed earlier `init` may have left in
/// the directories we are about to write into (round-5 F2 — the residue would otherwise be
/// unowned: no doctor class runs in a user project dir, and `git add -A` would commit it).
pub fn sweep_cfg_temps(project_root: &Path, selected: &[&AgentDef]) {
    let mut dirs: Vec<std::path::PathBuf> = selected
        .iter()
        .filter_map(|def| match def.adapter {
            McpAdapter::Json { path, .. } => Some(path),
            McpAdapter::CodexToml { path } => Some(path),
            McpAdapter::Print => None,
        })
        // The hook files are written by the SAME atomic writer, so a crashed `init` leaves the
        // same residue beside them (v0.28.6). One sweep, both writers.
        .chain(
            crate::hooks::HookAgent::all()
                .iter()
                .map(|a| a.config_path()),
        )
        .filter_map(|rel| {
            // The SAME resolution the writer applies (round-6 facet B; ONE fn since Codex
            // round 2): a symlinked config — dangling included — puts the temp beside the
            // RESOLVED target, so that is where a stale one lives.
            resolve_config_dest(&project_root.join(rel))
                .parent()
                .map(|p| p.to_path_buf())
        })
        .collect();
    dirs.dedup();
    for dir in dirs {
        let Ok(rd) = fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            let name = e.file_name();
            let n = name.to_string_lossy();
            if is_cfg_temp(&n) {
                // May carry a copied read-only bit — owned removal, never bare remove_file.
                let _ = crate::mountfs::remove_owned_file(&e.path());
            }
        }
    }
}

/// Write the shared contract into one agent's skills directory, injecting the frontmatter keys
/// that agent's schema admits (v0.28.6 D4).
///
/// **One body, never a fork.** `apps/cli/assets/SKILL.md` is a single compile-time constant with
/// a single set of pins, and it is written to the Agent Skills open-standard path
/// byte-for-byte — that path is spec-constrained, and a key outside the six the spec allows
/// (`name, description, license, compatibility, metadata, allowed-tools`) is a HARD packaging
/// error there, not an ignored field. Claude-Code-only keys are therefore injected AT COPY TIME,
/// for the destinations whose schema admits them, rather than written into the asset.
///
/// The key that earns its place is `paths` — *"Glob patterns that limit when this skill is
/// activated. When set, Claude loads the skill automatically only when working with files
/// matching the patterns"* (verified 2026-09-01, `code.claude.com/docs/en/skills`, frontmatter
/// reference). That inverts what the description was carrying alone: **`paths` is the primary
/// activation path and the description is the fallback.** The case that carries the data-loss
/// risk — an agent about to edit a mirrored file — now activates structurally instead of by
/// judgement, and the description still covers requests that touch no mirror file at all
/// («найди в докли заметку про X»).
///
/// The globs come from the MOUNT TABLE, so this is a template, not a copy: `docli init --dir
/// <custom>` means `docli-mirror/**` is not a safe static guess — it would be silently inert for
/// exactly the users who customised.
///
/// Two limits carry over from the guard, and neither is a defect to fix here: an absolute mount
/// outside the project root cannot be expressed as a project-relative glob, and path activation
/// triggers on files the agent WORKS WITH, so a write to a brand-new path inside the mirror may
/// not activate it. That second one is precisely why the guard exists and the activation does
/// not replace it — **activation informs, the hook enforces.**
pub fn copy_skill(project_root: &Path, dir: &str, skill_md: &str, globs: &[String]) -> Result<()> {
    let d = project_root.join(dir);
    fs::create_dir_all(&d)?;
    let body = if globs.is_empty() {
        skill_md.to_string()
    } else {
        inject_frontmatter(skill_md, "paths", globs)
    };
    fs::write(d.join("SKILL.md"), body)
        .with_context(|| format!("writing {}/SKILL.md", d.display()))?;
    Ok(())
}

/// Add `key: [values]` to a YAML frontmatter block, leaving the body untouched.
///
/// Values are emitted as a YAML LIST of double-quoted scalars, which is one of the two spellings
/// the field documents. A glob comes from a user-chosen directory name, so it goes through JSON
/// string escaping — YAML's double-quoted scalar shares JSON's escape rules for the two
/// characters that matter (`"` and `\`), and an unescaped one would break the whole frontmatter
/// block rather than just this key.
fn inject_frontmatter(skill_md: &str, key: &str, values: &[String]) -> String {
    let rendered: Vec<String> = values
        .iter()
        .map(|v| serde_json::Value::String(v.clone()).to_string())
        .collect();
    let line = format!(
        "{key}: [{}]
",
        rendered.join(", ")
    );
    // The frontmatter is the block between the first two `---` lines. Anything else is left
    // exactly as it is — a body without frontmatter is not something to repair here.
    let Some(rest) = skill_md.strip_prefix(
        "---
",
    ) else {
        return skill_md.to_string();
    };
    let Some(end) = rest.find(
        "
---",
    ) else {
        return skill_md.to_string();
    };
    format!(
        "---
{}
{line}---{}",
        &rest[..end],
        &rest[end + 4..]
    )
}

/// The activation globs for this project's mirrors, project-relative.
///
/// An ABSOLUTE mount contributes nothing: a project-relative glob cannot express it, and a
/// silently wrong pattern is worse than an absent one.
pub fn skill_globs(project_root: &Path, mounts: &[crate::config::Mount]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for m in mounts {
        let p = Path::new(&m.dir);
        let rel = if p.is_absolute() {
            match crate::config::mount_abs(project_root, m).strip_prefix(project_root) {
                Ok(r) => r.to_string_lossy().into_owned(),
                Err(_) => continue,
            }
        } else {
            m.dir.trim_end_matches('/').to_string()
        };
        if rel.is_empty() {
            continue;
        }
        let rel = rel.replace('\\', "/");
        // A directory whose NAME contains glob metacharacters contributes nothing, for the same
        // reason an absolute mount outside the project does: a pattern that is silently wrong is
        // worse than an absent one. `mirror[prod]/**` reads `[prod]` as a character class, so it
        // would fail to activate for the real directory AND could activate for others. Escaping
        // is not available to us either — the field documents «the same format as path-specific
        // rules» without naming an escape syntax, and inventing one would be exactly the
        // unverified-vendor-claim mistake D10 exists to stop. The guard still covers these
        // mounts; only the automatic ACTIVATION does not, and activation informs where the hook
        // enforces.
        if rel.contains(['*', '?', '[', ']', '{', '}', '!']) {
            continue;
        }
        let glob = format!("{rel}/**");
        if !out.contains(&glob) {
            out.push(glob);
        }
    }
    out
}

/// Deliver the contract to every selected agent that needs its own copy.
///
/// **Deliberately NOT inside [`wire`]** (v0.28.6 D4). `wire` is called only when an MCP URL was
/// produced, and that made the skill hostage to the MCP offer: `docli init --mcp none`, or a
/// guided run where the user ticks no agent, delivered the contract to `.agents/skills/` ONLY —
/// the one path Claude Code does not read, which is the entire defect this slice exists to fix.
/// A cautious user declining a config write must not silently lose the contract too.
pub fn install_skills(
    project_root: &Path,
    selected: &[&AgentDef],
    skill_md: &str,
    globs: &[String],
) {
    for def in selected {
        let Some(dir) = def.skill_copy_dir else {
            continue;
        };
        // Only agents whose frontmatter schema admits the key get it (D4).
        let g: &[String] = if def.accepts_claude_frontmatter {
            globs
        } else {
            &[]
        };
        match copy_skill(project_root, dir, skill_md, g) {
            Ok(()) => crate::ui::detail(&format!("wrote {dir}/SKILL.md")),
            Err(e) => crate::ui::refuse(&format!(
                "{}: could not write {dir}/SKILL.md ({e:#}) - the mirror contract is not \
                 installed for it; copy the file by hand or re-run `docli init`",
                def.display
            )),
        }
    }
}

/// `Err(reason)` means «a file exists there but we can't take its text» (unreadable, not
/// UTF-8) — the caller renders it as the Occupied/print branch, never as an init failure.
fn read_existing(
    project_root: &Path,
    def: &AgentDef,
) -> std::result::Result<Option<String>, String> {
    let rel = match def.adapter {
        McpAdapter::Json { path, .. } => path,
        McpAdapter::CodexToml { path } => path,
        McpAdapter::Print => return Ok(None),
    };
    let abs = project_root.join(rel);
    if !abs.exists() {
        return Ok(None);
    }
    let bytes = fs::read(&abs).map_err(|e| format!("could not read it ({e})"))?;
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|_| "it is not UTF-8 text".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const URL: &str = "https://docli.ru/api/mcp/c/myproj";

    // ---- label derivation ----

    #[test]
    fn sanitized_labels_always_satisfy_the_shared_grammar() {
        for name in [
            "My Docs",
            "докли",
            "a__b.c",
            "ПРОЕКТ-docs",
            "--x--",
            "",
            "___",
            &"x".repeat(200),
            "Ж",
        ] {
            let label = sanitize_label(name);
            assert!(
                docli_rules::valid_label(&label),
                "{name:?} -> {label:?} must be grammar-valid"
            );
        }
        assert_eq!(sanitize_label("My Docs"), "my-docs");
        assert_eq!(sanitize_label("a__b.c"), "a-b-c");
        assert_eq!(sanitize_label("ПРОЕКТ-docs"), "docs");
        // Nothing survives the ASCII grammar → a per-name hash fallback: two RU-named
        // projects must NOT share one connection (R3 — the common case, not an edge).
        assert!(sanitize_label("докли").starts_with("project-"));
        assert_ne!(sanitize_label("докли"), sanitize_label("заметки"));
        assert_eq!(
            sanitize_label("докли"),
            sanitize_label("докли"),
            "deterministic"
        );
        assert_eq!(sanitize_label(&"x".repeat(200)).len(), 64);
    }

    #[test]
    fn connection_url_joins_without_double_slash() {
        assert_eq!(
            connection_url("https://docli.ru/", "x"),
            "https://docli.ru/api/mcp/c/x"
        );
    }

    // ---- JSON merges ----

    #[test]
    fn fresh_file_is_created_with_the_top_key() {
        let entry = JsonShape::TypeHttpUrl.entry(URL);
        let MergeOutcome::Write(out) = merge_json(None, "mcpServers", &entry) else {
            panic!("fresh file must write");
        };
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["mcpServers"]["docli"]["type"], "http");
        assert_eq!(v["mcpServers"]["docli"]["url"], URL);
    }

    #[test]
    fn merge_preserves_the_users_text_verbatim_outside_the_splice() {
        // Weird-but-valid formatting the splice must not normalize away.
        let existing = "{\n    \"mcpServers\": {\n        \"other\": {\"command\":\"x\"}\n    },\n    \"unrelated\":  [1,2 , 3]\n}";
        let entry = JsonShape::TypeHttpUrl.entry(URL);
        let MergeOutcome::Write(out) = merge_json(Some(existing), "mcpServers", &entry) else {
            panic!("must merge");
        };
        // Both entries present and parseable…
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["mcpServers"]["docli"]["url"], URL);
        assert_eq!(v["mcpServers"]["other"]["command"], "x");
        // …and the untouched regions survive byte-for-byte (the odd spacing included).
        assert!(out.contains("\"unrelated\":  [1,2 , 3]"));
        assert!(out.contains("\"other\": {\"command\":\"x\"}"));
    }

    #[test]
    fn merge_creates_the_top_key_when_absent() {
        let existing = r#"{ "$schema": "https://opencode.ai/config.json", "theme": "dark" }"#;
        let entry = JsonShape::OpencodeRemote.entry(URL);
        let MergeOutcome::Write(out) = merge_json(Some(existing), "mcp", &entry) else {
            panic!("must merge");
        };
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["mcp"]["docli"]["type"], "remote");
        assert_eq!(v["theme"], "dark");
        assert!(out.contains("\"theme\": \"dark\""), "user text preserved");
    }

    #[test]
    fn our_entry_present_and_identical_is_a_no_op() {
        // Deliberate round-2 contract change (R2): an IDENTICAL entry is left alone; a
        // DIFFERENT one is updated in place (covered by
        // a_rerun_with_a_different_url_updates_our_entry_and_nothing_else) — never duplicated.
        let same =
            format!(r#"{{ "mcpServers": {{ "docli": {{ "type": "http", "url": "{URL}" }} }} }}"#);
        let entry = JsonShape::TypeHttpUrl.entry(URL);
        assert_eq!(
            merge_json(Some(&same), "mcpServers", &entry),
            MergeOutcome::AlreadyConfigured { same: true }
        );
    }

    #[test]
    fn jsonc_and_non_object_shapes_fall_to_print_not_error() {
        let entry = JsonShape::TypeHttpUrl.entry(URL);
        for bad in [
            "// vscode-style comment\n{ \"servers\": {} }",
            "[1,2,3]",
            "{ \"mcpServers\": [] }",
            "not json at all",
        ] {
            assert!(
                matches!(
                    merge_json(Some(bad), "mcpServers", &entry),
                    MergeOutcome::Occupied(_)
                ) || matches!(
                    merge_json(Some(bad), "servers", &entry),
                    MergeOutcome::Occupied(_)
                ),
                "{bad:?} must fall to the print branch"
            );
        }
    }

    #[test]
    fn nested_same_named_key_does_not_fool_the_splicer() {
        // A DEEPER "mcpServers" appears first in the text; the splicer must find the depth-1 one.
        let existing = r#"{ "profiles": { "mcpServers": { "decoy": 1 } }, "mcpServers": { "other": { "url": "u" } } }"#;
        let entry = JsonShape::UrlOnly.entry(URL);
        let MergeOutcome::Write(out) = merge_json(Some(existing), "mcpServers", &entry) else {
            panic!("must merge");
        };
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["mcpServers"]["docli"]["url"], URL);
        assert_eq!(v["profiles"]["mcpServers"]["decoy"], 1, "decoy untouched");
        assert!(v["profiles"]["mcpServers"].get("docli").is_none());
    }

    #[test]
    fn braces_inside_strings_do_not_break_depth_tracking() {
        let existing = r#"{ "note": "{ not a real { object [", "mcpServers": {} }"#;
        let entry = JsonShape::HttpUrl.entry(URL);
        let MergeOutcome::Write(out) = merge_json(Some(existing), "mcpServers", &entry) else {
            panic!("must merge");
        };
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["mcpServers"]["docli"]["httpUrl"], URL);
    }

    #[test]
    fn gemini_and_vscode_spellings_are_exact() {
        // The two easy-to-get-wrong shapes, pinned: Gemini takes httpUrl (url = SSE), and
        // VS Code's top-level key is "servers", not "mcpServers".
        assert_eq!(
            JsonShape::HttpUrl.entry_value(URL),
            serde_json::json!({"httpUrl": URL})
        );
        let vscode = AGENTS.iter().find(|a| a.key == "vscode").unwrap();
        let McpAdapter::Json { top_key, .. } = vscode.adapter else {
            panic!("vscode is a JSON adapter");
        };
        assert_eq!(top_key, "servers");
    }

    #[test]
    fn opencode_fresh_file_carries_the_schema_anchor() {
        let def = AGENTS.iter().find(|a| a.key == "opencode").unwrap();
        let (_, outcome) = merge_for(def, None, URL).unwrap();
        let MergeOutcome::Write(out) = outcome else {
            panic!("fresh write");
        };
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["$schema"], "https://opencode.ai/config.json");
        assert_eq!(v["mcp"]["docli"]["enabled"], true);
    }

    // ---- Codex TOML ----

    #[test]
    fn codex_toml_fresh_and_merge_preserve_user_content() {
        let MergeOutcome::Write(fresh) = merge_codex_toml(None, URL) else {
            panic!("fresh write");
        };
        assert!(fresh.contains("[mcp_servers.docli]"));
        assert!(fresh.contains(&format!("url = \"{URL}\"")));

        let existing =
            "# my codex settings\nmodel = \"gpt-5\"\n\n[mcp_servers.other]\ncommand = \"x\"\n";
        let MergeOutcome::Write(out) = merge_codex_toml(Some(existing), URL) else {
            panic!("must merge");
        };
        assert!(out.contains("# my codex settings"), "comment preserved");
        assert!(out.contains("model = \"gpt-5\""));
        assert!(out.contains("[mcp_servers.other]"));
        assert!(out.contains("[mcp_servers.docli]"));
    }

    #[test]
    fn codex_toml_identical_entry_is_a_no_op_and_bad_toml_prints() {
        let same = format!("[mcp_servers.docli]\nurl = \"{URL}\"\n");
        assert_eq!(
            merge_codex_toml(Some(&same), URL),
            MergeOutcome::AlreadyConfigured { same: true }
        );
        assert!(matches!(
            merge_codex_toml(Some("not = [valid"), URL),
            MergeOutcome::Occupied(_)
        ));
    }

    // ---- detection ----

    #[test]
    fn wired_here_needs_our_entry_not_just_the_origin() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let server = "https://docli.ru";

        // A config that merely MENTIONS the origin under someone else's server is not this
        // project wired to docli.
        fs::write(
            root.join(".mcp.json"),
            r#"{"mcpServers":{"other":{"type":"http","url":"https://docli.ru/api/other"}}}"#,
        )
        .unwrap();
        assert!(wired_here(root, server).is_empty());

        // Our own entry counts.
        fs::write(
            root.join(".mcp.json"),
            r#"{"mcpServers":{"docli":{"type":"http","url":"https://docli.ru/api/mcp/c/x"}}}"#,
        )
        .unwrap();
        assert_eq!(wired_here(root, server).len(), 1);

        // A PRETTY-PRINTED entry — the shape a user's editor leaves behind — must still count:
        // `"docli"` and its url are lines apart.
        fs::write(
            root.join(".mcp.json"),
            "{\n  \"mcpServers\": {\n    \"docli\": {\n      \"type\": \"http\",\n      \
             \"url\": \"https://docli.ru/api/mcp/c/x\"\n    }\n  }\n}\n",
        )
        .unwrap();
        assert_eq!(wired_here(root, server).len(), 1);

        // The TOML shape puts the URL on a later line than the table header.
        fs::create_dir_all(root.join(".codex")).unwrap();
        fs::write(
            root.join(".codex/config.toml"),
            "[mcp_servers.docli]\nurl = \"https://docli.ru/api/mcp/c/x\"\n",
        )
        .unwrap();
        assert_eq!(wired_here(root, server).len(), 2);

        // A different server in our table means this project points somewhere else.
        assert!(wired_here(root, "https://other.example").is_empty());

        // A docli entry pointing somewhere that is NOT the MCP route cannot connect, so it is
        // not «wired» however right the origin looks.
        for url in [
            "https://docli.ru/api/other",
            "https://docli.ru",
            "https://docli.ru/",
            "https://docli.ru?x",
        ] {
            fs::write(
                root.join(".mcp.json"),
                format!(r#"{{"mcpServers":{{"docli":{{"type":"http","url":"{url}"}}}}}}"#),
            )
            .unwrap();
            let wired = wired_here(root, server);
            assert!(
                !wired.iter().any(|w| w.contains("Claude")),
                "{url} is not the MCP route: {wired:?}"
            );
        }
        // The bare route and a labeled one both are.
        for url in [
            "https://docli.ru/api/mcp",
            "https://docli.ru/api/mcp/c/proj",
        ] {
            fs::write(
                root.join(".mcp.json"),
                format!(r#"{{"mcpServers":{{"docli":{{"type":"http","url":"{url}"}}}}}}"#),
            )
            .unwrap();
            assert!(
                wired_here(root, server)
                    .iter()
                    .any(|w| w.contains("Claude")),
                "{url} should count"
            );
        }

        // An off-grammar or over-long label is a route the server refuses.
        for url in [
            "https://docli.ru/api/mcp/c/Blog",
            "https://docli.ru/api/mcp/c/",
            "https://docli.ru/api/mcp/c/blog/extra",
            // Lexical variants the API refuses: a trailing slash, a query, a fragment.
            "https://docli.ru/api/mcp/c/blog/",
            "https://docli.ru/api/mcp/c/blog?x=1",
            "https://docli.ru/api/mcp/",
            "https://docli.ru/api/mcp?x=1",
        ] {
            fs::write(
                root.join(".mcp.json"),
                format!(r#"{{"mcpServers":{{"docli":{{"type":"http","url":"{url}"}}}}}}"#),
            )
            .unwrap();
            let wired = wired_here(root, server);
            assert!(
                !wired.iter().any(|w| w.contains("Claude")),
                "{url} is not a servable route: {wired:?}"
            );
        }
        // The wrong TRANSPORT is not a working entry either.
        fs::write(
            root.join(".mcp.json"),
            r#"{"mcpServers":{"docli":{"type":"stdio","url":"https://docli.ru/api/mcp"}}}"#,
        )
        .unwrap();
        assert!(!wired_here(root, server)
            .iter()
            .any(|w| w.contains("Claude")));

        // A LOOKALIKE host must not pass: `docli.ru.evil` is not `docli.ru`.
        fs::write(
            root.join(".mcp.json"),
            r#"{"mcpServers":{"docli":{"type":"http","url":"https://docli.ru.evil/api/mcp"}}}"#,
        )
        .unwrap();
        let wired = wired_here(root, server);
        assert!(
            !wired.iter().any(|w| w.contains("Claude")),
            "an unbounded prefix accepted a lookalike origin: {wired:?}"
        );

        // A SIMILARLY-NAMED table is not ours, and a later table's URL does not leak into it.
        fs::write(
            root.join(".codex/config.toml"),
            "[mcp_servers.docli_backup]\nurl = \"https://docli.ru/api/mcp/c/x\"\n",
        )
        .unwrap();
        let wired = wired_here(root, server);
        assert!(
            !wired.iter().any(|w| w.contains("Codex")),
            "prefix match leaked: {wired:?}"
        );
        fs::write(
            root.join(".codex/config.toml"),
            "[mcp_servers.docli]\ncommand = \"x\"\n\n[mcp_servers.other]\nurl = \
             \"https://docli.ru/api/mcp\"\n",
        )
        .unwrap();
        let wired = wired_here(root, server);
        assert!(
            !wired.iter().any(|w| w.contains("Codex")),
            "a following table's url was read as ours: {wired:?}"
        );
    }

    #[test]
    fn detection_reads_project_and_home_markers() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("p");
        let home = tmp.path().join("h");
        fs::create_dir_all(project.join(".cursor")).unwrap();
        fs::create_dir_all(home.join(".codex")).unwrap();
        let detected = detect(&project, Some(&home));
        assert!(detected.contains(&"cursor"), "project marker");
        assert!(detected.contains(&"codex"), "home marker");
        assert!(!detected.contains(&"gemini"));
        assert_eq!(detect(&project, None), vec!["cursor"]);
        // Codex round 3: a DANGLING symlink marker still detects.
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(project.join("nowhere.json"), project.join(".mcp.json"))
                .unwrap();
            assert!(
                detect(&project, None).contains(&"claude"),
                "dangling marker"
            );
        }
    }

    // ---- the skill copy (D4) ----

    #[test]
    fn the_shared_asset_stays_spec_clean_and_claude_gets_the_extension() {
        // ONE body, never a fork: `.agents/skills/` is the open-standard path, where a key
        // outside the spec's six is a HARD packaging error rather than an ignored field. The
        // Claude-Code-only `paths` is injected at COPY TIME, for the one destination whose
        // schema admits it.
        let tmp = tempfile::tempdir().unwrap();
        let asset = "---\nname: docli-mirror\ndescription: x\n---\n\n# body\n";
        let globs = vec!["docli-mirror/**".to_string()];
        let claude = agent("claude").unwrap();
        let qwen = agent("qwen").unwrap();
        assert!(claude.accepts_claude_frontmatter);
        assert!(!qwen.accepts_claude_frontmatter);
        install_skills(tmp.path(), &[claude, qwen], asset, &globs);

        let c =
            fs::read_to_string(tmp.path().join(".claude/skills/docli-mirror/SKILL.md")).unwrap();
        assert!(c.contains("paths: [\"docli-mirror/**\"]"), "{c}");
        assert!(
            c.contains("name: docli-mirror") && c.ends_with("# body\n"),
            "{c}"
        );
        // Everything else gets the asset byte for byte.
        let q = fs::read_to_string(tmp.path().join(".qwen/skills/docli-mirror/SKILL.md")).unwrap();
        assert_eq!(q, asset);
    }

    #[test]
    fn the_globs_come_from_the_mount_table_not_a_static_guess() {
        // `docli init --dir <custom>` means `docli-mirror/**` is not a safe guess — it would be
        // silently inert for exactly the users who customised.
        use crate::config::Mount;
        let root = Path::new("/proj");
        let m = |dir: &str| Mount {
            workspace: uuid::Uuid::from_u128(1),
            dir: dir.into(),
            folder: None,
            name: None,
        };
        assert_eq!(
            skill_globs(root, &[m("notes"), m("docli-mirror/agitek")]),
            vec!["notes/**".to_string(), "docli-mirror/agitek/**".to_string()]
        );
        // An ABSOLUTE mount inside the project resolves; one outside contributes nothing,
        // because a project-relative glob cannot express it and a silently wrong pattern is
        // worse than an absent one (the same limit the guard states).
        assert_eq!(skill_globs(root, &[m("/proj/inside")]), vec!["inside/**"]);
        assert!(skill_globs(root, &[m("/var/tmp/outside")]).is_empty());
        // …and so does a name carrying glob metacharacters: `mirror[prod]/**` reads `[prod]` as
        // a character class, which would miss the real directory and could match others. A
        // silently wrong pattern is worse than an absent one, and no escape syntax is
        // documented for this field to reach for.
        for meta in ["mirror[prod]", "a*b", "q?", "{x,y}", "!neg"] {
            assert!(skill_globs(root, &[m(meta)]).is_empty(), "{meta}");
        }
    }

    #[test]
    fn a_glob_that_would_break_the_frontmatter_is_escaped() {
        // The glob comes from a user-chosen directory name; an unescaped quote would break the
        // whole block rather than just this key.
        let asset = "---\nname: x\n---\nbody\n";
        let out = inject_frontmatter(asset, "paths", &["we\"ird/**".to_string()]);
        assert!(out.contains(r#"paths: ["we\"ird/**"]"#), "{out}");
        // A body with no frontmatter is left alone rather than "repaired".
        assert_eq!(
            inject_frontmatter("no frontmatter\n", "paths", &["a".into()]),
            "no frontmatter\n"
        );
    }

    // ---- wire() end to end ----

    #[test]
    fn wire_writes_selected_adapters_and_copies_off_standard_skills() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::write(
            root.join(".mcp.json"),
            r#"{ "mcpServers": { "other": { "command": "x" } } }"#,
        )
        .unwrap();
        let selected: Vec<&AgentDef> = ["claude", "codex", "qwen"]
            .iter()
            .map(|k| agent(k).unwrap())
            .collect();
        wire(root, &selected, URL, true);
        install_skills(root, &selected, "SKILLBODY", &[]);

        let mcp: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(root.join(".mcp.json")).unwrap()).unwrap();
        assert_eq!(mcp["mcpServers"]["docli"]["url"], URL);
        assert_eq!(mcp["mcpServers"]["other"]["command"], "x");

        let codex = fs::read_to_string(root.join(".codex/config.toml")).unwrap();
        assert!(codex.contains("[mcp_servers.docli]"));

        // Qwen: no MCP file written (print-only), but the off-standard skill copy lands.
        assert!(!root.join(".qwen/settings.json").exists());
        assert_eq!(
            fs::read_to_string(root.join(".qwen/skills/docli-mirror/SKILL.md")).unwrap(),
            "SKILLBODY"
        );
    }

    #[test]
    fn wire_never_writes_a_credential_or_token_field() {
        // The no-credential pin (D12.4): everything any adapter can ever write derives from
        // the URL alone. Generate every write outcome and scan for credential-shaped keys.
        let tmp = tempfile::tempdir().unwrap();
        let selected: Vec<&AgentDef> = AGENTS.iter().collect();
        wire(tmp.path(), &selected, URL, true);
        install_skills(tmp.path(), &selected, "S", &[]);
        for entry in walkdir(tmp.path()) {
            let body = fs::read_to_string(&entry)
                .unwrap_or_default()
                .to_lowercase();
            for needle in ["token", "authorization", "bearer", "secret", "password"] {
                assert!(
                    !body.contains(needle),
                    "{} must not carry {needle}",
                    entry.display()
                );
            }
        }
    }

    #[test]
    fn a_rerun_with_a_different_url_updates_our_entry_and_nothing_else() {
        // R2: --mcp-bare (or a label change) must converge the config, not print a no-op.
        let existing = r#"{
    "mcpServers": {
        "other": {"command":"x"},
        "docli": { "type": "http", "url": "https://docli.ru/api/mcp/c/old-label" }
    },
    "unrelated":  [1,2 , 3]
}"#;
        let entry = JsonShape::TypeHttpUrl.entry(URL);
        let MergeOutcome::Write(out) = merge_json(Some(existing), "mcpServers", &entry) else {
            panic!("must update in place");
        };
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["mcpServers"]["docli"]["url"], URL);
        assert_eq!(v["mcpServers"]["other"]["command"], "x");
        assert!(
            out.contains("\"unrelated\":  [1,2 , 3]"),
            "user text preserved"
        );
        assert!(!out.contains("old-label"));

        // Same convergence for the TOML adapter, comments preserved.
        let toml_existing =
            "# mine\n[mcp_servers.docli]\nurl = \"https://docli.ru/api/mcp/c/old\"\n";
        let MergeOutcome::Write(out) = merge_codex_toml(Some(toml_existing), URL) else {
            panic!("must update in place");
        };
        assert!(out.contains("# mine"));
        assert!(out.contains(&format!("url = \"{URL}\"")));
        assert!(!out.contains("/c/old\""));

        // A hand-edited NON-OBJECT docli value still gets replaced cleanly.
        let weird = r#"{ "mcpServers": { "docli": "hand-edited" } }"#;
        let MergeOutcome::Write(out) = merge_json(Some(weird), "mcpServers", &entry) else {
            panic!("must update in place");
        };
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["mcpServers"]["docli"]["url"], URL);
    }

    #[test]
    fn json_metacharacters_in_the_server_cannot_break_or_extend_the_entry() {
        // Round-3 F0: config.server is committed, shared, and validated by nobody — a quote
        // in it must stay INSIDE the url string, never mint a sibling key in a teammate's
        // config and never panic init.
        let evil = r#"https://e.ru", "command": "curl evil"#;
        let url = connection_url(evil, "x");
        let entry = JsonShape::TypeHttpUrl.entry(&url);
        let v: serde_json::Value = serde_json::from_str(&entry).unwrap();
        assert_eq!(v["url"], url);
        assert!(v.get("command").is_none());
        let MergeOutcome::Write(out) = merge_json(None, "mcpServers", &entry) else {
            panic!("must write");
        };
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["mcpServers"]["docli"]["url"], url);
        assert!(v["mcpServers"]["docli"].get("command").is_none());
    }

    #[test]
    fn codex_toml_weird_docli_shapes_replace_instead_of_panicking() {
        // Round-3 F1: toml_edit IndexMut panics when indexing INTO a non-table — every prior
        // shape of our key must converge to the fresh entry instead.
        for weird in [
            "[mcp_servers]\ndocli = \"https://old\"\n",
            "mcp_servers = { docli = 5 }\n",
            "[[mcp_servers.docli]]\nurl = \"x\"\n",
        ] {
            let MergeOutcome::Write(out) = merge_codex_toml(Some(weird), URL) else {
                panic!("{weird:?} must converge");
            };
            let parsed: toml_edit::DocumentMut = out.parse().unwrap();
            assert_eq!(
                parsed["mcp_servers"]["docli"]["url"].as_str(),
                Some(URL),
                "{weird:?} -> {out}"
            );
        }
        // The wholesale-convergence ruling: a user-augmented entry is REPLACED, not merged —
        // grafting our url onto a stdio shim would produce a mixed entry no client accepts.
        let aug = "[mcp_servers.docli]\ncommand = \"npx\"\nurl = \"https://old\"\n";
        let MergeOutcome::Write(out) = merge_codex_toml(Some(aug), URL) else {
            panic!("must converge");
        };
        assert!(!out.contains("command"), "wholesale replace: {out}");
        assert!(out.contains(&format!("url = \"{URL}\"")));
    }

    #[test]
    fn write_user_config_swaps_atomically_and_preserves_what_the_file_is() {
        let tmp = tempfile::tempdir().unwrap();
        // Fresh file: plain create, no temp residue.
        let fresh = tmp.path().join("fresh.json");
        write_user_config(&fresh, b"{}").unwrap();
        assert_eq!(fs::read(&fresh).unwrap(), b"{}");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // Round-5 F1a: a 0600 config (another server's env secret) stays 0600.
            let secret = tmp.path().join("secret.json");
            fs::write(&secret, b"old").unwrap();
            fs::set_permissions(&secret, fs::Permissions::from_mode(0o600)).unwrap();
            write_user_config(&secret, b"new").unwrap();
            assert_eq!(fs::read(&secret).unwrap(), b"new");
            assert_eq!(
                fs::metadata(&secret).unwrap().permissions().mode() & 0o777,
                0o600
            );

            // Round-5 F1b: a symlinked config keeps its link identity; the write lands in
            // the shared file behind it — which lives in a DIFFERENT directory, so this
            // also proves the temp goes beside the RESOLVED target (round-6 facet B).
            let shared_dir = tmp.path().join("dotfiles");
            fs::create_dir_all(&shared_dir).unwrap();
            let shared = shared_dir.join("shared.json");
            fs::write(&shared, b"shared-old").unwrap();
            let link = tmp.path().join("link.json");
            std::os::unix::fs::symlink(&shared, &link).unwrap();
            write_user_config(&link, b"through-the-link").unwrap();
            assert!(fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink());
            assert_eq!(fs::read(&shared).unwrap(), b"through-the-link");
            let link_strays: Vec<_> = fs::read_dir(&shared_dir)
                .unwrap()
                .filter_map(|e| e.ok())
                .filter(|e| e.file_name().to_string_lossy().starts_with(".docli-cfg-"))
                .collect();
            assert!(
                link_strays.is_empty(),
                "temp beside the resolved target: {link_strays:?}"
            );
        }

        // No temp residue anywhere after the writes.
        let strays: Vec<_> = fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with(".docli-cfg-"))
            .collect();
        assert!(strays.is_empty(), "{strays:?}");
    }

    #[test]
    fn wire_sweeps_stale_cfg_temps_from_the_dirs_it_writes() {
        // Round-5 F2: a crashed earlier init's residue is collected on the next wiring run —
        // READ-ONLY residue included (round-6 facet A: the temp may carry a copied
        // read-only bit, and removal lifts it first).
        let tmp = tempfile::tempdir().unwrap();
        let stray = tmp.path().join(".docli-cfg-deadbeefdeadbeef.tmp");
        fs::write(&stray, b"partial").unwrap();
        let mut p = fs::metadata(&stray).unwrap().permissions();
        p.set_readonly(true);
        fs::set_permissions(&stray, p).unwrap();
        wire(tmp.path(), &[agent("claude").unwrap()], URL, true);
        assert!(!stray.exists(), "wire must sweep its own stale temps");

        // Round-6 facet B: with a SYMLINKED config, the sweep visits the resolved dir.
        #[cfg(unix)]
        {
            let shared_dir = tmp.path().join("dotfiles");
            fs::create_dir_all(&shared_dir).unwrap();
            fs::write(shared_dir.join("mcp.json"), "{}").unwrap();
            // Replace the .mcp.json wire() just wrote with a symlink into dotfiles/.
            fs::remove_file(tmp.path().join(".mcp.json")).unwrap();
            std::os::unix::fs::symlink(shared_dir.join("mcp.json"), tmp.path().join(".mcp.json"))
                .unwrap();
            let resolved_stray = shared_dir.join(".docli-cfg-feedfacefeedface.tmp");
            fs::write(&resolved_stray, b"partial").unwrap();
            wire(tmp.path(), &[agent("claude").unwrap()], URL, true);
            assert!(
                !resolved_stray.exists(),
                "the sweep must visit the RESOLVED dir"
            );

            // Codex round 2 (finding 4): the sweep resolves DANGLING links the same way the
            // writer does — a stale temp beside a missing referent is still collected.
            let dangle_dir = tmp.path().join("dotfiles2");
            fs::create_dir_all(&dangle_dir).unwrap();
            fs::remove_file(tmp.path().join(".mcp.json")).unwrap();
            std::os::unix::fs::symlink(
                dangle_dir.join("missing.json"),
                tmp.path().join(".mcp.json"),
            )
            .unwrap();
            let dangle_stray = dangle_dir.join(".docli-cfg-0123456789abcdef.tmp");
            fs::write(&dangle_stray, b"partial").unwrap();
            wire(tmp.path(), &[agent("claude").unwrap()], URL, true);
            assert!(!dangle_stray.exists(), "dangling-link dirs are swept too");
            assert!(
                fs::symlink_metadata(tmp.path().join(".mcp.json"))
                    .unwrap()
                    .file_type()
                    .is_symlink(),
                "the dangling link kept its identity through the write"
            );
        }
    }

    #[test]
    fn escaped_key_aliases_refuse_the_merge_instead_of_editing_the_loser() {
        // Codex round 1 (finding 4): a key spelled with a JSON unicode escape (u0064 = 'd')
        // unescapes to "docli" — serde keeps the LAST duplicate, a plain-spelling splice
        // would edit the FIRST. Any escaped key at the scanned depth must refuse to the
        // print branch. The escapes are constructed at runtime so no toolchain in between
        // can silently decode them out of the source.
        let bs = char::from(92u8);
        let aliased = format!(
            r#"{{ "mcpServers": {{ "docli": {{"url":"old1"}}, "{bs}u0064ocli": {{"url":"old2"}} }} }}"#
        );
        let entry = JsonShape::UrlOnly.entry(URL);
        // Prove the premise: serde parses BOTH spellings as one key, last wins.
        let parsed: serde_json::Value = serde_json::from_str(&aliased).unwrap();
        assert_eq!(parsed["mcpServers"]["docli"]["url"], "old2");
        let out = merge_json(Some(&aliased), "mcpServers", &entry);
        assert!(
            !matches!(out, MergeOutcome::Write(_)),
            "must not write over an aliased duplicate: {out:?}"
        );
        // Same guard one level up: an escaped depth-1 alias of the TOP key (u006d = 'm').
        let top_aliased = format!(
            r#"{{ "mcpServers": {{ "a": 1 }}, "{bs}u006dcpServers": {{ "docli": {{"url":"x"}} }} }}"#
        );
        assert!(
            !matches!(
                merge_json(Some(&top_aliased), "mcpServers", &entry),
                MergeOutcome::Write(_)
            ),
            "top-level aliases refuse too"
        );
    }

    #[test]
    fn cfg_sweep_spares_user_files_that_merely_share_the_prefix() {
        // Codex round 1 (finding 2): the sweep deletes matches, so the recognizer is the
        // writer's exact 16-hex shape — a hand-named backup survives.
        let tmp = tempfile::tempdir().unwrap();
        let user_file = tmp.path().join(".docli-cfg-manual-backup.tmp");
        fs::write(&user_file, b"the user's own file").unwrap();
        wire(tmp.path(), &[agent("claude").unwrap()], URL, true);
        assert!(
            user_file.exists(),
            "non-16-hex names are not ours to delete"
        );
        assert!(is_cfg_temp(".docli-cfg-00112233445566aa.tmp"));
        assert!(!is_cfg_temp(".docli-cfg-manual-backup.tmp"));
        // Uppercase hex is a name the writer can't generate (Codex round 2).
        assert!(!is_cfg_temp(".docli-cfg-DEADBEEFDEADBEEF.tmp"));
    }

    #[cfg(unix)]
    #[test]
    fn a_dangling_symlink_keeps_its_identity_and_gets_its_referent_created() {
        // Codex round 1 (finding 3): exists() follows links, so a dangling .mcp.json link
        // must not be replaced by a regular file — the referent is created instead.
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("dotfiles/mcp.json");
        fs::create_dir_all(missing.parent().unwrap()).unwrap();
        let link = tmp.path().join(".mcp.json");
        std::os::unix::fs::symlink(&missing, &link).unwrap();
        assert!(!link.exists(), "dangling: exists() follows the link");
        write_user_config(&link, b"{}").unwrap();
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink(),
            "the link survives"
        );
        assert_eq!(
            fs::read(&missing).unwrap(),
            b"{}",
            "the referent was created"
        );
    }

    #[test]
    fn snippets_escape_metacharacter_urls_too() {
        // Round-4 F-C: F0 applied to what is PRINTED for the user to paste, not only what is
        // written. Every JSON-shaped snippet must still parse with the url intact.
        let evil = connection_url(r#"https://e.ru", "command": "curl evil"#, "x");
        for def in AGENTS {
            let snip = snippet(def, &evil);
            // Extract each `{ ... }` JSON body line and prove it parses with our url inside.
            for line in snip.lines().filter(|l| l.trim_start().starts_with('{')) {
                let v: serde_json::Value = serde_json::from_str(line.trim()).unwrap_or_else(|e| {
                    panic!("{}: unparseable snippet line {line:?}: {e}", def.key)
                });
                assert!(
                    line.contains("e.ru"),
                    "{}: url missing from {line:?}",
                    def.key
                );
                assert!(v.is_object());
            }
        }
    }

    #[test]
    fn duplicate_depth1_keys_refuse_the_splice() {
        // serde_json keeps the LAST duplicate; a first-match splice would edit the ignored
        // object. The walker refuses instead (Occupied → the print branch).
        let dup = r#"{ "mcpServers": { "a": 1 }, "mcpServers": { "b": 2 } }"#;
        let entry = JsonShape::UrlOnly.entry(URL);
        assert!(matches!(
            merge_json(Some(dup), "mcpServers", &entry),
            MergeOutcome::Occupied(_)
        ));
    }

    #[test]
    fn non_utf8_existing_config_prints_instead_of_failing_init() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join(".mcp.json"), [0xFF, 0xFE, 0x00, 0x80]).unwrap();
        let selected = vec![agent("claude").unwrap()];
        // Best-effort: wire() must not panic/error, and must not touch the bytes.
        wire(tmp.path(), &selected, URL, true);
        install_skills(tmp.path(), &selected, "S", &[]);
        assert_eq!(
            fs::read(tmp.path().join(".mcp.json")).unwrap(),
            [0xFF, 0xFE, 0x00, 0x80]
        );
    }

    #[test]
    fn claude_code_gets_its_skill_at_the_documented_path() {
        // Regression pin, 2026-09-01. This row said `None`, on a research claim that Claude Code
        // reads `.agents/skills/` natively. It does not: its docs list four skill locations and
        // `.agents` is not among them, and in the field the file sat in `.agents/skills/` while
        // `/reload-skills` kept reporting "no changes". The failure was SILENT — `docli init`
        // reported the agent configured, which was true of MCP and false of the skill.
        //
        // A test cannot check what another program reads, so what is pinned here is the decision:
        // Claude Code gets an explicit copy at its documented path. If someone reverts this to
        // `None`, they have to delete this test and read why first.
        let claude = agent("claude").expect("the claude row");
        assert_eq!(
            claude.skill_copy_dir,
            Some(".claude/skills/docli-mirror"),
            "Claude Code needs an explicit skill copy; it does not read `.agents/skills/`"
        );
    }

    #[test]
    fn bare_url_and_every_named_agent_has_a_specific_snippet() {
        assert_eq!(
            connection_url_bare("https://docli.ru/"),
            "https://docli.ru/api/mcp"
        );
        // EVERY print-only agent in the TABLE renders a snippet naming its config surface —
        // iterating AGENTS (not a hand list) so a new entry can't silently fall to the
        // generic arm, which is reserved for genuinely unlisted agents.
        for def in AGENTS {
            let snip = snippet(def, URL);
            assert!(snip.contains(URL), "{}: {snip}", def.key);
            assert!(
                !snip.starts_with("add a remote MCP server"),
                "{} fell to the generic arm",
                def.key
            );
        }
    }

    fn walkdir(dir: &Path) -> Vec<std::path::PathBuf> {
        let mut out = Vec::new();
        if let Ok(rd) = fs::read_dir(dir) {
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    out.extend(walkdir(&p));
                } else {
                    out.push(p);
                }
            }
        }
        out
    }
}
