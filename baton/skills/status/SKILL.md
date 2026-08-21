---
description: (Worker session) Report where THIS worktree's task stands — the bead and its acceptance criteria, blockers, PR and check state, and exactly one overall state from a fixed vocabulary (unstarted, blocked, in progress, waiting on CI, waiting on the human, merged but not finished, done and awaiting worktree delete, done with follow-ups elsewhere). Strictly read-only — it fires no hooks, claims nothing, closes nothing, and starts no work — so it is safe to run purely to look around. No-ops cleanly outside a baton worktree.
allowed-tools: Bash(*), Read, Glob, Grep
---

## baton:status

Answer "what is *this* worktree's task, and where does it stand?" and stop.

**This skill is strictly read-only, and that is its entire value.** A report you have to think
twice about running is a report you won't run. So: no `on_resume` (or any other) hooks, no `bd`
writes, no claiming, no closing, no labeling, no branch or worktree changes, and no routing into
`baton:resume` or `baton:finish` — even when the state obviously calls for one. Name the skill to
run next; never run it. Running this twice in a row must leave the machine exactly as it was.

Two things it *does* touch, both deliberate and neither a change to your work: `merge-state.sh`
runs `git fetch origin` (remote-tracking refs only — no local branch, no working tree, no index),
and `gh pr view` reads from GitHub. Both are what make the answer current rather than stale. If
you want neither, say so and they can be skipped, at the cost of a possibly out-of-date PR state.

It is the read-only sibling of `baton:resume`, which computes much the same picture and then
*acts* on it. Related but distinct: `baton:whereami` answers "which **context** am I in" (config
debugging, not per-task), and `baton:session-start`'s `status` task answers "what should I pick
up next" across the whole context. This one is about the single task this worktree serves.

### Step 1 — Am I in a baton worktree?

```bash
git rev-parse --git-dir >/dev/null 2>&1 || { echo "Not a git repo — nothing to report."; exit 0; }
IDENT="${CLAUDE_PLUGIN_ROOT:-$HOME/code/maestro/baton}/scripts/task-identity.sh"
[ -x "$IDENT" ] || IDENT="$HOME/code/maestro/baton/scripts/task-identity.sh"
ID="$("$IDENT" --worktree "$PWD" --no-backfill --format env)" \
  || { echo "Not a baton worktree — no task to report on."; exit 0; }
eval "$ID"          # capture first: `eval "$(cmd)"` would swallow cmd's exit status
                    # LEAF SLUG BR DIR SESSION_NAME SESSION_TITLE IDENTITY_SOURCE
```

`--no-backfill` matters here and nowhere else in baton. `--worktree` normally *writes* the
identity carrier back into the worktree's git dir whenever a fallback rung answered — a good
thing for every other caller, and a violation of this skill's one promise. Keep the flag.

**Do not parse the branch name yourself**, here or anywhere. `naming.branch` is configurable, so
a branch like `DOT-1234/some-description` carries no bead id at all. If the helper resolves
nothing, this isn't a baton worktree: say so plainly (that is the `not a baton worktree` answer
in the vocabulary below, and it is a normal outcome, not an error) and stop. `$IDENTITY_SOURCE`
says which rung answered — mention it when it wasn't `carrier`, since a `dir`/`branch` answer
means the worktree predates 0.5.0 and would normally have been backfilled by now.

### Step 2 — Resolve context + tracker

```bash
RESOLVER="${CLAUDE_PLUGIN_ROOT:-$HOME/code/maestro/baton}/scripts/resolve-context.sh"
[ -x "$RESOLVER" ] || RESOLVER="$HOME/code/maestro/baton/scripts/resolve-context.sh"
CTX="$("$RESOLVER")" || { echo "$CTX"; exit 1; }
export BEADS_DIR="$(echo "$CTX" | jq -r '.task_tracking.dir' | sed "s|^~|$HOME|")"
```

Read-only means read-only about the tracker too: **do not `bd dolt pull`** here, however tempting
a fresh view is. That's `align`'s job in `baton:session-start`. If the numbers look stale, say so
rather than syncing.

### Step 3 — Read the bead

