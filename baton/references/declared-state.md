# Declared task state

Every non-closed task can carry **one declared state** from a closed vocabulary. It says what the
task is doing *right now* in terms a person acts on: is it waiting on me, is someone working it,
is it stuck, has it been deliberately set aside. It is written through the tracker seam
(`tracker.sh state`) and read back on every task object, so a dashboard such as fermata shows it
instead of guessing it.

## Why it exists

Before this, a reader had to infer where a task stood from scattered, hand-kept signals: tracker
status `in_progress`, a `needs-decision` label, the order of DECISION/ANSWER notes, a description
prefix like `WAITING: PAUSED …`, whether a session was live. Each was kept up by a per-context
guidance rule, and each slipped. In one review, 7 of 12 in-progress tasks showed fermata's `idle`
badge, which meant "fermata does not know". An intentionally paused task, a task waiting on
someone else's review and a blocked task all looked identical, and blocked tasks disappeared
entirely because only `in_progress` was loaded.

Declared state makes that data instead of inference. Inference stays, but only as a cross-check
on what was declared (see "Cross-checks" below).

## How it relates to the other "states"

| thing | what it is | who decides |
|---|---|---|
| tracker `status` (`open`, `in_progress`, `blocked`, `closed`, `unknown`) | the coarse lifecycle | the backend's own field |
| **declared state** (this file) | where a not-closed task stands, in one word, with its reason | whoever is driving the task, through `tracker.sh state` |
| `scripts/task-state.sh` (`unstarted`, `waiting-on-ci`, …) | `baton:status`'s summary of one worktree, derived from git, the PR and the tracker | computed, never stored |

Declared state sits *inside* "not closed". It never changes the tracker status, with the one
exception in "Bookkeeping" below, and closing a task makes its declared state irrelevant: the
seam refuses to set one on a closed task.

## The vocabulary

The set is closed. A name outside it is refused on write and reads back as `unknown`.

| state | means | fields |
|---|---|---|
| `waiting-on-me` | the user has to answer or decide something before this moves | `why` (the question, briefly) |
| `working` | someone or something is actively on it | `on` (who: a session, a worker, a person), optional |
| `waiting-on-review` | the work is out for somebody else's review | `on` (the PR), optional |
| `blocked` | it cannot move until something outside it happens | **`on`** (what blocks it), optional `until` (when to look again) |
| `paused` | deliberately set aside; not stuck and not forgotten | **`why`**, optional `next` (resume hint) |
| `waiting-until` | nothing to do until a date, then something to check | **`until`**, optional `next` (what to check) |
| `done-pending-cleanup` | the work is finished; only cleanup or a follow-up keeps it open | none |

Bold fields are required for that state. Any state may also carry `why` and `next`.

`unknown` is not something you set. It is what a reader sees when a stored value cannot be
parsed or names a state outside the set, so a broken or newer write fails toward visible rather
than reading as "no state".

## The verb

```bash
"$TRK" state <id> <state> [--on <text>] [--until YYYY-MM-DD] [--why <text>] [--next <text>] [--by <who>]
"$TRK" state <id> clear
```

Prints the stored state object (or `null` for `clear`). The write records **when** (`at`, UTC)
and **who** (`by`: `--by` if given, else the backend's `whoami`). A state replaces the previous
one whole; there is no partial update, because a field from the old state (an `until` from a
`blocked`) silently surviving into the new one (`working`) would be wrong.

Exit status follows the seam: 1 for a refused value (unknown state, missing required field, a
non-ISO `until`, a closed task), 3 for an unknown id, 4 for a backend without the verb.

## Bookkeeping

Setting a state does the tracker housekeeping that used to be a guidance rule:

- **`waiting-on-me` claims an open task**, so a question cannot sit on an unassigned `open` task
  nobody is listing.
- **Leaving `waiting-on-me` removes `needs-decision`.** Moving to any other state (or `clear`)
  means the question was settled; the label must not linger and keep the task in "needs you".

The state is written first. If a bookkeeping step then fails, the verb exits 1 saying which step,
and the state stays set.

## On every task object

`get` and `list` add a `state` field: `null` when none was declared, else

```json
{"state": "blocked", "on": "Security sign-off", "until": "2026-12-01",
 "why": null, "next": null, "by": "Pat Doe", "at": "2026-11-23T14:02:11Z"}
```

Fields not given are `null`. `list --status active` returns every non-closed task (any backend
state that is not terminal), unlimited unless `--limit` is given; it is how a reader finds blocked
and stateless tasks, not just `in_progress` ones.

## Cross-checks a reader should run

A declared state is a claim. A reader with the world's facts in hand should check it, and show
the mismatch beside it rather than replace it:

| shown as | when |
|---|---|
| **stalled** | declared `working`, but no live worker or session and no change on the task for a few hours |
| **state stale** | declared `waiting-on-review`, but the PR has merged, closed or been approved |
| **overdue** | an `until` (on `blocked` or `waiting-until`) has passed |
| **unknown** | a task that is in progress (or waiting on a decision) has never had a state declared; counted, never shown as quiet "idle" |

`paused` never ages into stalled; that is the point of saying it.

World facts may outrank the declared state for display (a failing CI run on a
`waiting-on-review` task) as long as both are shown: `waiting-on-review · CI failing`.

## Who writes it

| caller | sets |
|---|---|
| `baton:start` | `working` when it hands off a leaf |
| `baton:pr` | `waiting-on-review --on <PR>` once the PR exists |
| `baton:finish` | when the leaf is left open on purpose: `waiting-until --until <date>` if it waits on a date, else `done-pending-cleanup` |
| an orchestrator or worker session | anything else, as the task moves (`waiting-on-me` when it asks the user, `paused`, `blocked`, `waiting-until`) |

Each skill write is best-effort: a backend without the verb (exit 4) or a failed write is
reported and the skill carries on, because a missing state degrades to `unknown` in the reader,
which is visible.

## Backends

The vocabulary, validation and stored shape live once in `scripts/tracker/lib-state.sh`; a
backend sources it, stores the object however suits it, and emits `state` on every task through
the shared `norm_state` jq definition. beads keeps it in issue metadata under `baton_state`, which
comes back on `list` with no per-task call. A comment-stream backend (jira, github-issues) would
store it the same way the branch registry does, as an appended `{"baton":"state",…}` entry folded
to the latest, or in a custom field.
