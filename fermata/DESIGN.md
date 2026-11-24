# fermata — design

A terminal dashboard for one-orchestrator, many-workers Claude Code work. It is the main place to see
every in-flight task, answer what is waiting on you, and steer work, so nothing depends on scrolling
back through a chat. (A fermata is the mark that holds a note until the conductor releases it, which
is what a task waiting on you is.)

Status: M1–M4 built, 2026-11-12. Scope modes (one context / all contexts) added 2026-11-16.
ccmon's Python hooks ported into `fermata hook` and shipped as the `fermata` plugin, 2026-11-20.
ccmon's remaining jobs (dead-session cleanup, snapshots, the jump-to-dashboard key) moved into
fermata and every session action checked against live sessions, 2026-11-21. Local to this machine; no
remote. See "As built" at the end for the refinements made while building.

## Goals

1. **Tasks first.** List every task in flight (in progress, or with a declared state or an open decision), ordered so that the item that most needs you is on top.
2. **One panel per task.** Show the ticket, epic, PR and checks, done and next, and history. Open any
   of them in the browser with one key.
3. **Answer and steer in place.** Type the answer to a waiting question, or the next instruction for a
   task, without switching sessions.
4. **Priority streams.** Pin tickets or epics. Anything in a pinned stream that needs you always sorts
   to the top.
5. **Replace ccmon.** Show live sessions with status, jump to them, quick-reply, send prompts, view
   logs, and close and reopen them, so `claude-monitor/dashboard.py` can be retired.

## Non-goals

- No knowledge of any specific ticket system, org or workflow in the core: no JIRA, no ticket-notes
  layout, no `jira-` labels. Those come in through per-context adapters (below).
- No changes to baton's `context.yaml` schema, and no task/ticket knowledge added to ccmon. baton's
  code changes only where fermata needs a tracker answer the seam did not give: so far one read
  verb, `whoami` (who `claim` assigns to), added for the mine/everyone filter.
- No outward actions of its own: no pushing, merging, commenting or ticket transitions. fermata
  relays instructions and answers; the orchestrator and its approval gates do the acting.
- No server, daemon or new credentials. Everything comes from existing local CLIs and files.

## Concepts

| Concept | Generic meaning | Source |
|---|---|---|
| Context | A baton context: tracker, member repos, workspace repo | `~/.config/baton/registry.yaml` → each workspace's `context.yaml` |
| Task | A tracker leaf: id, title, status, assignee, labels, notes, declared state | baton tracker seam (`tracker.sh list/get`) |
| Declared state | Where a not-closed task stands, from baton's closed vocabulary (`waiting-on-me`, `working`, `waiting-on-review`, `blocked`, `paused`, `waiting-until`, `done-pending-cleanup`), with its `on`/`until`/`why`/`next`, who set it and when | the task object's `state` field (baton `references/declared-state.md`) |
| You | The name `claim` assigns tasks to, per context | baton tracker seam (`tracker.sh whoami`) |
| Links | Ticket, epic, PR, plus any extra URLs | ticket/epic: context adapter; worktree and PR: the identity carriers in the member repos' git dirs, plus `gh`; the branch registry (`tracker.sh list-branches`) only on demand |
| Progress | One-line status, done, next, checklist count | context adapter (files), latest worker report |
| Decision | A question waiting on you, with an id, options, a recommendation and a status (open, answered, resolved); a task can hold several. Read for tasks labelled `needs-decision`; a legacy `DECISION` note still counts | baton tracker seam (`tracker.sh decisions`) |
| Review waiting | A `pr-review` task whose review is still waiting on you, or whose state could not be read | baton's `pr-review.sh rank`, fed fermata's PR cache |
| Session | A live or closed Claude Code session | fermata's session records (`sessions/*.json`, `closed/*.json`, `logs/*.log`, written by `fermata hook`; ccmon's older ones still read); tmux |
| History | Notes, worker reports, session events, transcripts | tracker notes, subagent transcripts, session logs, session transcripts |

## Information architecture

fermata has one axis for *what kind of thing* you look at (the views: Tasks, Decisions,
Sessions) and a separate one for *whose* things: the **scope**. They never mix: no view is a
context, and no context choice is a view.

**Scope modes.** There are exactly two, and every list view obeys the current one.

| Mode | Shows | Header |
|---|---|---|
| **All contexts** (the default) | every context's items, grouped under one heading per context | `all contexts` |
| **One context** | only the chosen context's items, with no headings | `context: <name>` |

- **Contexts are never interleaved.** In all-contexts mode each list is cut into one block per
  context, in registry order (the order of `~/.config/baton/registry.yaml`), and each block opens
  with a heading row: `── <name> · <n> tasks · <k> need you ──` (Decisions and Sessions name
  their own counts). Registry order is fixed on purpose, so a context's block does not jump
  around as its tasks change state; urgency is shown inside each block, and by the header count.
  A context with nothing to show in a list gets no heading in that list (the picker still lists
  it). Heading rows are not selectable: `j`/`k` step from the last item of one block to the first
  of the next.
- **Within a block** the order is the view's usual order (the task sort under "Sort", decisions
  likewise, sessions in ccmon's order).
- **Keys**, from Tasks, Detail, Decisions and Sessions:
  - `M` switches mode. Going to one-context mode reuses the last context picked; if none has been
    picked yet, it opens the picker.
  - `C` opens the **context picker**: one row per context with its tasks, need-you count,
    decisions and sessions. `Enter` switches to one-context mode on that context and returns to
    the view you came from; `Esc` returns without changing anything. The picker only chooses a
    context; it has no "all" row, because choosing the mode is `M`'s job.
  - `[` / `]` step to the previous / next context while in one-context mode (no-op otherwise).
  - `fermata --context <name>` starts in one-context mode on that context.
- **What belongs to a context.** A task: its tracker's context. A decision: its task's. A
  session: the context of the task whose worktree it runs in; else the context baton resolves
  its directory to, by `resolve-context.sh`'s cwd rungs: a context's own workspace or `home`,
  then its `member_repos` (globs allowed), each including its `-worktrees` sibling, ties to the
  first registered. fermata mirrors those two rungs in `config::context_of_path` rather than
  running the script per session on every redraw; baton's script owns the rule. The `default`
  rung is not used: a session in none of these directories belongs to no context. In
  all-contexts mode those sit in a final `── other ──` block; one-context mode hides them.
- **Counts follow the scope.** The tab counts (decisions, sessions) and "need you" count only the
  scope's items. In one-context mode, items needing you in other contexts are not hidden
  silently: the header adds `+N elsewhere`.
- **Selection, filters and detail** are unaffected by the scope's mechanics: a filter narrows
  the scoped list, selection stays on its item when the mode or context changes (falling back to
  the clamped row, as for any list change), and an open detail stays open. In the wide detail
  layout the left column is the scoped list, with the same headings.
- **Not scoped:** Task detail, the viewer and Help show one item or none, so the scope does not
  apply. `--dump` is not scoped either; it always prints every context.

**Mine or everyone.** Within the scope, the Tasks list shows only **your** tasks by default. In
a shared tracker most in-progress tasks belong to other people, and listing them all buries
yours. `u` switches between **mine** and **everyone**. The setting is not saved, so fermata
always starts on mine.

