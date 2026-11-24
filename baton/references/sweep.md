# The sweep

A **sweep** looks over one context for things that have quietly stopped: an answer nobody acted
on, a worker sitting at a prompt, a date that has passed. It has two parts, kept apart on purpose:

- **`scripts/sweep-check.sh`** decides what is wrong. It is read-only and can be run any time, by
  anyone.
- **The `sweep` startup task** decides when to look. It is opt-in, and schedules the check in the
  home session.

## The check

```bash
sweep-check.sh [--context <file|->] [--format text|json] [--backlog each|summary]
               [--today YYYY-MM-DD] [--now <ISO UTC>] [--stalled-after <hours>] [--stale-after <days>]
```

One line per problem, `<kind> <task id> <detail>`, most pressing kind first. Nothing to report
prints nothing.

| kind | reported when |
|---|---|
| `unreadable` | something could not be read, so one of the checks below is incomplete; the id is `-` when no single task is at fault |
| `answered` | a decision is `answered` but not resolved: the user is done, nobody has acted |
| `prompt` | a live session in a task's worktree is waiting at a prompt (a permission, a question) |
| `overdue` | an `until` before today: on a declared `blocked` or `waiting-until`, or on the registry entry of a `keep-task-open` task (read through `parked.sh`) |
| `stalled` | declared `working`, no live session in its worktree, and neither the task nor its state changed for 4 hours |
| `unknown` | the stored state or tracker status is unreadable; or `needs-decision` with no decision record (a legacy note: read it by hand); or in progress with no declared state, no live session, no unresolved decision, and not parked |
| `backlog` | open or blocked in the tracker, nothing declared, not parked, not `needs-decision`, and untouched for more than 14 days |

`--backlog summary` folds the `backlog` lines into one, with the count and the three oldest, for a
recurring report. The default is a line each.

`--format json` prints an array of `{kind, id, title, detail}`.

| exit | meaning |
|---|---|
| 0 | every check ran (with or without problems) |
| 1 | it could not run at all: usage, or no context |
| 2 | it ran, but something was `unreadable`; the output says what |

**A failed read is a line, never a silence.** If the task list cannot be read there is one
`unreadable` line and nothing else. If sessions cannot be listed, `stalled` and `unknown` are
judged as if none were live, and the `unreadable` line says so.

**Read-only.** Tracker verbs used: `list`, `whoami`, `get`, `decisions`, `list-branches`. It does
not sync the tracker, never backfills an identity carrier, and touches no session. On a
backend without `decisions` (exit 4) every `needs-decision` task is reported as `unknown`.

**Whose tasks.** Tasks assigned to somebody else are left out unless they carry `needs-decision`.
If `whoami` cannot be read, nothing is left out. A `prompt` is reported for any task in this
context, closed ones included, because the session is on this machine.

**What it cannot see.** A background sub-agent has no session of its own, so a task that only a
sub-agent is working reads as `stalled` once nothing has changed for 4 hours. fermata reads
worker transcripts and does not have this gap.

## Live sessions: `scripts/sessions.sh`

The check needs to know which sessions are live and which are stuck at a prompt. That comes
through a seam of its own, so nothing else knows where it is read from.

```bash
sessions.sh list
```

Prints a JSON array, one object per live session on this machine:

| field | value |
|---|---|
| `id` | the session's id |
| `name` | its display name, or `null` |
| `cwd` | its working directory |
| `status` | `working`, `idle`, `waiting` (at a prompt) or `unknown` |
| `waiting_for` | what it waits for, when the backend says; else `null` |

Exit 0 with the array (`[]` means none are live), 1 when it cannot tell, 4 for no such backend. A
failure is never `[]`.

A backend is an executable `scripts/sessions/<type>.sh` taking the same verb, picked by
`$BATON_SESSIONS_TYPE` (default `claude-code`). `claude-code` reads `claude agents --json`. A
status it does not recognise becomes `unknown`, never `idle`.

## The `sweep` startup task

Opt in by adding it to `startup_tasks` in `context.yaml`:

```yaml
startup_tasks: [align, doctor, cleanup, status, sweep]   # every 30 minutes
startup_tasks: [align, doctor, cleanup, status, sweep 20m]
```

The interval is `10m`, `15m`, `20m`, `30m` or `60m`. `baton:session-start` runs the check once and
schedules it in the home session with the harness's own in-session scheduler. There is no daemon:
the schedule ends with the session, and the next `baton:session-start` sets it again. A context
that does not list `sweep` is never swept on a schedule.

A scheduled sweep reports. Acting on a line follows the context's `guidance.md`, like anything
else the home session does.