```bash
BEAD="$(bd show "$LEAF" --json 2>/dev/null)"
BEAD_STATUS="$(jq -r 'if type=="array" then .[0] else . end | .status // "unknown"' <<<"$BEAD")"
[ -n "$BEAD_STATUS" ] || BEAD_STATUS=unknown   # bd or jq failed — never let that read as "open"
LABELS="$(bd label list "$LEAF" 2>/dev/null)"  # pass through verbatim; the scripts tolerate bullets

# Open blockers (what this waits on) and open dependents (what waits on this). The scripts take
# only OPEN ids — a closed edge is not a blocker — so filter here, and keep `blocks` edges only:
# a `parent-child` record is a hierarchy, not a gate.
BLOCKERS="$(bd dep list "$LEAF" --json 2>/dev/null \
  | jq -r '.[]? | select(.dependency_type=="blocks" and .status!="closed") | .id')"
UNBLOCKS="$(bd dep list "$LEAF" --direction=up --json 2>/dev/null \
  | jq -r '.[]? | select(.dependency_type=="blocks" and .status!="closed") | .id')"
PARENT="$(bd dep list "$LEAF" --json 2>/dev/null \
  | jq -r '.[]? | select(.dependency_type=="parent-child") | .id')"
```

`bd show --json` emits a single-element **array**, not a bare object (confirmed on bd 1.1.0),
hence the `type=="array"` guard — a bare `.status` makes jq exit 5 and the `// "unknown"` default
never fires, silently yielding an empty status that reads like an ordinary open bead.

