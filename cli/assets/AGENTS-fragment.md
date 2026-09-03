## docli mirror

This project mounts one or more docli («докли») workspaces as a **read-only local mirror**; see
`docli.toml` for which, and where. Mirrored notes are found and read **through the `docli` CLI**,
not through the filesystem: it applies the mirror's freshness and digest checks, and
`docli search` no longer prints a local mirror path for each result.

- **`docli search "…"` finds, `docli read <path>` opens.** Search prints each hit's server path
  and node id; `docli read` prints that note — `--lines 40-80` for a range, `--json` for the
  read_note envelope, `--mount` when more than one mount holds it. **Exit 3 means only that this
  mirror does not hold the requested note or file — never that it does not exist on the server.**
- **`docli read` answers the note's graph too** — `links`, `backlinks`, `embeds`, `unresolved`,
  `tags`, `title`, `aliases` under `--json`, and a counts line on stderr without it. The server
  computes it; the CLI only holds it. An empty list means empty; a `null` plus its `absent` entry
  means the CLI does not know, and that entry names the fix.
- **The mirror is never writable.** An edit made inside it is never synced, and it is destroyed
  with no conflict copy the next time that note changes on the server. To change a note, write
  through the docli MCP connection with `edit_note`, then run `docli sync`.
- **`docli sync --check` reports freshness by exit code — branch on the code, not the prose.**
  `0` the mirror is current · `1` it is behind, follow the printed remedy · `2` the check could not
  run, which says nothing about the mirror. A read-only home (an agent sandbox) gives `2`, because
  the check must record what it learns; reading still works there, so take `docli read`'s own
  disclosures as the signal instead of assuming the mirror is stale.
- **Absence is a server question.** The mirror can be scoped, behind, or incomplete. Only a
  `docli search "…"` that does not report an incomplete index establishes that a note does not
  exist.
- **`docli read` on a file** prints its id, MIME type, size, digest, wikilink and the notes that
  embed it. The bytes stay on the server; `read_attachment` over the docli MCP connection
  fetches them.

`docli status` reports sign-in, mounts, freshness and what is wired here.
