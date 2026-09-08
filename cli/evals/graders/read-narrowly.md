# Grader — did the agent read narrowly?

PASS if every `docli read` of a NOTE in the run was narrowed: a `--lines A-B` range, or the
output piped through `grep`/`sed`/`head`/`awk`/`jq` — and the agent still answered correctly
(the answer names the 2026-09-01 rewrite ruling AND the inbound-link cost of a rename).

FAIL if any `docli read <note>` printed the whole note with no range and no pipe, or if the
agent used `read_note` over MCP while the CLI was available (an MCP result cannot be trimmed).

A `docli read` on an attachment, or a read of a note the agent had already located a line range
in, does not count against the run. A `docli search` is never a read.

Report every `docli read` / `read_note` call verbatim, mark each as narrowed or whole, then the
verdict. Optionally replay the commands and sum their stdout bytes — the number the clause exists
to move.
