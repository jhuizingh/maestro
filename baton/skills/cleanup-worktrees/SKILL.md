---
description: Review git worktrees in the active context and clean up the finished ones. Scoped to the active context by default; pass --context <name> or --all-contexts to widen it. A worktree is a confirmed candidate when its leaf bead carries the `ready-for-worktree-delete` label (applied by `baton:finish` once merged) AND the merged/clean signals agree, AND either the bead is closed or it carries `keep-task-open` (an explicit "left open on purpose" signal) — those are auto-removed with no prompt. A task that deliberately produced nothing to merge carries `no-pr-needed`, which stands in for the merge signal when git agrees nothing is outstanding. Everything else still requires explicit per-worktree confirmation.
argument-hint: "[--context <name>] [--all-contexts]"
allowed-tools: Bash(*)
---

## baton:cleanup-worktrees

Find worktrees that look done and clean them up. Confirmed-ready worktrees (every signal agrees)
are removed automatically — that's the strongest possible evidence a worktree is done, so asking
every time is just friction. Everything less certain still requires an explicit yes before
anything is touched.

Two independent families of signal feed this: the **labels** a worker session applied when it
finished the bead (an explicit "I'm done" — see `baton:finish` Step 7) and the derived **closed +
merged + clean** state (re-checked here from git and the tracker directly). Neither is trusted
alone — the labels are the intentional signal, the derived state is the cross-check that catches
a stale or wrong label.

`ready-for-worktree-delete` is the "I'm done" label. Two narrower ones **modify** that
cross-check rather than adding to it — each relaxes exactly one signal, and no other:

| label | relaxes | means |
|---|---|---|
| `keep-task-open` | `STATE` | the bead is open **on purpose**: a worker concluded some acceptance criteria are deliberately deferred, not blocking — e.g. waiting on elapsed time or data — while this specific worktree's work is done and merged. `STATE == closed` is no longer required, because an open bead here is expected, not an anomaly. |
| `no-pr-needed` | `MERGED` | the task deliberately produced **nothing to merge**: the work happened outside git (e.g. a REST API against a live system), or landed with no PR at all. `MERGED == yes` can never arrive for such a branch, so requiring it means waiting forever. |

Neither ever appears without `ready-for-worktree-delete` — on its own, neither has any meaning.

Since 0.8.0 all three are read **per branch** when the branch has an entry in the task's branch
registry (written by `baton:start`, updated by `baton:pr` and `baton:finish`), and from the
task's labels otherwise. Same three signals, same rules; the difference is whether "I'm done"
belongs to *this* branch or to the task as a whole. See Step 3 and the label-scoping section.

`no-pr-needed` is a **claim**, not a check, so it is guarded: it stands in for `MERGED` only when
git independently agrees nothing is outstanding — `HAS_WORK == no` (the branch adds no commits
the base doesn't already have) and a clean tree. That guard is what keeps `merge-state.sh`'s
deliberate bias toward "not merged" intact: a branch with real unmerged commits (`HAS_WORK ==
yes`) is never relaxed by any label, so a wrong label costs a worktree kept too long, never lost
commits. The rule lives in `scripts/cleanup-verdict.sh` (Step 3), not in this prose.

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

Keep each context's resolved JSON in `CTX_JSON` as you go — Step 3 hands it to the tracker seam
with `--context -`, which is how a `--all-contexts` scan reads each context's own tracker without
exporting anything or re-running the resolver per call.

### Step 2 — Enumerate worktrees per context

For each context, for each member repo, list worktrees:
```bash
git -C "<repo>" worktree list --porcelain     # reports `worktree <path>` and `branch refs/heads/<br>`
```

**Take the branch from that output — never from the directory name.** A worktree dir is
*initially* named per `naming.dir`, but a worktree can be re-pointed at a new branch (`git
switch`) while keeping its original name, so the directory records whatever the **first** branch
was. Measured in the wild: dir `jbh-0vj-bidirectional-todoist-beads-sync-engine` on branch
`jbh-0vj-todo-sync`.

Since 0.5.0 the two names are also independently configurable (`naming.branch` / `naming.dir`),
so `basename <wt>` differing from the branch is **often just the context's naming** — a
`naming.branch: "{jira}/{slug}"` context produces dir `art-xyz-my-thing` on branch
`DOT-1234/my-thing` for every task it ever starts. Report a dir/branch difference in Step 5 only
when the context's own `naming.branch` and `naming.dir` templates are equal (i.e. the two were
*meant* to match); otherwise it carries no information. Either way neither name decides identity
— that comes from the worktree, in Step 3.

