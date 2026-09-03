---
name: docli-mirror
description: The docli (докли) notes mirror in this project — docli.toml, .docli/mirror/, *.docli markers. Use when asked to search, read, or change the user's docli/докли notes, vault, or workspace, and ALWAYS before editing any file under a mirror directory: those files are a read-only cache, and a hand edit there is destroyed with no conflict copy.
allowed-tools: Bash(docli *)
---

# The docli mirror contract

These are standing rules for the whole task, not a checklist to run once. They hold from the
moment this file is loaded until the task ends.

**docli and «докли» are the same product** — Russian is its primary name, so the user may write
either, in either alphabet, and may say «докли», «вольт», «заметки» or «пространство» for the
things named below. All of them are this contract's subject.

This project mounts one or more docli workspaces as a **read-only mirror** (see `docli.toml`).
The mirror is a local copy of server state, maintained by the `docli` CLI.
Every rule below exists because breaking it can silently destroy data or misrepresent what the
server holds.

## Notes are found and read through the CLI

- **`docli search "<query>"` finds; `docli read <server-path>` opens.** Search runs the product's
  BM25 search with Russian and English stemming on the server, across every mount, and prints
  each hit's **server path** and node id. `docli read` takes that path (or `--id <uuid>`) and
  prints the note.
- `docli search` publishes **no local mirror path** for its results, so a mirrored note is read
  with `docli read` rather than by opening a file. `--lines 40-80` reads a range (`40-` for the rest, `40` for one
  line); `--json` returns the read_note envelope; `--mount <name-or-workspace-id>` picks a mount
  when more than one holds the path, and an ambiguous path is refused rather than guessed.
  (The mirror's *location* is not secret — `docli.toml` names every mount directory, and a mount
  with no `name` is tagged by that directory. What no longer happens is being handed a note's
  address while doing something else. Reading a mirror file directly is still the wrong move: it
  is what `docli read` does correctly, with the freshness and digest caveats attached.)
- **`docli read` exits 3 when this mirror does not hold the requested note or file.** That is an
  answer about the mirror and never about the server: it may be outside the mount's folder scope,
  blocked from being written, or simply not synced yet. For a note, only a `docli search` that
  does not report an incomplete index establishes that it does not exist.
- **`docli read` also answers the note's graph** — `links`, `backlinks`, `embeds`, `unresolved`,
  `tags`, `title` and `aliases`, on the `--json` envelope, with a counts line on stderr in plain
  mode. Every one of them is computed on the SERVER and delivered with the sync; the CLI resolves
  no wikilinks of its own, so a link the server resolved is the link you get.
- The envelope's `absent` map names every field the CLI could not fill and why. Fields it cannot
  answer are `null` and listed there — never an empty list, which would be indistinguishable from
  a note that genuinely has none. So `"backlinks": []` means nothing links here, while
  `"backlinks": null` plus its `absent` entry means the CLI does not know — and that entry names
  the fix, which is usually `docli sync`.
- A refusal under `--json` is `{"error": {"code", "message"}}` on stdout, with the same exit code.
  `code` is `not_in_mirror` (exit 3), or `usage` / `unavailable` / `ambiguous` / `no_such_mount` /
  `ambiguous_mount` (exit 2) — a caller's mistake and a gap on our side are never the same code.
- `disclosures` (and the same sentences on stderr) report what the CLI cannot vouch for. The
  content is still served; the caveat travels with it. Five codes — the first three say
  something about the CONTENT just handed over, the last two about what could not be checked:
  - `digest_mismatch` — the bytes on disk are not the bytes the mirror recorded writing, so the
    content may not be what the server holds;
  - `not_utf8` — invalid sequences were replaced, so the text is not byte-exact;
  - `mirror_not_usable` — the CLI cannot vouch for this mirror's freshness or completeness right
    now; the message names what to run;
  - `digest_unknown` — no digest was recorded for what was served, so its bytes could not be
    checked at all;
  - `mounts_unresolved` — another mount could not be consulted, so it is **not established** that
    this is the only mount holding the requested note or file. Select the intended mount with
    `--mount`; `docli status` lists the mounts.

## The mirror is never writable

- A hand edit inside the mirror is **never synced and never protected**. It silently persists,
  falsely representing server state (delta sync only re-delivers notes whose revision has
  advanced) — until the note next changes server-side, when the edit is **destroyed with no
  conflict copy**.
- The files are marked read-only at the filesystem level, but that protection is advisory:
  editors that replace files by deleting and recreating them go straight through it. A mirror
  file is never "fixed" in place, and no file is ever created inside a mirror directory.
