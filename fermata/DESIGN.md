# fermata — design

A terminal dashboard for one-orchestrator, many-workers Claude Code work. It is the main place to see
every in-flight task, answer what is waiting on you, and steer work, so nothing depends on scrolling
back through a chat. (A fermata is the mark that holds a note until the conductor releases it, which
is what a task waiting on you is.)

Status: M1–M4 built (ccmon parity actions untested live), 2026-11-12. Local to this machine; no
remote. See "As built" at the end for the refinements made while building.

## Goals

1. **Tasks first.** List every in-progress task, ordered so that the item that most needs you is on top.
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
- No changes to baton's code or its `context.yaml` schema. No task/ticket knowledge added to ccmon.
- No outward actions of its own: no pushing, merging, commenting or ticket transitions. fermata
  relays instructions and answers; the orchestrator and its approval gates do the acting.
- No server, daemon or new credentials. Everything comes from existing local CLIs and files.

## Concepts

| Concept | Generic meaning | Source |
|---|---|---|
| Context | A baton context: tracker, member repos, workspace repo | `~/.config/baton/registry.yaml` → each workspace's `context.yaml` |
| Task | A tracker leaf: id, title, status, labels, notes | baton tracker seam (`tracker.sh list/get/list-branches`) |
| Links | Ticket, epic, PR, plus any extra URLs | ticket/epic: context adapter; PR: the branch registry (`pr`, `pr_url`) plus `gh` |
| Progress | One-line status, done, next, checklist count | context adapter (files), latest worker report |
| Decision | A task labelled `needs-decision`, plus its latest `DECISION` note | tracker |
| Session | A live or closed Claude Code session | ccmon's `sessions/*.json`, `closed/*.json`, `logs/*.log`; tmux |
| History | Notes, worker reports, session events, transcripts | tracker notes, subagent transcripts, ccmon logs, session transcripts |

## Views

- **Tasks** (home). One row per in-progress task across all contexts: priority marker, state badge
  (needs you / overdue / worker running / interactive / waiting on CI / ready to merge / parked),
  task id, title (plus "waiting on X until D" on a parked or overdue row), ticket, PR number with
  check summary, checklist progress, and age of the last change.
  - Sort order: needs-you in a pinned stream (by pin order), then any other needs-you, then
    overdue parked tasks, then the rest of the pinned streams, then everything else by most
    recent change.
- **Task detail** (Enter on a task).
  - Header: ticket (key, title, status), epic, PR (state, review decision, checks).
  - Keys: `o` ticket, `e` epic, `p` PR, `c` failing or latest CI run, `w` open the worktree in a new
    tmux window.
  - Body: the decision (question, options, recommendation) when present; done / next; checklist.
  - Timeline: tracker notes, worker reports, session prompts and notifications, newest first.
    Enter on an item opens it in full, and on a transcript opens the transcript viewer.
- **Decisions.** The `needs-decision` items only, in priority order. Number keys pick an option;
  `a` types a free-form answer.
- **Sessions.** ccmon parity: status, repo, branch, tool, age and calls, now with the task each
  session belongs to. Keys as in ccmon: jump, quick reply, send prompt, log view, describe, close,
  reopen, snapshot and restore.
- **Transcript viewer.** Renders a Claude Code `.jsonl` as user and assistant turns, with tool calls
  collapsed to one line (expand on Enter), and searching with `/`.
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
progress:
  status: '~/notes/tickets/{key}/STATUS.md'      # first line = one-line status
  checklist: '~/notes/tickets/{key}/tasks.md'    # counts "- [x]" vs "- [ ]"
pr:
  provider: github                        # PR state and checks via `gh`
priorities:                               # pinned streams, top first; editable from fermata
  - epic: ABC-100
  - ticket: ABC-123
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
- Non-zero exit, a timeout (5s), or invalid JSON: fermata shows the key and the `url` template only,
  marks the panel "adapter error", and logs the stderr. It never blocks the UI.
- Output is cached per key for `ttl`; `r` in the detail view forces a refresh.
- The adapter script is owned by the context. In a JIRA-backed context it is the only code that touches the
  `jira` CLI or the ticket notes directory.

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
3. **Always** record it with `tracker.sh note <id> "ANSWER (fermata, <ts>): …"`, so history is
   complete. Answering a decision does not clear `needs-decision` itself; the orchestrator does that
   once it has acted.

