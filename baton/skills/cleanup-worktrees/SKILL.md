---
description: Review git worktrees in the active context and clean up the finished ones. Scoped to the active context by default; pass --context <name> or --all-contexts to widen it. A worktree is a confirmed candidate when its leaf bead carries the `ready-for-worktree-delete` label (applied by `baton:finish` once merged) AND the merged/clean signals agree, AND either the bead is closed or it carries `keep-task-open` (an explicit "left open on purpose" signal) — those are auto-removed with no prompt. Everything else still requires explicit per-worktree confirmation.
argument-hint: "[--context <name>] [--all-contexts]"
allowed-tools: Bash(*)
---

## baton:cleanup-worktrees

Find worktrees that look done and clean them up. Confirmed-ready worktrees (all four signals
agree) are removed automatically — that's the strongest possible evidence a worktree is done, so
asking every time is just friction. Everything less certain still requires an explicit yes before
anything is touched.

Two independent signals feed this: the `ready-for-worktree-delete` **label** (an explicit
"I'm done" from the worker session that finished the bead — see `baton:finish` Step 6) and the
derived **closed + merged + clean** state (re-checked here from git/bd directly). Neither is
trusted alone — the label is the intentional signal, the derived state is the cross-check that
catches a stale or wrong label.

A third, narrower label — `keep-task-open` — modifies that cross-check rather than adding a new
one. It means the bead is being left open **on purpose** (see `baton:finish` Step 7: a worker
concluded some acceptance criteria are deliberately deferred, not blocking — e.g. waiting on
elapsed time/data — while this specific worktree's work is done and merged). When it's present
alongside `ready-for-worktree-delete`, the worktree lifecycle and the bead's open/closed status
are explicitly decoupled: `STATE == closed` is no longer required for confirmed-ready, because an
open bead here is expected, not an anomaly. `keep-task-open` never appears without
`ready-for-worktree-delete` — it has no independent meaning.

### Step 1 — Choose contexts to scan

```bash
REG="${BATON_REGISTRY:-$HOME/.config/baton/registry.yaml}"
RESOLVER="${CLAUDE_PLUGIN_ROOT:-$HOME/code/maestro/baton}/scripts/resolve-context.sh"
[ -x "$RESOLVER" ] || RESOLVER="$HOME/code/maestro/baton/scripts/resolve-context.sh"
```

Default: **the active context only** — run `"$RESOLVER"` the same way every other baton skill
does (env override, then cwd match, then a context marked `default:true`) and scan just that
one. A context-scoped command reaching into another context's repos by default is a footgun:
worktrees under a different context's member repos (e.g. a `work` repo) aren't yours to offer
up for removal from a `personal` session.

Two ways to widen the scan, both explicit:
- `--context <name>` — scan exactly that one context, regardless of what's active.
- `--all-contexts` — iterate **all** registered contexts (`yq -r '.workspaces[]' "$REG"`, read
  each `context.yaml`), each in its own clearly-labeled section. Use this only when the user
  asked for a cross-context view.

If the resolver can't resolve anything (no cwd match and no context marked `default:true`) and
neither flag was given, say so and ask the user to pick `--context <name>` or `--all-contexts`
rather than silently falling back to scanning everything.

### Step 2 — Enumerate worktrees per context

For each context, for each member repo, list worktrees:
```bash
git -C "<repo>" worktree list --porcelain
```
Each worktree dir under `<worktree_base>` is named for its branch `<leaf-id>-<slug>`.

### Step 3 — Classify each worktree

For each worktree, compute the label signal plus the three cross-check signals (set `BEADS_DIR`
to that context's tracker for the bead checks):

```bash
LEAF="$(basename <wt> | sed -E 's/^([a-z0-9]+-[a-z0-9.]+)-.*/\1/')"   # id may contain a dot (dotted child bead)
LABELS="$(BEADS_DIR=<tracker> bd label list "$LEAF" 2>/dev/null)"
LABELED="$(echo "$LABELS" | grep -qw ready-for-worktree-delete && echo yes || echo no)"
KEEP_OPEN="$(echo "$LABELS" | grep -qw keep-task-open && echo yes || echo no)"
STATE="$(BEADS_DIR=<tracker> bd show "$LEAF" --json 2>/dev/null | jq -r '.status // "unknown"')"   # closed?
git -C <repo> fetch origin --quiet
if (cd <repo> && gh pr view "<branch>" --json state --jq '.state == "MERGED"') 2>/dev/null | grep -q true; then
  MERGED=yes
elif git -C <repo> branch --merged origin/main | grep -qw "<branch>"; then
  MERGED=yes
else
  MERGED=no
fi
DIRTY="$(git -C <wt> status --porcelain)"                                                          # empty = clean
```

Prefer `gh pr view` over git ancestry: `git branch --merged` false-negatives on squash and rebase
merges (a new commit lands on the target that isn't an ancestor of the feature branch), so ask
GitHub directly first and only fall back to ancestry when there's no PR to ask about.

Classify into four buckets:
- **Confirmed ready** — `LABELED == yes` AND (`STATE == closed` OR `KEEP_OPEN == yes`) AND
  `MERGED == yes` AND `DIRTY` empty. The worker explicitly signaled done, and the independent
  checks agree — an open bead doesn't block this when `keep-task-open` says it's intentional.
- **Label/state mismatch** — `LABELED == yes` but any of the cross-check signals disagree (bead
  reopened after being marked ready with no `keep-task-open` cover, branch not actually merged, or
  tree dirty again). This is an anomaly, not a green light — flag it prominently and do **not**
  offer to remove; say which signal disagreed.
- **Looks done, unlabeled** — `STATE == closed` AND `MERGED == yes` AND `DIRTY` empty, but no
  label (e.g. finished before this label existed, or via a work mode that never ran
  `baton:finish`). Still a plausible removal candidate, but call out that it wasn't explicitly
  confirmed by a worker session.
- Everything else — **in progress / not ready** — show which signal is red so the user knows why
  it's being kept.

#### A note on label scoping (multi-worktree-per-bead)

Both `ready-for-worktree-delete` and `keep-task-open` live on the **bead** (`bd label list
<leaf>`), not on any specific worktree or branch. If the same bead ever has two worktrees over its
lifetime — an earlier one that finished and was labeled ready (possibly with `keep-task-open`,
since that's exactly the scenario the label exists for), then a *later* worktree opened against
that same still-open bead for the follow-up work — both labels are visible from the new worktree
too, even though only the finished one is actually ready.

This is deliberately **not fixed here** (deferred, not overlooked):
- The per-worktree `MERGED` and `DIRTY` checks in this step are computed fresh from git each time,
  never from the label, so they're the real safety net regardless of what the bead's labels say.
  The still-active later worktree fails its own merged/clean check and can never land in
  confirmed-ready by mistake — the worst outcome is a presentation issue, not a wrongful deletion.
- Concretely, that active-but-unrelated worktree lands in **label/state mismatch** (flagged as an
  anomaly) instead of plain **in progress / not ready**, because the stale bead-level label makes
  it look like something disagrees when really it's just unrelated, still-in-progress work on an
  old label. A human glancing at the mismatch reasoning (merged=no / dirty) can recognize this at
  a glance and move on — it's a false-positive nuisance, not a safety gap.
- A real fix would make the signal branch/worktree-scoped (e.g. storing it against the specific
  branch/commit rather than the bead), which beads has no mechanism for today. Revisit if this
  false-positive shows up often enough in practice to be worth building; until then the cost is a
  few extra seconds of human judgment on an already-flagged row, not a risk of losing work.

### Step 4 — Remove confirmed-ready automatically, ask for the rest

Show all four groups, each with bead id/title and reasoning.

**Confirmed ready** — remove immediately, no prompt. All four independent signals already agree
(label, closed, merged, clean), so there's nothing left for a human to confirm:

```bash
WT=<wt-path>; BR=<branch>
git -C <repo> worktree remove "$WT" --force
git -C <repo> branch -d "$BR"
```

**Looks done, unlabeled** — still ask, per worktree (or offer "remove all unlabeled-but-done" as
its own batch) — there's no explicit "I'm done" signal from a worker session here, so a human
should confirm before removing. Use the same removal commands once confirmed.

**Label/state mismatch** — never offer removal; it's an anomaly by definition. Flag it and move
on.

**In progress / not ready** — never touched.

Never bundle groups together into a single blanket "remove all" — confirmed-ready acts on its
own (automatically), and unlabeled-but-done is its own separate ask.

For every removal (auto or confirmed), run the context's `hooks.home.on_cleanup` actions, with
`$WT` (worktree path) and `$BR` (branch name) set in the environment so hook actions can
reference them. This is where the worktree's tmux session gets torn down — `baton:configure`
seeds `on_cleanup` with `tmux kill-session -t "baton-${BR//[^A-Za-z0-9_-]/_}"` by default,
matching the session name `baton:start` creates for the default plain-tmux handoff, precisely so
this step isn't a silent no-op. If a context's `on_cleanup` is still empty, say so — the tmux
session will leak. Likewise if the context sets a custom `handoff.launcher` whose session naming
doesn't match the teardown target: the kill silently no-ops, so flag the mismatch rather than
reporting a clean removal.

### Step 5 — Summary

Report what was auto-removed (confirmed-ready, with reasons — the four agreeing signals), what
was removed after confirmation, and what was kept (with reasons). Call out any label/state
mismatches even if the user didn't ask about them — they indicate something worth double-checking
(a bead reopened after being marked ready, or a branch that got un-merged). Never remove a
worktree that isn't in the confirmed-ready or looks-done group, even if asked to "clean
everything" — surface the blocker instead.
