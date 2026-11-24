# Orchestrator replies

How the orchestrator (a context's home session) words its replies so that a reader can tell which
task each part is about. One orchestrator session carries every task in the context, so without a
convention its transcript is one interleaved conversation. With it, a reader such as fermata can
show each task its own part (fermata's Conversation view).

Baton ships this, so no context repeats it in its `guidance.md`:
`hooks/session-start-detect.sh` tells every home session, at start and again after a resume or a
compaction, and points here. A home session is one whose directory is the context's workspace or
its `home`, and is not a linked worktree.

## Sections

**Head each part of a reply that is about a task with `### <task-id>`**, whenever the turn was not
started by a message that names that one task. A sweep over several tasks, a worker's report that
just arrived, and a reply that strays to a second task all get headings:

```markdown
### ctx-3
CI is green; merging.

### ctx-7
Still waiting on review.
```

- The heading is exactly level 3, outside code fences, and opens with the id. A title may follow
  the id after a colon or a dash (`### ctx-3: deploy fix`); several ids may share one section
  (`### ctx-3, ctx-7`).
- A section runs to the next level 1–3 heading. Sub-headings inside it are level 4 or deeper.
- Use the tracker id. The ticket key is accepted too.
- A reply to one `[fermata <task-id>] …` message needs no heading: the message says which task it
  is about. Text outside every section belongs to the tasks the prompt named.

## Messages from fermata

Input typed into fermata arrives as `[fermata <task-id>] text`, or, for input on a decision, as
`[fermata <task-id> <decision-id> <kind>] text` (`kind` is `answer` or `question`). Messages typed
while a turn runs can arrive merged; each marker starts a new message.

When you answer a `question`, also record the reply on the decision, so it is on the task and not
only in the transcript:

```bash
"$TRK" reply <task-id> <decision-id> --kind response --text "<your reply>"
```

An `answer` is acted on and then resolved (`tracker.md`, "Decisions").

## Writing to a task

A turn that writes to a task (a note, its state, a decision, a message to its worker) should say
so in that task's section. A turn that does not shows up on the task as "unattributed activity",
so the write is visible but its reason is one click away in the transcript.