If no orchestrator session is found, fermata says so and keeps the note only. The orchestrator picks
it up on its next sweep.

Never send keys to a busy (`working`) pane unless the user confirms, except when the pane runs
Claude Code, which queues input typed while it works. Input typed into another busy program can
interleave with its own output.

Panes are found by process, never by ccmon's recorded `tmux_target` alone. ccmon records that
address once, at SessionStart, and pane indexes shift as panes are opened and closed: a recorded
`ccmon:1.2` can come to name an unrelated shell, which once received a routed answer as a command.
See "Pane resolution" under "As built".

## Linking things to a task

- **Worktree → task:** the identity carrier at `$(git -C <wt> rev-parse --git-dir)/baton-identity`
  (`leaf=`, `kind=`, `pr=`), read-only.
- **Task → worktree, PR:** `tracker.sh list-branches <id>` (`worktree`, `pr`, `pr_url`, `status`).
- **Session → task:** the session's cwd (`worktree` in ccmon's record) matched against task
  worktrees.
- **Background worker → task:** the subagent's `.meta.json` `description` starts with the task id
  (a convention the orchestrator follows: `"ctx-01z: review #4602"`). Transcripts live at
  `~/.claude/projects/<cwd>/<session>/subagents/agent-<id>.jsonl`; the last assistant message is
  the worker's report.
- **Session → transcript:** ccmon records `transcript_path` from the hook payload (small, generic
  ccmon change).

## Workflow conventions this relies on

These go into the context's `guidance.md` and worker brief, not into fermata:

- A worker's Agent `description` starts with its task id.
- A worker, or the sweep, updates the context's status file for the task (for example `STATUS.md`).
- Decisions use the `needs-decision` label plus a `DECISION (…)` note with a question, options and a
  recommendation.

## Data refresh

- **Sessions:** watch ccmon's `sessions/` directory (kqueue via the `notify` crate), event-driven.
- **Tasks:** watch each tracker's `.beads/` directory for changes, then re-run `tracker.sh list`.
  Debounce to at most once every 2s. Never poll the tracker on a timer.
- **PRs:** `gh pr view --json …` per visible task, cached 60s, refreshed on focus.
- **Ticket info:** adapter `ttl`.
- Logging: warnings and errors only by default, size-capped. Never log per redraw.

## Tech

- Rust (stable). `ratatui` + `crossterm` (resize and mouse), `tokio` for subprocess fan-out,
  `notify` for file watching, `serde`/`serde_json`/`serde_yaml`, `open` for the browser.
- All external access is through existing CLIs (`tracker.sh`, `gh`, `tmux`, adapter commands). No
  direct API clients or tokens.

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
   `transcript_path`. Then retire `dashboard.py`.

## As built (2026-11-12)

Refinements made while building M1–M4. These are the current behaviour.

### Modes and keys

- `fermata` runs the TUI; `--dry-run` logs input instead of sending keys or writing notes, and
  turns `w`, session jump and every ccmon action into logged no-ops (read-only probes such as
  `ps` and `tmux has-session` still run so the preview is accurate). `--dump [--task <id>]` prints the joined task list, or one
  task plus its timeline, as JSON; `--dump-sessions` prints ccmon sessions.
- Global: `t` tasks, `d` decisions, `s` sessions, `?` help, `R` full reload (re-resolves contexts,
  bypasses caches), `Esc`/`q` back, `q` on a top view quits, `m` toggles mouse capture. Mouse
  capture is **off** by default so terminal text selection works; the footer shows its state.
- Tasks and detail: `o` `e` `p` `c` `w` as above, `:`/`i` input bar, `*` pin/unpin the ticket,
  `+` pin/unpin the epic, `r` refresh (tasks: full reload; detail: `tracker get` + re-join, no
  caches).
- Decisions and detail: `1`–`9` **pre-fill** `Option N: <text>` into the input bar instead of
  sending immediately, so a stray key never answers a decision; `a` free-form.
