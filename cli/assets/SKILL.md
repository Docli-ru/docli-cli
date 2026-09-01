---
name: docli-mirror
description: The docli (докли) notes mirror in this project — docli.toml, docli-mirror/, docli-cache/, *.docli markers. Use when asked to search, read, or change the user's docli/докли notes, vault, or workspace, and ALWAYS before editing any file under a mirror directory: those files are a read-only cache, and a hand edit there is destroyed with no conflict copy.
allowed-tools: Bash(docli *)
---

# The docli mirror contract

These are standing rules for the whole task, not a checklist to run once. They hold from the
moment this file is loaded until the task ends.

**docli and «докли» are the same product** — Russian is its primary name, so the user may write
either, in either alphabet, and may say «докли», «вольт», «заметки» or «пространство» for the
things named below. All of them are this contract's subject.

This project mounts one or more docli workspaces as a **read-only mirror** (see `docli.toml`).
The mirror is a projection of server state onto local files, kept honest by the `docli` CLI.
Every rule below exists because breaking it can silently destroy data or misrepresent what the
server holds.

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

- `docli sync --check` is the gate. **Branch on the exit code, not the prose**: a zero exit
  confirms freshness; after any non-zero exit the mirror is not authoritative — run `docli sync`
  and resolve any reported error before reading from it.
- `CACHE_INCOMPLETE.docli` at a mirror root means that mirror is currently incomplete. A file
  missing from an incomplete mirror says nothing about the server.

## Absence is a server question, never a local one

- Local grep may supplement server search results, but **only a non-degraded server search** may
  establish that a note does not exist. The mirror can be scoped, parked, or behind — grep
  finding nothing proves nothing.
- `docli search "<query>"` runs the product's BM25 search with Russian and English stemming on
  the server, across every mount, and prints LOCAL paths that can be opened directly. A hit
  marked «not mirrored» exists on the server but not on this disk; read it over MCP.
- When the output says the index was **degraded**, the answer is INCONCLUSIVE about absence:
  retry, or read over MCP. The same holds for file results, which may be truncated or may
  include a superset of matches — the command reports either condition explicitly.

## Files ending in `.docli` are markers, not content

- Each is a metadata sidecar carrying the attachment ID, MIME type, size, SHA-256 digest and a
  wikilink. The bytes live on the server; `read_attachment` over the docli MCP connection
  fetches them.
- `sha256 unknown` in a marker means the digest is genuinely unknown server-side — not zero, not
  empty. `wikilink not-expressible` means no correct wikilink exists for that path, so the
  `path` line is the one to use.

## `~/.docli/` is a secret store

It holds a **full sync-plane credential acting as the workspace owner**. Nothing in it is read,
printed, copied, transmitted or committed. ("Read-only" describes this CLI's behaviour, not the
credential.)

## Command summary

| command | use |
| --- | --- |
| `docli sync` | bring every mount to the server's head (one-shot) |
| `docli sync --check` | cheap freshness gate — exit 0 = fresh, non-zero = sync first |
| `docli sync --full` | authoritative resync: re-derive the mirror, prune stale files |
| `docli search "q"` | server search across mounts, local paths in the output |
| `docli doctor` | full three-way reconciliation (server / disk / state) — slow, thorough |
| `docli status` | one screen: sign-in, mounts, mirror freshness, wired agents |
| `docli list` | every workspace this account reaches; `*` marks the ones mounted here |

`--json` works on `search`, `doctor`, `status` and `list`, and `--no-input` guarantees nothing
prompts. Results go to stdout, progress and warnings to stderr, so parsing stdout never picks up
narration.
