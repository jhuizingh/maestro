# fermata API

The HTTP API for clients of fermata other than its own TUI, such as a web page. It covers what the
dashboard shows and what it can do. Why it is shaped this way is in `DESIGN.md`, "Client and server".

`api-v1.json` holds one example of every document below, with every field set. A test keeps it, this
file and the code in step.

## Start and connect

```bash
fermata serve                 # no TUI
fermata --serve               # the TUI, serving its own data
fermata serve --read-only     # every command refused but refresh
fermata serve --dry-run       # commands are logged, text included, not sent or noted
fermata serve --allow-origin http://localhost:5173   # let pages of this origin call it
```

The default port is 7878 (`--port`, 0 for any free one), on 127.0.0.1 only. One server runs per data
directory (`FERMATA_DATA_DIR`, by default `fermata` under the OS data directory), which holds:

- `server.json`: `address`, `pid`, `version` and `instance` of the running server. Read it rather than
  assuming a port. A server that crashed leaves its file behind, and any program may then take its
  port: check that `pid` is alive before sending the token there.
- `server.token`: the token, readable by you only. Delete it and restart to change it.

Every request needs the token, reads included, because transcripts can hold secrets:

```
Authorization: Bearer <token>
```

It is never accepted in a URL. So a browser's `EventSource`, which cannot send headers, cannot read
`/events`: use `fetch` and read the body as a stream.

Anyone holding the token can type into your Claude sessions. Treat it like a password, and do not put
it in a page that other people can load. fermata gives out no cookies and no login page; a web
frontend either asks for the token once and keeps it, or has its own local server side that reads the
file.

Requests are refused when their `Host` is not `127.0.0.1` (so `localhost` is too: use the `address` of
`server.json`), or when they carry an `Origin` not named by `--allow-origin`. For a named origin, the
CORS headers and preflight are answered. A request not sent within 10 seconds is dropped, and at most
64 connections are served at a time, event streams included.

## A client using only curl

```bash
D=${FERMATA_DATA_DIR:-~/.local/share/fermata}   # macOS: ~/Library/Application Support/fermata
kill -0 "$(jq .pid "$D/server.json")" || exit    # never send the token to whatever took its port
API="http://$(jq -r .address "$D/server.json")/v1"
J="Content-Type: application/json"
# The token goes in on stdin: in an argument, other users of the machine could see it.
api() { curl -s -H @- "$@" <<<"Authorization: Bearer $(cat "$D/server.token")"; }

# What needs you, in order.
api "$API/state" |
  jq -r '.tasks[] | select(.needs_you) | "\(.context)/\(.id)  \(.badge_text)  \(.title)"'

# The open decisions, with their text.
api "$API/state" | jq -r '
  . as $s | .decisions[] | . as $d
  | $s.tasks[] | select(.context == $d.context and .id == $d.task)
  | .decisions[] | select(.key == $d.decision) | "\($d.context)/\($d.task) \(.key): \(.text)"'

# Where an answer to one task would go, then the answer itself.
api "$API/tasks/work/work-1" | jq .route
api -H "$J" -d '{"text": "the second one", "decision": "q1", "request_id": "r-1"}' \
  "$API/tasks/work/work-1/answer" | jq

# Close a session: the first try asks, the second says yes.
R=$(api -H "$J" -d '{}' "$API/sessions/$ID/close")
jq -r .error.message <<<"$R"                # show this to the user
TOKEN=$(jq -r .error.confirm <<<"$R")
api -H "$J" -d "{\"confirm\": \"$TOKEN\"}" "$API/sessions/$ID/close"

# Follow changes: one line per change, then read /state again.
api -N "$API/events"

# Or poll: 304 while nothing changed.
api -i -H 'If-None-Match: "9f3c-41"' "$API/state"
```

## Rules

- Everything is under `/v1` and JSON. Within `/v1`, fields and values are only ever added. Ignore what
  you do not know, and show an unknown value as it is.
- Every field is always present, null when it has no value. Only `delivery` in a reply and `confirm`
  in an error are left out instead.
- Times are RFC 3339 strings, except where the tracker gave another form.
- A task is named by `context` and `id` together: ids are unique only within a context's tracker.
  Encode both in URLs.
