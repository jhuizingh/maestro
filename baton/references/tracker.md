# The tracker provider interface

baton talks to a task tracker through **one seam**: `scripts/tracker.sh`. No skill, script or
hook invokes a backend tool directly. `task_tracking.type` in a context's `context.yaml` selects
which backend answers, and that selection is the only place in baton that knows what the backend
is.

This file is the contract. It documents the verb set, the shapes that cross the seam, and what a
new backend has to implement. Read it before adding a verb, a backend, or a caller.

```
  skills, scripts, hooks
          │
          │  tracker.sh <verb> …            ← the only thing callers may use
          ▼
  scripts/tracker.sh                        ← dispatch: reads task_tracking.type
          │
          ├─ scripts/tracker/beads.sh       ← implemented
          ├─ scripts/tracker/jira.sh        ← not built; sketched at the end of this file
          └─ scripts/tracker/github-issues.sh
                    │
                    └─ scripts/tracker/lib-registry.sh   ← shared by every backend whose
                                                            substrate is an append-only
                                                            comment stream (all three)
```

## Why a seam at all

`task_tracking.type` shipped in the very first `context.yaml` and was never dispatched on: every
skill read `task_tracking.dir` and ran `bd`. Fourteen skills did, and the count grew with each
new skill, because with no seam to reach for a new skill just adds its own direct calls. The
declared extension point did nothing, and porting to any other tracker meant touching all of
them.

The second reason is the **branch registry** below. Recording which branches a task has spawned
is a tracker write, and doing it beads-specifically would have to be redone for the next
backend — so the registry is defined as verbs on this interface, not as `bd` calls in a skill.

## Calling it

```bash
TRK="${CLAUDE_PLUGIN_ROOT:-$HOME/code/maestro/baton}/scripts/tracker.sh"
[ -x "$TRK" ] || TRK="$HOME/code/maestro/baton/scripts/tracker.sh"
"$TRK" get "$LEAF"
```

That two-line resolve-with-fallback is the same shape every other baton helper uses; keep it.

Options accepted before the verb:

| option | meaning |
|---|---|
| `--context <file\|->` | pre-resolved context JSON (`-` reads stdin). Default: run `resolve-context.sh`. Pass this when you already have `$CTX` — it saves a resolver call per verb. |
| `--tracker <dir>` | override the tracker location (`task_tracking.dir`). |
| `--type <name>` | override the backend. Only `baton:configure` needs this, to set a tracker up before any context exists. |

`tracker.sh` exports `BATON_TRACKER_DIR` (the resolved, `~`-expanded `task_tracking.dir`) into
the backend, and the beads backend turns that into `BEADS_DIR` on every invocation. **A caller no
longer needs to export `BEADS_DIR` itself** — that was the ambient-drift footgun `baton:beads`
Step 2 exists to catch, and going through the seam removes it by construction.

### Exit status

| code | meaning |
|---|---|
| 0 | the verb succeeded |
| 1 | usage error, unknown verb, unreadable context, missing backend |
| 3 | **not found** — the id does not exist in this tracker |
| 4 | **unsupported** — this backend does not implement this verb |

3 and 4 are distinct from 1 on purpose: a caller may reasonably continue past either
("no such bead in this context", "this backend has no registry"), and neither is a bug.

## The verb set

Everything is JSON on stdout unless noted. Every verb that names a task takes its id as the first
argument after the verb.

### Core — reading

| verb | returns |
|---|---|
| `get <id>` | one **task object** (below). Exit 3 if unknown. |
| `list [--status <s>] [--label <l>] [--limit <n>]` | array of task objects. `--status all` for everything. |
| `ready` | array of task objects with nothing blocking them. |
| `children <id>` | array of task objects that are children of `<id>`. |
| `deps <id> [--direction down\|up]` | array of **edge objects** (below). `down` (default) = what `<id>` depends on; `up` = what depends on `<id>`. |
| `label-list <id>` | array of label strings. |

### Core — writing