- Changing a note means writing through the docli MCP connection; the next `docli sync` brings
  the change back down. When writing over MCP, **prefer `edit_note`** (exact-string replacement)
  to `write_note`, which replaces the entire body — `write_note` called with content derived
  from a stale or partial copy silently removes everything omitted from that copy. When
  `write_note` is unavoidable, the current body is read first and the full body sent based on
  it, and `conflictSiblingId` is checked after every write: a non-null value means the base was
  stale, so the current note is re-read, the full-body update redone, and the conflict sibling
  deleted only once its content is confirmed no longer needed.
- `docli doctor` detects hand edits (digest mismatch) and every other divergence; `docli sync
  --full` is the repair.

## The mirror is trustworthy only when it is fresh

- `docli sync --check` is the gate. **Branch on the exit code, not the prose** — and its three
  codes mean three different things:
  - **`0`** — the mirror is current.
  - **`1`** — the mirror is behind or incomplete. It is not authoritative; follow the remedy the
    check printed, which is not always a plain `docli sync`.
  - **`2`** — the check could not run at all. This says nothing about the mirror either way, so it
    is not a reason to treat it as stale. The common cause is an environment where the check cannot
    write: it must record what it learns, and an agent sandbox that leaves the home directory
    read-only stops it. Reading still works there, and `docli read` still discloses what it cannot
    vouch for, so on `2` take those disclosures as the freshness signal instead.
  A `2` that is not about writability — a network failure, a refused workspace — is reported in the
  message, and the same rule applies: it is a failure to CHECK, never a verdict on the mirror.
- `CACHE_INCOMPLETE.docli` at a mirror root means that mirror is currently incomplete. A file
  missing from an incomplete mirror says nothing about the server.
- `docli search` reports, beside its results, when the local mirror needs attention — its state
  could not be read, the server has changes it has not applied, the workspace was resynced, or
  its live item count says a hard delete was missed. That line describes the LOCAL mirror only.
  It never weakens the results themselves, which came from the server, and it is not the freshness
  gate: `docli sync --check` remains the exit code to branch on. **`docli read` is the opposite
  case** — its answer *is* the mirror, so its disclosures are about the content it just handed
  over. Its stderr also carries the occasional update notice, so `--json`'s `disclosures` array is
  the channel to branch on rather than the stream.

## Absence is a server question, never a local one

- Nothing local settles absence: **only a server search that does not report an incomplete
  index** may establish that a note does not exist. The mirror can be scoped, behind, or blocked
  from writing some notes, and neither a `docli read` exit 3 nor an empty directory proves
  anything about the server.
- When the human output says the note index was **incomplete**, or `--json` carries
  `"degraded": true`, the answer is INCONCLUSIVE about absence: retry, or read over MCP. The same
  holds for file results, which may be truncated or may include a superset of matches — the
  command reports either condition explicitly.

## Files are metadata here, not bytes

- `docli read` on a file prints its ID, MIME type, size, SHA-256 digest and a wikilink, plus the
  notes that embed it (`embeddedIn`). The bytes live on the server; `read_attachment` over the
  docli MCP connection fetches them.
- `sha256 unknown` means the digest is genuinely unknown server-side — not zero, not empty.
  `wikilink not-expressible` means no correct wikilink exists for that path, so the `path` is the
  one to use. In `--json` both arrive as `null` with the reason named in `absent`.

## Command summary

| command | use |
| --- | --- |
| `docli sync` | bring every mount to the server's head (one-shot) |
| `docli sync --check` | cheap freshness gate — 0 = current, 1 = behind (follow the printed remedy), 2 = could not check |
| `docli sync --full` | authoritative resync: re-derive the mirror, prune stale files |
| `docli search "q"` | server search across mounts — server paths and node ids |
| `docli read "path"` | print a mirrored note; `--lines`, `--id`, `--mount`, `--json` |
| `docli doctor` | full three-way reconciliation (server / disk / state) — slow, thorough |
| `docli status` | one screen: sign-in, mounts, mirror freshness, wired agents |
| `docli list` | every workspace this account reaches; `*` marks the ones mounted here |

`--json` works on `search`, `read`, `doctor`, `status` and `list`, and `--no-input` guarantees
nothing prompts. **Parse `--json`, not the human output**: for `search`, `doctor`, `status` and
`list` the whole screen is the result, so their warnings — including the mirror line above — share
stdout with it, and only `--json` guarantees stdout carries data alone. `docli read` already splits
them: the note is on stdout, every warning on stderr.
