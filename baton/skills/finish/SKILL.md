---
description: Finish a task — run pre-finish hooks, verify acceptance criteria, run a documentation pass, close the leaf bead, run post-finish hooks, and (if enabled) run a retrospective whose feedback is folded back into this context's guidance/hooks. If the PR isn't merged yet, always checks its check-run status and flags failing/pending checks before offering to fix them or proceed anyway — unless the leaf is labeled `autonomous-safe`, in which case it waits for checks and merges automatically once they're green (never over red checks). Never deletes the directory it runs in; once the branch is merged it labels the leaf bead `ready-for-worktree-delete` (plus `keep-task-open` if the bead is being left open on purpose for follow-up work) so a later session's `baton:cleanup-worktrees` can clean it up explicitly.
argument-hint: "[bead-id]"
allowed-tools: Bash(*), Read, Edit
---

## baton:finish

### Step 1 — Resolve context + the leaf

```bash
RESOLVER="${CLAUDE_PLUGIN_ROOT:-$HOME/code/maestro/baton}/scripts/resolve-context.sh"
[ -x "$RESOLVER" ] || RESOLVER="$HOME/code/maestro/baton/scripts/resolve-context.sh"
CTX="$("$RESOLVER")" || { echo "$CTX"; exit 1; }
export BEADS_DIR="$(echo "$CTX" | jq -r '.task_tracking.dir' | sed "s|^~|$HOME|")"
WS="$(echo "$CTX" | jq -r '._workspace')"
GUIDE="$WS/$(echo "$CTX" | jq -r '.guidance // "guidance.md"')"
```

`LEAF` = `$ARGUMENTS` if given; else derive from the current branch (`<leaf-id>-<slug>`); else
ask. `bd show "$LEAF" --json`. Read `$GUIDE` and honor it.

Check the bead's labels for `autonomous-safe` (`AUTONOMOUS=yes`/`no`). This changes Step 3 and
Step 7 below — everywhere else in this skill behaves the same regardless. It never changes the
hard gates: a failing `pre_finish` hook or red CI still stops the flow for a human, autonomous or
not.

### Step 2 — Run worker.pre_finish hooks

Run each action in `hooks.worker.pre_finish` (e.g. `npm test`, lint, build). If any fails, stop
and report — don't close the bead over a failing gate unless the user overrides.

### Step 3 — Verify acceptance criteria

List the bead's acceptance criteria and their status. **Same-session shortcut:** if this session
already implemented and verified them, state which and how (one line each) and ask a single
confirmation rather than re-checking each — unless `AUTONOMOUS=yes`, in which case state them and
proceed without waiting for that confirmation (there's no human expected to be watching this
session). If any are unmet, list them and ask how to proceed — do not silently implement; this
applies even when `AUTONOMOUS=yes`, since unmet acceptance criteria are a real blocker, not a
confirmation gate.

One valid answer to "how to proceed" is that this worktree's work is genuinely done and merged,
but one or more criteria are *deliberately deferred* rather than blocked — e.g. a criterion that
can't be verified until more time/data has elapsed. In that case the bead should stay open for
the follow-up, not be closed and not be treated as stuck. When the user confirms that reading (or
states it up front), set `LEFT_OPEN=yes` and carry it into Steps 5 and 7 below: Step 5 skips
closing the bead, and Step 7 applies the `keep-task-open` label instead.

### Step 4 — Documentation pass

Follow the shared procedure in `../../references/doc-check.md` (resolve relative to
`${CLAUDE_PLUGIN_ROOT:-$HOME/code/maestro/baton}`). This is a backstop — if `baton:pr` already
ran for this task's PR, its own documentation pass (its Step 4) likely already caught anything
worth catching, so expect this to often find nothing new. It still matters for tasks that skipped
`baton:pr`, or where something changed after the PR opened.

If the PR is already merged, "same PR" isn't an option — offer a new PR now, or filing a backlog
item, instead.