| verb | does |
|---|---|
| `create <title> [--description <t>] [--parent <id>] [--labels a,b] [--priority <n>]` | creates a task; prints **only the new id**. |
| `update <id> [--status <s>] [--priority <n>]` | updates fields. |
| `claim <id>` | assign to the current user **and** set `in_progress`. One verb because every backend does both, and callers always want both. |
| `close <id> --reason <text>` | closes with a reason. |
| `reopen <id>` | reopens. |
| `label-add <id> <label>` / `label-remove <id> <label>` | one label per call. |
| `link <from> <to> --type <blocks\|parent-child\|relates-to\|discovered-from>` | records a dependency edge. |
| `note <id> <text>` | appends to the task's long-form notes. |

### Registry — branches created against a task

| verb | does |
|---|---|
| `record-branch <id> --repo <r> --branch <b> --worktree <p> [--status <s>] [--created <ts>]` | records that a branch now exists for this task. |
| `list-branches <id>` | array of **branch entries** (below), oldest first. |
| `update-branch <id> <branch> <field=value> …` | records new values for one branch's entry. |

**There is deliberately no "gate the parent on all its children" verb.** `baton:split` used to
prescribe `bd update --waits-for-gate all-children`; that flag does not exist in bd 1.1.0 and the
command has never worked. Rather than carry a phantom across the seam, the verb set leaves it
out: the parent/child edge already *is* the structure baton acts on (`baton:start` refuses to
worktree a parent with open children, and `deps` reports the edges), and "are all the children
done" is a query each backend answers its own way (`bd epic status`, a JQL rollup, a GitHub task
list). Add a verb for it when a caller genuinely needs one — not before, and not by inventing a
flag on `update`.

### Backend metadata

| verb | returns |
|---|---|
| `capabilities` | object: `{type, verbs:[…], registry:true\|false, tools:[…]}`. `baton:doctor` reads `tools`; a caller can check `verbs` before using an optional one. |
| `sync [--pull\|--push]` | best-effort tracker sync. Exit 4 when the backend has nothing to sync. |
| `remote` | object `{configured, remote}` naming the sync remote, live — never a config comment. |
| `init <dir>` | create a brand-new tracker. Destructive-by-omission: only for a tracker that exists nowhere yet. |
| `bootstrap <dir>` | non-destructively connect to a tracker that may already exist. **This is the one to use** for a fresh clone or worktree. |

## Shapes crossing the seam

### Task object

Every backend normalizes to this. Fields it cannot supply are `null` (or `[]`); a caller must
tolerate that rather than assume a backend is beads.

```json
{
  "id": "jbh-sh5.2",
  "title": "…",
  "description": "…",
  "acceptance_criteria": "…",
  "notes": "…",
  "status": "in_progress",
  "priority": 2,
  "type": "task",
  "assignee": "…",
  "labels": ["architecture", "baton"],
  "parent": "jbh-sh5",
  "created_at": "2026-08-18T18:27:33Z",
  "updated_at": "2026-08-28T18:26:05Z",
  "closed_at": null,
  "close_reason": null
}
```

**`status` is a closed vocabulary**: `open`, `in_progress`, `blocked`, `closed`, `unknown`. A
backend maps its own states onto these; anything unrecognized becomes `unknown`, never a silent
`open`. (`unknown` matters: `cleanup-verdict.sh` treats "the lookup failed" and "the task is
open" completely differently, and collapsing them is how a lookup failure turns into a wrong
verdict.)

**`get` returns a bare object, never an array.** `bd show --json` emits a single-element array,
and three separate call sites had to carry a `if type=="array" then .[0] else . end` guard —
one of which was written *after* a bare `.status` silently produced an empty result on every
cleanup run. That normalization now happens once, here.

### Edge object

```json
{"id": "jbh-f5u", "title": "…", "status": "closed", "type": "relates-to", "direction": "down"}
```

`type` is one of `blocks`, `parent-child`, `relates-to`, `discovered-from`. Callers filter:
a **blocker** is a `down` edge of type `blocks` whose `status` is not `closed`; a `parent-child`
edge is hierarchy, not a gate.