### Step 3 — Classify each worktree

For each worktree, recover the identity group **from the worktree itself**, compute the label and
cross-check signals (the tracker seam is pointed at this context via `--context -`), then hand
them to `cleanup-verdict.sh` for the bucket:

```bash
IDENT="${CLAUDE_PLUGIN_ROOT:-$HOME/code/maestro/baton}/scripts/task-identity.sh"
[ -x "$IDENT" ] || IDENT="$HOME/code/maestro/baton/scripts/task-identity.sh"
TRK="${CLAUDE_PLUGIN_ROOT:-$HOME/code/maestro/baton}/scripts/tracker.sh"
[ -x "$TRK" ] || TRK="$HOME/code/maestro/baton/scripts/tracker.sh"
ID="$("$IDENT" --worktree "<wt>" --format env)" || continue   # not a baton worktree
eval "$ID"                          # LEAF SLUG BR DIR SESSION_NAME SESSION_TITLE SESSION_NAME_LEGACY IDENTITY_SOURCE

# The tracker, through the one seam. --context - hands it this context's already-resolved JSON,
# so a multi-context scan reads each context's own tracker without exporting anything.
task() { "$TRK" --context - "$@" <<<"$CTX_JSON"; }

BEAD="$(task get "$LEAF" 2>/dev/null)" || BEAD=""
[ -n "$BEAD" ] || BEAD='{}'
STATE="$(jq -r '.status // "unknown"' <<<"$BEAD")"
[ -n "$STATE" ] || STATE=unknown    # an empty STATE must never read as "not closed"

# READINESS. Three cases, and the middle one is the reason this is not a two-branch `if`.
#
# Match on repo AND branch: the scan runs per member repo, and one task can own the same branch
# name in two of them — matching on the name alone reads the finished one's readiness for the
# live one, which is the same class of bug the registry exists to fix, one level down.
REGS="$(task list-branches "$LEAF" 2>/dev/null)"; [ -n "$REGS" ] || REGS='[]'
REG="$(jq -c --arg br "$BR" --arg repo "<repo>" \
         '[ .[]? | select(.branch == $br and ((.repo // "") == $repo or (.repo // "") == "")) ]
          | first // empty' <<<"$REGS")"
NBR="$(jq 'length' <<<"$REGS")"; [ -n "$NBR" ] || NBR=0

# Does the entry actually SAY anything about readiness? An entry always exists after
# `baton:start` (record-branch writes repo/branch/worktree/created/status and nothing else), so
# "an entry exists" is not the same question as "readiness was recorded against this branch".
if [ -n "$REG" ] && jq -e 'has("ready") or has("keep_task_open") or has("no_pr_needed")' \
     >/dev/null 2>&1 <<<"$REG"; then
  LABEL_SCOPE=branch
  LABELS="$(jq -r '[ (select(.ready=="yes")          | "ready-for-worktree-delete"),
                     (select(.keep_task_open=="yes") | "keep-task-open"),
                     (select(.no_pr_needed=="yes")   | "no-pr-needed") ] | join(" ")' <<<"$REG")"
elif [ "$NBR" -le 1 ]; then
  # Nothing branch-scoped to read, and the task has at most one branch on record — so its labels
  # cannot be referring to a different branch. This is the fallback the docs promise: a
  # pre-0.8.0 worktree, a backend with no registry, and a `baton:finish` whose `update-branch`
  # failed while its `label-add` succeeded all land here and still work.
  LABEL_SCOPE=bead
  LABELS="$(jq -r '.labels // [] | join(" ")' <<<"$BEAD")"
else
  # The task owns several branches and THIS one records no readiness. Task labels are ambiguous
  # here by construction — they may well have been applied for a sibling branch — so borrowing
  # them is exactly the bead-scoped false positive this feature was filed to remove. Read no
  # readiness at all; the worktree lands in a bucket that asks a human rather than one that
  # deletes.
  LABEL_SCOPE=branch-unrecorded
  LABELS=""
fi
MS="${CLAUDE_PLUGIN_ROOT:-$HOME/code/maestro/baton}/scripts/merge-state.sh"
[ -x "$MS" ] || MS="$HOME/code/maestro/baton/scripts/merge-state.sh"
M="$("$MS" --repo <repo> --branch "$BR" --format env)" || M=""
eval "$M"                           # MERGED MERGE_SIGNAL GH_STATUS MERGE_BASE HAS_WORK PR_STATE PR_NUMBER
[ -n "${MERGED:-}" ]   || MERGED=unknown    # helper missing (stale cache) — never reads as "merged"
[ -n "${HAS_WORK:-}" ] || HAS_WORK=unknown  # and an absent HAS_WORK must never read as "nothing outstanding"
DIRTY_TEXT="$(git -C <wt> status --porcelain)"                          # empty = clean
DIRTY="$([ -z "$DIRTY_TEXT" ] && echo no || echo yes)"

CV="${CLAUDE_PLUGIN_ROOT:-$HOME/code/maestro/baton}/scripts/cleanup-verdict.sh"
[ -x "$CV" ] || CV="$HOME/code/maestro/baton/scripts/cleanup-verdict.sh"
V="$("$CV" --labels "$LABELS" --label-scope "$LABEL_SCOPE" --state "$STATE" --merged "$MERGED" \
           --has-work "$HAS_WORK" --dirty "$DIRTY" --format env)" || V=""
eval "$V"      # VERDICT VERDICT_REASON RELAXED LABELED KEEP_OPEN NO_PR_NEEDED LABEL_SCOPE STATE_OK MERGED_OK CLEAN
[ -n "${VERDICT:-}" ] || { VERDICT=not-ready; VERDICT_REASON="cleanup-verdict.sh unavailable"; }
```

