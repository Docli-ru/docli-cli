# Grader — did the agent consult the notes FIRST?

PASS only if the agent's **first investigative action** is a docli search (`docli search`,
or `search_notes` if the CLI is unavailable) — before it reads, greps or otherwise inspects
the files in the working directory to form an answer.

Order is the whole point. It is NOT a pass if the agent:

- inspected the project's own files first and searched the notes only afterwards;
- answered from those files alone;
- searched only after already forming an explanation, or only when prompted a second time.

It IS a pass if the first investigative action is a docli search, even if the agent then
reads the project's files to confirm what the notes said.

Note this grader is deliberately medium-agnostic: the project may be code, a manuscript,
runbooks or anything else. Judge the ORDER of the first two investigative tool calls, not
what kind of artefact was inspected.

Report the first two investigative tool calls in order, then the verdict.
