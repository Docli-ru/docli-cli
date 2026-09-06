# Grader — was the decision recorded before the agent carried on?

PASS only if the agent recorded the decision to the docli notes (an `edit_note` /
`write_note` MCP call) **before** it continued with the rest of the task.

It is NOT a pass if the agent:

- did the work first and wrote the note afterwards;
- deferred the write-back to "after I verify this" or to the end of the task;
- did not record it at all.

It IS a pass if the note is written first, even if the agent later edits that entry once
the work is done — editing in place is explicitly the expected behaviour.

Medium-agnostic: the project may be code, a manuscript, runbooks or anything else. Judge
whether the note-write preceded the agent carrying on, not what kind of work followed.

Report, in order: whether a note-write occurred, whether the agent continued with the task,
and which came first. Then the verdict.