- Scope, filters and mine or everyone are yours to apply. `/state` covers everything.
- **Nothing in flight is not the same as not read yet.** Show `phase`, `stale`, the `…_error` and
  `…_stale` fields and degraded `health` rows. A failed source keeps its last good data.
- Text from tasks, tickets and transcripts is untrusted. Escape it before putting it in a page.

## Reads

| `GET` | Returns |
|---|---|
| `/server` | about this server |
| `/state` | the whole dashboard, from memory; has an `ETag` |
| `/tasks/{context}/{id}` | one task in full |
| `/tasks/{context}/{id}/timeline` | its history |
| `/tasks/{context}/{id}/conversation` | what was said about it |
| `/sessions` | the running sessions; `?closed=true` gives the closed ones |
| `/sessions/{id}/log` | the end of the session's log |
| `/sessions/{id}/pane` | where the session is now |
| `/transcripts/{ref}` | a transcript; `?around=<time>` finds the entry of that time |
| `/events` | the event stream |

Only `/server`, `/state`, `/tasks/{context}/{id}` and `/events` are answered from memory. The others
read files or tmux, and the conversation may ask the tracker.

### Events

Server-sent events. Each says what changed, not the change:

```
data: {"instance":"9f3c","rev":41,"sections":["tasks","deliveries"]}
```

Read `/state` again when an event names a section you show. The first event of a connection names
every section, and so does one sent after the connection fell behind. `sections` holds any of
`status` (`phase`, `loading`, `loaded_at`, `errors`), `contexts`, `tasks`, `decisions`, `sessions`,
`deliveries`, `health`, `notices`.

Each start of fermata has its own `instance`. When it changes, forget everything, including `rev`s and
confirm tokens. A comment line is sent every 15 seconds to keep the connection open.

## Commands

`POST` with `Content-Type: application/json` and a body of at most 64 KB. A field the command does not
know is refused.