### Branch entry

```json
{
  "repo": "maestro",
  "branch": "jbh-sh5.2-tracker-branch-registry",
  "worktree": "/Users/jh/code/maestro-worktrees/jbh-sh5.2-tracker-branch-registry",
  "created": "2026-08-28T18:26:05Z",
  "status": "merged",
  "pr": "16",
  "ready": "yes",
  "keep_task_open": "no",
  "no_pr_needed": "no",
  "updated": "2026-08-29T10:02:11Z",
  "revisions": 3
}
```

`status` moves `open` → `pr-open` → `merged` | `no-change` | `abandoned`. `no-change` is the
shape `no-pr-needed` names: work that deliberately landed outside the repo.

`ready`, `keep_task_open` and `no_pr_needed` are the **branch-scoped** counterparts of the three
bead labels `baton:cleanup-worktrees` reads. That is the point of recording them here — see
"Why the registry is per-branch" below.

`updated` and `revisions` are computed by the fold, not written by a caller.

## The registry is an append-only log, folded on read

`record-branch` and `update-branch` both **append**; nothing is ever edited or deleted.
`list-branches` reads the whole log and folds it: entries are grouped by `repo` + `branch`, and
within a group later entries overlay earlier ones **key by key**, so a partial update touches
only the fields it names.

This is not an implementation detail to route around — it is forced by the substrate. Every
backend on the table stores this in a comment stream (beads comments, Jira comments, GitHub issue
comments), and comment streams are append-only in practice: editing someone else's comment is
either unavailable, or a permission a workflow tool should not need. Designing the fold into the
interface means no backend has to fake mutation.

It pays for itself twice. The log *is* the provenance record — "this bead had three branches over
its life across PRs #176/#177/#178" is reconstructable, which was the original complaint. And an
interrupted session can only ever leave a stale entry, never a half-written one.

`update-branch` matches on the branch name alone. If a task has entries for the same branch name
in two different repos, pass `--repo` to disambiguate; without it the update applies to every
matching entry, which is the right answer for the overwhelmingly common one-repo case.

### Why the registry is per-branch

`ready-for-worktree-delete`, `keep-task-open` and `no-pr-needed` live on the **task**.
`baton:cleanup-worktrees` documented the consequence as a deferred limitation: one bead can own
several worktrees over its life, and a task-level label is visible from all of them, so a
still-active later worktree gets flagged as an anomaly because of a label an earlier, finished
worktree left behind.

The registry entry is keyed by branch, so those three signals become branch-scoped for free.
Cleanup prefers the registry entry for the branch it is actually looking at and falls back to
task labels only when no entry exists — which is exactly the pre-0.8.0 worktrees that have none.
`cleanup-verdict.sh` reports which of the two answered in `$LABEL_SCOPE`, so a removal is never
silently justified by a label belonging to different work.

The task labels are still written, by `baton:finish`, alongside the registry update. They are
what `baton:whereami` counts to say "3 worktrees are flagged ready for cleanup", and they keep an
older baton (or a hand inspection with `bd label list`) working.

## Who calls what

| caller | verbs |
|---|---|
| `baton:start` | `get`, `children`, `create`, `ready`, `list`, `claim`, `deps`, **`record-branch`** |
| `baton:resume` | `get` |
| `baton:status` | `get`, `label-list`, `deps` (read-only: never `claim`, `sync`, or any write) |
| `baton:pr` | `get`, **`update-branch`** (`status=pr-open`, `pr=<n>`) |
| `baton:finish` | `get`, `close`, `label-add`, **`update-branch`** (`status=merged\|no-change`, `ready=yes`, …) |
| `baton:cleanup-worktrees` | `get`, `label-list`, **`list-branches`** |
| `baton:split` | `get`, `create`, `link`, `update`, `label-add` |
| `baton:task-add` | `create` |
| `baton:task-list` | `ready`, `list`, `children` |
| `baton:whereami` | `list --label ready-for-worktree-delete` |
| `baton:session-start` | `sync --pull`, `remote`, `list`, `ready` |
| `baton:configure` | `init`, `bootstrap` |
| `baton:doctor` | `capabilities` |
| `hooks/session-start-detect.sh` | `get` |

