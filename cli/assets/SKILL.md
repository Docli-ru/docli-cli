---
name: docli-mirror
description: Work with the docli read-only mirror in this project — sync at session start, search server-side, treat local files as a cache that must never be edited.
---

# The docli mirror contract

This project mounts one or more docli workspaces as a **read-only local mirror** (see
`docli.toml`). The mirror is a *cache for reading*, kept honest by the `docli` CLI. Follow this
contract exactly — every rule below exists because violating it can silently cause data loss
or misrepresent server content.

## At session start

1. Run `docli sync --check`. **Branch on the exit code, not the prose**: a zero exit
   confirms freshness; after any non-zero exit, do not rely on the mirror — run `docli sync`
   and resolve any reported error before reading from it.
2. If `CACHE_INCOMPLETE.docli` is present at a mirror root, that mirror is currently
   incomplete. Do not infer from a missing local file that the file is absent from the
   server.

## Reading

- Read mirrored notes (`.md` files) freely — they are real files, fast to grep.
- Files ending in `.docli` are **markers**, not attachment content. Each is a metadata
  sidecar containing the attachment ID, MIME type, size, SHA-256 digest, and a wikilink. The
  bytes live on the server; fetch them through your docli MCP connection (`read_attachment`)
  if you need them.
- `sha256 unknown` in a marker means the digest is genuinely unknown server-side — not zero,
  not empty. `wikilink not-expressible` means no correct wikilink exists for that path; use the
  `path` line instead.

## Searching — the split-brain rule

- **Local grep may supplement server search results, but only a non-degraded server search
  may establish that a note does not exist.** The mirror can be scoped, parked, or behind —
  grep finding nothing proves nothing.
- Use `docli search "<query>"` — it runs the product's BM25 search with Russian and English
  stemming on the server, across every mount, and prints LOCAL paths you can open directly. A hit
  marked «not mirrored» exists on the server but not on this disk — read it over MCP.
- If the output says the index was **degraded**, the answer is INCONCLUSIVE about absence.
  Retry, or read over MCP. The same rule applies to file results: they may be truncated or
  may include a superset of matches, and the command reports either condition explicitly.

## Never edit the mirror

The mirror is **not writable state**, and violating this rule can cause data loss:

- A hand edit inside the mirror is **never synced and never protected**. It silently
  persists, falsely representing the server state (delta sync only re-delivers notes whose
  revision has advanced) — until the note next changes server-side, when your edit is
  **destroyed with no conflict copy**.
- The files are marked read-only at the filesystem level, but that protection is advisory:
  editors that replace files by deleting and recreating them can bypass it. Do not "fix" a
  mirror file. Do not create files inside a mirror directory.
- To change a note, write through your docli MCP connection — the next `docli sync` brings
  the change back down. **When writing through MCP**: prefer `edit_note` (exact-string
  replacement) to `write_note`, which replaces the entire body — calling `write_note` with
  content derived from a stale or partial copy silently removes everything omitted from that
  copy. When you must use `write_note`, read the current body first and send the full body
  based on it, and check `conflictSiblingId` after every write — a non-null value means your
  base was stale: re-read the current note, redo the full-body update, and delete the
  conflict sibling only after confirming its content is no longer needed.
- `docli doctor` detects hand edits (digest-mismatch) and every other divergence; `docli sync
  --full` is the repair.

## Credentials

`~/.docli/` holds a **full sync-plane credential acting as the workspace owner** — treat it as
a secret store. Never read, print, copy, or transmit anything from `~/.docli/`; never commit
it. ("Read-only" is a property of this CLI's behavior, not of the credential.)

## Command summary

| command | use |
| --- | --- |
| `docli sync` | bring every mount to the server's head (one-shot) |
| `docli sync --check` | cheap freshness gate — exit 0 = fresh, non-zero = sync first |
| `docli sync --full` | authoritative resync: re-derive the mirror, prune stale files |
| `docli search "q"` | server search across mounts, local paths in the output |
| `docli doctor` | full three-way reconciliation (server / disk / state) — slow, thorough |