### Step 5 — Close the leaf (unless intentionally left open)

If `LEFT_OPEN=yes` from Step 3, skip this step entirely — do not close the bead. The
`keep-task-open` label applied in Step 7 is what communicates "this worktree is done, the open
bead is intentional" in its place.

Otherwise:

```bash
bd close "$LEAF" --reason "<one-line summary of what was done>"
```

If a `parent` exists and this was its last open child, note that the parent is now unblocked /
can close (respect any `all-children` gate).

### Step 6 — Run post_finish hooks

Run each action in `hooks.worker.post_finish` (from `$CTX`).

### Step 7 — Signal readiness for cleanup (do NOT delete your own cwd)

A worktree can never remove itself — a session cannot delete the directory it's running in, and
per explicit correction, it must not try even when the harness would technically survive it
(cwd disappearing out from under a live session). Removal always happens from a *different*
session, later. This step is about signaling "I'm done", not removing anything.

Two labels communicate different things here, and `baton:cleanup-worktrees` reads both:
- `ready-for-worktree-delete` — this worktree/branch has no more work to do; safe to remove.
- `keep-task-open` — the bead is being left open on purpose (Step 3/5 concluded `LEFT_OPEN=yes`):
  more work is coming (e.g. a follow-up criterion needs elapsed time/data), so its open status is
  not an anomaly. Only ever applied alongside `ready-for-worktree-delete` — a bead is
  "intentionally still open" only in the context of a worktree that's already done; on its own it
  means nothing.

If you're running from **outside** the worktree for `$LEAF` and its branch is already merged +
clean, you may just offer to remove it now (same logic as `baton:cleanup-worktrees`) and skip the rest of
this step.

Otherwise, check whether `$LEAF`'s branch is merged, using the shared helper — the same one
`baton:resume` (Step 4) and `baton:cleanup-worktrees` (Step 3) call, so all three read the same
evidence and a fix to the ladder lands once:

```bash
MS="${CLAUDE_PLUGIN_ROOT:-$HOME/code/maestro/baton}/scripts/merge-state.sh"
[ -x "$MS" ] || MS="$HOME/code/maestro/baton/scripts/merge-state.sh"
BR="$(git rev-parse --abbrev-ref HEAD)"
M="$("$MS" --branch "$BR" --checks --format env)" || M=""
eval "$M"   # MERGED MERGE_SIGNAL GH_STATUS HAS_WORK PR_STATE PR_NUMBER FAILING PENDING
[ -n "${MERGED:-}" ] || { MERGED=no; MERGE_SIGNAL=none; FAILING=; PENDING=; }   # helper missing
```

