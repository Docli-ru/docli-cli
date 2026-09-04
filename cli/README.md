# docli-cli

A local **read-only mirror** of your [**docli**](https://docli.ru) workspaces — built for power
users who work primarily through coding agents (Claude Code, Codex, and friends).

The CLI **complements a docli MCP connection, never replaces it**: your agent still writes
through its MCP connector, while the CLI reads from a local copy with no network round-trip per
read.

- **The mirror is a disposable cache.** `docli sync` applies new server changes;
  `docli sync --full` rebuilds it and prunes stale files. Deleting the mirror and syncing again is
  safe. Local edits are unsupported: they are never uploaded, and they survive only until a full
  sync rebuilds the mirror or the note next changes server-side — then they are overwritten with
  no conflict copy.
- **`docli search` finds and `docli read` opens.** Search runs server-side — the product's BM25
  search with Russian and English stemming — and prints each hit's server path and node id. For a
  note, `docli read` prints its mirrored content; for a file, the marker metadata described below.
  `docli search` no longer publishes a local mirror path for each result: one verb finds things,
  one verb opens them, and the address is the one the server uses. (`docli doctor` still prints
  directories and paths — it has to, to report a discrepancy.)

  **Upgrading from 0.1.4 — a breaking output change.** `docli search` no longer prints a local
  path, and `--json` no longer carries `local_path`. Pass a hit's `server_path` to `docli read`,
  or its `id` to `docli read --id`.
- **Attachments are metadata here.** `docli read` on a file prints its ID, MIME type, size,
  SHA-256 digest and wikilink; the bytes remain on the server and are fetched over MCP.
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
docli init                   # guided setup: sign-in, workspace, directory, agents
```

One command walks the whole way: if the device is not connected yet, `docli init` offers to sign
in, then you pick a workspace from a list (arrow keys — no UUIDs to type), the mirror directory,
which agent configurations to wire, and whether to append the `.gitignore` lines.

The same steps separately, and for scripts where nothing may ask a question:

```sh
docli login                              # browser sign-in via loopback OAuth
docli list                               # every workspace; the ones mounted here are marked *
docli init --workspace <id> --dir .docli/mirror/notes --gitignore
docli sync                               # one-shot sync of every mount
docli search "what you need"             # server search across mounts
docli read "Notes/plan.md"               # print a mirrored note (--lines, --id, --json)
docli status                             # sign-in, mounts, mirror freshness, wired agents
docli doctor                             # server / disk / state reconciliation
docli logout                             # disconnect this device and drop the credential
```

Commit `docli.toml`: it identifies workspaces by ID, never by handle, and grants no access.

**Git is not required.** If the project is not a git repository, there is no `.gitignore`
requirement and everything simply works. The rule applies exactly when a mirror lands inside a
git work tree: there the mirror directories and `.docli/` must be listed in `.gitignore`, or
`docli sync` refuses to run — one `git add -A` would otherwise push somebody else's notes to a
remote. `docli init --gitignore` appends the lines for you (the guided setup asks).

## Signing in without a browser

`docli login` needs a browser and a writable home directory. An agent sandbox, a CI job or a
container usually has neither: a coding agent's sandbox typically leaves `$HOME` read-only, and a
stored sign-in cannot be refreshed there — so the CLI works until the access token lapses and
then stops.

Mint a key with the `sync` scope in the access list on docli.ru, then use either of these.

**Store it on the machine** — the guided `docli init` offers this as one of its sign-in
choices, and these are the same thing without the questions:

```sh
docli login --token -            # reads the token from stdin, so it stays out of your history
docli init  --token -            # the same, then sets the project up
```

The key is checked against the server before it is stored, and it is stored *without an
expiry*, because it has none we can read. That absence is the point: a browser sign-in has to
be refreshed, refreshing has to be written down, and writing has to happen somewhere — which is
exactly what a read-only home does not offer. A key is never due, so nothing ever writes.

**Or hand it in per process**, storing nothing at all:

```sh
export DOCLI_TOKEN=<the key>
docli search "what you need"
```

`DOCLI_TOKEN` outranks whatever is stored, is never written to disk and is never refreshed. It
is **bound to one origin** — `https://docli.ru`, or whatever `DOCLI_TOKEN_SERVER` names.
`docli.toml` is a committed file that anyone on the project can edit, and its `server` line
decides where the CLI sends its bearer, so a mismatch is refused rather than followed.

Either way, `docli sync` still needs a writable home — it writes the mirror. `search`, `read`,
`list` and `status` do not.

### Or keep the browser sign-in, and let Codex renew it

Codex's sandbox leaves your home directory read-only, which is why a stored browser sign-in stops
working there the moment its token needs renewing. One writable path fixes that, and
`docli init --codex-sandbox` writes it — appending the docli credentials directory to
`sandbox_workspace_write.writable_roots` in `.codex/config.toml`. The guided setup offers the same
thing, and only when it would do something: Codex has to be wired, and the sign-in has to be one
that can lapse (a minted key never renews, so it doesn't ask).

The grant is **only** `~/.docli/auth`, which holds the credentials and nothing else. That is why
they live in their own directory: `writable_roots` is recursive, so granting the whole docli
folder would also make the mirror writable to shell commands in the sandbox — and shell writes
are the one thing the mirror hook cannot refuse.

Three things to know afterwards: `docli login` refuses while `DOCLI_TOKEN` is set, because a
stored credential would be shadowed on every later command; `docli logout` clears what is on
this machine but cannot unset your environment, and says so; and logging out of a stored key
removes it here without revoking it — it stays live until you retire it in the access list.

## Interactive and non-interactive

Every command works both ways. Questions are asked only at an attended terminal; in a pipe, in
CI, or under `--no-input` nothing is asked, and when an answer is genuinely required the refusal
names the flag that replaces it (`docli uninstall --yes`).

| Flag | Effect |
| --- | --- |
| `--no-input` | Never ask anything (scripts, CI) |
| `-q`, `--quiet` | Drop the narration; results and warnings stay |
| `--no-color` | No colour; so do `NO_COLOR`, `TERM=dumb`, and a non-TTY stdout |
| `--json` | Machine-readable output for `list`, `status`, `search`, `read`, `doctor` |

Streams are split: results go to stdout, progress to stderr — so `docli read … | head`,
`docli status --json | jq` and `docli sync 2>/dev/null` all behave. Where a command's whole
screen IS the result (`search`, `status`, `list`, human-readable `doctor`), its warnings go to
stdout with it: a `docli status > file` that dropped half the screen would be worse than no
redirect. `docli read` is not one of those: the note is the product, so it has stdout to itself
and every caveat goes to stderr. Under `--json` nothing but the JSON reaches stdout.

`docli read` exits **3** when no selected mount holds what was asked for — its own code, so a
script can tell "not in this local mirror" from a failure. It says nothing about the server: only
a `docli search` that does not report an incomplete index settles whether a note exists.

## Uninstalling

```sh
docli uninstall              # removes the binary and ~/.docli, disconnecting the device first
docli uninstall --purge      # also removes this project's mirrors and .docli/
```

Everything that will be deleted is listed before you confirm. Without `--purge` the project's
files stay: a mirror is rebuildable, but it lives in your repository and that call is yours.
`docli.toml` and your agent configurations are never touched.

## Wiring coding agents

`docli init` installs the mirror contract at `.agents/skills/docli-mirror/SKILL.md` — the
standard path defined by the open Agent Skills specification, and read there by Codex (whose
documented scan order names it). **Claude Code does not read that path** — it reads
`.claude/skills/`, so the contract is copied there too, with `paths:` activation globs derived
from your mount table, so it loads automatically when an agent works with a mirrored file. Other
agents with their own skills directory (Qwen, Cline, Trae) get their own copy. `--skills none`
opts out; `--skills claude,codex,…` names a list.

### Enforcement, and where it stops

The contract is a document, and a document only helps once an agent has read it. For the two
agents with a hook mechanism — **Claude Code and Codex** — `docli init --hooks auto` installs
two hooks:

- a `PreToolUse` hook that **refuses** writes into the mirror, with a reason that names the
  correct alternative; and
- a `SessionStart` hook that reports mirror freshness into the session, unprompted.

They are never installed unless you ask: the offer arrives unticked in the guided setup, and
under `--no-input` only `--hooks` writes them. Both agents also ask you to trust the project
before any project-local hook runs.

Two limits, stated rather than left to inference. **Shell writes are not covered** — a `sed -i`
or a `>` redirect into the mirror goes through on both agents, because deciding whether a
command string targets a mirror means parsing shell. And **no other agent gets enforcement at
all**: for them the mirror is marked read-only and the contract asks them not to edit it, which
is advice, not a guarantee. `docli status` reports whether the hooks are installed here and
whether the binary they name still resolves.

`docli init --instructions` additionally writes a short docli section into `AGENTS.md` (read by
Codex, Cursor, Gemini CLI, Zed, Copilot and about fifteen other tools) and, **only when there is
no `CLAUDE.md` to damage**, creates one containing `@AGENTS.md`. An existing `CLAUDE.md` is never
edited; the one line to add is printed for you to paste.

In the guided setup, wiring agents is a step of its own: the configurations found here arrive
pre-ticked, and only what stays ticked is written. It can also — **only if you ask** — add this
project's docli MCP connection to your agents'
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