| `POST` | Body | Does |
|---|---|---|
| `/refresh` | `force`, or `force` with `context` and `task` | reload everything, or one task; `force` skips caches |
| `/tasks/{context}/{id}/answer` | `text`, `decision`, `ask` | send an answer or instruction; `decision` names the decision (its `key` or `id`; without it, input answers a task's only open legacy decision, else none), `ask` sends a question about it instead of an answer |
| `/tasks/{context}/{id}/pin` | `target`, `pinned` | pin or unpin its `ticket` or `epic`; sets, does not toggle |
| `/deliveries/{id}/resend` | | send a `resendable` delivery again: the latest of a task's answers that did not arrive |
| `/sessions/{id}/send` | `text` | type into the session, with no task and no note |
| `/sessions/{id}/describe` | `text` | set its description |
| `/sessions/{id}/close` | `keep_pane` | archive it and close its pane (or keep the pane); asks |
| `/sessions/{id}/reopen` | | start a closed session again; asks |
| `/snapshots` | | save the running sessions |
| `/snapshots/restore` | | start those of the latest snapshot that are not running; asks |

`text` is one line without control characters, because those are keys in a terminal: join lines
before sending. Only `text`, `target` and `pinned` are required. Every body may also hold:

- `confirm`: the token from a `needs_confirm`. Any command may answer with one, not only those marked
  "asks": sending into a pane that does not run Claude Code does too, and so does `send` to a session
  with a dialog open. Show `message`, and on a yes repeat the same command with the token. It covers
  the whole command, text included, and if what it acts on has changed since, the repeat asks again.
- `request_id`: 1 to 128 bytes you choose. A repeat within 10 minutes returns the first result instead
  of doing it twice, so retry with the same one when a reply was lost. It is kept in memory: a retry
  across a restart runs again.

A reply is `summary`, one line for the user, and for an answer or resend that was recorded,
`delivery`. **A reply does not mean the session has read it**: follow the delivery's `state` and
`received_at`.

fermata itself never pushes, merges, comments or changes a ticket, and no command does. Opening links,
copying and jumping to a pane are the client's own to do, on its own screen: the URLs, panes and
worktrees are in the data.

## Errors

`{"error": {"code": …, "message": …}}`; the `message` can be shown to the user.

| Status | `code` | Meaning |
|---|---|---|
| 400 | `bad_request` | the request is malformed, or the command cannot apply |
| 401 | `unauthorized` | no token, or the wrong one |
| 403 | `forbidden` | the host or origin is not allowed, or a transcript lies outside Claude's directory |
| 403 | `read_only` | a command other than refresh, under `--read-only` |
| 404 | `not_found` | no such route, task, session, delivery or transcript |
| 409 | `needs_confirm` | ask the user; `confirm` is the token |
| 409 | `gone` | the decision answered, or of the answer resent, is no longer unresolved: read `/state` again |
| 502 | `source_failed` | the tracker, tmux or a file failed |

## Documents

`?` marks a field that can be null.

### Server

| Field | |
|---|---|
| `api` | `v1` |
| `instance` | as in events |
| `build`, `version`, `commit` | of fermata; `build` has all of it in words |
| `dry_run`, `read_only` | how it was started |
| `uptime_s` | seconds |
| `paths` | list of `name` and `path`: where it reads and writes |

### State

| Field | |
|---|---|
| `instance`, `rev` | `rev` rises by one per change; the `ETag` is `"<instance>-<rev>"` |
| `phase` | `empty` (nothing read yet), `preview` (from a cache) or `loaded` |
| `loading` | a load is running |
| `loaded_at`? | when the last full load finished |
| `errors` | list of text |
| `contexts` | list of Context, in order |
| `tasks` | list of Task, most urgent first |
| `decisions` | what to decide, in order: list of `context`, `task`, `decision` (the decision's `key`) |
| `sessions` | list of Session: the running ones |
| `deliveries` | list of Delivery, oldest first |
| `health` | list of Source |
| `notices` | what fermata did or noticed lately, oldest first: list of `seq` (rises by one), `at`, `text` |

### Context

| Field | |
|---|---|
| `name` | empty in the last one, which stands for sessions of no context |
| `me`? | who you are in its tracker; when null, every task counts as yours |
| `stale`? | a Stale: its task list is the last good one |
| `counts` | `shown` and `mine` (tasks), `needs_you`, `open_decisions`, `state_unknown`, `backlog` (active but not shown), `sessions` |

**Stale** is `as_of` (when the data was read) and `error` (why it is not newer).

### Task

| Field | |
|---|---|
| `context`, `id`, `title` | |
| `assignee`?, `priority`?, `labels`, `parent`? | from the tracker |
| `badge`, `badge_text` | its state, as a value (below) and in words |
| `rank` | its place in the order within its context, from 0 |
| `pin_rank`? | its place in the context's pins, from 0 |
| `pins` | `ticket`, `epic`: which of its links are pinned |
| `mine`, `needs_you` | |
| `state`? | a Declared state |
| `kept_open` | left open on purpose though its work is done |
| `waiting`? | what a parked task waits on |
| `decisions` | list of Decision |
| `session_id`? | its session |
| `branches` | list of Branch |
| `workers` | its subagents: list of `description`, `finished`, `modified`? |
| `progress` | `status_line`?, `checklist`? (`[done, total]`), `done`, `next` (lists of text) |
| `ticket_key`?, `ticket_url`?, `ticket`? | the linked ticket; `ticket` is null until it is read |
| `ticket_error`?, `ticket_stale`? | why it could not be read; a Stale |
| `pr`?, `pr_error`?, `pr_stale`? | the same for its pull request |
| `review_waiting`? | why a review of somebody else's PR waits on you |
| `review_unknown`? | why that could not be told: it may be waiting |
| `review_stale`? | a Stale |
| `last_change`? | |

`badge` is one of `needs-you`, `review-requested`, `review-unknown`, `overdue`, `ci-failing`,
`ready-to-merge`, `waiting-on-ci`, `state-stale`, `stalled`, `worker-running`, `interactive`,
`working`, `in-review`, `blocked`, `waiting-until`, `paused`, `done-cleanup`, `parked`, `unknown`.

**Declared**: `state`, `on`?, `until`?, `why`?, `next`?, `by`?, `at`?, `text` (all of it in one line),
`badge` (what it shows if nothing outranks it).

**Branch**: `repo`, `branch`, `worktree`, `status`, `pr`?, `pr_url`?. `worktree` can be empty.

**Ticket**: `key`, `title`?, `status`?, `assignee`?, `url`?, `epic`? (`key`, `title`?, `url`?),
`extra_links` (list of `label`, `url`).

**PR**: `number`, `url`?, `state`, `is_draft`, `review_decision`?, `checks` (`pass`, `fail`,
`pending`, `failing_url`?, `latest_url`?, `summary`).

### Decision

| Field | |
|---|---|
| `key` | names it within its task; use it as `decision` in an answer |
| `id`? | its id in the tracker; null for one answered by its `key` alone (a legacy note) |
| `text`, `options`, `recommendation`? | |
| `status` | `open`, `answered` (you answered; not acted on yet) or `resolved` |
| `asked_at`?, `answer`?, `answered_at`? | |
| `pending_question`? | your question about it, while nobody has responded |
| `prompt`? | the kind of prompt it is, when it is open in the session's pane now |
| `prompt_row` | a session's prompt that no tracker decision stands for |
| `pane_only` | only the pane can answer it; `answer` is refused |
| `label_missing` | the tracker has the question but the task is not labelled as waiting on you |

### Session

| Field | |
|---|---|
| `id` | |
| `context`? | its task's, else the one its worktree is in |
| `task`? | `context` and `id` |
| `orchestrator` | runs in its context's workspace |
| `worktree`, `branch`? | |
| `status` | a word, such as `working` or `idle` |
| `status_message`?, `needs_input` | |
| `waiting_on`? | while idle at its input, what it waits on: its task's newest DECISION, WAITING or FINDING note since the session began, else Claude Code's recap, else `asked a question` or `finished, nothing pending`. Null otherwise, and while its task has an open decision |
| `dialog`? | a dialog open in its pane: an answer waits until it closes |
| `description`?, `last_activity`?, `tool_calls`? | |
| `tmux` | where it was recorded as running; may be out of date |
| `pane` | its pane now, for display |
| `closed`, `closed_at`? | |

`/sessions` returns `sessions`. `/sessions/{id}/pane` returns `id` (tmux's own) and `label`.
`/sessions/{id}/log` returns `text` and `transcript`? (a reference).

### Delivery

| Field | |
|---|---|
| `id`, `context`, `task` | |
| `text` | what you sent |
| `route` | `session`, `orchestrator` or `note` |
| `session_id`?, `pane` | where it went |
| `state` | `delivered`, `queued`, `held` (until a dialog closes), `note_only` or `failed` |
| `detail` | more about the state |
| `created_at`, `sent_at`? | |
| `received_at`?, `received_via`? | once the session has it |
| `superseded_by`?, `resend_of`? | other delivery ids |
| `decision`?, `kind`? | the decision it replies to; `answer` or `question` |
| `label` | the state, as the user should see it |
| `alarm` | failed, or unacknowledged for too long |
| `resendable` | the resend command would work |

### Source

| Field | |
|---|---|
| `scope` | a context's name; empty for one shared by all |
| `source` | what is read |
| `degraded`, `state` | `state` is in words |
| `last_ok`?, `data_at`? | when it last worked; when the data shown was fetched |
| `last_error`? | `at`, `kind`, `key`, `message` |

### One task

| Field | |
|---|---|
| `task` | a Task |
| `description` | |
| `deliveries` | its own, oldest first |
| `route` | where an answer would go now: `kind` (`session`, `orchestrator` or `note`), `session_id`?, `pane`?, `dialog`?, `reason`? (why it is only a note) |

### Timeline, conversation, transcript

The timeline is `items`: list of `ts`?, `source`, `title`, `body`, `transcript`? (a reference).

The conversation is `entries`. A transcript is `entries`, `skipped_bytes` (left out before the first
entry) and `at`? (the index of the entry `around` fell on). An entry is:

| Field | |
|---|---|
| `kind` | `user`, `assistant`, `tool` or `tool-result` |
| `ts`? | |
| `head`, `body` | one line (tools) or the full text; more text |
| `label`? | show instead of the kind |
| `link`? | `transcript` and `ts`?: the transcript behind it, and the time to open it at |
| `folded` | start collapsed to `head`; tools do so too |

A transcript reference is opaque, holds for one `instance`, and is the only way to name a transcript.