If the bead isn't found, don't stop: report everything git can still tell you, and say the leaf
id from the worktree carrier doesn't resolve in this context's tracker (usually the wrong context,
or a tracker that hasn't been pulled). `$BEAD_STATUS` stays `unknown` and the state machine
degrades on its own.

### Step 4 — Ask git and GitHub where the branch stands

The same shared helper `baton:resume` (Step 4), `baton:finish` (Step 7) and
`baton:cleanup-worktrees` (Step 3) use, so all four agree about one branch:

```bash
MS="${CLAUDE_PLUGIN_ROOT:-$HOME/code/maestro/baton}/scripts/merge-state.sh"
[ -x "$MS" ] || MS="$HOME/code/maestro/baton/scripts/merge-state.sh"
M="$("$MS" --branch "$BR" --checks --format env)" || M=""   # capture first, then eval
eval "$M"   # MERGED MERGE_SIGNAL GH_STATUS MERGE_BASE HAS_WORK PR_STATE PR_NUMBER FAILING PENDING
[ -n "${MERGED:-}" ]   || MERGED=unknown     # helper missing (stale cache) — never reads as merged
[ -n "${HAS_WORK:-}" ] || HAS_WORK=unknown   # and never as "nothing outstanding"
DIRTY="$([ -z "$(git status --porcelain)" ] && echo no || echo yes)"
```

**Never hand-roll this with `git log main..HEAD`.** An empty range means "already merged" *and*
"nothing done yet" equally — opposite situations wanting opposite reports. The helper separates
them by where the tip sits, prefers `gh pr view` over ancestry (ancestry false-negatives on
squash and rebase merges), and reports which rung answered in `$MERGE_SIGNAL` (`pr` / `ancestry`
/ `none`) and why in `$GH_STATUS`. Say which signal answered whenever it wasn't `pr`, and when
`$MERGE_SIGNAL` is `none` say the merge state is genuinely unknown rather than implying "no".

`--checks` returns the PR's check runs from the same `gh` call, so `$FAILING` and `$PENDING` cost
nothing extra. Both are empty when there's no open PR.

### Step 5 — Reduce it to one state

Two scripts, in order. `cleanup-verdict.sh` owns "is this finished" (the same rules
`baton:cleanup-worktrees` removes worktrees on, so status can never call something done that
cleanup wouldn't); `task-state.sh` layers the not-yet-done states underneath it:

```bash
CV="${CLAUDE_PLUGIN_ROOT:-$HOME/code/maestro/baton}/scripts/cleanup-verdict.sh"
[ -x "$CV" ] || CV="$HOME/code/maestro/baton/scripts/cleanup-verdict.sh"
V="$("$CV" --labels "$LABELS" --state "$BEAD_STATUS" --merged "$MERGED" \
           --has-work "$HAS_WORK" --dirty "$DIRTY" --format env)" || V=""
eval "$V"      # VERDICT VERDICT_REASON RELAXED LABELED KEEP_OPEN NO_PR_NEEDED ...
[ -n "${VERDICT:-}" ] || { VERDICT=unknown; VERDICT_REASON=""; }

TS="${CLAUDE_PLUGIN_ROOT:-$HOME/code/maestro/baton}/scripts/task-state.sh"
[ -x "$TS" ] || TS="$HOME/code/maestro/baton/scripts/task-state.sh"
S="$("$TS" --verdict "$VERDICT" --verdict-reason "$VERDICT_REASON" --labels "$LABELS" \
           --status "$BEAD_STATUS" --merged "$MERGED" --has-work "$HAS_WORK" --dirty "$DIRTY" \
           --pr-state "$PR_STATE" --pr-number "$PR_NUMBER" \
           --failing "$FAILING" --pending "$PENDING" \
           --blockers "$BLOCKERS" --unblocks "$UNBLOCKS" --format env)" || S=""
eval "$S"      # STATE STATE_LABEL STATE_REASON STATE_NEXT STATE_NOTES
[ -n "${STATE:-}" ] || { STATE=unknown; STATE_LABEL=unknown; STATE_REASON="task-state.sh unavailable — is the plugin current? (baton:doctor)"; STATE_NEXT=""; STATE_NOTES=""; }
```

**Report `$STATE` as computed. Do not talk yourself into a different one.** The whole point of
putting the ladder in a script is that the two shapes git cannot tell apart — a fresh worktree and
a task whose work deliberately landed outside git — stop being decided by whoever is reading the
output today. The vocabulary is closed; every possible answer is in it:

| state | means | typical next step |
|---|---|---|
| `unstarted` | no commits, no PR, clean tree | `baton:resume` |
| `blocked` | an open blocker bead stands in the way | finish the blocker's own worktree |
| `in-progress` | commits or uncommitted changes, no PR yet | keep going, then `baton:pr` |
| `waiting-on-ci` | PR open, checks still running | wait; `baton:finish` once green |
| `waiting-on-human` | PR open and needing a person — green and awaiting review, **red and needing a look**, or closed without merging | review/merge, or fix the red checks |
| `merged-needs-finish` | the branch landed but `baton:finish` never ran | `baton:finish` |
| `done-awaiting-delete` | finished and flagged `ready-for-worktree-delete` | nothing — a later `baton:cleanup-worktrees` removes it |
| `done-followups-elsewhere` | finished, with follow-up work on another bead (`keep-task-open`, or it unblocks an open bead) | nothing here; the work moved |
| `unknown` | the signals couldn't decide, and it says which were missing | usually `baton:doctor` |
| *not a baton worktree* | Step 1 resolved no leaf — reported there, never reaches the scripts | — |

A red check is deliberately still `waiting-on-human`, not a state of its own: it needs a person
exactly as a green PR awaiting review does. `$STATE_REASON` leads with the failing check names —
put them in your headline too, don't let "waiting on the human" bury a broken build.

### Step 6 — Assess the acceptance criteria

List the bead's acceptance criteria and mark each one **met / unmet / can't tell** against what's
actually in the worktree — read the diff (`git diff "$MERGE_BASE"...HEAD --stat`, then the files
that matter), the tests, the docs. This is the one judgment call in the skill; everything else is
assembled from signals.

Two rules, both from the read-only contract:
- **`can't tell` is a real answer.** A criterion like "firmware flashed and confirmed working" or
  "confirmed in real use" cannot be checked from a diff. Say so; don't infer it from the code
  looking right, and don't quietly drop it.
- **Never act on an unmet criterion here.** Not even a one-line fix, and not even when the fix is
  obvious. Report it and name `baton:resume` as the way to pick the work up.

If `$BEAD_STATUS` is `unknown` (Step 3 couldn't read the bead), skip this step and say why.

### Step 7 — Render

One compact report. Suggested shape — adapt it, but keep the state on its own line near the top,
and keep the reason next to it so the answer is never a bare word:

```
jbh-sca — Add a read-only baton:status skill for the current worktree's task
State:  in progress — commits on this branch that the base does not have; no PR yet
Bead:   in_progress · labels: baton, repo-maestro · worktree jbh-sca-baton-status-skill (carrier)

Acceptance criteria
  ✅ reports bead id + title, criteria, blockers, PR/check state, one overall state
  ✅ identity comes from task-identity.sh; no branch parsing
  ❔ verifiably read-only — asserted by construction; not yet exercised twice on a live worktree

Blockers:  none            Unblocks:  none
PR:        none yet        Branch:    3 commits ahead of origin/main (signal: ancestry, gh: no-pr)
Next:      keep going; run baton:pr once the acceptance criteria are met
```

Then, always:
- Print every line of `$STATE_NOTES`. They exist because a signal lost the headline (an open
  blocker under an open PR), or contradicted another (a `ready-for-worktree-delete` label on an
  unmerged branch), or was missing entirely. None of that is noise; the notes are where this
  report earns its keep over a glance at `git status`.
- Close with `$STATE_NEXT` as a *suggestion*, and stop. Do not run it, and do not offer to run it
  as this skill — if the user wants the action, they'll invoke `baton:resume`, `baton:pr`,
  `baton:finish` or `baton:cleanup-worktrees` themselves.