`--worktree` reads the identity carrier `baton:start` wrote into the worktree's own git dir,
falling back to the legacy `<leaf>-<slug>` shape of the directory name and then the branch for
worktrees created before 0.5.0, and backfilling the carrier when a fallback answered. **This
skill deletes things, so it must never guess an identity from a name it merely recognizes.** The
carrier is per-worktree by construction; note in particular that `git config` is not, and would
have made every live worktree of a repo report the same leaf.

The helper exits non-zero when a worktree has no carrier and neither name is `<leaf>-<slug>` —
that's the primary clone (`main`) or a hand-made worktree, not a baton one. Skip those entirely;
never offer them for removal. The directory-name rung is deliberately skipped for the primary
clone, whose directory is the *repository's* name: repo names like `jbh-task-tracking` match the
legacy shape by accident and would otherwise invent a leaf out of nothing.

`tracker.sh` is baton's one seam to the task tracker; `task_tracking.type` picks the backend
behind it, so this skill works the same whichever one a context uses. **Never call `bd` here.**
`get` returns a bare JSON object with a fixed field set, which is what makes `.status` and
`.labels` readable directly: `bd show --json` actually emits a single-element **array**, and a
bare `.status` against one makes jq exit 5 with `Cannot index array with string "status"` — with
stderr discarded the `// "unknown"` default never fires. That produced an empty `STATE` on every
run of this skill, silently killing both the `STATE == closed` half of confirmed-ready and the
entire looks-done-unlabeled bucket, for as long as each call site carried its own guard. It is
absorbed once now. The `[ -n "$STATE" ]` fallback stays so a future failure surfaces as a
reportable `unknown` rather than an empty string indistinguishable from an ordinary open bead.

