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
merged + clean** state (re-checked here from git/bd directly). Neither is trusted alone — the
labels are the intentional signal, the derived state is the cross-check that catches a stale or
wrong label.

`ready-for-worktree-delete` is the "I'm done" label. Two narrower ones **modify** that
cross-check rather than adding to it — each relaxes exactly one signal, and no other:

| label | relaxes | means |
|---|---|---|
| `keep-task-open` | `STATE` | the bead is open **on purpose**: a worker concluded some acceptance criteria are deliberately deferred, not blocking — e.g. waiting on elapsed time or data — while this specific worktree's work is done and merged. `STATE == closed` is no longer required, because an open bead here is expected, not an anomaly. |
| `no-pr-needed` | `MERGED` | the task deliberately produced **nothing to merge**: the work happened outside git (e.g. a REST API against a live system), or landed with no PR at all. `MERGED == yes` can never arrive for such a branch, so requiring it means waiting forever. |

Neither ever appears without `ready-for-worktree-delete` — on its own, neither has any meaning.

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
cross-check signals (set `BEADS_DIR` to that context's tracker for the bead checks), then hand
them to `cleanup-verdict.sh` for the bucket:

```bash
IDENT="${CLAUDE_PLUGIN_ROOT:-$HOME/code/maestro/baton}/scripts/task-identity.sh"
[ -x "$IDENT" ] || IDENT="$HOME/code/maestro/baton/scripts/task-identity.sh"
ID="$("$IDENT" --worktree "<wt>" --format env)" || continue   # not a baton worktree
eval "$ID"                          # LEAF SLUG BR DIR SESSION_NAME SESSION_TITLE SESSION_NAME_LEGACY IDENTITY_SOURCE
LABELS="$(BEADS_DIR=<tracker> bd label list "$LEAF" 2>/dev/null)"   # pass through verbatim
STATE="$(BEADS_DIR=<tracker> bd show "$LEAF" --json 2>/dev/null | jq -r 'if type=="array" then .[0] else . end | .status // "unknown"')"
[ -n "$STATE" ] || STATE=unknown    # bd or jq failed — an empty STATE must never read as "not closed"
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
V="$("$CV" --labels "$LABELS" --state "$STATE" --merged "$MERGED" \
           --has-work "$HAS_WORK" --dirty "$DIRTY" --format env)" || V=""
eval "$V"      # VERDICT VERDICT_REASON RELAXED LABELED KEEP_OPEN NO_PR_NEEDED STATE_OK MERGED_OK CLEAN
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

`bd show --json` emits a single-element **array**, not a bare object (confirmed on bd 1.1.0), so
`.status` must be read through the `type=="array"` guard above — a bare `.status` makes jq exit 5
with `Cannot index array with string "status"`, and because stderr is discarded the `// "unknown"`
default never fires. That produced an empty `STATE` on every run, silently killing both the
`STATE == closed` half of confirmed-ready and the entire looks-done-unlabeled bucket. The `[ -n
"$STATE" ]` fallback exists so a future failure here surfaces as a reportable `unknown` rather
than an empty string indistinguishable from an ordinary open bead — see the note on `unknown` in
the buckets below. **Any skill that pipes `bd show --json` into jq needs the same array
handling** — the type guard rather than a bare `.[0]` so it keeps working if bd ever switches to
emitting a bare object.

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
`BEADS_DIR`, a deleted bead, an unreadable tracker, or a `bd`/`jq` change breaking the parse).
`$VERDICT_REASON` marks it explicitly; surface that marker rather than folding it into a bucket's
reasoning as though the bead were merely open, since every downstream classification is unreliable
for that worktree. It is still safe by construction — `unknown` can never satisfy
`STATE == closed`, so nothing gets removed on a lookup failure.

#### A note on label scoping (multi-worktree-per-bead)

All three of `ready-for-worktree-delete`, `keep-task-open` and `no-pr-needed` live on the **bead**
(`bd label list <leaf>`), not on any specific worktree or branch. If the same bead ever has two
worktrees over its lifetime — an earlier one that finished and was labeled ready (possibly with
`keep-task-open`, since that's exactly the scenario the label exists for), then a *later* worktree
opened against that same still-open bead for the follow-up work — every one of those labels is
visible from the new worktree too, even though only the finished one is actually ready.

This is deliberately **not fixed here** (deferred, not overlooked):
- The per-worktree `MERGED`, `HAS_WORK` and `DIRTY` checks in this step are computed fresh from
  git each time, never from the label, so they're the real safety net regardless of what the
  bead's labels say. The still-active later worktree fails its own checks and can never land in
  confirmed-ready by mistake — the worst outcome is a presentation issue, not a wrongful deletion.
  This holds for a stale `no-pr-needed` too, and is exactly what its `HAS_WORK` guard buys: the
  follow-up worktree either has commits of its own (`HAS_WORK == yes`, guard fails) or has none
  yet, in which case there is nothing there to lose.
- Concretely, that active-but-unrelated worktree lands in **label/state mismatch** (flagged as an
  anomaly) instead of plain **in progress / not ready**, because the stale bead-level label makes
  it look like something disagrees when really it's just unrelated, still-in-progress work on an
  old label. A human glancing at the mismatch reasoning (merged=no / dirty) can recognize this at
  a glance and move on — it's a false-positive nuisance, not a safety gap.
- A real fix would make these signals worktree-scoped by recording them next to identity in the
  per-worktree carrier at `.git/worktrees/<name>/baton-identity` that 0.5.0 shipped — beads itself
  still has no branch-scoped label mechanism. That would fix this false positive and the
  bead-scoping of `no-pr-needed` in one move, at the cost of a schema decision about what else
  the identity carrier is allowed to hold. Revisit if the false positive shows up often enough in
  practice to be worth building; until then the cost is a few extra seconds of human judgment on
  an already-flagged row, not a risk of losing work.

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

Report any worktree whose `$IDENTITY_SOURCE` was not `carrier` — those are pre-0.5.0 worktrees
resolved by name and backfilled on this run. Nothing is wrong with them; it is worth one line so
a second run showing the same worktrees as `carrier` confirms the backfill stuck.

Never remove a worktree that isn't in the confirmed-ready or looks-done group, even if asked to
"clean everything" — surface the blocker instead.