`baton:beads` is the exception and is *supposed* to be: it is the beads backend's own audit
skill, so it runs `bd` directly and says so. A second backend would get its own equivalent.

## Writing a backend

Drop an executable `scripts/tracker/<type>.sh`, add `<type>` to `task_tracking.type`'s enum in
`references/context.schema.json`, and implement the verbs. Rules:

1. **Normalize on the way out.** The task object above, with the closed `status` vocabulary. A
   caller must never need to know which backend answered.
2. **Never invent a status.** Map what you cannot recognize to `unknown`.
3. **`get` exits 3 for a missing id**, not 1, and prints nothing on stdout.
4. **Exit 4 for a verb you do not implement**, and leave it out of `capabilities.verbs`. Do not
   emulate it badly — a caller that can degrade will, and one that cannot should fail loudly.
5. **Source `lib-registry.sh`** if your substrate is a comment stream; it gives you the fold, the
   entry envelope, and the parsing rules, so three backends do not write three folds.
6. **Declare your tools** in `capabilities.tools` so `baton:doctor` checks for them.
7. Add cases to `scripts/test-tracker.sh`. The dispatch, the normalization and the fold are all
   testable with a stub backend and no network.

### On paper: `jira`

Proof the verb set is sufficient without building it.

| verb | mapping |
|---|---|
| `get` | `GET /rest/api/3/issue/{key}` → `fields.summary`→`title`, `fields.description`→`description`, `fields.labels`→`labels`, `fields.parent.key`→`parent`. `acceptance_criteria` is a custom field (`customfield_NNNNN`), configured per context. |
| `status` mapping | Jira statuses are per-workflow, so the context configures the map: `To Do`→`open`, `In Progress`→`in_progress`, `Blocked`→`blocked`, anything in the `Done` **status category**→`closed`, else `unknown`. |
| `list` / `ready` | JQL — `project = X AND status = "To Do"`; `ready` is JQL plus a blocked-by-open-issue filter. |
| `create` | `POST /rest/api/3/issue`; `--parent` sets `fields.parent`. |
| `claim` | `PUT .../assignee` + a transition to In Progress. Two calls behind one verb — exactly why `claim` is a verb rather than two `update`s. |
| `close` | a transition; the reason goes in a comment (Jira has no close-reason field). |
| `label-*` | `PUT /issue/{key}` with `update.labels[].add`/`.remove`. |
| `link` | `POST /rest/api/3/issueLink`, mapping `blocks` to the "Blocks" link type. |
| `deps` | `fields.issuelinks`, split into `down`/`up` by `inwardIssue`/`outwardIssue`. |
| **registry** | `POST /issue/{key}/comment` with the same JSON envelope; `list-branches` reads the comments and calls the shared fold. A custom field would fit a single current branch but not a history, and the history is the point. |
| `sync` | exit 4 — a REST tracker has no local copy to sync. |
| `init`/`bootstrap` | exit 4 — projects are created by an admin, not by baton. |

Nothing above needs a verb this file does not have, and the two Jira-shaped problems — workflow
statuses and the missing close-reason field — are absorbed by the normalization rules rather than
by new verbs.

### On paper: `github-issues`

`gh issue view --json`, `gh issue create`, `gh issue edit --add-label`, `gh issue comment`,
`gh issue list --json`. Registry entries are issue comments, folded by the same helper. `status`
is `open`/`closed` only, so `in_progress` maps from a configured label (`status:in-progress`) —
the same trick baton already uses for `autonomous-safe`. `deps` has no native edge type: task
lists and `Closes #N` cross-references are the substrate, so a first cut may exit 4 for `deps`
and let callers degrade (they already do — a missing blocker list reads as "no blockers known",
not as a failure). `sync`, `init` and `bootstrap` exit 4.