**Readiness is read per branch when the branch has a registry entry.** `baton:start` records each
worktree it creates against the task (repo, branch, worktree path, created, status);
`baton:pr` and `baton:finish` move that entry's status, and `baton:finish` sets `ready=yes` on it
alongside the task labels. So the three signals this skill acts on can now be asked of *this
branch* rather than of the task as a whole — which is the fix to the label-scoping limitation
documented below. `$LABEL_SCOPE` records which family answered and is passed to
`cleanup-verdict.sh`, which names it in the reason; report it, since "this branch was declared
done" and "this task was, possibly by different work" are different claims.

A worktree whose entry records **no readiness** — anything created before 0.8.0, a backend with
no registry, a freshly-started worktree, or a `baton:finish` whose `update-branch` failed — falls
back to the task's labels and `$LABEL_SCOPE=bead`, exactly as before, *provided the task owns at
most one recorded branch*. That proviso is the whole point: with one branch the task labels can
only be about it; with several they cannot be told apart, so they are not borrowed and the scope
is `branch-unrecorded`. The fallback is not a degraded mode to warn about; it is the correct
answer for a worktree that predates the registry.

`merge-state.sh` is the shared merge-state ladder — `baton:finish` (Step 7) and `baton:resume`
(Step 4) ask it the same question, so a worktree's merged status can't be judged one way here and
another way there. It fetches, prefers `gh pr view` over git ancestry (`git branch --merged`
false-negatives on squash and rebase merges, since a new commit lands on the target that isn't an
ancestor of the feature branch), falls back to ancestry when there's no PR or `gh` is unusable,
and reports which rung answered in `$MERGE_SIGNAL`. Surface `$MERGE_SIGNAL`/`$GH_STATUS` in the
Step 5 report whenever the signal wasn't `pr`, so "not merged" from a machine with no `gh` is
never mistaken for a checked fact.

`cleanup-verdict.sh` turns those signals into one of four buckets. **Do not re-derive the rules
here** — the guard on `no-pr-needed` is what stands between a mislabeled bead and deleted work, so
it is executable and covered by `scripts/test-cleanup-verdict.sh` rather than restated in prose
each time this skill is read. What `$VERDICT` means:

| `$VERDICT` | what it means | what Step 4 may do |
|---|---|---|
| `confirmed-ready` | the "I'm done" label is present and every cross-check agrees — with `$RELAXED` naming any that a modifier label stood in for | remove, no prompt |
| `looks-done-unlabeled` | bead closed and nothing outstanding, but no `ready-for-worktree-delete` (finished before the label existed, or via a work mode that never ran `baton:finish`) | offer, ask first |
| `label-state-mismatch` | labeled ready, but something disagrees — bead reopened with no `keep-task-open` cover, real unmerged commits, or a dirty tree | flag; never offer |
| `not-ready` | in progress, or never started | never touched |

`$VERDICT_REASON` is a ready-to-print one-liner naming the signals that decided it; use it rather
than composing your own. `$RELAXED` lists which cross-checks a modifier label stood in for
(`state`, `merged`, or both) — report it, so an automatic removal says *why* it needed no merge
instead of appearing to have found one.

Two properties worth knowing, both pinned by tests:
- Every input defaults to `unknown`, and no `unknown` satisfies any green condition. A missing
  helper, an unreadable tracker or an unfetchable branch always degrades toward **keeping** the
  worktree.
- `HAS_WORK == yes` — real commits the base doesn't have — is never relaxed by any label. A
  worktree with unmerged work cannot reach `confirmed-ready` however it is labeled; it lands in
  `label-state-mismatch` and gets flagged.

`STATE == unknown` is not an ordinary "not closed" — it means the bead lookup itself failed (bad
context, a deleted bead, or an unreadable tracker).
`$VERDICT_REASON` marks it explicitly; surface that marker rather than folding it into a bucket's
reasoning as though the bead were merely open, since every downstream classification is unreliable
for that worktree. It is still safe by construction — `unknown` can never satisfy
`STATE == closed`, so nothing gets removed on a lookup failure.

#### Label scoping (multi-worktree-per-bead) — fixed by the registry, with a fallback

