# Docli CLI

A local **read-only mirror** of your [docli](https://docli.ru) workspaces — built for power
users who work primarily through coding agents (Claude Code, Codex, and friends).

The CLI **complements a docli MCP connection, never replaces it**: your agent still writes
through its MCP connector; the CLI gives it a fast local corpus to read and grep — no network
round-trip per read.

- **The mirror is a disposable cache.** `docli sync` brings it in line with the current server
  state; deleting it and syncing again is safe. Local edits are unsupported: they are never
  uploaded and are overwritten when the note next changes server-side.
- **`docli search` runs server-side.** It uses the product's BM25 search with Russian and
  English stemming and prints local paths. Local grep can supplement server results, but a
  local miss does not prove that content is absent from the server.
- **Attachments are represented by markers.** Each attachment is mirrored as a `*.docli`
  metadata sidecar (ID, MIME type, size, SHA-256 digest, and wikilink); its bytes remain on
  the server and are fetched over MCP.
- **`docli doctor`** — a three-way reconciliation (server / disk / state) with typed discrepancies.

## Install

```sh
curl -fsSL https://docli.ru/install.sh | sh        # macOS / Linux
irm https://docli.ru/install.ps1 | iex             # Windows (PowerShell)
```

Secondary channel: `npx @docli/cli` (npm). Updates: `docli self-update` (release signatures are
verified against a key pinned inside the binary).

## Quick start

```sh
docli login                  # browser sign-in via loopback OAuth; stores a device credential
docli init                   # writes docli.toml + the agent skill, lists your workspaces
docli init --workspace <id> --dir notes-mirror
docli sync                   # one-shot sync of every mount
docli search "what you need" # server search, local paths
```

Commit `docli.toml`: it identifies workspaces by ID, never by handle, and grants no access.
The mirror directories and `.docli/` must be listed in `.gitignore`; otherwise `docli sync`
refuses to run.

## Wiring coding agents

`docli init` installs the mirror contract at `.agents/skills/docli-mirror/SKILL.md` — the
standard path defined by the open Agent Skills specification, read natively by Claude Code,
Codex, Gemini CLI, Cursor, Windsurf, Zed, GitHub Copilot in VS Code, Copilot CLI, OpenCode,
and Amp.

It can also — **only if you ask** — add this project's docli MCP connection to your agents'
configs: `docli init --mcp auto` (detected agents), `--mcp claude,codex,…` (a list), or the
prompt shown during an interactive run. The generated docli entry contains the URL and any
client-required transport fields, but no token or other credential: each agent authorizes
itself in the browser on first connection, and the docli entry itself is safe to commit.
The URL carries a per-project connection label (persisted as
`mcp_label` in `docli.toml`; override with `--mcp-label`, or `--mcp-bare` for the unlabeled
`…/api/mcp` if your client doesn't send RFC 8707 `resource`). Agents whose config we can't
write safely get a copy-paste snippet instead.

## Repositories

The PRIMARY public repository and discussion venue is **GitVerse:
[agitek/docli-cli](https://gitverse.ru/agitek/docli-cli)** (Russian README). This GitHub mirror
([Docli-ru/docli-cli](https://github.com/Docli-ru/docli-cli)) exists for ecosystem reach
(Releases feed brew/scoop/npm). PRs are accepted in both — see CONTRIBUTING.md.

## License

MIT (see LICENSE). The CLI and its crates (`docli-sync-wire`, `docli-rules`) build standalone
from this repository.