It fetches, prefers `gh pr view` over git ancestry — `git branch --merged` false-negatives on
squash and rebase merges, since those create a new commit on the target branch that isn't an
ancestor of the feature branch — and falls back to ancestry only when there's no PR (e.g.
`in-place` work mode, or a direct push with no PR at all) or when `gh` is unusable. `--checks`
gets the PR's check runs from the same call, so `$FAILING` and `$PENDING` below are already
populated (newline-separated check names; empty when green, or when there's no open PR).
`$MERGE_SIGNAL` says which rung answered — mention it when it isn't `pr`.

- **`MERGED=yes`** (the PR is already in): apply the explicit cleanup signal to the leaf bead —
  ```bash
  bd label add "$LEAF" ready-for-worktree-delete
  [ "$LEFT_OPEN" = yes ] && bd label add "$LEAF" keep-task-open
  ```
  Tell the user: this worktree is flagged `ready-for-worktree-delete`; a later home session
  (`baton:cleanup-worktrees`, or the `cleanup` startup task) will remove the worktree, branch, and
  matching tmux session — this session doesn't touch any of that itself. If `LEFT_OPEN=yes`, also
  say that the bead was left open on purpose and is flagged `keep-task-open`, so cleanup won't
  treat that as an anomaly.
- **`MERGED=no`** (PR not merged yet, or no PR): do **not** apply the label yet — labeling now
  would be a false signal. Before telling the user to come back later, always report the open
  PR's check-run status from `$FAILING`/`$PENDING` above — this is the point of this step, not an
  optional extra:

  - If `$FAILING` is non-empty: flag it by name and ask the user how to proceed — (a) investigate
    and fix (`gh run view --log-failed` on the failing run, reproduce, fix, push, re-check), or
    (b) merge anyway. "Merge anyway" means: stop blocking on it, tell the user the checks are red
    but merging is their informed call, and continue with the messaging below. **This applies even
    when `AUTONOMOUS=yes`** — a red check is a hard stop regardless of the flag; autonomous-safe
    means skipping the wait for a human's OK on a *green* PR, not authorizing a merge over failing
    checks. Never let a red check pass by silently, autonomous or not.
  - If `$PENDING` is non-empty and `$FAILING` is empty:
    - `AUTONOMOUS=no` (default): note that checks are still running — no need to block, just flag
      it so the user knows to re-check shortly, then fall through to the "come back later"
      messaging below.
    - `AUTONOMOUS=yes`: don't just flag it and wait for the user to come back — actually wait
      here, since no human is expected to be watching this session:
      ```bash
      gh pr checks "$BR" --watch
      ```
      This blocks until every check finishes, and exits non-zero if any failed. When it returns,
      re-run the `merge-state.sh --checks` call above to refresh `$FAILING`/`$PENDING` — don't
      infer the outcome from its exit status. If `$FAILING` is now non-empty, treat it
      exactly as the failing-checks case above (flag + ask — never auto-merge red). Otherwise fall
      through to the green-checks case below, now that checks have actually finished.
  - If both are empty (checks are green, or none are configured):
    - `AUTONOMOUS=no` (default): no extra messaging needed — fall through to the "come back
      later" messaging below.
    - `AUTONOMOUS=yes`: merge now, using the merge-commit strategy (not squash):
      ```bash
      gh pr merge "$BR" --merge
      bd label add "$LEAF" ready-for-worktree-delete
      [ "$LEFT_OPEN" = yes ] && bd label add "$LEAF" keep-task-open
      ```
      Tell the user this leaf was autonomous-safe, checks were green, so the PR was merged
      automatically and the worktree is flagged `ready-for-worktree-delete` (plus `keep-task-open`
      if `LEFT_OPEN=yes`) for the next
      `baton:cleanup-worktrees` pass — same as the `MERGED=yes` case above. Skip the "come back
      later" messaging entirely; there's nothing left to come back for.

  For the `AUTONOMOUS=no` cases above (or if merging failed/was declined), tell the user the
  worktree stays as-is until the PR merges, and that they should come back to *this* session
  (don't close it) and ask to re-check once it has — do not rely on remembering to start a fresh
  session soon after merge; the label is what makes cleanup eventually happen even if that's much
  later.

### Step 8 — Retrospective (if enabled)

Print "Done. `<LEAF>` closed." when `LEFT_OPEN` was not set, or "Done. `<LEAF>` left open
(`keep-task-open`)." when it was — throughout this step, substitute whichever applies.

If `AUTONOMOUS=yes`, skip this step — there's no human present to ask, and blocking here would
defeat the point of the flag. Print the appropriate line above (plus the merge outcome from
Step 7) and stop.

Otherwise, if `retro.enabled` is true and `finish` is in `retro.when`, ask:

> Anything in this flow that could have gone better?

- No / nothing → print the appropriate line above and stop.
- Feedback → identify whether it implies a durable preference, a hook, or a config change, and
  **propose the specific edit to this workspace** (append to `$GUIDE`, add a `hooks.*` action, or
  change a `context.yaml` field). Apply only after the user confirms — this is `baton:remember`'s
  write path. Never edit the plugin. Then print the appropriate line above with "; guidance
  updated." appended.