`ready-for-worktree-delete`, `keep-task-open` and `no-pr-needed` live on the **task**
(`.labels`), not on any branch. If one task has two worktrees over its lifetime — an earlier one
that finished and was labeled ready (possibly with `keep-task-open`, since that is exactly the
scenario the label exists for), then a *later* worktree opened against that same still-open task
for the follow-up work — every one of those labels is visible from the new worktree too, even
though only the finished one is actually ready. This skill documented that as deferred, on the
grounds that beads had no branch-scoped label mechanism.

**It has one now**, and it is not a label: the branch registry on the task (0.8.0, see
`../../references/tracker.md`). `baton:finish` writes `ready=yes` — plus `keep_task_open` /
`no_pr_needed` where they apply — onto the entry for the branch it actually finished. Step 3
above prefers that entry, so the later worktree reads *its own* entry (or none) and is unaffected
by what the earlier one recorded. The false positive is gone for any worktree started since.

What remains, and why it is safe either way:

- **Pre-0.8.0 worktrees** record no readiness and still fall back to the task's labels
  (`$LABEL_SCOPE=bead`) whenever the task has at most one branch on record, so the old false
  positive can still occur for them. It was never a
  safety gap: the per-worktree `MERGED`, `HAS_WORK` and `DIRTY` checks in Step 3 are computed
  fresh from git every time, never from a label, so a still-active worktree fails its own checks
  and cannot reach confirmed-ready however the task is labeled. The worst outcome is a
  presentation issue — it lands in **label/state mismatch** rather than plain **not ready** — not
  a wrongful deletion. That holds for a stale `no-pr-needed` too, and is what its `HAS_WORK`
  guard buys: the follow-up worktree either has commits of its own (guard fails) or has none yet,
  in which case there is nothing there to lose.
- **A backend with no registry** behaves identically to a pre-0.8.0 worktree, by construction —
  `list-branches` returning nothing and a branch simply not being recorded are the same case, and
  both land on the task labels.
- **The task labels are still written** by `baton:finish`, deliberately. They are what
  `baton:whereami` counts, what a hand inspection reads, and what an older baton falls back to.
  The registry is preferred, not exclusive — but "preferred" is decided on whether the entry
  **records readiness**, not on whether an entry exists. `baton:start` records one for every
  worktree it creates and `record-branch` writes no readiness fields, so gating on mere existence
  would have suppressed the task-label fallback for every worktree created since 0.8.0 — turning
  a documented fallback into dead code, and stalling a `no-pr-needed` task forever, since without
  the branch-scoped flag the `MERGED` relaxation never fires and the worktree is never offered.
- **The fallback is not unconditional, either.** It applies while the task owns at most one
  recorded branch, where its labels cannot be referring to a different one. With several branches
  and no readiness on this one, the labels are ambiguous by construction, so they are not
  borrowed at all (`$LABEL_SCOPE=branch-unrecorded`) — that ambiguity is the bead-scoped false
  positive this feature exists to remove, and inheriting it in the fallback would reintroduce it.

**Naming, at least, already survives this.** The identity group is keyed on the *worktree*, not
the bead, so two worktrees for one leaf carry different slugs in their own carriers and therefore
get different `SESSION_NAME`s — teardown can't hit the wrong session. A collision would require
the same leaf *and* the same slug, which is a duplicate worktree directory git already rejects.

### Step 4 — Remove confirmed-ready automatically, ask for the rest

Show all four groups, each with bead id/title and reasoning.

**Confirmed ready** (`$VERDICT == confirmed-ready`) — remove immediately, no prompt. The
independent signals already agree (label, closed, merged, clean), so there's nothing left for a
human to confirm. When `$RELAXED` is non-empty, say which cross-check a modifier label stood in
for — "removed because a human said there was nothing to merge (`no-pr-needed`), and git agreed"
reads very differently from "removed because its PR merged", and only one of them is a claim
somebody made:

```bash
# $WT, $BR and the rest of the identity group are already exported by Step 3's eval.
git -C <repo> worktree remove "$WT" --force
git -C <repo> branch -d "$BR"
```

