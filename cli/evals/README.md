# CLI agent-behaviour evals — what the orientation text buys

Four cases, each a behaviour the CLI's agent-facing orientation exists to produce: consult the notes
FIRST (in a code project and in a manuscript), read NARROWLY, write a decision back BEFORE carrying
on. The measurements behind them (2026-09-05 / 09-08) are in `docs/changelogs/v0.29.6.md` and
`v0.29.9.md`; they were driven through subagent fixtures because `claude plugin eval` was not yet
available. **It is now (verified 2026-09-21 at `claudeVersion 2.1.278`), and this suite loads and
runs on it** — restructured 2026-09-22 from a guessed schema that never loaded.

## What is under test, and how it reaches the run

The SessionStart hook's `additionalContext` — the shipped `ORIENTATION` const in
`apps/cli/src/sync_cmd.rs`. The harness loads no hooks, so each case carries the text in
`execution.append_system_prompt`, **pinned byte-for-byte to the const** by
`the_eval_cases_carry_the_shipped_orientation` (apps/cli). Edit the const; the pin fails until the
cases follow. A channel difference is disclosed rather than hidden: a system-prompt append is not
literally a hook's context injection. `v0.28.6`'s measurement is that this class of channel reaches
context reliably; whether it WINS is what the graders score.

## Why there is a mock `docli` server, and what that changes

Each run gets a throwaway HOME: no sign-in, no mirror. So `docli search` cannot answer, and the
orientation's own fallback — «Fall back to the MCP tools whenever the CLI cannot answer … not
installed» — is the path exercised. `mocks/docli/` stands in for the server: `search_notes`,
`read_note`, `edit_note`, `write_note`, `append_to_note` answer from fixture text, and the
`searched-first` grader already accepts `search_notes` when the CLI is unavailable.

**And on this machine Bash is not granted at all**, which narrows the measurement one step further
and is stated rather than hidden: `claude plugin eval` refuses a Bash grant when `~/.docker` holds
symlinks inside it (Docker Desktop's CLI plugins under `~/.docker/bin/`), and it inspects
`~/.docker` regardless of `DOCKER_CONFIG` — measured 2026-09-22, both ways. So the cases list
`[Read, Glob, Grep]` only — **and the CLI attempt is STILL visible**: an ungranted tool is refused,
not hidden, and in the first n=5 run of `consult-before-files` every one of the five runs opened
with `Bash(docli search …)`, was refused, and fell back to `search_notes` exactly as the
orientation instructs. So a run measures both halves — «the CLI first» (as an attempt) and
«the notes before the local files» (as an outcome) — and the grader reads the ORDER from the trace.

**The mock needs the real tool schema.** `mocks/docli/_tools.json` is the live server's
`tools/list` (saved 2026-09-22 from `v0.38.0`); without it the harness serves the mocked tools
with a permissive schema and no description, the model invents argument names, and a mock's
`expect:` guard ABORTS the run — which is what the first n=5 produced for the manuscript and
write-back cases (0/5 each, `aborted: … path = (missing)`), and why those guards are now minimal.
Re-save the file when the catalog changes.

`read-narrowly` cannot be honestly measured this way at all — narrowing is a CLI property (an MCP
result cannot be trimmed) and needs a real mirror. It is kept loadable, tagged `needs-real-mount`,
and is run with a real mount by the 2026-09-08 method until the harness can carry one.

## How to run

```
cd apps/cli
claude plugin eval --eval-dir evals --scaffold --tag consult --tag write-back \
  --allow-tools Write Edit --threshold 0 --no-publish --trust-plugin -j 4 . --json run.json
```

`--scaffold` is required: each case's `fixture.sh` builds the mounted project (a `docli.toml`, the
repo's `AGENTS.md`, and files that make the local path tempting). `--allow-tools Write Edit` is what
lets the write-back case's «put that decision into effect» be a real edit — without it the work half
of that task is impossible and the case measures nothing; the consult cases ignore the grant. The
two `--tag`s skip `read-narrowly` (above). `--threshold 0` because a case scoring below 1.0 is a
measurement, not a broken command. On a machine whose `~/.docker` has no symlinks inside it, add
`--allow-tools Bash` to measure the CLI attempt as well. **A pass rate is not the result** — read
`graders[].evidence` (the `trace` focus shows every tool call in order) and, for the write-back
case, `mock_calls`; the api suite's D6 write-up (`docs/changelogs/v0.38.0.md`) records why.

## First measurements — 2026-09-22, `claudeVersion 2.1.278`, n=5 per case, no Bash grant

Read these as the suite's baseline, not as pass rates; the sequences are from the trace focus.

| case | judged | what the traces show |
|---|---|---|
| `consult-before-files` | **5/5** (twice) | every run reached the notes before any local read — 2–5 of them by TRYING `docli search` first (refused, then `search_notes`); a run also quoted the `v0.38.0` staleness notice back |
| `consult-before-manuscript` | **1/5** | four runs went `Glob → Read → Read` on the chapter files FIRST and consulted the note only afterwards; three judges unanimous each time. **The orientation loses to the local path in a non-code project** — the regression this case was written to catch, and a finding for the next CLI-orientation slice |
| `write-back-before-next-step` | **2/5** | three runs wrote the note before any code edit (`edit_note` at step 2–4); run 4 did the code `Write` first and never wrote the note — the 2026-09-05 failure, reproduced; run 3 wrote nothing. Zero aborts, zero turn-cap hits after the fixture fix |

Two harness lessons rode along, both recorded above: the mock needs `_tools.json` or `expect:`
guards abort well-formed calls, and a `Bash` grant is refused on a machine whose `~/.docker`
holds symlinks. Cost of the three-case run: ~$5, ~4.5 min at `-j 4`.

## Pins

- `the_eval_cases_carry_the_shipped_orientation` — each case's `append_system_prompt` == the const.
- `the_consult_cases_share_one_grader` — the two consult arms are scored by byte-identical rubrics
  (a byte-identical rubric is one RUBRIC; it is not by itself one measurable bar — a rubric that
  quotes fixture content grades an arm lacking it against a different bar; these quote none).
