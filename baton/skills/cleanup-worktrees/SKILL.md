---
description: Review git worktrees in the active context and clean up the finished ones. Scoped to the active context by default; pass --context <name> or --all-contexts to widen it. Runs its scan in a background agent by default, so the calling session sees only a per-worktree question (when one needs an explicit yes) and the final report; pass --inline to run the scan in the foreground. A worktree is a confirmed candidate when its leaf bead carries the `ready-for-worktree-delete` label (applied by `baton:finish` once merged) AND the merged/clean signals agree, AND either the bead is closed or it carries `keep-task-open` (an explicit "left open on purpose" signal) — those are auto-removed with no prompt. A task that deliberately produced nothing to merge carries `no-pr-needed`, which stands in for the merge signal when git agrees nothing is outstanding. Everything else still requires explicit per-worktree confirmation. A separate branch pass then prunes local branches whose upstream is gone AND whose merge is confirmed (PR merged or landed on origin's default branch), and asks about merged-but-not-gone and gone-but-unmerged ones. Policy: the context's optional cleanup block.
argument-hint: "[--context <name>] [--all-contexts] [--inline]"
allowed-tools: Bash(*), Read, Agent
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

### Where the scan runs: a background agent, by default

The scan (Steps 1–4) is dozens of tool calls — per worktree a `merge-state.sh` fetch, a tracker
lookup, a registry read, a `git status`, a verdict — and **nothing in it needs a human until the
end**. Run inline in a home session it floods the orchestrator's context with output that only
the classification consumes. So by default the scan is dispatched to a **background agent**
(Step 0), the calling session gets control back immediately, and the result arrives later as a
task notification, handled in Step 6. `--inline` (alias `--foreground`) runs the whole thing in
the calling session instead — that is the escape hatch for debugging the scan itself, and the
mode to use when no `Agent` tool is available.

The two modes split the work along one seam, and the seam is what keeps the auto-removal
semantics identical in both:

| | background agent | calling session |
|---|---|---|
| Steps 1–3: enumerate + classify | ✅ | — |
| Step 4: remove **confirmed-ready**, run `on_cleanup` | ✅ | — |
| Step 4: **looks-done-unlabeled** | never asks; returns them as `needs_confirmation` | asks, removes on a yes, runs `on_cleanup` |
| Step 4: **label-state-mismatch**, **not-ready** | never touched; returned as `flagged` / `kept` | never touched |
| Step 4b: branch **`prune`** | deleted (`branch-prune.sh apply`) | — |
| Step 4b: branch **`confirm-*`** | never asks; returns them in `branches.needs_confirmation` | asks, deletes on a yes |
| Step 5: report | returns the [report contract](#the-report-contract) as JSON | renders the human summary |

The rule that matters: **the agent never asks a human anything, and never removes anything less
than confirmed-ready.** Where the inline flow would stop and ask, the agent records the question
and hands it back. It also never invents an answer to missing information — an unresolvable
context, an unreadable tracker, a missing helper script — that goes into `blocked` for the
calling session to surface. In `--inline` mode there is no hand-off: Step 4 asks directly and
Step 5 prints the summary, exactly as it always has.

### Step 0 — Dispatch

```bash
# Locate baton: harness env → dev clone → install record → newest cached (references/locating-baton.md)
BATON="${CLAUDE_PLUGIN_ROOT:-}"; [ -n "$BATON" ] && [ -d "$BATON/scripts" ] || BATON="$HOME/code/maestro/baton"
_BP="${CLAUDE_CONFIG_DIR:-$HOME/.claude}/plugins"
[ -d "$BATON/scripts" ] || BATON="$(jq -r '.plugins["baton@maestro"] // [] | ((.[] | select(.scope=="user")) // .[0]) | .installPath // empty' "$_BP/installed_plugins.json" 2>/dev/null)"
[ -d "$BATON/scripts" ] || BATON="$(ls -d "$_BP"/cache/maestro/baton/* 2>/dev/null | sort -V | tail -1)"
[ -d "$BATON/scripts" ] || { echo "baton: cannot locate plugin scripts (set CLAUDE_PLUGIN_ROOT or install the plugin)" >&2; exit 1; }
SKILL_DIR="$BATON/skills/cleanup-worktrees"
```

Parse the arguments: `--context <name>`, `--all-contexts`, and `--inline` / `--foreground`. The
first two scope the scan (Step 1); the third picks the mode.

**`--inline` given, or no `Agent` tool available** → run Steps 1–5 here, in this session. Skip
Step 6. When the tool is missing rather than the flag given, say in one line that the scan is
running inline for that reason.

**Otherwise** → launch **one** agent with the `Agent` tool: `subagent_type: "general-purpose"`
(a fresh agent, **not** `fork` — the scan needs nothing from this conversation, and a fork would
drag the whole orchestrator context along with it), `description: "cleanup-worktrees scan"`, and
this prompt, with the placeholders filled in:

```text
You are running the scan half of the baton:cleanup-worktrees skill as a background agent.
Working directory: <cwd>. Scope flags: <"--context NAME" | "--all-contexts" | none>.
<If $BATON_CONTEXT is set in this session: "BATON_CONTEXT=<value> is set; export it before running the resolver.">

Read <SKILL_DIR>/SKILL.md and carry out Steps 1 through 5 exactly as written, in the
"background agent" column of the mode table. That means:
- Steps 1–3: enumerate and classify every worktree. Anything task-identity.sh cannot
  resolve — the primary clone, hand-made or other tools' worktrees — goes into not_baton
  and is otherwise left alone. Never derive an identity from a directory or branch name
  yourself.
- Step 4: remove confirmed-ready worktrees (git worktree remove + branch -d) and run the
  context's hooks.home.on_cleanup for each, with the identity group exported. Remove
  NOTHING else. Do not ask anyone anything: looks-done-unlabeled worktrees go into
  needs_confirmation with their full identity group; label-state-mismatch into flagged;
  not-ready into kept.
- Step 4b: per member repo, run branch-prune.sh apply. It deletes only the `prune` bucket.
  Put every row in the report's `branches` section. Delete nothing in the confirm-* buckets
  and ask nobody about them.
- If you hit something you cannot resolve on your own (no context resolves and no scope
  flag was given, a helper script is missing, a tracker is unreadable), put it in blocked
  and keep going with whatever you can still classify.
- Step 5: your ENTIRE final message is one fenced ```json block holding the report
  contract from the skill, and nothing else — no prose before or after it. The calling
  session renders it; it does not read your transcript.
```

Then tell the user, in one line, that the scan is running in the background and the report
will arrive as a task notification — and **return control**. Do not wait on it, poll it, or run
the scan yourself in parallel: the notification re-enters this skill at Step 6. Whatever the
user was doing (a `baton:session-start` routine carrying on to `status`, say) continues.

### Step 1 — Choose contexts to scan

```bash
REG="${BATON_REGISTRY:-$HOME/.config/baton/registry.yaml}"
# Locate baton: harness env → dev clone → install record → newest cached (references/locating-baton.md)
BATON="${CLAUDE_PLUGIN_ROOT:-}"; [ -n "$BATON" ] && [ -d "$BATON/scripts" ] || BATON="$HOME/code/maestro/baton"
_BP="${CLAUDE_CONFIG_DIR:-$HOME/.claude}/plugins"
[ -d "$BATON/scripts" ] || BATON="$(jq -r '.plugins["baton@maestro"] // [] | ((.[] | select(.scope=="user")) // .[0]) | .installPath // empty' "$_BP/installed_plugins.json" 2>/dev/null)"
[ -d "$BATON/scripts" ] || BATON="$(ls -d "$_BP"/cache/maestro/baton/* 2>/dev/null | sort -V | tail -1)"
[ -d "$BATON/scripts" ] || { echo "baton: cannot locate plugin scripts (set CLAUDE_PLUGIN_ROOT or install the plugin)" >&2; exit 1; }
RESOLVER="$BATON/scripts/resolve-context.sh"
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
# Locate baton: harness env → dev clone → install record → newest cached (references/locating-baton.md)
BATON="${CLAUDE_PLUGIN_ROOT:-}"; [ -n "$BATON" ] && [ -d "$BATON/scripts" ] || BATON="$HOME/code/maestro/baton"
_BP="${CLAUDE_CONFIG_DIR:-$HOME/.claude}/plugins"
[ -d "$BATON/scripts" ] || BATON="$(jq -r '.plugins["baton@maestro"] // [] | ((.[] | select(.scope=="user")) // .[0]) | .installPath // empty' "$_BP/installed_plugins.json" 2>/dev/null)"
[ -d "$BATON/scripts" ] || BATON="$(ls -d "$_BP"/cache/maestro/baton/* 2>/dev/null | sort -V | tail -1)"
[ -d "$BATON/scripts" ] || { echo "baton: cannot locate plugin scripts (set CLAUDE_PLUGIN_ROOT or install the plugin)" >&2; exit 1; }
IDENT="$BATON/scripts/task-identity.sh"
TRK="$BATON/scripts/tracker.sh"
# --context - so a name-derived leaf is checked against THIS context's tracker. Never --backfill:
# this is a read pass. Non-zero = not a baton worktree; record it under not_baton and move on.
ERRF="$(mktemp)"
ID="$("$IDENT" --worktree "<wt>" --context - --format env 2>"$ERRF" <<<"$CTX_JSON")" \
  || { NOT_BATON_WHY="$(head -1 "$ERRF")"; continue; }
eval "$ID"                          # LEAF SLUG BR DIR SESSION_NAME SESSION_TITLE SESSION_NAME_LEGACY IDENTITY_SOURCE

# The tracker, through the one seam. --context - hands it this context's already-resolved JSON,
# so a multi-context scan reads each context's own tracker without exporting anything.
task() { "$TRK" --context - "$@" <<<"$CTX_JSON"; }

BEAD="$(task get "$LEAF" 2>/dev/null)" || BEAD=""
[ -n "$BEAD" ] || BEAD='{}'
STATE="$(jq -r '.status // "unknown"' <<<"$BEAD")"
[ -n "$STATE" ] || STATE=unknown    # an empty STATE must never read as "not closed"

# READINESS. Ask the task's branch registry about THIS branch, in THIS repo. The rule (which
# entry is ours, whether it records readiness, and when the task labels may stand in) lives in
# branch-readiness.sh — do not re-derive it here; see below for why.
#
# Match on repo AND branch: the scan runs per member repo, and one task can own the same branch
# name in two of them — matching on the name alone reads the finished one's readiness for the
# live one, which is the same class of bug the registry exists to fix, one level down. The
# registry keys `repo` by the member repo's NAME; passing the PATH is fine, the helper reduces it.
BRR="$BATON/scripts/branch-readiness.sh"
REGS="$(task list-branches "$LEAF" 2>/dev/null)"; [ -n "$REGS" ] || REGS='[]'
RD="$("$BRR" --branch "$BR" --repo "<repo>" \
             --task-labels "$(jq -r '.labels // [] | join(" ")' <<<"$BEAD")" --format env <<<"$REGS")" || RD=""
eval "$RD"                          # LABEL_SCOPE LABELS REG_ENTRY REG_FOUND NBR REPO_KEY
# Helper missing (stale cache): the pre-0.8.0 answer, task labels with bead scope.
[ -n "${LABEL_SCOPE:-}" ] || { LABEL_SCOPE=bead; LABELS="$(jq -r '.labels // [] | join(" ")' <<<"$BEAD")"; }
MS="$BATON/scripts/merge-state.sh"
M="$("$MS" --repo <repo> --branch "$BR" --format env)" || M=""
eval "$M"                           # MERGED MERGE_SIGNAL GH_STATUS MERGE_BASE MERGE_BASE_FROM HAS_WORK PR_REPO PR_STATE PR_NUMBER
[ -n "${MERGED:-}" ]   || MERGED=unknown    # helper missing (stale cache) — never reads as "merged"
[ -n "${HAS_WORK:-}" ] || HAS_WORK=unknown  # and an absent HAS_WORK must never read as "nothing outstanding"
DIRTY_TEXT="$(git -C <wt> status --porcelain)"                          # empty = clean
DIRTY="$([ -z "$DIRTY_TEXT" ] && echo no || echo yes)"

CV="$BATON/scripts/cleanup-verdict.sh"
V="$("$CV" --labels "$LABELS" --label-scope "$LABEL_SCOPE" --state "$STATE" --merged "$MERGED" \
           --has-work "$HAS_WORK" --dirty "$DIRTY" --format env)" || V=""
eval "$V"      # VERDICT VERDICT_REASON RELAXED LABELED KEEP_OPEN NO_PR_NEEDED LABEL_SCOPE STATE_OK MERGED_OK CLEAN
[ -n "${VERDICT:-}" ] || { VERDICT=not-ready; VERDICT_REASON="cleanup-verdict.sh unavailable"; }
```

`--worktree` reads the identity carrier `baton:start` wrote into the worktree's own git dir,
falling back to the legacy `<leaf>-<slug>` shape of the directory name and then the branch for
worktrees created before 0.5.0 — but only when the context's tracker has the leaf that name
implies, and without writing anything back. **This skill deletes things, so it must never guess
an identity from a name it merely recognizes.** The
carrier is per-worktree by construction; note in particular that `git config` is not, and would
have made every live worktree of a repo report the same leaf.

The helper exits non-zero when a worktree has no carrier and no name yields a leaf the tracker
knows — the primary clone, a hand-made worktree, or one another tool created (`iq:issue-start`'s
`clear-spec-schema` on `docs/clear-spec-schema` splits into the leaf `clear-spec`, which is no
task). Never offer those for removal; report them under `not_baton` with the helper's first
stderr line, so a scan over repos driven by other tooling reads as "not mine" rather than as
baton tasks in an unknown state. Both name rungs are skipped for the primary clone outright: its
directory is the *repository's* name and its branch is never a baton task branch.

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

`branch-readiness.sh` is the one place that rule lives, and it is tested
(`scripts/test-branch-readiness.sh`). It used to be a jq expression pasted here and into
`baton:status` with a note that the two must stay byte-for-byte identical — and both copies were
wrong in the same way for a whole release: they compared the entry's `repo` to the member repo's
absolute **path**, while `baton:start` records its **name**. The match never succeeded, so every
worktree started since 0.8.0 fell through to the task-label fallback and the per-branch signal
was never read — invisibly, because the fallback is a correct answer too (`jbh-7xb8`). The
canonical key is the repo's name (`references/tracker.md`, "Branch entry"); the helper reduces
whatever it is given to that, on both sides, so an entry written with a path still matches.

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
never mistaken for a checked fact. It never lets `gh` guess the repo (a fork with an `upstream`
remote would otherwise be asked about upstream's PRs) or assumes the base is `main`: `$PR_REPO`
and `$MERGE_BASE`/`$MERGE_BASE_FROM` say what it actually asked and compared against.

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
get different `SESSION_NAME`s — one worktree's teardown can't hit its sibling's session. A
collision would require the same leaf *and* the same slug, which is a duplicate worktree
directory git already rejects. (That covers sibling worktrees only; keeping the teardown off the
*home* session takes the guards on the seeded `on_cleanup` line — see Step 4.)

### Step 4 — Remove confirmed-ready automatically, ask for the rest

Sort every worktree into its four groups, each with bead id/title and reasoning.

**Confirmed ready** (`$VERDICT == confirmed-ready`) — remove immediately, no prompt, **in either
mode**. The independent signals already agree (label, closed, merged, clean), so there's nothing
left for a human to confirm. When `$RELAXED` is non-empty, say which cross-check a modifier label
stood in for — "removed because a human said there was nothing to merge (`no-pr-needed`), and git
agreed" reads very differently from "removed because its PR merged", and only one of them is a
claim somebody made:

```bash
# Locate baton: harness env → dev clone → install record → newest cached (references/locating-baton.md)
BATON="${CLAUDE_PLUGIN_ROOT:-}"; [ -n "$BATON" ] && [ -d "$BATON/scripts" ] || BATON="$HOME/code/maestro/baton"
_BP="${CLAUDE_CONFIG_DIR:-$HOME/.claude}/plugins"
[ -d "$BATON/scripts" ] || BATON="$(jq -r '.plugins["baton@maestro"] // [] | ((.[] | select(.scope=="user")) // .[0]) | .installPath // empty' "$_BP/installed_plugins.json" 2>/dev/null)"
[ -d "$BATON/scripts" ] || BATON="$(ls -d "$_BP"/cache/maestro/baton/* 2>/dev/null | sort -V | tail -1)"
[ -d "$BATON/scripts" ] || { echo "baton: cannot locate plugin scripts (set CLAUDE_PLUGIN_ROOT or install the plugin)" >&2; exit 1; }
# $WT, $BR and the rest of the identity group are already exported by Step 3's eval.
git -C <repo> worktree remove "$WT" --force
"$BATON/scripts/branch-prune.sh" delete --repo <repo> --branch "$BR" --require-merged --no-fetch
```

The branch goes through `branch-prune.sh delete`, not `git branch -d`. `-d` judges "merged"
against the branch's upstream or the primary clone's **current HEAD** — local `main`, routinely
stale — so it refused genuinely merged branches on 2026-08-06 until `main` was pulled, and it
always refuses a squash-merged one. `--require-merged` re-asks `merge-state.sh` (origin's default
branch and the PR), deletes only at the SHA it judged, and refuses protected branches outright. A
refusal is not an error to chase here: the branch stays, and Step 4b reports it.

`git worktree remove` deletes the per-worktree git dir, and the identity carrier with it — so
this worktree cannot be asked what it was after this line runs. Everything downstream (the
`on_cleanup` hooks below included) must use the values Step 3 already exported, not re-derive
them.

**Looks done, unlabeled** — there's no explicit "I'm done" signal from a worker session here, so
a human confirms before anything is removed. What that means depends on which thread is running:

- *Inline:* ask, per worktree (or offer "remove all unlabeled-but-done" as its own batch), and use
  the same removal commands once confirmed.
- *Background agent:* **do not ask** — there is no human on the other end of an agent, and a
  question asked there is a question nobody sees. Record the worktree in the report's
  `needs_confirmation` with its `$VERDICT_REASON` and its full identity group (the carrier is
  still there, but the calling session should not have to rescan to act on a yes). Step 6 asks
  and removes.

**Label/state mismatch** — never offer removal; it's an anomaly by definition. Flag it, print
`$VERDICT_REASON` so the disagreeing signal is named, and move on (`flagged` in the report).

**Not ready** — never touched (`kept` in the report).

Never bundle groups together into a single blanket "remove all" — confirmed-ready acts on its
own (automatically), and unlabeled-but-done is its own separate ask.

For every removal (auto or confirmed), run the context's `hooks.home.on_cleanup` actions with the
full identity group in the environment — `$WT` (worktree path) plus `$LEAF`, `$SLUG`, `$BR`,
`$DIR`, `$SESSION_NAME`, `$SESSION_TITLE` from the Step 3 `eval`, all already exported — never
re-resolved from the now-deleted worktree. In background mode the agent runs them for the
worktrees it removed, and the calling session runs them for the ones it removes in Step 6; the
hooks come from the same `context.yaml` either way.

This is where the worktree's tmux session gets torn down. `baton:configure` seeds `on_cleanup`
with:

```bash
[ -n "$SESSION_NAME" ] && [ "$SESSION_NAME" != "$([ -n "$TMUX_PANE" ] && tmux display-message -p -t "$TMUX_PANE" "#S" 2>/dev/null)" ] && tmux kill-session -t "=$SESSION_NAME" 2>/dev/null || true
```

That target is **the same string `baton:start` used to create the session**, because both come
from `task-identity.sh` reading the same carrier — the teardown agrees with the launch by
construction, not by a human keeping two transforms in sync. It holds for custom `handoff.launcher`s too: a launcher is handed
`$SESSION_NAME` rather than deriving its own, so there's no naming to check.

Agreeing with the launch is not the whole of "can't hit the wrong session", though: these hooks
run *in the home session*, usually inside tmux. A bare `tmux kill-session -t "$SESSION_NAME"`
kills that session when the name is empty (tmux reads an empty `-t` as the current session) or
when a `SESSION_NAME` leaked into the home environment names it, and prefix-matches a different
session when the target has already exited. The seeded line guards all three — non-empty, not
the session this pane belongs to, `=` for an exact match. If a context's `on_cleanup` still has
the unguarded form, say so in the summary and point at `baton:doctor`, which offers the rewrite;
the reasoning for each guard lives in `../../references/hooks.md` (`home.on_cleanup`).

If a context's `on_cleanup` is empty, say so — the tmux session will leak. (In the report:
`hooks: "none-configured"` on each removal row.)

**Transitional:** worktrees started before the identity group existed are still named
`baton-<sanitized-branch>`, and are never renamed in flight. `$SESSION_NAME_LEGACY` is exported
alongside the rest for exactly that window; if a kill no-ops and `tmux ls` still shows the legacy
name, mention it so the user can add a second teardown line (or kill it by hand) until the last
old worktree is gone.

### Step 4b — Prune landed branches (a separate pass, after the worktrees)

The worktree pass only deletes a branch together with the worktree it removes. A branch whose
worktree went some other way, or that never had one, would otherwise stay local forever. In
homebernetes on 2026-08-06, 41 of 47 local branches had a deleted upstream. This pass is
branch-centric and runs **after** Step 4, once per member repo, so every run leaves each repo
closer to clean without anyone scheduling a purge:

```bash
# Locate baton: harness env → dev clone → install record → newest cached (references/locating-baton.md)
BATON="${CLAUDE_PLUGIN_ROOT:-}"; [ -n "$BATON" ] && [ -d "$BATON/scripts" ] || BATON="$HOME/code/maestro/baton"
_BP="${CLAUDE_CONFIG_DIR:-$HOME/.claude}/plugins"
[ -d "$BATON/scripts" ] || BATON="$(jq -r '.plugins["baton@maestro"] // [] | ((.[] | select(.scope=="user")) // .[0]) | .installPath // empty' "$_BP/installed_plugins.json" 2>/dev/null)"
[ -d "$BATON/scripts" ] || BATON="$(ls -d "$_BP"/cache/maestro/baton/* 2>/dev/null | sort -V | tail -1)"
[ -d "$BATON/scripts" ] || { echo "baton: cannot locate plugin scripts (set CLAUDE_PLUGIN_ROOT or install the plugin)" >&2; exit 1; }
# Same context JSON as Step 3; its optional `cleanup:` block is the policy.
BP="$BATON/scripts/branch-prune.sh"
PR_JSON="$("$BP" apply --repo <repo> --context - <<<"$CTX_JSON")" || PR_JSON=""
```

`apply` runs `git fetch --prune origin` first, because `[gone]` is only accurate after a prune.
It then classifies every local branch, deletes the `prune` bucket and returns every row. **Do not
re-derive the buckets here.** The rules live in the script and are pinned by
`scripts/test-branch-prune.sh`:

| `verdict` | when | what happens |
|---|---|---|
| `prune` | upstream gone **and** `merge-state.sh` says merged (PR MERGED, or landed on origin's default branch) | deleted by `apply`, no prompt, **in either mode** |
| `confirm-merged` | merged, but the upstream is still on the remote or was never pushed (`prune_merged`) | ask first |
| `confirm-orphaned` | upstream gone, **no** merge signal | ask first, never automatic: the remote branch was deleted unmerged (a closed PR, say), so this may be the only copy of its commits |
| `keep` | protected, checked out in any worktree, live, policy switched off, or merge state unknown | never touched; `reason` says which |

Gone-upstream alone never deletes anything. That is the rule the pass exists to enforce, and the
first real run proved it on the first try: two homebernetes branches were `[gone]` because their
PRs had been **closed unmerged**. Branches checked out in a worktree are always `keep`, because
Steps 3–4 own them. Origin's default branch (from `origin/HEAD`, never a hardcoded `main`) is
protected along with the `protect` list.

Deletes are SHA-pinned (`git update-ref -d <ref> <sha>`), so they never fall back to `-D`. A
branch that moved after it was judged is left alone.

Asking, per mode, in the same way as Step 4's looks-done-unlabeled bucket:

- *Inline:* show the `confirm-merged` rows as one batch ask and the `confirm-orphaned` rows as
  another, never combined, because they carry opposite risks. Show each row's `reason` and short
  `sha`. For each yes:
  `"$BP" delete --repo <repo> --branch <branch> --expect <sha> --no-fetch`. `--expect` makes the
  yes cover exactly the commit that was shown.
- *Background agent:* do not ask. Put the confirm rows in the report's `branches.needs_confirmation`
  (below) and let Step 6 ask.

The policy is the context's optional `cleanup:` block, and the plugin defaults apply when it is
absent, so an unconfigured context still gets the pass:

```yaml
cleanup:
  prune_gone_upstream: true          # the [gone] set: auto when merged, ask when not
  prune_merged: true                 # merged branches whose upstream is not gone: ask
  protect: [main, master, develop]   # never deleted; replaces this default list when set
```

### Step 5 — Summary

*Inline:* print the human summary described here. *Background agent:* emit the
[report contract](#the-report-contract) instead — the calling session prints this same summary
from it in Step 6. Both carry the same facts; the contract is the summary with its fields named.

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
resolved by name (and confirmed against the tracker). This scan does not backfill them; the next
`baton:resume` in that worktree will.

Report the `not_baton` worktrees as one short "skipped — not baton worktrees" list, path and
reason each. They were never candidates for anything.

Surface `$MERGE_SIGNAL` / `$GH_STATUS` for any row where the signal wasn't `pr`, so "not merged"
from a machine with no `gh` is never mistaken for a checked fact — and for those rows include
`$PR_REPO` and `$MERGE_BASE` (noting `$MERGE_BASE_FROM` when it is `guess`), so a wrong repo or
base is visible in the report instead of silently keeping the worktree. Mark any row whose `STATE`
was `unknown` — that is a failed lookup, not an open bead.

Report the branch pass (Step 4b) per repo, **every row, never truncated**. Silent truncation reads
as "nothing to do" when there was something. List what was deleted (with short SHAs, so a
deletion can be recovered with `git branch <name> <sha>`), what is awaiting confirmation, and
what was kept with its `reason`. Collapse the `keep` rows to one line per reason with a count and
the branch names when there are many; that shortens the list without hiding anything. Call out
`confirm-orphaned` rows in particular: their remote branch was deleted without a merge.

Never remove a worktree that isn't in the confirmed-ready or looks-done group, even if asked to
"clean everything" — surface the blocker instead.

### The report contract

What the background agent hands back, and what Step 6 consumes. One JSON object; every list may
be empty, and an empty `needs_confirmation` **and** empty `blocked` is what lets a run finish
with no interaction at all. The inline flow never serializes this — it is the seam between the
two threads, not a file format — but its rows are the same facts Step 5 prints.

```json
{
  "contexts": [ { "name": "jbh", "repos_scanned": 3, "worktrees_seen": 5, "on_cleanup_configured": true } ],
  "removed": [
    { "context": "jbh", "repo": "/abs/path/to/repo", "worktree": "/abs/path/to/worktree",
      "branch": "jbh-abc-thing", "leaf": "jbh-abc", "title": "bead title",
      "verdict": "confirmed-ready", "reason": "<$VERDICT_REASON>", "relaxed": "",
      "label_scope": "branch", "identity_source": "carrier",
      "merge_signal": "pr", "gh_status": "ok",
      "session_name": "thing-jbh-abc", "hooks": "ran" }
  ],
  "needs_confirmation": [
    { "context": "jbh", "repo": "/abs/path/to/repo", "worktree": "/abs/path/to/worktree",
      "branch": "jbh-def-other", "leaf": "jbh-def", "title": "bead title",
      "verdict": "looks-done-unlabeled", "reason": "<$VERDICT_REASON>",
      "label_scope": "bead", "identity_source": "carrier",
      "merge_signal": "ancestry", "gh_status": "no-pr",
      "identity": { "WT": "...", "LEAF": "...", "SLUG": "...", "BR": "...", "DIR": "...",
                    "SESSION_NAME": "...", "SESSION_TITLE": "...", "SESSION_NAME_LEGACY": "..." } }
  ],
  "flagged": [ { "...": "label-state-mismatch rows, same shape as removed minus session_name/hooks" } ],
  "kept":    [ { "context": "jbh", "worktree": "...", "branch": "...", "leaf": "...", "title": "...",
                 "verdict": "not-ready", "reason": "<$VERDICT_REASON>" } ],
  "anomalies": [
    { "kind": "dir-branch-mismatch", "worktree": "...", "branch": "...", "note": "naming templates are equal" },
    { "kind": "identity-by-name", "worktree": "...", "source": "dir" },
    { "kind": "state-unknown", "worktree": "...", "leaf": "...", "note": "tracker lookup failed" },
    { "kind": "legacy-session", "worktree": "...", "session_name_legacy": "baton-..." },
    { "kind": "no-on-cleanup-hook", "context": "jbh" }
  ],
  "not_baton": [ { "context": "jbh", "repo": "/abs/path/to/repo", "worktree": "...", "branch": "docs/x",
                   "why": "<first stderr line from task-identity.sh>" } ],
  "branches": {
    "deleted":            [ { "context": "jbh", "repo": "/abs/path/to/repo", "branch": "old-thing", "sha": "<full sha>",
                              "verdict": "prune", "reason": "upstream gone; merged (pr, PR #38)" } ],
    "needs_confirmation": [ { "context": "jbh", "repo": "/abs/path/to/repo", "branch": "closed-pr", "sha": "<full sha>",
                              "verdict": "confirm-orphaned", "reason": "upstream gone but NOT merged (...)" } ],
    "kept":               [ { "context": "jbh", "repo": "...", "branch": "main", "verdict": "keep", "reason": "protected" } ],
    "failed":             [ { "context": "jbh", "repo": "...", "branch": "...", "sha": "...", "reason": "..." } ]
  },
  "blocked": [ { "what": "resolve context", "why": "no cwd match and no default:true; pass --context or --all-contexts" } ]
}
```

Field rules, so the two sides agree:

- `removed` rows are **facts about things already gone** — the worktree, branch and carrier no
  longer exist. `hooks` is `ran`, `none-configured` (the context has no `on_cleanup`, so a tmux
  session is leaking), or `failed: <first line>`.
- `needs_confirmation` rows carry the **full identity group** under `identity`, exactly as Step
  3's `eval` exported it. Step 6 removes on those values and runs `on_cleanup` with them; it does
  not re-run `task-identity.sh`.
- `relaxed` is `$RELAXED` verbatim (`""`, `state`, `merged`, or `state merged`); Step 6 gives any
  row mentioning `merged` its own line, as Step 5 requires.
- `anomalies` is where every "also report, separately" item from Step 5 lands, one `kind` each;
  the agent adds nothing that Step 5 does not already ask for.
- `branches` rows come straight from `branch-prune.sh` (Step 4b), each with `context` and
  `repo` added. A `needs_confirmation` row **must** carry its full `sha`, because Step 6 deletes
  with `--expect <sha>` and the yes covers that commit only.
- `blocked` is for anything the agent could not derive on its own. It never guesses past one —
  a context it could not resolve is not scanned, and the row says so.
- Extra fields are fine (`has_work`, `dirty`, `pr_number`, a free-text `note`) — Step 6 reads
  the fields named here and passes anything else through to the summary where it helps. What
  it must never do is *miss* a named one: a row without `identity` cannot be acted on.

### Step 6 — Receive the report (calling session, background mode only)

The task notification carrying the agent's result re-enters here. Extract the JSON block from
it. If there is no parseable block, say the scan failed, quote whatever the agent did return,
and offer `--inline` — never proceed on a guessed report.

1. **`blocked` non-empty** → surface each row first. A blocked context was not scanned; say so
   rather than letting an empty `kept` read as "nothing there".
2. **`needs_confirmation` and `branches.needs_confirmation` both empty** → print the Step 5 summary from the report and stop. This is
   the no-interaction path: every worktree was either removed on agreement of all signals or
   left alone, and the summary is all the human sees.
3. **`needs_confirmation` non-empty** → ask **exactly** that: which worktree(s), and the
   `reason` for each — nothing else is put to the human, and nothing else waits on the answer.
   Ask per worktree, or offer the batch as its own single ask, never mixed with any other group.
   For each yes, from the row's `identity`:

   ```bash
   # Locate baton: harness env → dev clone → install record → newest cached (references/locating-baton.md)
   BATON="${CLAUDE_PLUGIN_ROOT:-}"; [ -n "$BATON" ] && [ -d "$BATON/scripts" ] || BATON="$HOME/code/maestro/baton"
   _BP="${CLAUDE_CONFIG_DIR:-$HOME/.claude}/plugins"
   [ -d "$BATON/scripts" ] || BATON="$(jq -r '.plugins["baton@maestro"] // [] | ((.[] | select(.scope=="user")) // .[0]) | .installPath // empty' "$_BP/installed_plugins.json" 2>/dev/null)"
   [ -d "$BATON/scripts" ] || BATON="$(ls -d "$_BP"/cache/maestro/baton/* 2>/dev/null | sort -V | tail -1)"
   [ -d "$BATON/scripts" ] || { echo "baton: cannot locate plugin scripts (set CLAUDE_PLUGIN_ROOT or install the plugin)" >&2; exit 1; }
   RESOLVER="$BATON/scripts/resolve-context.sh"
   eval "$(jq -r '.identity | to_entries[] | "export \(.key)=\(.value|@sh)"' <<<"$ROW")"
   git -C "$(jq -r .repo <<<"$ROW")" worktree remove "$WT" --force
   "$BATON/scripts/branch-prune.sh" delete --repo "$(jq -r .repo <<<"$ROW")" --branch "$BR" --require-merged
   CTX_JSON="$(BATON_CONTEXT="$(jq -r .context <<<"$ROW")" "$RESOLVER")"
   # then hooks.home.on_cleanup from $CTX_JSON, with the identity group exported (Step 4)
   ```

   A no keeps the worktree, and the row is reported under kept with "declined".
3b. **`branches.needs_confirmation` non-empty** → ask about the branches the same way, as their own
   asks, never folded into the worktree question. Use one batch for `confirm-merged` and a
   separate one for `confirm-orphaned`, showing `reason` and the short `sha`. For each yes:
   `"$BATON/scripts/branch-prune.sh" delete --repo <repo> --branch <branch> --expect <sha>`. A
   `refused` answer (the branch moved, or is now checked out) is reported, not retried with
   weaker flags.
4. Print the Step 5 summary: auto-removed (from `removed`), removed after confirmation, kept,
   flagged, and every `anomalies` row — the same summary the inline flow prints, so the mode a
   run happened to use changes nothing about what the human learns from it.