- Sessions: `Enter` jump (`tmux switch-client`), `r` quick reply to that pane, `l` ccmon log,
  `v` session transcript, `o` the session's task, `e` describe, `x` close, `X` toggle the closed
  list (where `Enter` reopens and `e` edits the record's description), `S` save a snapshot, `L`
  restore the latest snapshot. Close, reopen and restore always ask first (see "ccmon actions").
- Viewer: `j/k`, `Space`/`b` page, `g`/`G`, `Enter` expand/collapse a tool call or result, `/`
  search (matches inside collapsed tool bodies land on the tool line), `n`/`N`.

### Data

- **Locating baton** follows baton's own rule: `$CLAUDE_PLUGIN_ROOT` → `~/code/maestro/baton` →
  `installed_plugins.json` (user scope first) → newest `cache/maestro/baton/*` by version.
- **Context JSON** comes from `resolve-context.sh` with `BATON_CONTEXT=<name>`. It is cached in
  memory until `R` and on disk keyed by the baton path and the mtimes of context.yaml and the
  registry. If resolving fails, the context.yaml converted to JSON is used.
- **Notes** arrive as one text blob. An entry starts at a line shaped like `WORD (…):` or
  `WORD:`; other lines continue the previous entry. Stamps are usually date-only; in the timeline
  a date-only note sorts at the end of its day, capped at the task's `updated_at`.
- **Decision options.** The orchestrator's format is parsed first:
  `DECISION (<ts>): <question> | 1) <option> | 2) <option> | … | Rec: <n or text>`. A numeric
  `Rec` is shown as `n) <option>`. Otherwise (the fallback): numbered or bulleted lines after the
  first, or inline `(1) … (2) …` markers, with a trailing `Rec:`/`Recommendation:`. A `needs-decision` task with
  no `DECISION` note still shows as a decision.
- **PR:** the newest registry entry with `pr_url`/`pr` wins. With none recorded, an `open` branch
  is looked up with `gh pr view <branch> --repo <owner>/<repo>`; "no pull requests" is silent.
- **Checks.** Only the latest run per check counts (CheckRuns keyed by workflow + name, status
  contexts by context; latest by `startedAt`). Runs superseded on the same head, such as
  concurrency-cancelled duplicates, are ignored. A check is failing when its latest run concluded
  `FAILURE`, `TIMED_OUT` or `ACTION_REQUIRED`; a latest run that was itself `CANCELLED` is not
  failing. `c` opens the first failing check, else the most recent one.
- **Badges** (first match): needs you (`needs-decision`, or the live session is `needs_input` /
  `needs_permission`) → **overdue** (kept open and past its `until` date, see "Parked tasks") →
  **CI failing** (open PR with a failing check) → ready to merge (open, not
  draft, APPROVED, no failing or pending checks) → waiting on CI (open with pending checks) →
  worker running (a matched subagent without `finished`) → interactive (a live session) →
  **parked** (`ready-for-worktree-delete` or `keep-task-open`) → idle.
- **Parked tasks.** A task is kept open when it carries `keep-task-open` or a registry entry says
  `keep_task_open: yes`. The newest registry entry that recorded `waiting_on` or `until` (written
  by `baton:finish`) says what it waits on. Parked and overdue rows show "waiting on X until D"
  after the title, and the detail view adds a `parked` line ("no reason recorded" when there is
  none). Overdue means `until` is before today's local date; due today is not overdue, and a date
  that is not `YYYY-MM-DD` never is.
- **Sort.** The groups (needs-you pinned, needs-you, overdue, pinned, rest) come first. Overdue
  sits just below needs-you because a missed look-again date is the next most urgent thing.
  Within a group the order is badge tier (needs you > overdue > CI failing > ready to merge > waiting on CI
  > working, meaning worker running or interactive > parked > idle), then pin order, then most
  recent change.
- **Workers:** description `<task-id>:` (or `<task-id> `) is the convention. A description that
  starts with the task's ticket key is also accepted, because workers launched before the
  convention used ticket keys. The worker's report is its final `SubagentHandback` message (that
  is how background agents deliver it), else the last assistant text; only the last 1 MB of the
  transcript is read.
- **Session transcripts:** ccmon does not record `transcript_path` yet, so fermata derives
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
- ccmon rewrites a session file on every hook, so a sessions-dir event only re-reads
  `sessions/*.json`, re-matches sessions to tasks and re-badges. It never calls the tracker.

### Caching and load time

