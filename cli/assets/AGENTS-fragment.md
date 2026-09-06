## docli mirror

This project mounts one or more docli («докли») workspaces as a **read-only local mirror**; see
`docli.toml` for which, and where. Mirrored notes are found and read **through the `docli` CLI**,
not through the filesystem: it applies the mirror's freshness and digest checks, and
`docli search` no longer prints a local mirror path for each result.

**These notes are a primary source for the work here, not a side archive** — the decisions,
background, research and plans behind it. The files here say what; the notes say why. (That need
not be source code: this may be a repository, a manuscript, a research folder or a set of
runbooks.)

Two rules, and both fire on an **event**, not on your judgement of whether you need them:

- **Before you answer a «why is this the way it is / what did we decide / was this already tried»
  question from the files here, run `docli search` first.** Not "whenever you feel you need
  context" — you will not feel it, because the files always answer *something*; they just answer a
  different question. Nobody has to say the word «notes» for a search to be the right move.
- **When a decision or finding lands, write it back before you carry on.** Use `edit_note` (or
  `write_note`) over the docli MCP connection, then run `docli sync`. Not at the end of the session:
  batching it to a checkpoint is exactly how it gets lost, and a decision is not less true for being
  recorded before the work around it is finished. If it later changes, edit the entry in place. The
  mirror is never the write path.
- **When the docli MCP tools are also available, prefer the CLI for reading.** `docli read` answers
  from the local mirror, with no network call; `docli search` asks the SERVER (which is what makes a
  clean search — one that reports no incomplete index — the only thing that can establish a note
  does not exist). `search_notes`/`read_note` reach the server for every note, including ones the
  mirror already holds. Fall back to the MCP tools whenever the CLI cannot answer: it exits 3 (this mirror
  does not hold that note), it discloses the mirror is stale, **or it fails outright** — a bad
  `docli.toml`, no mount here, not installed. **A broken CLI is not a reason to stop**: the notes
  are still reachable with `search_notes`/`read_note`, and an unanswered question is worse than a
  slower answer. Always use the MCP tools to write.
- **The fallback is PER VERB, not per CLI.** The verbs have different dependencies: `sync` needs the
  network AND a writable `~/.docli`; `search` needs the network; **`read` needs neither** — it serves
  the local mirror offline. So a failing `docli sync` says nothing about `docli read`, and «the CLI
  is broken» is almost never the right conclusion from one failed command. Try the verb you actually
  need before falling back.
- **And when the failure IS the network, the fallback runs the other way.** `search_notes` and
  `read_note` reach the same server over the same network, so they cannot answer either; `docli
  read` can. In a sandbox with no egress, or offline, the mirror is the only thing that CAN answer —
  read from it and treat its disclosures as the caveat. The rule underneath both directions: fall
  back toward whatever does not depend on the thing that just failed.
- **They are ALTERNATIVES, not complements — one read per note.** If `docli read` answered, do not
  also call `read_note` for the same note. Beyond the wasted round trip, the two can DISAGREE (the
  server is current, the mirror may be behind), and holding both answers leaves you with no rule for
  which to believe — the split-brain this contract exists to remove.

- **`docli search "…"` finds, `docli read <path>` opens.** Search prints each hit's server path
  and node id; `docli read` prints that note — `--lines 40-80` for a range, `--json` for the
  read_note envelope, `--mount` when more than one mount holds it. **Exit 3 means only that this
  mirror does not hold the requested note or file — never that it does not exist on the server.**
- **Exit 4 means the server listed that note as changed since this mirror last applied it**, so
  `docli read` will not serve what it holds. Run `docli sync` and read it again. It fires only for a
  note the server named — never as a general «something might be stale» — though a note whose stamp
  this mirror never recorded can be named by an ordinary rename, so the first exit 4 after an
  upgrade may be over bytes that are already current; one `docli sync` settles it either way.
  Where nothing has searched yet, the gate has learned nothing and reads behave exactly as before.
  **In a sandbox with a read-only home the gate cannot learn anything new, but marks an earlier
  session left are still honoured — and `docli sync` cannot run there**, so treat exit 4 like any
  other answer the CLI cannot give and read the note with `read_note` over the docli MCP connection.
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
