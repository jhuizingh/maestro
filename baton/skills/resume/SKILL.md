---
description: (Worker session) Pick up the task for the current git worktree — read the leaf bead id from the worktree's identity carrier, load it from the active context's tracker, and start implementing. This is what a handed-off session runs to orient itself. Checks first whether the branch already landed (preferring the PR over git ancestry): if it merged, routes to baton:finish instead of restarting the work, or stops outright when the bead is also already closed. No-ops cleanly outside a baton worktree.
allowed-tools: Bash(*), Read
---

## baton:resume

**Want the picture without the action?** `baton:status` reports the same task, criteria,
blockers, PR/check state and overall state and then stops — no hooks, no claiming, no
implementing. Use it when the question is "where does this stand"; use this skill when the answer
is "and now continue it".

### Step 1 — Am I in a baton worktree?

Recover the identity group from the worktree, using the same helper `baton:start` minted it with:

```bash
git rev-parse --git-dir >/dev/null 2>&1 || { echo "Not a git repo — nothing to resume."; exit 0; }
IDENT="${CLAUDE_PLUGIN_ROOT:-$HOME/code/maestro/baton}/scripts/task-identity.sh"
[ -x "$IDENT" ] || IDENT="$HOME/code/maestro/baton/scripts/task-identity.sh"
ID="$("$IDENT" --worktree "$PWD" --format env)" || { echo "Not a baton worktree — nothing to resume."; exit 0; }
eval "$ID"          # capture first: `eval "$(cmd)"` would swallow cmd's exit status
echo "Worktree: $DIR  branch: $BR  → leaf: $LEAF ($IDENTITY_SOURCE)  session: $SESSION_NAME"
```

`--worktree` reads the identity carrier `baton:start` wrote into this worktree's git dir. If
there isn't one — a worktree created before baton 0.5.0 — it falls back to parsing the directory
name and then the branch for the legacy `<leaf-id>-<slug>` shape, and backfills the carrier so
the next read is authoritative. `$IDENTITY_SOURCE` says which rung answered (`carrier`, `dir`,
`branch`); mention it in the summary when it wasn't `carrier`.

**Do not parse the branch name yourself.** The branch is configurable (`naming.branch`) and may
be free-form — `DOT-1234/some-description` carries no bead id at all. If the helper can't
resolve an identity, this isn't a baton worktree — say so and stop (no error).

### Step 2 — Resolve context + tracker

```bash
RESOLVER="${CLAUDE_PLUGIN_ROOT:-$HOME/code/maestro/baton}/scripts/resolve-context.sh"
[ -x "$RESOLVER" ] || RESOLVER="$HOME/code/maestro/baton/scripts/resolve-context.sh"
CTX="$("$RESOLVER")" || { echo "$CTX"; exit 1; }
export BEADS_DIR="$(echo "$CTX" | jq -r '.task_tracking.dir' | sed "s|^~|$HOME|")"
WS="$(echo "$CTX" | jq -r '._workspace')"
GUIDE="$WS/$(echo "$CTX" | jq -r '.guidance // "guidance.md"')"
```

### Step 3 — Load the bead

```bash
bd show "$LEAF" --json
```
If not found, tell the user the branch doesn't map to a known bead and ask how to proceed (they
may want `baton:start` or a different id). Read `$GUIDE` and honor it.

### Step 4 — Has this branch already landed?

Do this **before** orienting to the work. A resumed session is frequently waking up *after* its
PR merged: the gap between "PR merged" and "`baton:finish` ran" is precisely where a worker
session gets abandoned (machine reboot, tmux server restart, closed window), because the
interesting work is already over. Assuming the worktree is mid-flight is wrong every one of
those times.

Ask the shared helper — the same one `baton:finish` (Step 7) and `baton:cleanup-worktrees`
(Step 3) use, so all three agree and the ladder is fixed in one place:

```bash
MS="${CLAUDE_PLUGIN_ROOT:-$HOME/code/maestro/baton}/scripts/merge-state.sh"
[ -x "$MS" ] || MS="$HOME/code/maestro/baton/scripts/merge-state.sh"
M="$("$MS" --branch "$BR" --format env)" || M=""   # $BR from Step 1; capture first, as there
eval "$M"   # MERGED MERGE_SIGNAL GH_STATUS MERGE_BASE HAS_WORK PR_STATE PR_NUMBER
[ -n "${MERGED:-}" ] || { MERGED=no; MERGE_SIGNAL=none; GH_STATUS=unavailable; HAS_WORK=unknown; }
```