- An on-disk cache lives at `~/Library/Caches/fermata/cache/` (or `$FERMATA_CACHE_DIR`):
  - tracker state (in-progress tasks plus each task's branches), keyed by the tracker write
    signature;
  - resolved contexts;
  - ticket info, honouring the adapter `ttl`;
  - PR state, 60s.
  `R`, and `r` on the Tasks view, bypass it.
- Contexts resolve in parallel. Within a context, the `list-branches` calls run in parallel and
  each task's adapter `info` and `gh` calls run side by side. On a signature miss the previous
  tracker state is joined in the background while the tracker is read, which warms the ticket
  and PR caches.
- `--dump` on the current data (6 tasks, 2026-11-12):
  - everything cached: 0.02s
  - PR cache expired: 1.5–2.4s
  - tracker changed (with ticket and PR caches expired too): 3.2–3.6s
  - no cache at all: about 6.7s

  The floor for a tracker change is `list` (about 1s) plus six concurrent `list-branches`
  (about 3s, which contend on the embedded Dolt lock).

### ccmon actions

- The ccmon directory is `$FERMATA_CCMON_DIR`, default `~/code/claude-monitor`.
- **Describe** sets `description` in `sessions/<id>.json` (or `closed/<id>.json`), keeping every
  other field. The write is atomic (temp file plus rename) but takes no lock, unlike ccmon's
  hooks.
- **Close** (confirm: `y` kills claude and the pane, `k` kills claude only) follows ccmon:
  1. Write `closed/<id>.json` with `{session_id, description, worktree, branch, tmux_session,
     closed_at}`.
  2. SIGTERM the `claude_pid`, wait up to 2s, then SIGKILL. The pid is only signalled if `ps`
     names it `claude` or `node`, which guards against pid reuse.
  3. Optionally `tmux kill-pane` on the pane resolved by PID (see "Pane resolution"). The pane is
     resolved before step 2, while claude still runs in it. Without a PID match it is left open
     and the result says why: close never kills a pane by its recorded address.
  4. Delete the live session file.
- **Reopen** (confirm) runs `$SHELL -lc 'claude --resume <id>; exec $SHELL -l'`: in a new
  window of the record's tmux session if that session exists, else in a new detached session
  named after it. Unlike ccmon this does not pass `--dangerously-skip-permissions`, which many
  setups disallow. ccmon's session_start hook removes the closed record.
- **Snapshot**: `S` runs `python3 <ccmon>/session_snapshot.py save`; `L` (confirm) runs
  `… restore`. Restore relaunches through ccmon's tmuxinator template.
- Every live effect goes through a `System` trait. `RealSystem` does the work; `FakeSystem`
  backs the unit tests, which run against scratch copies of session JSON; `DryRunSystem` is the
  `--dry-run` path. None of these actions has been run against live sessions.

### Routing details

- Text is flattened to one line before sending (a newline would submit early), then typed with
  `tmux send-keys -l` and a separate `Enter`. For a Claude Code pane, `C-u` goes first to clear
  anything already in its input line; a real answer once arrived prefixed with stray characters.
  A pane that cannot be resolved falls back to note only.
- The busy check (`working`) applies to both the task's session and the orchestrator, and is
  skipped for Claude Code panes.
- Routing and notes go through `Tmux` and `Tracker` traits. Unit tests use fakes; one ignored
  test (`cargo test -- --ignored`) drives real tmux against a scratch session it creates and kills.
- Logs (warnings and errors, 1 MB then rotated once) go to `~/Library/Caches/fermata/fermata.log`
  on macOS, or `$FERMATA_LOG`.

### Pane resolution

`panes::resolve(table, session, use)` is the one way a session's pane is found. Every pane action
(input routing, quick reply, jump, close) targets the stable pane id (`%NN`) it returns, never
the recorded address.

- **The table** is `tmux list-panes -a -F '#{pane_id}\t#{pane_pid}\t#{session_name}:#{window_index}.#{pane_index}\t#{pane_current_command}\t#{pane_current_path}'`
  plus `ps -A -o pid=,ppid=`, read together. Reads are cached for 2s (`panes::CACHE_TTL`); close
  always reads fresh. Both `Tmux` (routing) and `System` (ccmon actions) expose it, so the fakes
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
- ccmon could record the stable `$TMUX_PANE` at SessionStart beside `tmux_target`. fermata would
  then prefer it when it is live and keep PID resolution as the check and the fallback. That is
  a ccmon change and is not made here.
