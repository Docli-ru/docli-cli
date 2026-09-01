## docli mirror

This project mounts one or more docli («докли») workspaces as a **read-only local mirror**; see
`docli.toml` for which, and where. The mirrored notes are ordinary `.md` files you can read and
grep. Files ending in `.docli` are metadata markers, not attachment bytes.

- **The mirror is never writable.** An edit made inside it is never synced, and it is destroyed
  with no conflict copy the next time that note changes on the server. To change a note, write
  through the docli MCP connection with `edit_note`, then run `docli sync`.
- **Trust the mirror only after `docli sync --check` exits zero.** Branch on the exit code, not
  on the prose.
- **A missing local file does not mean the note does not exist.** The mirror can be scoped,
  behind, or incomplete. Only a non-degraded `docli search "…"` establishes absence.

`docli status` reports sign-in, mounts, freshness and what is wired here.
