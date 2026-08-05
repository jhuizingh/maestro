# baton

**Context-aware, [beads](https://github.com/steveyegge/beads)-backed git-worktree workflow
orchestrator for Claude Code.** You conduct from a home session; each task is handed off —
*passed the baton* — to a fresh worker session running in its own git worktree, tracked by a
single bead, and brought home for review-and-confirm cleanup when it's done.

Part of the [maestro](../README.md) plugin collection. Prefix: `baton:`.

---

## 🧭 Many worlds, one tool — and they never mix

> [!IMPORTANT]
> **Your side projects, your day job, and the open-source org you help maintain are three
> different worlds. baton keeps them that way.**
>
> Each is a **context** — its own task tracker, GitHub owner, code root, startup routine, PR
> gates, and accumulated preferences. Add as many as you like. Nothing bleeds across.

```
~/.config/baton/registry.yaml          ← lists your workspaces; the only shared file
        │
        ├── ~/code/personal-workspace/     🏠 personal
        │      context.yaml                   tracker: ~/code/personal-tracking
        │      guidance.md                    owner:   your-user
        │                                     gate:    (none)
        │
        ├── ~/work/acme-workspace/         💼 work
        │      context.yaml                   tracker: ~/work/acme-tracking
        │      guidance.md                    owner:   acme-corp
        │                                     gate:    typecheck, test, changelog
        │
        └── ~/code/oss-workspace/          🌍 oss
               context.yaml                   tracker: ~/code/oss-tracking
               guidance.md                    owner:   some-org
                                              gate:    lint, test, DCO
```

**You never pick one manually.** `cd` into a repo and the right context activates — baton
matches your directory against each context's `member_repos`. A task you start at work is
filed in the work tracker, opened under the work org, and gated by the work checks. Even
`baton:cleanup-worktrees` scopes to the active context by default, so a Friday tidy-up in your
personal session can't offer up your employer's worktrees.

Each context also *learns separately*: `baton:remember` and the retrospective fold preferences
into that context's own `guidance.md`. Your work conventions never leak into your OSS commits.

## 🔄 It updates itself — twice over

> [!TIP]
> **You never run an update command, and you never re-teach it the same lesson.**

**The plugin keeps itself current.** The `align` startup task — first in the default
`startup_tasks`, so it runs every time you open a home session — compares the installed baton
version against the repo's `plugin.json` and runs `claude plugins update baton@maestro` when
they differ (noting if a restart is needed). It also fast-forwards your member repos and syncs
the context's tracker in the same pass, failing soft so one bad pull never blocks the rest.

**And the workflow keeps improving itself — by asking.** Every task ends with a retrospective.
With `retro.enabled` (default: on, at `finish`), `baton:finish` puts one question to you:

> *Anything in this flow that could have gone better?*

Answer "nothing" and it stops there. Answer with something and baton classifies it — a durable
preference, a lifecycle hook, or a config change — then **proposes the specific edit** and
applies it only once you confirm. Say a project needs a changelog entry before every PR and it
becomes a `worker.pre_pr` hook; say you prefer terse commit subjects and it becomes a line in
`guidance.md`, which every skill loads on every run. `baton:remember` writes the same file on
demand, anytime.

Every iteration asks, so the workflow converges on how *you* work — one answer at a time. (The
prompt is skipped for `autonomous-safe` tasks, where no human is present to ask, and
`retro.when` controls which points trigger it.)

**Corrections become configuration** — and since `guidance.md` lives in each context's own
workspace repo, the learning is per-world and versioned in git. Your work conventions never
leak into your OSS commits, and you can see exactly when a preference was added, and why.

### Why this beats hand-rolling it

If you run several unrelated project sets and lean on git worktrees to parallelize, you end up
hand-managing a lot: which tracker to use, which repos belong where, spinning up sessions, and
cleaning up worktrees afterward. baton turns that into a small set of skills driven entirely by
**per-context config you own** — so the same plugin serves every context, and anyone can adopt
it by writing their own config (no plugin edits).

## Core concepts

### Contexts
As above: a named set of projects sharing a task tracker, a GitHub owner, a code root, and
preferences. Each lives in its own **workspace repo** (private, yours) as a `context.yaml`
beside an evolving `guidance.md` — see
[`references/context.example.yaml`](./references/context.example.yaml) for every field.

Resolution runs on every skill invocation, in this order:

1. Is your cwd inside some context's `member_repos` (or one of their worktree bases)? → that context.
2. Otherwise → the context marked `default: true`.

`baton:whereami` reports which context is active and the paths it resolved to — the quickest
way to confirm you're pointed where you think you are before starting work.

### The task model: one worktree ⇔ one leaf bead
baton tracks work in [beads](https://github.com/steveyegge/beads) (`bd`), a git-native graph
issue tracker. Hierarchy is arbitrary depth (epic → task → subtask → …):

- A bead that needs decomposing is a **parent** — a planning container, never worked in a
  worktree directly.
- Each unit that gets its own worktree is a **leaf bead**. Its branch is `<leaf-id>-<slug>`,
  so a worker session always resolves **exactly one** bead from its own branch — even with
  many worktrees open under one parent at once.
- Split work mid-stream with `baton:split`: the current worktree keeps finishing *its own*
  bead; each carved-off chunk becomes a **new leaf + new worktree**. Branches are never
  rebound to a different id.

### Autonomous-safe tasks
By default, every worktree comes home for a human to confirm at three points: opening the PR
(implicit — you invoke `baton:pr`), merging it, and worktree cleanup. Some tasks are low-impact
and easy enough not to need that — mark one with the `autonomous-safe` label (e.g.
`baton:task-add "..." --autonomous-safe`, or add it later with `bd label add <id>
autonomous-safe`) and its worker session runs `baton:pr` → `baton:finish` straight through:
`baton:finish` waits for CI and merges automatically once checks are green, then signals cleanup
the same way it always does. It never merges over a **red** check — that gate is never skipped,
autonomous or not — and a failing `pre_pr`/`pre_finish` hook still stops the flow. Everything
else stays human-gated by default.

### File-free handoff
Dispatching a task creates the worktree and opens a fresh session — but writes **no file**
into the worktree. The worker learns its task from beads, keyed by its branch name. Nothing
to clean up, nothing to accidentally commit, and the tracker stays the single source of truth.

### Config-driven customization (no plugin edits)
Everything tailorable lives in your workspace repo:

- **`startup_tasks`** — what runs every time you start a context (default: align → tool check
  → cleanup review → status).
- **Lifecycle `hooks`**, grouped by the session they run in:
  - `home` (orchestrator session): `on_dispatch`, `on_cleanup`.
  - `worker` (worktree session): `on_resume`, `pre_finish` (e.g. tests/lint/build), `post_finish`.
- **`guidance.md`** — evolving preferences the skills load and honor on every run.
- **Retrospective** (toggleable) — after configured points, baton asks "what could have gone
  better?" and folds your answer back into `guidance.md`/hooks/preferences. The workflow
  improves per context over time, with no skill changes.

## Requirements

- [`bd`](https://github.com/steveyegge/beads) (beads) — task tracking.
- `git` (worktrees), `gh` (GitHub operations).
- `yq` + `jq` — the config resolver.
- `tmux` — only for the default new-session handoff, which uses **plain tmux**: baton opens the
  worker session itself, so there's nothing to configure. Without tmux, use a same-session work
  mode. [`tmuxinator`](https://github.com/tmuxinator/tmuxinator) or any other wrapper is
  **optional** — point `handoff.launcher` at it if you prefer.

`baton:doctor` checks all of this and offers to fix what's missing — and it re-runs as a
default startup task, so environment drift gets caught over time.

## Getting started

```sh
claude plugins marketplace add <you>/maestro
claude plugins add baton@maestro
```

Then, in Claude Code:

1. **`baton:setup`** — one-time machine bootstrap (shell wiring, tool check).
2. **`baton:configure`** — create your first context: it scaffolds a `<name>-workspace` repo,
   asks for your member repos / GitHub owner / preferences, registers it, and (optionally)
   creates the context's task-tracking repo. Each context you add gains a `<name>-start`
   shell command automatically.
3. **`<name>-start`** (e.g. `personal-start`) — open a home session for that context: it
   aligns everything and shows you what to pick up.
4. **`baton:start`** — begin a task: pick or create a leaf bead, get a worktree + worker
   session. **`baton:finish`** closes it out. **`baton:cleanup-worktrees`** reviews worktrees,
   auto-removing confirmed-ready ones and asking for the rest.

## Skill catalog

| Skill | What it does |
|-------|--------------|
| `baton:setup` | One-time machine bootstrap: shell wiring, loader, tool check. |
| `baton:doctor` | Verify required tools are present; offer to fix. Also a default startup task. |
| `baton:configure` | Create/edit a context (workspace repo + `context.yaml` + guidance). |
| `baton:onboard` | Briefing on how baton works and your registered contexts. |
| `baton:whereami` | Report the auto-detected context and resolved paths. |
| `baton:start [task]` | Start a task: resolve a leaf bead, create a worktree, hand off. |
| `baton:resume` | (Worker session) pick up the bead for the current branch and begin. |
| `baton:finish [task]` | Close the leaf, run finish hooks, and optionally run a retrospective. Auto-merges + skips confirmations for `autonomous-safe` leaves. |
| `baton:cleanup-worktrees` | Review finished worktrees; auto-remove confirmed-ready ones, confirm the rest. |
| `baton:session-start` | Run the context's startup tasks (invoked by `<name>-start`). |
| `baton:split [parent]` | Decompose a bead into child leaves mid-work. |
| `baton:new-repo <name>` | Propose + create a new repo under the context's owner (with signoff). |
| `baton:task-add` / `baton:task-list` | Quick-capture / list tasks in the active context. `--autonomous-safe` marks a task for end-to-end unattended handling. |
| `baton:remember` | Save a preference into the active context's `guidance.md`. |

## Adapt it to your own contexts

baton ships **no** assumptions about your repos, orgs, or paths. To use it, you write a
`context.yaml` (via `baton:configure`) describing:

- where your task tracker lives (`task_tracking`),
- which repos belong to the context (`member_repos`) — the cwd-based auto-detection key,
- your GitHub owner and new-repo naming (`github`),
- how work starts (`work_mode`, `handoff`), what runs on startup (`startup_tasks`),
- lifecycle `hooks` and `guidance.md`.

No code changes, no fork. That's the whole point.

### Sample config

**[`references/context.example.yaml`](./references/context.example.yaml)** is a fully
annotated `context.yaml` — every field baton reads, with valid values, defaults, and the
optional direnv-on-dispatch pattern. Read it to see the whole surface at a glance, or copy it
into your workspace repo and fill in the `<placeholders>`.

`baton:configure` writes this same schema interactively and is the recommended way to create a
context; the sample is the reference you keep open while editing one by hand.

Your own config lives **outside** this plugin — nothing you tailor is ever committed here:

```
~/.config/baton/registry.yaml      # lists your workspace repos ($BATON_REGISTRY overrides)
~/code/<name>-workspace/
├── context.yaml                   # the schema above
└── guidance.md                    # evolving preferences the skills load every run
```

## License

MIT — see [../LICENSE](../LICENSE).
