# Asking the user a question

How any baton session asks the user something: a task's worker session, the context's home
(orchestrator) session, or a skill running in either. Baton ships this, so no context repeats it in
its `guidance.md`: `hooks/session-start-detect.sh` tells every baton session at start, and again
after a resume or a compaction, and points here.

## The rule

**Ask in plain text, as a numbered list. Never open a modal picker** (`AskUserQuestion`, or any
dialog that takes over the input line).

```text
Merge #12 now?

1. Merge and finish (recommended): checks are green and the branch is current.
2. Keep it open: wait for the second review.

Reply with a number, or your own answer.
```

- The question comes first, on its own line.
- One numbered option per line. A short reason may follow the label after a colon.
- Mark the recommendation with `(recommended)` after its label. At most one option has it. With no
  recommendation, mark none.
- The last line is exactly `Reply with a number, or your own answer.`
- Several questions in one turn get one block each, separated by a blank line.
- A yes/no question still gets options (`1. Yes` / `2. No`), so an answer of `1` is unambiguous.

**Why.** fermata types the user's answer into the session's input line. It cannot answer a dialog,
and it holds every answer while one is open, so a session blocked on a picker stays stuck until
someone visits its pane. A plain-text question is answered from anywhere: fermata, the pane, or a
phone. fermata also reads a last message that ends in a numbered list as "asked a question", so the
wait shows up in its task list even before the decision syncs.

## Queue it as a decision, in the same turn

Every question is also recorded with `decide`, so fermata shows it under Decisions and the user can
answer without visiting the pane. Do it in the same turn as the text, before you stop:

```bash
"$TRK" decide <task-id> --question "Merge #12 now?" \
  --option "Merge and finish" --option "Keep it open" --rec 1 --by session
```

- **Worker session**: the task is your own leaf (`$LEAF`), `--by session`.
- **Home session**: the task the question is about, `--by orchestrator`. Head the question's section
  with `### <task-id>` as usual ([orchestrator-replies.md](./orchestrator-replies.md)).
- The options are the same labels, in the same order, as the numbered list, so `Option 2` means
  the same thing in both places. `--rec` is the recommended option's number.
- One `decide` per question. Check `"$TRK" decisions <task-id>` first: an open question does not need
  posing twice.
- A question that is about no task (the home session asking how to file a new one, say) has nothing
  to queue on. Ask it in plain text alone.
- A backend without the decision verbs (`capabilities.decisions` is not `true`, exit 4) gets the
  plain-text question alone.

## Reading the answer

Accept any of these as the answer:

| the reply | means |
|---|---|
| `2` | option 2 |
| `Option 2: Keep it open` | option 2. fermata sends this form when the user picks an option there |
| `[fermata <task> <decision> answer] Option 2: …` | option 2, from fermata, to the home session |
| anything else | the user's own answer. Act on what it says; it need not match an option |

A reply ending in `?`, or `[fermata <task> <decision> question] …`, is a question about the
decision, not an answer. Answer it and leave the decision open.

Once you have acted on the answer, close the decision:

```bash
"$TRK" resolve <task-id> <decision-id> --outcome "<what you did>"
```

If the answer came in the pane rather than through fermata, record it first, so the task carries
it: `"$TRK" reply <task-id> <decision-id> --kind answer --by user --text "<the answer>"`.

## What is not a question

- **Permission prompts and plan approval** are Claude Code's own dialogs, not ours to replace.
  fermata shows them as "open in the pane".
- **A statement of what happens next** ("merging once CI is green") needs no answer, so it gets no
  numbered list and no decision.

## When a picker opens anyway

If a session does open `AskUserQuestion` in a task worktree, fermata mirrors each question as a
decision (`decide --by session`) and resolves it when the session moves past it (fermata's
`DESIGN.md`, "Prompts"). That is a safety net, not the path: a home session's pickers are not
mirrored, because a home session has no single task to put them on.