That last line covers a plugin cache too old to have the helper: an unset `MERGED` must degrade
to "carry on as normal", never to an untaken branch below.

**Never hand-roll this with `git log main..HEAD`.** An empty range means "already merged" *or*
"nothing done yet" — opposite situations wanting opposite responses — so reading it directly
sends a resumed session down a chain of manual re-derivation (observed on `jbh-5no`: an empty
range, then a second comparison against `origin/main`, then a `gh pr view`, to learn what one
call answers). The helper prefers `gh pr view` over ancestry for the reason `baton:finish`
documents — `git branch --merged` false-negatives on squash and rebase merges — and falls back
to ancestry when there is no PR, or when `gh` is missing or unauthenticated. That fallback is
never fatal; `$MERGE_SIGNAL` (`pr` / `ancestry` / `none`) and `$GH_STATUS` say which signal
actually answered, so report it whenever it wasn't `pr`.

Route on the result:

- **`MERGED=yes` and the bead is closed** — there is nothing to resume *and* no finish to run.
  Say so (naming the PR and the signal used), point out that a later home session's
  `baton:cleanup-worktrees` removes the worktree, and stop. Don't start on acceptance criteria,
  and don't run `baton:finish`.
- **`MERGED=yes` and the bead is open** — the work landed but was never closed out. Say so and
  run **`baton:finish`** instead of orienting to the acceptance criteria; it will verify them,
  close the bead, and apply `ready-for-worktree-delete`. Skip the rest of this skill.
- **`MERGED=no`, `HAS_WORK=no`, and the bead carries `no-pr-needed`** — this task's work was
  finished *outside git* (an API against a live system, say), so there are no commits by design
  and no merge is ever coming. The labels are already in Step 3's `bd show` output; check them
  before reading the empty branch as "unstarted". If the bead is closed, say so and stop — a
  later `baton:cleanup-worktrees` removes the worktree. If it's open, run **`baton:finish`** to
  close it out. Either way, do **not** start on the acceptance criteria; that would redo work
  that is already done.
- **`MERGED=no`, `HAS_WORK=no`, no such label** — nothing has been committed on this branch yet.
  This is a normal fresh worktree, *not* a merged one; carry on to Step 5. (Say "no commits yet"
  rather than anything about merging — the reason this reading is spelled out is that plain
  ancestry gets it exactly backwards.)
- **`MERGED=no`, `HAS_WORK=yes`** — genuinely mid-flight. Carry on to Step 5, and mention any
  open PR (`$PR_NUMBER`) so the session knows one already exists.

If `$MERGE_SIGNAL` is `none` (neither GitHub nor a usable base could be consulted), say that the
merge state is unknown and proceed as if unmerged — the cost is a redundant look at the work,
whereas a wrong "merged" strands it.

### Step 5 — Fire worker.on_resume hooks

Run each action in `hooks.worker.on_resume` (from `$CTX`) here in the worktree. The identity
group (`$LEAF`, `$SLUG`, `$BR`, `$DIR`, `$SESSION_NAME`, `$SESSION_TITLE`) is already exported
from Step 1 and is available to them.

Skip this when Step 4 routed to `baton:finish` or stopped — these hooks prepare a session for
*implementing*, and in those cases nothing is going to be implemented here. Say that they were
skipped rather than silently dropping them.

### Step 6 — Orient + begin

Reached only when Step 4 concluded the branch has **not** landed.

Print a short summary (bead title, description, acceptance criteria, current status) and start
working through the acceptance criteria, beginning with the first unmet one. If the bead is
already claimed by someone else or closed, flag it and confirm before continuing.

Check `$LEAF`'s labels (already in the `bd show --json` output from Step 3) for
`autonomous-safe`. If present, say so and note the implication for the rest of this session: once
the acceptance criteria are met, run `baton:pr` and then `baton:finish` straight through —
those skills' autonomous paths will create the PR, wait for checks, merge, and signal cleanup
without pausing for confirmation. This does **not** relax hard gates that apply regardless of the
flag: failing `pre_pr`/`pre_finish` hooks and red CI still stop the flow for a human to look at.
If the label is absent, work proceeds as normal — human confirms at PR/merge/cleanup.

When the work is done and merged, run **`baton:finish`**.