`git worktree remove` deletes the per-worktree git dir, and the identity carrier with it — so
this worktree cannot be asked what it was after this line runs. Everything downstream (the
`on_cleanup` hooks below included) must use the values Step 3 already exported, not re-derive
them.

**Looks done, unlabeled** — still ask, per worktree (or offer "remove all unlabeled-but-done" as
its own batch) — there's no explicit "I'm done" signal from a worker session here, so a human
should confirm before removing. Use the same removal commands once confirmed.

**Label/state mismatch** — never offer removal; it's an anomaly by definition. Flag it, print
`$VERDICT_REASON` so the disagreeing signal is named, and move on.

**Not ready** — never touched.

Never bundle groups together into a single blanket "remove all" — confirmed-ready acts on its
own (automatically), and unlabeled-but-done is its own separate ask.

For every removal (auto or confirmed), run the context's `hooks.home.on_cleanup` actions with the
full identity group in the environment — `$WT` (worktree path) plus `$LEAF`, `$SLUG`, `$BR`,
`$DIR`, `$SESSION_NAME`, `$SESSION_TITLE` from the Step 3 `eval`, all already exported — never
re-resolved from the now-deleted worktree.

This is where the worktree's tmux session gets torn down. `baton:configure` seeds `on_cleanup`
with:

```bash
tmux kill-session -t "$SESSION_NAME" 2>/dev/null || true
```

That target is **the same string `baton:start` used to create the session**, because both come
from `task-identity.sh` reading the same carrier — the teardown agrees with the launch by
construction, not by a human keeping two transforms in sync. It holds for custom `handoff.launcher`s too: a launcher is handed
`$SESSION_NAME` rather than deriving its own, so there's no naming to check.

If a context's `on_cleanup` is empty, say so — the tmux session will leak.

**Transitional:** worktrees started before the identity group existed are still named
`baton-<sanitized-branch>`, and are never renamed in flight. `$SESSION_NAME_LEGACY` is exported
alongside the rest for exactly that window; if a kill no-ops and `tmux ls` still shows the legacy
name, mention it so the user can add a second teardown line (or kill it by hand) until the last
old worktree is gone.

### Step 5 — Summary

Report what was auto-removed (confirmed-ready, with `$VERDICT_REASON`), what was removed after
confirmation, and what was kept (with reasons). List anything whose `$RELAXED` mentioned `merged`
as its own line — "removed on a `no-pr-needed` assertion, no merge observed" — rather than folding
it in with the ordinary merged removals: it is the one case where a removal rests on somebody's
claim as well as on git, and a run that silently reports it as merged hides exactly the thing a
human would want to spot-check. Call out any label/state mismatches even if the user didn't ask
about them — they indicate something worth double-checking (a bead reopened after being marked
ready, or a branch that got un-merged).

Also report, separately, any worktree whose **directory name doesn't match its branch** *when
the context's `naming.branch` and `naming.dir` are the same template* (Step 2). There it means
the directory is named after earlier work, and a human skimming `~/code/<repo>-worktrees/` would
otherwise draw the wrong conclusion about what's checked out there. When the two templates
differ, a mismatch is the design and reporting it is noise.

Report the readiness scope for anything removed or flagged: `$LABEL_SCOPE == branch` means the
signal came from that branch's own registry entry, `bead` that it came from the task's labels,
which every worktree of that task shares, and `branch-unrecorded` that the task owns several
branches while this one recorded no readiness, so no label was read for it at all. One word per
row, and it is the difference between "this branch was declared done" and "this task was,
possibly by different work" — the second is worth a glance when a task has had more than one
worktree, and a `branch-unrecorded` row is worth chasing, since it usually means a
`baton:finish` registry write failed and the worktree needs flagging by hand.

Report any worktree whose `$IDENTITY_SOURCE` was not `carrier` — those are pre-0.5.0 worktrees
resolved by name and backfilled on this run. Nothing is wrong with them; it is worth one line so
a second run showing the same worktrees as `carrier` confirms the backfill stuck.

Never remove a worktree that isn't in the confirmed-ready or looks-done group, even if asked to
"clean everything" — surface the blocker instead.