- **Who you are** is asked per context through the tracker seam: `tracker.sh whoami`, the name
  `claim` assigns tasks to (beads: bd's actor, `BEADS_ACTOR`, else git `user.name` in the
  tracker's repo, else `USER`). No name is configured or hardcoded. A backend that cannot answer
  leaves you **unknown** in that context.
- **A task is yours** when any of these holds:
  - its `assignee` is you (trimmed, ignoring case);
  - it has no assignee: nobody else owns it, and leaving it out would hide orphaned work;
  - it is waiting on you, whoever the assignee is: needs you (an unanswered decision, or its
    live session is asking) or a review waiting on you;
  - you are unknown in its context. fermata never guesses, so that context shows everyone.
- **Nothing disappears silently.** In mine mode the header says `mine`, plus `· N hidden` when
  the filter hides any of the scope's tasks, and `· everyone in <ctx>` for each context where you
  are unknown. Each context heading in all-contexts mode adds `· N hidden`, and so does that
  context's row in the context picker. In everyone mode the header says `everyone`.
- **What it touches.** Only the Tasks list, its wide-detail column, and the counts drawn from
  them (list title, headings, the picker's task count). Every task that needs you is yours by the
  rule above, so the Decisions view and the "need you" and `+N elsewhere` counts are the same in
  both modes. Sessions are this machine's, so they are not filtered. `--dump` is unfiltered and
  carries each task's `assignee` and each context's `me`.

### Screens and keys at a glance

| Screen | Reached by | Scoped | Shows |
|---|---|---|---|
| Tasks (home) | `t` | yes | tasks in flight: yours, or everyone's (`u`) |
| Task detail | `Enter` on a task or decision | — | one task, its timeline |
| Decisions | `d` | yes | decisions waiting on you |
| Sessions | `s` (`X` closed) | yes | live (or closed) Claude sessions |
| Context picker | `C` | — | the contexts, to choose one |
| Viewer | `Enter` on a timeline item, `l`, `v` | — | a note, log or transcript |
| Help | `?` | — | keys, and the build (version, commit, build time) in its title |
| Health | `H`, or the header's `⚠ N sources degraded` | — | each external source per context: last success, last error, data age |

## Views

- **Tasks** (home). One row per task in flight (see "Which tasks load") in the scope that is yours (or everyone's after
  `u`; see "Mine or everyone"): priority marker, state badge
  (needs you / review requested / review ? / overdue / worker running / interactive / waiting on CI / ready to merge / parked),
  task id, title (plus "waiting on X until D" on a parked or overdue row), ticket, PR number with
  check summary, checklist progress, and age of the last change.
  - Sort order (within each context's block in all-contexts mode): reviews of other people's PRs that are waiting on you (or whose state is unknown), then needs-you in a
    pinned stream (by pin order), then any other needs-you, then overdue parked tasks, then the
    rest of the pinned streams, then everything else by most recent change.
- **Task detail** (Enter on a task).
  - Header: ticket (key, title, status), epic, PR (state, review decision, checks).
  - Keys: `o` ticket, `e` epic, `p` PR, `c` failing or latest CI run, `w` go to the task's session,
    `W` open the worktree in a new tmux window.
  - `w` (also on the Tasks and Decisions lists) switches the client to, in order: the task's live
    Claude pane (the routing plan's `Session` route, found by process as under "Pane
    resolution"; never the orchestrator); else a tmux session named for the task (`session_name`,
    then `session_name_legacy`, from `task-identity.sh --worktree`), which covers a Claude that
    exited while its session is still up; else it opens a new window in the worktree and says
    there was no live session. Outside tmux there is no client to move, so it shows the
    `tmux attach -t …` command instead.
  - Body: every unresolved decision (id, question, options, recommendation, state and age, your
    pending question or answer); done / next; checklist. With several, `▶` marks the one input goes to.
  - Timeline: tracker notes, worker reports, session log lines (prompts, notifications, stops), newest first.
    It is split in two: the item list on top, and below it the full text of the highlighted
    item, wrapped, which changes as the highlight moves (`J`/`K` scroll it). Enter on an item
    opens it in the viewer, and on a transcript opens the transcript viewer.
- **Context picker** (`C`). The contexts, with their counts; Enter shows only that one. See
  "Information architecture".
- **Decisions.** One row per unresolved decision, grouped by context, in task priority order.
  Open ones need you. An answered one stays listed, dimmed, as "answered N ago, waiting on the
  orchestrator" (yellow after 30 minutes) until the orchestrator resolves it. Number keys pick an
  option; `a` types a free-form answer; `Q` asks a question instead.
- **Sessions.** ccmon parity: status, repo, branch, tool, age and calls, now with the task each
  session belongs to, grouped by context like the other lists. Keys as in ccmon: jump, quick reply, send prompt, log view, describe, close,
  reopen, snapshot and restore. Each row starts with an **index key**, tmux choose-tree style:
  rows 0–9 take the plain digit, rows 10–35 take `M-a`…`M-z` (Meta+letter); later rows get none.
  Pressing it acts on that row as `Enter` would (jump; reopen, confirmed, in the closed list).
  Indices follow the displayed order (grouping, filter), so they are recomputed on every reload.
- **Transcript viewer.** Renders a Claude Code `.jsonl` as user and assistant turns, with tool calls
  collapsed to one line (expand on Enter), and searching with `/`.
- **Selection is by item, not by row.** The task, decision and session lists keep their cursor on
  the same item across reloads, re-sorts and session changes, wherever it moves. When the
  selected item leaves the list (a decision was answered, say), the cursor stays on the same row,
  clamped to the list's new length.
- **Filtering lists** (`/` on Tasks, Decisions or Sessions), k9s-style. `/` opens a filter
  prompt in the footer and the list narrows as you type. Enter locks the view with that filter;
  Esc while typing clears it and shows the whole list again; Esc on a locked filtered view does
  the same (a second Esc then goes back as usual). See "Filtering" under "As built".
- **Wide layout.** On a terminal at least 140 columns wide, Enter on a task or decision keeps
  the list it came from as a left column and shows the detail beside it; moving the list
  selection moves the detail with it. Narrower, the detail replaces the list. See "Layout and
  colour" under "As built". A filtered list stays filtered in that left column.
- **Input bar** (`:` or `i` from any view). Sends an instruction or answer to the selected task (see
  "Routing input").

## Per-context adapters

The core never interprets ticket systems. Each context **may** provide an adapter file in its
workspace repo, `<workspace>/fermata.yaml`, next to its `guidance.md`. A context with no file gets no
ticket or epic panel; everything else still works.

```yaml
# <workspace>/fermata.yaml
ticket:
  key_from: label                         # "label": the first label matching `pattern`
  pattern: '^jira-([A-Z]+-[0-9]+)$'       # capture group 1 is the key
  url: 'https://example.atlassian.net/browse/{key}'
  info: ['./fermata/ticket-info.sh', '{key}']   # run from the workspace dir; prints TicketInfo JSON
  ttl: 300                                # seconds to cache info output
  timeout: 5                              # seconds `info` may run (default 5, clamped to 1–30)
progress:
  status: '~/notes/tickets/{key}/STATUS.md'      # first line = one-line status
  checklist: '~/notes/tickets/{key}/tasks.md'    # counts "- [x]" vs "- [ ]"
pr:
  provider: github                        # PR state and checks via `gh`
priorities:                               # pinned streams, top first; editable from fermata
  - epic: ABC-100
  - ticket: ABC-123
workers:
  warn_unlinked: true                     # warn on a home-session Agent naming no task id (default off)
```

### TicketInfo contract (what `info` prints)

```json
{
  "key": "ABC-110",
  "title": "Billing: retry failed invoice exports",
  "status": "In Progress",
  "assignee": "…",
  "url": "https://…/browse/ABC-110",
  "epic": { "key": "ABC-100", "title": "…", "url": "https://…/browse/ABC-100" },
  "extra_links": [ { "label": "Design doc", "url": "https://…" } ]
}
```

- Every field except `key` is optional. Unknown fields are ignored.
- Output is cached per key for `ttl`; `r` in the detail view forces a refresh. A failure keeps
  the last good output on screen, marked stale (see "Failures, staleness and logging").
- The adapter script is owned by the context. In a JIRA-backed context it is the only code that touches the
  `jira` CLI or the ticket notes directory.

### When `info` fails

An adapter that cannot produce TicketInfo says why by printing an **error object** on stdout
instead:

```json
{ "error": { "kind": "unavailable", "message": "jira: request timed out" } }
```

| `kind` | means | panel label |
|---|---|---|
| `not_found` | the source answered and has no such ticket | `ticket not found` |
| `unavailable` | transient: network, VPN, expired auth, the source timed out. Worth retrying later | `ticket source unavailable` |
| `error` | anything else, misconfiguration included | `adapter error` |

- `kind` is the only field fermata acts on. A missing or unrecognised `kind` reads as `error`.
  `message` is optional, should be one line, and is shown after the label, truncated.
- An error object wins over the exit status, so an adapter may print one and exit non-zero.
- What fermata does on its own, for adapters that never print an error object (backward
  compatible with every adapter written before this contract):
  - a non-zero exit, or stdout that is not valid JSON: `adapter error`, with the first line of
    stderr as the message;
  - the adapter outliving its `timeout`: fermata kills it and shows `adapter timed out after Ns`.
- In every case the panel still shows the key and the `url` template, or the last good
  TicketInfo marked stale when there was one. One log line records the outcome with the context,
  exit status and a stderr summary; repeats of the same failure are collapsed. The UI never
  blocks.
- `--dump` carries the result as `ticket_error: {"kind", "message"}`, with `kind` one of
  `not_found`, `unavailable`, `timeout` or `error` (`timeout_secs` too for a timeout).
- A failure is cached for less than `ttl`: `not_found` for half of it, the others for a tenth
  (at least 15 s), each doubling on repeats up to `ttl`. Failures are kept on disk too, so a
  restart does not retry every failing key at once. `r` retries at once.
- Why an error object and not exit codes: shells exit 2 on a syntax or usage error, so "exit 2 =
  not found" would turn a broken script into "ticket not found", the very misreading this
  contract exists to stop.

Conventions for adapter authors:

- **Pass the underlying tool's error text through to stderr.** fermata logs it; swallowing it
  and printing "no result" reads as "the ticket doesn't exist" when the real cause was a timeout
  or an expired login.
- **Finish within budget.** fermata exports `FERMATA_INFO_TIMEOUT` (whole seconds) to the
  adapter. Bound your own calls below it, so the adapter's reason (`unavailable: … timed out`)
  reaches the panel rather than fermata's bare kill.
- Don't assume a local layout exists: a fallback that reads files must treat a missing file as
  "no data", not as an error, and never as `not_found` unless the source of truth said so.

`examples/ticket-info.sh` is a portable example (POSIX `sh`, no GNU `timeout`) that follows all
three, using GitHub issues via `gh` as the ticket source. Copy it into a workspace and replace the
lookup.

`priorities` entries match a task when its ticket key equals `ticket`, or its ticket's epic key
equals `epic`. Editing priorities from fermata rewrites only the `priorities` block.

## Routing input

When you type an answer or an instruction for a task:

1. **The task has a live interactive session** (a ccmon session whose cwd is the task's worktree):
   send it to that pane with `tmux send-keys`, as ccmon does today.
2. **Otherwise** (background worker, or no session): send it to the **orchestrator's** pane,
   prefixed `[fermata <task-id>]`. The orchestrator relays it to the worker or acts on it, which
   keeps every approval gate in one place. The orchestrator is the ccmon session whose cwd is the
   context's workspace directory.
3. **Always** record it in the tracker, so history is complete:
   - **On a decision with an id** (the selected row in Decisions, else the task's only open
     one): `tracker.sh reply <id> <decision> --kind answer|question --by user --via fermata`.
     It is a **question** when the text ends in `?` or was typed after `Q`; a question keeps
     the decision open (and needing you) until the orchestrator responds. An answer marks it
     answered; the orchestrator resolves it once it has acted. The orchestrator payload is
     `[fermata <task> <decision> <kind>] text`, so it knows which decision and what kind.
   - **Otherwise** (plain instructions, or a legacy note decision):
     `tracker.sh note <id> "ANSWER (fermata, <ts>, <outcome>): …"`; `<outcome>` is what
     routing did (`delivered to a:1.1`, `queued for o:1.2`, `held for a:1.1`, `note only`,
     `send failed to a:1.1`). An `ANSWER` note newer than the task's
     latest `DECISION` note answers a legacy decision. Notes never touch a decision with an id,
     so free text can never hide one.

If no orchestrator session is found, fermata says so and keeps the note only. The orchestrator picks
it up on its next sweep.

Never send keys to a busy (`working`) pane unless the user confirms, except when the pane runs
Claude Code, which queues input typed while it works. Input typed into another busy program can
interleave with its own output.

Never type into a Claude Code pane that has a dialog open (AskUserQuestion, a permission prompt,
plan approval): the keystrokes answer the dialog. The answer is **held** and sent when the dialog
closes. Every routed answer keeps a record of what became of it; see "Answer delivery".

Panes are found by process, never by ccmon's recorded `tmux_target` alone. ccmon records that
address once, at SessionStart, and pane indexes shift as panes are opened and closed: a recorded
`ccmon:1.2` can come to name an unrelated shell, which once received a routed answer as a command.
See "Pane resolution" under "As built".

## Linking things to a task

- **Worktree → task:** the identity carrier at `$(git -C <wt> rev-parse --git-dir)/baton-identity`
  (`leaf=`, `kind=`, `pr=`), read-only.
- **Task → worktree, PR:** the same carriers, read as a directory store. For each member repo
  (globs expanded), fermata lists `<git common dir>/worktrees/*/`; a directory with a
  `baton-identity` file is a baton worktree of task `leaf=`, at the path its `gitdir` file names,
  on the branch its `HEAD` names (else the carrier's `branch=`). A review worktree's `pr=` is its
  PR. These are plain file reads: no `git`, no tracker. A worktree whose directory is gone is
  skipped. Where the worktree lives (`worktree_base`) does not matter, because git records every
  worktree in the repo's own git dir.
- **Task → registry entries:** `tracker.sh list-branches <id>`, only on demand: for the detail
  refresh of one task (`r` in the detail view), and for parked tasks (labelled `keep-task-open`)
  that the mine filter shows, which need `waiting_on`/`until`. Registry entries come first and a
  local worktree the registry already names is not listed twice. Other people's tasks, which the
  mine filter hides, are never fanned out over.
- **Session → task:** the session's cwd (`worktree` in its record) matched against task
  worktrees.
- **Background worker → task:** the subagent's `.meta.json` `description` names the task id
  (a convention the orchestrator follows: `"ctx-01z: review #4602"`), or the subagent ran in one
  of the task's worktrees (see "Workers" below). Transcripts live at
  `~/.claude/projects/<cwd>/<session>/subagents/agent-<id>.jsonl`; the last assistant message is
  the worker's report.
- **Session → transcript:** the record's `transcript_path`, taken from the hook payload.

## Workflow conventions this relies on

These go into the context's `guidance.md` and worker brief, not into fermata:

- A worker's Agent `description` starts with its task id.
- A worker, or the sweep, updates the context's status file for the task (for example `STATUS.md`).
- Decisions are posed with `tracker.sh decide` (question, options, recommendation) and closed
  with `tracker.sh resolve` once acted on; `reply --kind response` answers a user's question.
  The verbs keep the `needs-decision` label in step. A `DECISION (…)` note with the label is
  the legacy form and is still read; so is a `DECISION` note left as the task's last note
  without the label, marked "label missing", so a slipped label stays visible.

## Data refresh

- **Sessions:** watch the `sessions/` directories of the record store (kqueue via the `notify`
  crate), event-driven.
- **Tasks:** watch each tracker's `.beads/` directory for changes, then re-run `tracker.sh list`.
  Debounce to at most once every 2s. Never poll the tracker on a timer, except to retry a
  failed read with backoff. A failed or timed-out read keeps the last good task list on
  screen, marked stale; the list is never blanked while there was ever a success.
- **PRs:** `gh pr view --json …` per visible task, cached 60s, refreshed on focus. A failure is
  cached for less and backs off; the last good state stays on screen, marked stale.
- **Review state:** one `pr-review.sh rank` per context per refresh, over all its review tasks,
  fed from the PR cache (see "Review waiting").
- **Ticket info:** adapter `ttl`, with the same failure rules.
- Logging: warnings and errors, plus one info line per answer delivery, size-capped. Never log per redraw, and collapse
  a repeated failure into one line rather than one per key per retry.

## Tech

- Rust (stable). `ratatui` + `crossterm` (resize and mouse), `tokio` for subprocess fan-out,
  `notify` for file watching, `serde`/`serde_json`/`serde_yaml`, `open` for the browser.
- All external access is through existing CLIs (`tracker.sh`, `gh`, `tmux`, adapter commands). No
  direct API clients or tokens.
- Every external command runs under one deadline that covers its output as well as its exit. It
  gets its own process group; at the deadline fermata kills the whole group and stops waiting on
  its pipes, so a forked descendant still holding stdout (say, one stuck on a lock) cannot keep a
  source waiting past its timeout. A descendant that has released the pipes is left alone.

## Milestones

0. **Spike.**
   - Confirm the subagent transcript layout and the `description` → task convention (layout
     confirmed 2026-11-12).
   - Test routing input to a pane against a scratch tmux session, never a live one.
   - Install the Rust toolchain and scaffold the crate.
1. **Read-only core.** Contexts, tasks, links, detail panel, open-in-browser, priority sort, the
   first per-context adapter (`fermata.yaml` + `ticket-info.sh`).
2. **Decisions and input.** Decision view, answering, input bar, routing, notes.
3. **History.** Timeline and transcript viewer.
4. **ccmon parity.** Sessions view with ccmon's actions. Fix ccmon's notification hook (reads nested
   keys; Claude Code sends top-level `message` / `notification_type`) and record
   `transcript_path`. Then retire `dashboard.py`. (Done by porting the hooks into fermata; see
   "Session records and hooks". Retiring the dashboard is separate work.)

## As built (2026-11-12)

Refinements made while building M1–M4. These are the current behaviour.

### Modes and keys

- `fermata` runs the TUI; `--dry-run` logs input instead of sending keys or writing notes, and
  turns `w`, session jump and every session action into logged no-ops (and skips the sweep) (read-only probes such as
  `ps` and `tmux has-session` still run so the preview is accurate). `--context <name>` starts in
  one-context mode on that context. `--dump [--task <id>]` prints the joined task list, or one
  task plus its timeline, as JSON; `--dump-sessions` prints the live session records.
  `fermata hook <event>` is the Claude Code hook entry point (see "Session records and hooks").
  `fermata goto` jumps to the running fermata (see "Jumping to fermata"); `fermata cleanup
  [--dry-run]` and `fermata snapshot save|restore|list` run the sweep and the snapshot actions
  once (see "Dead sessions" and "Session actions").
  `--version`/`-V` prints `fermata <version> (<commit>, built <time>)` and exits (see "Build identity").
  `--doctor` loads once, prints the health report (see "Health") and exits 1 when any source
  is degraded.
- Global: `t` tasks, `d` decisions, `s` sessions, `H` health (all four from any view but the viewer), `?` help, `R` full reload (re-resolves contexts,
  bypasses caches), `Esc`/`q` back, `q` on a top view quits, `m` toggles mouse capture. Mouse
  capture is **off** by default so terminal text selection works; the footer shows its state.
- Tasks, Decisions and Sessions: `/` filters the list (see "Filtering").
- Tasks, Detail, Decisions and Sessions: `M` mode, `C` context picker, `[`/`]` previous/next
  context (see "Information architecture").
- Tasks and Detail: `u` mine ↔ everyone (see "Mine or everyone").
- Tasks and detail: `o` `e` `p` `c` `w` as above, `:`/`i` input bar, `*` pin/unpin the ticket,
  `+` pin/unpin the epic, `r` refresh (tasks: full reload; detail: `tracker get` + re-join, no
  caches).
- Wide detail (140+ columns): `j`/`k`/`g`/`G` move the list column and the detail follows;
  `Tab` or Enter focuses the timeline, `Tab` the list again. See "Layout and colour".
- Decisions and detail: `1`–`9` **pre-fill** `Option N: <text>` into the input bar instead of
  sending immediately, so a stray key never answers a decision; `a` free-form; `Q` the same,
  but recorded as a question.
- Sessions: `Enter` jump (`tmux switch-client`), `r` quick reply to that pane, `l` session log,
  `v` session transcript, `o` the session's task, `e` describe, `x` close, `X` toggle the closed
  list (where `Enter` reopens and `e` edits the record's description), `S` save a snapshot, `L`
  restore the latest snapshot. Close, reopen and restore always ask first (see "Session actions").
  `0`–`9` and `M-a`…`M-z` act on the row showing that index key, as `Enter` would. Digits mean
  this only in Sessions; Decisions and detail keep them for options. `M-` needs the terminal to
  send Option/Alt as Meta (iTerm2: Profiles → Keys → "Esc+"; Terminal.app: "Use Option as Meta
  key"); otherwise Option+letter types a character and the key does nothing. In Sessions a Meta
  letter is never a global key, so `M-d`, `M-s`, `M-t` pick rows rather than switching views.
- Viewer: `j/k`, `Space`/`b` page, `g`/`G`, `Enter` expand/collapse a tool call or result, `/`
  search (matches inside collapsed tool bodies land on the tool line), `n`/`N`.

### Filtering

- Each of the three lists keeps its own filter, so switching views keeps them; the closed and
  live session lists share the Sessions one.
- A row matches when every space-separated word of the filter appears in it, ignoring case.
  The text searched is what the row shows: tasks match on id, state, title (with its waiting
  text), ticket key and context (so in all-contexts mode a context's name narrows to its block); decisions on task id, title and decision text; sessions on status, recorded tmux
  address, worktree, branch, task id and description.
- While typing, every key goes to the prompt (so `t`, `q` and the rest type themselves).
  Backspace deletes, Ctrl-U clears, Up/Down move the cursor in the narrowed list. `/` on a
  locked filter reopens the prompt with the filter in it, to refine it.
- Locking an empty filter is the same as clearing it.
- The cursor stays on its item while the filter narrows, if the item still matches; otherwise
  it goes to the first row. Selection-by-item (above) applies to the narrowed list.
- The list's title shows the scope, `shown/total` and the filter, so neither a locked filter
  nor one-context mode is ever invisible. Header counts (decisions, sessions, "need you") are
  the scope's totals, never narrowed by a filter.

### Data

- **Locating baton** follows baton's own rule: `$CLAUDE_PLUGIN_ROOT` → `~/code/maestro/baton` →
  `installed_plugins.json` (user scope first) → newest `cache/maestro/baton/*` by version.
- **Context JSON** comes from `resolve-context.sh` with `BATON_CONTEXT=<name>`. It is cached in
  memory until `R` and on disk keyed by the baton path and the mtimes of context.yaml and the
  registry. If resolving fails, the context.yaml converted to JSON is used.
- **Notes** arrive as one text blob. An entry starts at a line shaped like `WORD (…):` or
  `WORD:`; other lines continue the previous entry. Stamps are usually date-only; in the timeline
  a date-only note sorts at the end of its day, capped at the task's `updated_at`. A hand-typed
  time-only stamp (`DECISION (14:02Z): …`) is kept but its date is unknown: the time column shows
  `?? 14:02Z` and the note sorts with the dated note before it in the blob (as the oldest when
  none is). Nothing guesses the date. New writes are clock-stamped by whoever writes them.
- **Decision options.** The orchestrator's format is parsed first:
  `DECISION (<ts>): <question> | 1) <option> | 2) <option> | … | Rec: <n or text>`. A numeric
  `Rec` is shown as `n) <option>`. Otherwise (the fallback): numbered or bulleted lines after the
  first, or inline `(1) … (2) …` markers, with a trailing `Rec:`/`Recommendation:`. A `needs-decision` task with
  no decision at all still shows a placeholder decision, so the label is never invisible.
- **Seam decisions.** For each task labelled `needs-decision`, `tracker.sh decisions <id>` is
  read (in parallel); resolved ones are dropped. A failed read keeps the last good list; a
  backend without the verb falls back to notes alone, with no health failure.
- **Legacy decisions.** Of a task's `DECISION` and `ANSWER` notes, only the latest counts. If it
  is an `ANSWER` (from fermata or anyone else), the legacy decision is answered and no longer
  waiting. Notes are never matched against decisions with ids.
- **Unlabelled decisions (legacy).** A task without `needs-decision` whose *last* note of all is a
  `DECISION` shows as a decision too (needs you, in the Decisions view), headed
  "DECISION (label missing: no needs-decision)". Any later note, `ANSWER` or not, takes it out:
  without the label there is nothing else to say it is still open. Decisions posed with
  `tracker.sh decide` carry the label by construction, so this applies only to legacy notes.
- **PR:** the newest entry with `pr_url`/`pr` wins (a review carrier's `pr=` takes the task's own
  PR URL when that URL names the same number, so it shares the PR cache entry). With none
  recorded, an `open` branch (every local worktree is one) is looked up with
  `gh pr view <branch> --repo <owner>/<repo>`; "no pull requests" is silent. `<repo>` is the member
  repo's directory name. A task with no local worktree and no registry read has no PR link.
- **Checks.** Only the latest run per check counts (CheckRuns keyed by workflow + name, status
  contexts by context; latest by `startedAt`). Runs superseded on the same head, such as
  concurrency-cancelled duplicates, are ignored. A check is failing when its latest run concluded
  `FAILURE`, `TIMED_OUT` or `ACTION_REQUIRED`; a latest run that was itself `CANCELLED` is not
  failing. `c` opens the first failing check, else the most recent one.
- **Which tasks load.** One `tracker.sh list --status active` per context: every non-closed
  task, blocked and deferred included. A backend that refuses `active` (an older baton) is asked
  for `in_progress` instead, and fermata logs that it fell back. Of the active tasks the list
  shows those that are `in_progress`, have a declared state, are labelled `needs-decision`, or
  carry the context adapter's ticket label. The rest, stateless backlog, are not rows: each
  context counts them as **backlog** (`· backlog N` on its heading, and in the header for the
  scope), so a blocked task is visible once something declared it, and nothing hides silently.
- **Declared state** (the task's `state`, written by `tracker.sh state`) is what a task says it
  is doing; the badges below are what fermata sees. The row shows both: the badge column holds
  the first match of the ladder, and when the task declared a state the title cell ends with it
  (`— waiting-on-review PR #12`, `— blocked on Security sign-off until 2026-12-01`,
  `— paused: user stopped the CI wait`). The detail view has a `state` line with who set it and
  when. A declared value fermata cannot read is shown as `state ?`, never as no state.
- **Badges** (first match): needs you (an open decision, your pending question
  included, though not an answered one; the live session is `needs_input` /
  `needs_permission`; or declared `waiting-on-me`) → **review requested** (see
  "Review waiting") → **review ?** (its state could not be read) → **overdue** (kept open and
  past its registry `until` date, see "Parked tasks"; or a declared `blocked` / `waiting-until`
  whose `until` has passed) → **CI failing** (open PR with a failing check) → **state stale**
  (declared `waiting-on-review`, but its PR has merged, closed or been approved) → **stalled**
  (declared `working`, but no unfinished worker, no live session, and no change for 4 h) → ready
  to merge (open, not draft, APPROVED, no failing or pending checks) → waiting on CI (open with
  pending checks) → worker running (a matched subagent without `finished`) → interactive (a live
  session) → the declared state itself: working, in review, blocked, waiting until, paused,
  done, cleanup → **parked** (`ready-for-worktree-delete` or `keep-task-open`) → **state unknown**
  (nothing above, and no state declared, or one fermata cannot read).
- **Never silently idle.** There is no "idle" badge. A shown task that declared nothing and has
  no world fact is **state unknown**, and the header counts them (`N state unknown`) for the
  scope. `paused` never ages into stalled: saying it is the point.
- **Change time for stalled.** "No change" uses the task's last change (tracker update,
  registry entries, session activity, worker transcripts) and the declared state's `at`,
  whichever is newest; with no readable time at all, a `working` task with nobody on it is
  stalled.
- **Parked tasks.** A task is kept open when it carries `keep-task-open` or a registry entry says
  `keep_task_open: yes` (`baton:finish` writes both together, so the label is what decides that
  the registry is read). The newest registry entry that recorded `waiting_on` or `until` (written
  by `baton:finish`) says what it waits on. Parked and overdue rows show "waiting on X until D"
  after the title, and the detail view adds a `parked` line ("no reason recorded" when there is
  none). Overdue means `until` is before today's local date; due today is not overdue, and a date
  that is not `YYYY-MM-DD` never is.
- **Review waiting.** A task labelled `pr-review` (a review of somebody else's PR, from
  `baton:start --review`) is asked about through baton's `pr-review.sh rank`, the one place the
  rule lives: fermata pipes the tasks to it and reads back `review_pending` (`yes`, `unknown`,
  `no`) and `review_why`. It is waiting while the leaf and the PR are open and you are in the
  PR's review requests or have submitted no review; once you submit, the PR closes or merges, or
  the leaf closes, it drops back to normal ordering. The row shows the reason after the title.
  - **One call, no GitHub of its own.** Each refresh makes one `rank` call per context, holding
    every review task, run beside that context's joins. fermata reads each review PR (the first
    PR URL in the task's description, then notes, the same rule as `rank`) through its PR cache,
    whose `gh` fields include `reviewRequests` and `latestReviews`, and passes them as
    `--pr-data`. A PR URL is one cache entry whatever repo slug the caller has, so the review
    lookup and the task's own PR line share it. The GitHub login comes from `gh api user` once
    per process and is passed as `--login`; a failed lookup is retried after 60 s, and it is not
    asked at all when no PR could be read. With a warm PR cache a refresh starts one `rank`
    process and makes no GitHub calls.
  - **Unknown is its own state.** A PR that could not be read (its error goes in `--pr-data` as
    `{"error": …}`), or a `rank` call that failed or timed out, makes the task **review ?**,
    never "not waiting". Only the affected tasks are unknown: one deleted PR leaves the others
    ranked. An unknown review sorts in the review group, just below the confirmed ones, because it
    may well be waiting; the row reads "review status unknown: <why>". The first unknown answer
    logs one line; repeats of the same reason are collapsed (see "Failures, staleness and
    logging").
  - **Caching.** Answers are cached 60 s per task (keyed by id and status, so a closed leaf is
    asked again at once); an unknown answer is cached 20 s, doubling on repeats up to 60 s, so a
    failure is not re-fired on the very next refresh. A task that had a known answer keeps it
    when a later lookup fails, marked stale ("as of 14:02, refresh failed: …"); it is unknown only
    when there was never an answer. A verdict ranked from stale PR data is marked stale too.
    Concurrent refreshes of the same review tasks wait for the rank already running. All of it is
    skipped when PR fetching is off. `r` on a review task re-ranks it alone, bypassing the
    caches.
- **Sort.** The groups (review waiting or unknown, needs-you pinned, needs-you, overdue, pinned, rest) come
  first. A waiting review is on top because somebody else is blocked on it, and it was an explicit
  instruction. Overdue sits just below needs-you because a missed look-again date is the next most
  urgent thing. Within a group the order is badge tier (needs you > review requested > review ? > overdue >
  CI failing > state stale > stalled > ready to merge > waiting on CI > working, meaning worker
  running, interactive or declared working > state unknown > in review > blocked > waiting
  until > paused > done, cleanup and parked), then pin order, then most recent change.
- **Workers:** description `<task-id>:` is the convention, but a worker links when its description
  names the task id (or the task's ticket key, which workers launched before the convention
  used) anywhere as a whole word: `[XY] ctx-1v.2.8 resume and ship` links to `ctx-1v.2.8`, not to
  `ctx-1v.2` or `ctx-1v`. Not whole: preceded by a letter, digit, `-`, `_` or `.`; followed by
  one of those, or by `.` and more id (a sentence-ending `.` is fine). A worker also links,
  whatever its description, when its transcript's `cwd` is one of the task's worktrees or below
  it. Only the last 8 KB of the transcript is read for `cwd` (every entry records it, after its
  message), cached per file by length, so a finished transcript is read once per process. The worker's report is its final `SubagentHandback` message (that
  is how background agents deliver it), else the last assistant text; only the last 1 MB of the
  transcript is read.
- **Session transcripts:** the record's `transcript_path` when the file exists. Records written
  before it was recorded fall back to
  `~/.claude/projects/<cwd with non-alphanumerics as '-'>/<session_id>.jsonl`.
- **Transcript viewer** streams line by line, reads at most the last 32 MB of a file, caps each
  text block at 20 KB and keeps at most 20,000 entries.

### Refresh

- **Tracker write signature.** Measured on a real beads tracker with read-only verbs
  (`list`, `get`, `list-branches`): reads bump mtimes on the Dolt `manifest`, the journal file
  and `journal.idx`, write `temptf/` metrics, and `get` rewrites `last-touched`. No file size
  changes, and no small file's content changes except `last-touched`. A Dolt write appends to
  the journal and moves the root hash in `manifest`. That second half is inferred from Dolt's
  design, because writes could not be tested here. The signature is a hash of every file's size
  plus the content of files of 4 KB or less, skipping `temptf/`, `LOCK`, `*.lock` and
  `last-touched`.
- A tracker watch event only marks "check". At most every 500 ms fermata recomputes the
  signatures, and it reloads (debounced to one per 2s) only when one differs from the signature
  taken at the last tracker read. Reads by fermata or the orchestrator no longer cause reloads.
- The header shows `refreshed <age> ago`.
- The hooks rewrite a session file on every event, so a sessions-dir event only re-reads
  `sessions/*.json`, re-matches sessions to tasks and re-badges. It never calls the tracker.

### Caching and load time

- An on-disk cache lives at `~/Library/Caches/fermata/cache/` (or `$FERMATA_CACHE_DIR`):
  - tracker state (the active tasks), keyed by the tracker write signature;
  - the registry entries of parked tasks you see, keyed by the tracker write signature and who
    you are;
  - resolved contexts;
  - ticket info, honouring the adapter `ttl` (`ticket2-*` entries, keyed by workspace and key);
  - PR state, 60s, including the review fields `rank` needs (`pr3-*` entries; older `pr-*` and
    `pr2-*` ones are ignored);
  - who you are per context (`whoami`), 1 hour, an unknown answer included so a backend
    without the verb is not asked on every load. It is asked beside the tracker read, and kept
    when that read fails.
  `R`, and `r` on the Tasks view, bypass it.
- Contexts resolve in parallel. Within a context, a refresh costs one `list` plus one
  `list-branches` per parked task you see, never one per task: on an embedded-Dolt tracker each
  call takes 1.5–2s and holds an exclusive lock, so per-task calls serialize (82 in-progress
  tasks once cost about 160s against a 20s timeout). The worktree scan is local file reads.
  Each task's adapter `info` and `gh` calls run side by side, with the context's one review
  `rank` call beside them. On a signature miss the previous
  tracker state is joined in the background while the tracker is read, which warms the ticket
  and PR caches.
- `--dump` on the current data (6 tasks, 2026-11-12):
  - everything cached: 0.02s
  - PR cache expired: 1.5–2.4s
  - tracker changed (with ticket and PR caches expired too): 3.2–3.6s
  - no cache at all: about 6.7s

  Those numbers predate the directory-store join; the floor for a tracker change is now `list`
  (about 1s) plus any parked tasks' `list-branches`.
- **First paint.** On start the TUI shows a cache-only view at once, while the real load runs
  beside it: each context's last good task list, registry entries and who you are from disk,
  joined with whatever ticket, PR and review data is cached at any age. It makes no tracker,
  adapter or `gh` call, so a slow or hung tracker never leaves the screen blank. The header
  reads `loading…` until the real load lands and replaces it; a real snapshot is never
  replaced by a preview. Contexts with nothing cached are simply absent from the preview.
  After that, every refresh already runs in the background with the previous view on screen.
- **Stale task list.** When a context's `tracker list` fails or times out, fermata serves that
  context's last good task list (held in memory, else the disk cache at any age) and marks it
  stale: `tracker_stale: {<context>: {as_of, error}}` in `--dump`, `⚠ <ctx> tasks as of 14:02`
  in the header (`⚠ N contexts stale` when several are), and a `tasks` line in the detail view
  ("as of 14:02, refresh failed: …"). The failure is recorded in the Health view as usual. With
  no previous success the context errors as before. A stale context is retried on a timer, not
  on the next signature check: 15 s after the first failure, doubling on each consecutive one up
  to 5 minutes. A tracker write (a moved signature) or `R` retries at once; only a success resets
  the backoff. `--doctor` prints a `stale:` line per such context.
- **Registry entries** (`list-branches`) keep a last good copy per task, in memory and on disk
  (`registry-<ctx>`). A failed read uses the task's previous entries rather than none, so its
  `pr_url`, `waiting_on` and `until` survive a timeout; a task never read before gets none.
  Reads for parked tasks run at most four at a time, since each call holds the tracker's
  exclusive lock and unbounded fan-out only queues them into the timeout.

### Build identity

- `build.rs` stamps the binary with the git commit it was built from (12 hex digits, plus
  `-dirty` when `fermata/` has uncommitted changes) and the build time in UTC. Outside a git
  checkout the commit reads `unknown`; `FERMATA_BUILD_COMMIT` overrides it, and
  `SOURCE_DATE_EPOCH` fixes the time for reproducible builds.
- `--version`, the Help title and the Health view all show it, so which source a running
  build came from, and how old it is, can be read off the screen.
- The build script re-runs when `HEAD`, the checked-out branch's ref, `src/` or `Cargo.toml`
  changes, so the stamp tracks the source rather than the last `cargo clean`.

### Health

Every place fermata calls something external reports the outcome to one registry
(`health.rs`), keyed by scope (a context name, or global), source and key:

| Source | Scope | Keys | Recorded at |
|---|---|---|---|
| tracker | context | `list`, `list-branches`, `whoami`, `resolve-context` | each live read (`list-branches` only when a parked task was read); a signature-cache hit, or a stale task list served after a failed `list`, records the served data's save time |
| ticket adapter | context | the ticket key | each `info` run (not cache hits, which `fetch.rs` decides) |
| review rank | context | `rank` | each `pr-review.sh rank` call |
| gh | global | the PR cache key | each `gh pr view` |
| session records | global | `sessions` | each load: is the store's `sessions/` readable (fermata's, else ccmon's; see "Session records and hooks") |
| tmux | global | `list-panes` | each load in the TUI and `--doctor`; never in tests |

- Each source keeps its last success, its last failure (time, kind, message, key), the keys
  whose latest call failed, and the time the data now served was fetched. **Degraded** means at
  least one key's latest call failed, which is also when those keys are served stale (see
  "Failures, staleness and logging").
- Kinds: a ticket failure keeps the adapter contract's kind (`unavailable`, `timeout`,
  `error`). Elsewhere the message decides: `timeout`, `auth`, `network`, else `error`. A ticket
  `not_found`, and `gh` saying there is no such PR, are answers, so they count as successes
  (the task's own row still shows them).
- A failing ticket, gh or review key that is not asked again for an hour is dropped: those
  sources are re-asked on a TTL while the task is visible, so silence means the task went
  away. Tracker, session-record and tmux keys are only re-asked on a load and are never dropped.
- Rows exist from the first load for every source a context uses (the ticket adapter only when
  it has an `info` command; review rank once a context has review tasks), reading `not asked
  yet` until called, or `cached` while everything shown came from the disk cache.
- **TUI.** `H` opens the Health view: the build, the baton, session-record, ccmon (older records), cache and log paths, then
  one row per source with state, last success, data age and last error. While anything is
  degraded the header shows `⚠ N sources degraded · H`.
- **Headless.** `--doctor` prints the same report as text and exits 1 when a source is
  degraded. `--dump` carries it as `health`.
- The health view reads; it never retries. `R` re-asks everything.

### Failures, staleness and logging

The ticket, PR and review caches share one set of rules, implemented once in `fetch.rs`
(`KeyedCache`) and followed by the batched review cache. The tracker's task list and registry
entries follow the stale-on-error rule too (see "Caching and load time"):

- **Single-flight per key.** A request for a key whose fetch is already running waits for that
  fetch and shares its result, so two joins (the background warm-up and the real one) never run
  the adapter or `gh` twice for the same key. A forced refresh joins a running fetch too.
- **Stale on error.** Each entry keeps the last good value and the last failure separately. When
  the latest outcome is a failure and there was ever a success, the last good value is served,
  however old, with `*_stale: {as_of, error}` beside it (`ticket_stale`, `pr_stale`,
  `review_stale` in `--dump`). The detail view shows "as of 14:02, refresh failed: …"; the row
  keeps its badge and text. Nothing is shown only when there was never a success.
- **Negative caching.** A failure is cached for less than a success, by class: `not_found` (the
  source answered: an adapter `not_found`, or `gh` reporting no such PR) for half the success
  TTL; anything transient (unavailable, timeout, error) for a tenth of it, at least 15 s. Each
  consecutive failure doubles that, up to the success TTL. A success resets it. Ticket: 150 s
  and 30 s with the default `ttl: 300`; PR: 30 s and 15 s.
- **On disk.** The whole entry (last good value, last failure, its streak) is saved after every
  fetch and read back at any age on a cold start, so a restart serves the last good values
  stale and does not retry every failing key at once.
- **Logging.** A failure logs one line: source, context, key, the error, and (for an adapter)
  the exit status and a stderr summary. The same failure (same source, context, kind and
  message, with the key masked) on any key within 15 minutes is counted rather than logged.
  The next one after that window logs a summary, `ticket info [ctx]: ticket source unavailable
  — ×58 across 30 keys since 14:02`. When every key in it has succeeded again it logs one
  recovery line, only if anything was suppressed. A ten-minute outage over 30 tickets therefore
  writes a handful of lines, not one per key per retry.

### Session records and hooks

fermata owns the session record: its format, its directory and the hooks that write it. They
replace ccmon's Python hooks, whose records fermata used to read.

- **Store.** `store.rs` is the one place that knows the layout. The root is `$FERMATA_DATA_DIR`,
  default `fermata` under the OS data directory (`~/Library/Application Support/fermata` on
  macOS), holding `sessions/<id>.json`, `closed/<id>.json` and `logs/<id>.log`. Everything that
  reads or writes a record (the hooks, the Sessions view, describe and close, the timeline, the
  log viewer, the watcher) goes through it.
- **ccmon's records during the switch.** ccmon's directory (`$FERMATA_CCMON_DIR`) has the same
  layout and is read as a fallback, never written: a record found only there is moved into the
  root by its next write, and a root record shadows a ccmon one with the same id. Pointing
  `FERMATA_DATA_DIR` at ccmon's directory makes them one store, which keeps ccmon's own tools
  (dashboard, cleanup, snapshots) seeing new records until they are retired; fermata now does
  all three jobs (see "Dead sessions", "Session actions" and "Jumping to fermata").
- **Writes.** Each write is atomic (temp file plus rename), so a reader never sees half a record.
  Read-modify-write takes an exclusive lock on `<root>/.lock`, a separate file because rename
  would orphan a lock held on the record itself. A record cut short by an older writer is read
  up to the end of its first JSON object.
- **Hooks.** `fermata hook <event>` reads the hook JSON on stdin, needs a `session_id` made of
  letters, digits and `-` (it names files), and always exits 0; errors go to fermata's log. An
  event other than session-start only updates an existing record, never creates one.

  | Event | Effect |
  |---|---|
  | `session-start` | Retire the seat's older record (below); create or refresh this one: `claude_pid`, tmux `session`/`window`/`pane`/`target`, `tmux_pane_id` (`$TMUX_PANE`), `worktree` (cwd), `branch`, `transcript_path`, status `idle`, `prompt_open` cleared. A new record lifts `description` off a `closed/` record for the same id and deletes it. |
  | `user-prompt-submit` | `working`; `prompt_open` cleared; `last_prompt` = first 120 characters on one line; log `PROMPT:`; mark matching answer deliveries received and log `RECEIVED: delivery <ids>` (see "Answer delivery"). Claude Code fires this for messages it injects too; one that starts (after whitespace) with `<agent-message`, `<task-notification` or `[SYSTEM NOTIFICATION` still sets `working` but keeps the last human `last_prompt` and logs `AGENT_MESSAGE:` instead. |
  | `pre-tool-use` | `working`, `last_tool`, `tool_calls` + 1; `prompt_open` = the tool for `AskUserQuestion`/`ExitPlanMode`, else cleared. On an `Agent` (or `Task`) call, the unlinked-worker check below. |
  | `notification` | `notification_type` (top level; ccmon's nested `notification.type` also accepted), compared without `_` and case: `permission_prompt` → `needs_permission`; `idle_prompt`, `elicitation_dialog` → `needs_input`; `auth_success` → `working`. `permission_prompt` and `elicitation_dialog` also set `prompt_open` (`permission`, `elicitation`); other types leave it alone, since `idle_prompt` can fire while a dialog is up. Another type whose message asks a question ("which option", "should I", …) → `needs_input`; anything else is only logged, as `(ignored)`. |
  | `stop` | `idle`, "Waiting for input", `prompt_open` cleared; log `STOP`. The `idle_prompt` notification overwrites the status about a minute later, so the log line is what shows Stop fired. |
  | `subagent-stop` | Only `last_activity`; log `SUBAGENT_STOP`, with `type=` and `id=` when the payload has `agent_type` / `agent_id`. ccmon marked the session idle here, which wrongly cleared `needs_input` or `working` when a background agent finished. |
  | `session-end` | Remove the live record. A named one (non-empty `description`) is archived to `closed/` first, unless the reason is `clear`. ccmon had this hook but never registered it. |

  Every update stamps `last_activity` and, when the payload has one, `transcript_path`.
- **Unlinked-worker check** (opt-in: `workers.warn_unlinked: true` in the context's
  `fermata.yaml`). A safety net for the `<task-id>:` convention; it never blocks the call. It
  fires when the hook's cwd is not in a linked git worktree (so a home session or a primary
  checkout; a worker in a task worktree links by cwd anyway), the cwd's context opted in, and the
  Agent's `description` names none of the ids in that context's last cached task list
  (`tracker-<context>` in the disk cache, so the hook calls no tracker). With no cached list it
  stays silent rather than guess. It prints `systemMessage` (for the user) and
  `hookSpecificOutput.additionalContext` (for the model) asking for the id, and logs
  `AGENT_UNLINKED: <description>` to the session log so compliance shows in the timeline.
- **Seats.** At session start an older live record in the same seat is retired: the same Claude
  pid means `/clear` or a fork, so it is deleted; the same tmux pane with another pid that is no
  longer running means a restart, so it is marked `dead`, "Replaced by new session …". The pane
  only counts when there is one, or every session outside tmux would match every other. If the
  old pid is still running (`kill(pid, 0)`), the new session is a nested `claude` started from
  the old one's shell (`claude -p` from a tool call, a script, an eval run), which inherits
  `TMUX_PANE`; both records stay live. ccmon retired these and marked the running parent dead.
- **`claude_pid`** is the hook's parent, skipping up to three `sh`/`bash`/`zsh`/`dash` parents in
  case a shell did not exec. Verified against a live headless session: it names `claude`.
- **Plugin.** `fermata/` is also a Claude Code plugin in the maestro marketplace:
  `.claude-plugin/plugin.json` and `hooks/hooks.json`, which registers the seven events above
  (10s timeout) through `hooks/run.sh <event>`. The wrapper runs `$FERMATA_BIN`, else `fermata`
  on `PATH`, else `~/.cargo/bin/fermata`; with none it drains stdin and exits 0, so the plugin
  without the binary is a no-op. A binary older than `fermata hook` exits 1 with usage, a
  non-blocking hook error, so install the binary first. baton's own hooks stay in baton.
- **Cost.** About 30 ms per event through the wrapper, against about 135 ms for ccmon's Python
  hooks (50 `pre-tool-use` calls each, 2026-11-20).
- **Dead sessions.** A session that crashes, is killed or loses its pane never reaches
  session-end, so its live record would stay forever. The **sweep** (`Actions::sweep`) is the
  backstop: a live record whose `claude_pid` no longer exists, or now names something other than
  `claude`/`node`, is ended the way session-end ends it (`Store::end`: a named record is archived
  to `closed/`, the live one removed), re-checked under the store lock so a hook that just
  rewrote the record wins. It logs `SWEPT: claude pid N is gone`; logs are kept. A record with no
  `claude_pid` is left alone, and a record that does not parse is removed only once it is an
  hour old. The TUI sweeps every minute (not in `--dry-run`); `fermata cleanup [--dry-run]` runs
  it once. Unlike ccmon's `cleanup.py` it does not end sessions for being idle or for their
  recorded pane being gone: a live claude is never swept.

### Session actions

- Records go through the store (see "Session records and hooks").
- **Describe** sets `description` in `sessions/<id>.json` (or `closed/<id>.json`), keeping every
  other field, under the store lock like the hooks.
- **Close** (confirm: `y` kills claude and the pane, `k` kills claude only) follows ccmon:
  1. Write `closed/<id>.json` with `{session_id, description, worktree, branch, tmux_session,
     closed_at}`.
  2. SIGTERM the `claude_pid`, wait up to 2s, then SIGKILL. The pid is only signalled if `ps`
     names it `claude` or `node`, which guards against pid reuse.
  3. Optionally `tmux kill-pane` on the pane resolved by PID (see "Pane resolution"). The pane is
     resolved before step 2, while claude still runs in it. Without a PID match it is left open
     and the result says why: close never kills a pane by its recorded address.
  4. Delete the live session file.
- **Relaunch** is the one way a session is started again, shared by reopen and restore. It runs
  `$SHELL -lc 'claude --resume <id>; exec $SHELL -l'` in the record's worktree:
  1. in a new window of the record's tmux session if that session exists;
  2. else, if `FERMATA_RELAUNCH` is set, that command line through `sh -c`, with `{session}`,
     `{worktree}` and `{session_id}` replaced by the values, each single-quoted (for a layout
     tool such as tmuxinator; the command must start the session itself);
  3. else in a new detached tmux session named after the record.
  Unlike ccmon this does not pass `--dangerously-skip-permissions`, which many setups disallow.
- **Reopen** (confirm) relaunches a closed record, named `resume-<id prefix>` when it has no
  tmux session. The session-start hook removes the closed record.
- **Snapshots.** `S` (or `fermata snapshot save`) writes `snapshots/<UTC time>.json` and a copy
  as `snapshots/latest.json` under the store root: one entry per live session whose claude still
  runs and that has a worktree and a tmux session (`tmux_session`, `worktree`, `branch`,
  `session_id`, `status`, `last_activity`; ccmon's field names). `L` (confirm, or `fermata
  snapshot restore`) relaunches each entry of the latest snapshot (the root's, else
  `<ccmon dir>/snapshots/latest.json`) unless its session is still live, its worktree is gone,
  or its tmux session already existed; a second entry for a session this restore created gets
  a window in it. The result names every skip. `fermata snapshot list` prints the latest. ccmon's
  `pause` and its unsaved-nvim-buffer check are not carried over.
- Every live effect goes through a `System` trait. `RealSystem` does the work; `FakeSystem`
  backs the unit tests, which run against scratch copies of session JSON; `DryRunSystem` is the
  `--dry-run` path.
- **Checked live** (2026-11-21, against a scratch claude session in its own tmux session):
  jump, quick reply, describe, the log and transcript viewers, close with `k` and with `y`, the
  closed list and reopen, snapshot save and restore (restore relaunched the one missing session
  and skipped the five running ones), the sweep on a claude killed with SIGKILL, and `fermata
  goto` both finding a running fermata and starting one. That check found and fixed the closed
  list showing (and filtering on) an empty tmux column, as closed records keep only
  `tmux_session`. Known gaps: the log viewer labels the whole log as one assistant entry, and an
  empty description cannot be set to clear one.

### Jumping to fermata

`fermata goto`, meant for a tmux key binding (`bind-key C-g run-shell 'fermata goto'`), jumps to
the running fermata: the pane whose `pane_current_command` is `fermata`, preferring one in the
tmux session `$FERMATA_SESSION` (default `fermata`). With none running it starts fermata in a new
window of that session, or a new detached session of that name, by `send-keys` so the shell
survives fermata exiting, and jumps there. Inside tmux it switches the client; outside it
attaches.

### Answer delivery

Routing used to be fire-and-forget: an answer was noted as sent whether or not the session ever
saw it. Answers were lost to dialogs, glued together, or mangled, and nothing showed it.

- **Record.** Each routed answer writes `<root>/deliveries/<id>.json` (`delivery.rs`): task,
  text, the exact payload typed, the route, the target session id and pane, a **state** and its
  timestamps. Records older than 14 days are pruned by the TUI's sweep. `--dry-run` writes none.

  | State | Means | Detail shows |
  |---|---|---|
  | `delivered` | typed into an idle pane | `sent to a:1.1`, then `received 14:02:11 (hook)` |
  | `queued` | typed into a `working` Claude pane, which queues it | `queued: a:1.1 is busy` |
  | `held` | the pane had a dialog open; nothing typed | `held: a:1.1 is at a permission prompt; sent when it closes` |
  | `note_only` | no session, no orchestrator, or the pane could not be found | `note only: <reason>` |
  | `failed` | `tmux send-keys` failed | `send failed: …` (red) |

- **Receipt.** The `user-prompt-submit` hook marks every pending delivery for its session (or
  its tmux pane, should the session id have changed) whose payload, whitespace squashed, appears
  in the submitted prompt. That catches two answers merged into one prompt and re-wrapped text;
  a mangled prefix does not match, so it is reported. As a cross-check for prompts the hook
  missed, the TUI looks for the payload in the session's transcript (a user, attachment or
  queued-input entry no older than the send) for up to an hour after sending.
- **Not yet received.** A sent answer with no receipt after 30 s turns red, unless its session is
  still `working` (Claude Code submits queued input when it next stops), which extends the wait to
  10 minutes: `sent 14:02:11, not yet
  received`. The status line announces it once, and `A` in the task's detail **resends** it: the
  same text through a fresh route, recorded as a new delivery with `resend_of`, the old one
  marked `superseded_by`, and no second tracker note. A resent decision reply keeps its
  decision and kind (both kept on the delivery record) and is not replied a second time.
- **Holding.** The hooks keep `prompt_open` on the session record: `pre-tool-use` sets it for
  `AskUserQuestion` and `ExitPlanMode`; `notification` sets it for `permission_prompt`
  (`permission`) and `elicitation_dialog` (`elicitation`); `pre-tool-use` of any other tool,
  `user-prompt-submit`, `stop` and `session-start` clear it. Routing re-reads the record at send
  time (the TUI's copy can lag a dialog opening) and holds rather than types. Whenever sessions
  change, and every 5 s, the TUI releases held answers whose session has left the dialog: under
  the store lock, so two fermata instances never both send one. A held answer whose session ends
  becomes `note only`. Known gap: after an approved permission `prompt_open` stays set until the
  session's next tool call or stop, so the answer waits until then; `PostToolUse` is not hooked,
  to keep the per-tool-call hook cost at one event.
- **Logging.** Every routed, released, resent and received answer logs one `INFO` line (task,
  context, route, pane, session, state, text length; never the text). These are the only `INFO`
  lines.

### Routing details

- Text is flattened to one line before sending (a newline would submit early), then typed with
  `tmux send-keys -l` and a separate `Enter`. For a Claude Code pane, `C-u` goes first to clear
  anything already in its input line; a real answer once arrived prefixed with stray characters.
  A pane that cannot be resolved falls back to note only.
- The busy check (`working`) applies to both the task's session and the orchestrator, and is
  skipped for Claude Code panes.
- Routing and notes go through `Tmux` and `Tracker` traits. Unit tests use fakes; one ignored
  test (`cargo test -- --ignored`) drives real tmux against a scratch session it creates and kills.
- Logs (warnings and errors, plus one `INFO` line per answer delivery; 1 MB then rotated once) go to `~/Library/Caches/fermata/fermata.log`
  on macOS, or `$FERMATA_LOG`.

### Pane resolution

`panes::resolve(table, session, use)` is the one way a session's pane is found. Every pane action
(input routing, quick reply, jump, close) targets the stable pane id (`%NN`) it returns, never
the recorded address.

- **The table** is `tmux list-panes -a -F '#{pane_id}\t#{pane_pid}\t#{session_name}:#{window_index}.#{pane_index}\t#{pane_current_command}\t#{pane_current_path}'`
  plus `ps -A -o pid=,ppid=`, read together. Reads are cached for 2s (`panes::CACHE_TTL`); close
  always reads fresh. Both `Tmux` (routing) and `System` (session actions) expose it, so the fakes
  can supply one.
- **By PID first.** The pane whose `pane_pid` is the session's `claude_pid`, or one of its first
  three ancestors (the pane's shell, or a wrapper above it). A dead pid matches nothing.
- **Fallback, for interaction only:** the pane at the recorded `tmux_target`, but only when its
  `pane_current_path` is the session's worktree. Otherwise "session pane not found; it may have
  closed", and routing keeps the note only.
- **Destructive use never falls back.** Close needs a PID match, and also that `ps` names the pid
  `claude` or `node`; otherwise it leaves the pane open and says why.
- **Claude panes.** A pane found by PID, or whose `pane_current_command` is `claude`, is treated as
  Claude Code: no busy confirmation, and `C-u` before typing.
- **Display.** The sessions table, task detail and input labels show the resolved address, with
  "(moved)" when it differs from the recorded one. Closed sessions keep the recorded address.
- Tests cover a moved pane, a dead pid with a matching and a mismatched path, a refused close, and
  a wrapper ancestor. The ignored real-tmux test resolves a pane through its child process after
  a split has moved it.
- The session-start hook records the stable `$TMUX_PANE` as `tmux_pane_id` beside `tmux_target`.
  Pane resolution does not use it yet; it could prefer it when live and keep PID resolution as
  the check and the fallback.

### Layout and colour

**Breakpoint.** `ui::split(width)` is the one rule. At 140 columns or more the detail view is
split: the list column takes `width * 3 / 10` clamped to 40–56 columns, the detail the rest (at
140 that is 42 + 98, so the detail keeps the width of an ordinary 100-column terminal). Below
140 it returns no split and the detail fills the screen, as before. The rule is a pure function
of the current width, evaluated on every draw, so a resize switches layout live; nothing about
the selection or the open item depends on it.

**Wide detail.**
- The left column is the list the detail was opened from: Tasks (id and title, with the badge
  as a coloured `●` and the pin star) or Decisions. Its cursor is that list's own cursor, so it
  is still on the same item when you go back.
- Focus starts on the list. `j`/`k`/`g`/`G` move the list and the detail follows: it switches
  to the selected item and reloads its timeline. `Tab` (or Enter) moves focus to the timeline,
  where `j`/`k` and Enter work as in the narrow view; `Tab` again returns to the list. The
  focused pane's border is the accent colour.
- In the narrow layout focus is irrelevant: `j`/`k` always move the timeline, as before. A
  resize keeps the focus value, so going narrow and back restores it.

**Active pane.** In every view with more than one pane, the pane `j`/`k` drive has a bold
accent-coloured border; the others have the plain one. That is the list column or the timeline
in the wide detail, the timeline in the narrow detail, and the list in the Decisions view. The
item preview under the timeline never takes focus: `J`/`K` scroll it from wherever focus is, and
moving the timeline highlight resets it to the top.
- If the item under the detail leaves the list (a reload dropped it), the detail moves to
  whatever the list cursor is now on.

**Palette.** Every colour comes from `theme.rs`; no widget names a `Color` itself. It uses the
16 named ANSI colours only, which every terminal theme tunes for its own background, and never
uses white, black or yellow for text that is not on a fixed background.

| Role | Colour | Used for |
|---|---|---|
| id | magenta | task/bead ids and ticket and epic keys |
| pr | blue | PR numbers (`#42`) |
| accent | cyan | active tab, focused border, input prompt, timeline source |
| dim | dark gray | labels, ages, timestamps, keys line |
| selected | dark gray background | the cursor row |
| badges | as listed under "Badges" | state badges |

Ids and PR numbers are coloured wherever they appear as text: the task, decision and session
lists, the detail header and fields, the timeline, notes and decision text. Free text is
scanned by `theme::Marks`: the ids of every task in the snapshot and every ticket and epic key,
matched as whole words, plus `#<digits>`. The transcript viewer is not scanned; it colours by
speaker.
