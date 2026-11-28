---
description: (Worker session) Pick up the task for the current git worktree — read the leaf bead id from the worktree's identity carrier, load it from the active context's tracker, and start implementing. This is what a handed-off session runs to orient itself. Checks first whether the branch already landed (preferring the PR over git ancestry): if it merged, routes to baton:finish instead of restarting the work, or stops outright when the bead is also already closed. On a `pr-review` leaf it instead catches up with the author's pushes (force-pushes included, never discarding commits of ours) and orients a read-only review. No-ops cleanly outside a baton worktree.
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
# Locate baton: harness env → dev clone → install record → newest cached (references/locating-baton.md)
BATON="${CLAUDE_PLUGIN_ROOT:-}"; [ -n "$BATON" ] && [ -d "$BATON/scripts" ] || BATON="$HOME/code/maestro/baton"
_BP="${CLAUDE_CONFIG_DIR:-$HOME/.claude}/plugins"
[ -d "$BATON/scripts" ] || BATON="$(jq -r '.plugins["baton@maestro"] // [] | ((.[] | select(.scope=="user")) // .[0]) | .installPath // empty' "$_BP/installed_plugins.json" 2>/dev/null)"
[ -d "$BATON/scripts" ] || BATON="$(ls -d "$_BP"/cache/maestro/baton/* 2>/dev/null | sort -V | tail -1)"
[ -d "$BATON/scripts" ] || { echo "baton: cannot locate plugin scripts (set CLAUDE_PLUGIN_ROOT or install the plugin)" >&2; exit 1; }
IDENT="$BATON/scripts/task-identity.sh"
ID="$("$IDENT" --worktree "$PWD" --backfill --format env)" || { echo "Not a baton worktree — nothing to resume."; exit 0; }
eval "$ID"          # capture first: `eval "$(cmd)"` would swallow cmd's exit status
echo "Worktree: $DIR  branch: $BR  → leaf: $LEAF ($IDENTITY_SOURCE)  session: $SESSION_NAME"
```

`--worktree` reads the identity carrier `baton:start` wrote into this worktree's git dir. If
there isn't one — a worktree created before baton 0.5.0 — it falls back to parsing the directory
name and then the branch for the legacy `<leaf-id>-<slug>` shape. A name-derived leaf is accepted
only if the tracker has that task; `--backfill` then records it as the carrier so the next read
is authoritative. `$IDENTITY_SOURCE` says which rung answered (`carrier`, `dir`,
`branch`); mention it in the summary when it wasn't `carrier`.

**Do not parse the branch name yourself.** The branch is configurable (`naming.branch`) and may
be free-form — `DOT-1234/some-description` carries no bead id at all. If the helper can't
resolve an identity, this isn't a baton worktree — say so and stop (no error).

### Step 2 — Resolve context + tracker

```bash
# Locate baton: harness env → dev clone → install record → newest cached (references/locating-baton.md)
BATON="${CLAUDE_PLUGIN_ROOT:-}"; [ -n "$BATON" ] && [ -d "$BATON/scripts" ] || BATON="$HOME/code/maestro/baton"
_BP="${CLAUDE_CONFIG_DIR:-$HOME/.claude}/plugins"
[ -d "$BATON/scripts" ] || BATON="$(jq -r '.plugins["baton@maestro"] // [] | ((.[] | select(.scope=="user")) // .[0]) | .installPath // empty' "$_BP/installed_plugins.json" 2>/dev/null)"
[ -d "$BATON/scripts" ] || BATON="$(ls -d "$_BP"/cache/maestro/baton/* 2>/dev/null | sort -V | tail -1)"
[ -d "$BATON/scripts" ] || { echo "baton: cannot locate plugin scripts (set CLAUDE_PLUGIN_ROOT or install the plugin)" >&2; exit 1; }
RESOLVER="$BATON/scripts/resolve-context.sh"
CTX="$("$RESOLVER")" || { echo "$CTX"; exit 1; }
TRK="$BATON/scripts/tracker.sh"
WS="$(jq -r '._workspace' <<<"$CTX")"
GUIDE="$WS/$(jq -r '.guidance // "guidance.md"' <<<"$CTX")"
```

`tracker.sh` is the one seam to the task tracker — `task_tracking.type` picks the backend behind
it, and it pins the tracker location per call, so nothing here needs `BEADS_DIR` and no skill
knows whether the answer came from beads. Never call `bd` directly; the verb set is documented in
`../../references/tracker.md`.

### Step 3 — Load the task

```bash
BEAD="$("$TRK" get "$LEAF")" || BEAD=""     # exit 3 = no such task in this context's tracker
```

`get` returns a bare JSON **object** with a fixed field set (`title`, `description`,
`acceptance_criteria`, `status`, `labels`, `parent`, …) — the same shape from every backend, so
read it with `jq` and don't reach for a backend's own output format.

Exit status 3 specifically means the id doesn't exist here. Tell the user the worktree's leaf
doesn't map to a known task and ask how to proceed (they may want `baton:start`, a different id,
or a different context). Read `$GUIDE` and honor it.

### Step 3b — A review task? Catch up with the author instead

If `$TASK_KIND` (from Step 1's carrier) is `review`, or the bead carries the `pr-review` label,
this worktree is a review of **somebody else's PR**. Steps 4 and 6 do not apply. "Has this
branch landed" asks about the author's merge, and the acceptance criteria are the reviewer's own
judgement. Instead:

1. **Say it is a read-only review** of PR #`$REVIEW_PR` (the registry entry's `pr_url` names it
   fully). No commits or pushes to the author's branch unless the user asks. Posting comments,
   approving and requesting changes each need an explicit go-ahead, every time, and are posted only as point 5 says.
2. **Catch up with the author:**
   ```bash
   # Locate baton: harness env → dev clone → install record → newest cached (references/locating-baton.md)
   BATON="${CLAUDE_PLUGIN_ROOT:-}"; [ -n "$BATON" ] && [ -d "$BATON/scripts" ] || BATON="$HOME/code/maestro/baton"
   _BP="${CLAUDE_CONFIG_DIR:-$HOME/.claude}/plugins"
   [ -d "$BATON/scripts" ] || BATON="$(jq -r '.plugins["baton@maestro"] // [] | ((.[] | select(.scope=="user")) // .[0]) | .installPath // empty' "$_BP/installed_plugins.json" 2>/dev/null)"
   [ -d "$BATON/scripts" ] || BATON="$(ls -d "$_BP"/cache/maestro/baton/* 2>/dev/null | sort -V | tail -1)"
   [ -d "$BATON/scripts" ] || { echo "baton: cannot locate plugin scripts (set CLAUDE_PLUGIN_ROOT or install the plugin)" >&2; exit 1; }
   SYNC="$("$BATON/scripts/pr-review.sh" sync --worktree "$PWD")" || SYNC=""
   jq -r '"sync: \(.result) — \(.ours // "?") commit(s) of ours, head \(.head[0:9])"' <<<"$SYNC"
   ```
   | `result` | what happened | say |
   |---|---|---|
   | `up-to-date` | nothing new | nothing to catch up on |
   | `fast-forward` | the author pushed; HEAD moved forward | name the new commits (`git log --oneline <old>..HEAD`) |
   | `reset` | the author **force-pushed** and we held no commits of ours, so HEAD was reset to theirs | say it was a force-push; earlier review notes may refer to commits that no longer exist |
   | `ahead` | we hold commits of ours; the author pushed nothing | nothing moved |
   | `diverged` | the author's head moved **and** we hold commits of ours | **stop and ask**: nothing was touched. Options: keep ours and review the author's head separately, or discard ours (`git reset --hard @{u}`) on an explicit yes |
   | `dirty` | uncommitted changes | **stop and ask**: nothing was touched |
   | `error` | the fetch failed (gh/network, or the author's branch was deleted after merge) | say so and carry on with what is checked out |
3. **Fire `worker.on_resume`** as in Step 5. `$TASK_KIND=review` is exported, so a context's
   ticket glue can skip it.
4. **Orient**: the PR (number, title, author, state, base), what changed since the last sync, and
   that closing the review is the reviewer's call whenever they like. `baton:finish` closes it,
   whatever the PR's state. Then wait for the reviewer's direction rather than starting on
   anything.
5. **Pick the verdict by one rule.** Sort every finding into *blocking* (something must change
   before this merges) or *non-blocking* (a suggestion, a nit, a follow-up worth doing later).
   - Any blocking finding → **`REQUEST_CHANGES`**. The blocking findings are the review; any
     non-blocking ones may ride along in it, marked as such.
   - Nothing blocking → **`APPROVE`** (or **`COMMENT`**, when the reviewer wants to say something
     without approving). The non-blocking notes do **not** go in the review, where they read as
     conditions on the approval. They go on the **ticket linked to the PR**, through the
     context's `hooks.worker.on_review_notes` (in `$CTX` from Step 2). When that hook is empty
     there's nowhere else for them, so put them in the review body under a "Non-blocking"
     heading and say that's where they went.

   Never approve over a blocking finding to keep things moving, and never request changes for
   notes that are all non-blocking. When it is unclear which side a finding falls on, that is the
   reviewer's call: ask.
6. **Posting a review goes through `pr-review.sh post`, and nothing else.** Never regex a
   free-form draft into `gh` at post time, and never call `gh api …/reviews` by hand. Draft the
   review as an exact payload, and when the draft is shown to the user stamp it with the time.
   Notes headed for the linked ticket travel in the same payload as `ticket_notes`, a helper key
   `post` strips before sending (Markdown, exactly as they'll be posted):
   ```json
   {"commit_id": "<HEAD of this worktree>", "event": "APPROVE|REQUEST_CHANGES|COMMENT",
    "body": "…", "drafted_at": "<date -u +%Y-%m-%dT%H:%M:%SZ>",
    "comments": [{"path": "<file>", "line": 65, "side": "RIGHT", "body": "…"}],
    "ticket_notes": "…"}
   ```
   Show the user the event, the body, every inline comment and the `ticket_notes`, saying the
   notes go to the PR's linked ticket. Run the checks with `--dry-run` before asking, and post
   only after an explicit yes to **this** payload. A yes to the review but not the notes means
   drop `ticket_notes` from the payload — never post notes the user didn't confirm:
   ```bash
   # Locate baton: harness env → dev clone → install record → newest cached (references/locating-baton.md)
   BATON="${CLAUDE_PLUGIN_ROOT:-}"; [ -n "$BATON" ] && [ -d "$BATON/scripts" ] || BATON="$HOME/code/maestro/baton"
   _BP="${CLAUDE_CONFIG_DIR:-$HOME/.claude}/plugins"
   [ -d "$BATON/scripts" ] || BATON="$(jq -r '.plugins["baton@maestro"] // [] | ((.[] | select(.scope=="user")) // .[0]) | .installPath // empty' "$_BP/installed_plugins.json" 2>/dev/null)"
   [ -d "$BATON/scripts" ] || BATON="$(ls -d "$_BP"/cache/maestro/baton/* 2>/dev/null | sort -V | tail -1)"
   [ -d "$BATON/scripts" ] || { echo "baton: cannot locate plugin scripts (set CLAUDE_PLUGIN_ROOT or install the plugin)" >&2; exit 1; }
   PRR="$BATON/scripts/pr-review.sh"
   "$PRR" post --worktree "$PWD" --payload <payload file> --dry-run | jq .   # checks only
   "$PRR" post --worktree "$PWD" --payload <payload file> | jq .             # after the yes
   ```
   Helper keys of your own (`note`, `head_moved`, …) may stay in the file; only `commit_id`,
   `event`, `body` and `comments` are sent. `post` refuses (exit 2, nothing sent, every reason
   listed) when the body or an inline comment is empty, a comment is not on a RIGHT-side line
   of the diff at the head, `commit_id` is not this worktree's HEAD or the PR's head, the PR is
   not open, its description was edited after `drafted_at`, or anyone but bots and the reviewer
   commented or reviewed after it. **A refusal means the decision is stale.** Sync, show the
   user what changed, and draft again. Never retry the same payload, and never move
   `drafted_at` forward to get past a refusal. Exit 1 with `result: failed` means GitHub
   rejected the POST. Relay its message. `--payload -` reads stdin, so a payload held in a
   variable is passed `<<<"$PAYLOAD"`, never echoed.
7. **Send the notes to the linked ticket** — only when `post` returned `result: posted`, the
   posted payload carried a non-blank `ticket_notes`, and `hooks.worker.on_review_notes` has
   actions. Write the notes to a file and run each action (shell command or natural-language
   instruction, as in Step 5) with the identity group and these exported:
   ```bash
   # $RESULT holds post's JSON output; $PAYLOAD_FILE the payload that was posted
   # Locate baton: harness env → dev clone → install record → newest cached (references/locating-baton.md)
   BATON="${CLAUDE_PLUGIN_ROOT:-}"; [ -n "$BATON" ] && [ -d "$BATON/scripts" ] || BATON="$HOME/code/maestro/baton"
   _BP="${CLAUDE_CONFIG_DIR:-$HOME/.claude}/plugins"
   [ -d "$BATON/scripts" ] || BATON="$(jq -r '.plugins["baton@maestro"] // [] | ((.[] | select(.scope=="user")) // .[0]) | .installPath // empty' "$_BP/installed_plugins.json" 2>/dev/null)"
   [ -d "$BATON/scripts" ] || BATON="$(ls -d "$_BP"/cache/maestro/baton/* 2>/dev/null | sort -V | tail -1)"
   [ -d "$BATON/scripts" ] || { echo "baton: cannot locate plugin scripts (set CLAUDE_PLUGIN_ROOT or install the plugin)" >&2; exit 1; }
   ID="$("$BATON/scripts/task-identity.sh" --worktree "$PWD" --format env)" && eval "$ID"
   REVIEW_NOTES_FILE="$(mktemp "${TMPDIR:-/tmp}/baton-review-notes.XXXXXX")"
   jq -r '.ticket_notes // ""' "$PAYLOAD_FILE" >"$REVIEW_NOTES_FILE"
   PR_URL="$(jq -r '.pr' <<<"$RESULT")"; REVIEW_EVENT="$(jq -r '.event' <<<"$RESULT")"
   REVIEW_URL="$(jq -r '.review_url // ""' <<<"$RESULT")"
   export REVIEW_NOTES_FILE PR_URL REVIEW_EVENT REVIEW_URL
   ```
   Not a gate: the review is already up. If an action fails, say so and give the file's path so
   the notes can be posted by hand. Which ticket is "linked", and how it's written to, is the
   context's — see `../../references/hooks.md` (`worker.on_review_notes`).

Then stop. Steps 4–6 below are for ordinary tasks.

### Step 4 — Has this branch already landed?

Do this **before** orienting to the work. A resumed session is frequently waking up *after* its
PR merged: the gap between "PR merged" and "`baton:finish` ran" is precisely where a worker
session gets abandoned (machine reboot, tmux server restart, closed window), because the
interesting work is already over. Assuming the worktree is mid-flight is wrong every one of
those times.

Ask the shared helper — the same one `baton:finish` (Step 7) and `baton:cleanup-worktrees`
(Step 3) use, so all three agree and the ladder is fixed in one place:

```bash
# Locate baton: harness env → dev clone → install record → newest cached (references/locating-baton.md)
BATON="${CLAUDE_PLUGIN_ROOT:-}"; [ -n "$BATON" ] && [ -d "$BATON/scripts" ] || BATON="$HOME/code/maestro/baton"
_BP="${CLAUDE_CONFIG_DIR:-$HOME/.claude}/plugins"
[ -d "$BATON/scripts" ] || BATON="$(jq -r '.plugins["baton@maestro"] // [] | ((.[] | select(.scope=="user")) // .[0]) | .installPath // empty' "$_BP/installed_plugins.json" 2>/dev/null)"
[ -d "$BATON/scripts" ] || BATON="$(ls -d "$_BP"/cache/maestro/baton/* 2>/dev/null | sort -V | tail -1)"
[ -d "$BATON/scripts" ] || { echo "baton: cannot locate plugin scripts (set CLAUDE_PLUGIN_ROOT or install the plugin)" >&2; exit 1; }
MS="$BATON/scripts/merge-state.sh"
M="$("$MS" --branch "$BR" --format env)" || M=""   # $BR from Step 1; capture first, as there
eval "$M"   # MERGED MERGE_SIGNAL GH_STATUS MERGE_BASE MERGE_BASE_FROM HAS_WORK PR_REPO PR_STATE PR_NUMBER
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
  and no merge is ever coming. The labels are already in `$BEAD` from Step 3; check them
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

Check `$LEAF`'s labels (`jq -r '.labels[]' <<<"$BEAD"`, already fetched in Step 3) for
`autonomous-safe`. If present, say so and note the implication for the rest of this session: once
the acceptance criteria are met, run `baton:pr` and then `baton:finish` straight through —
those skills' autonomous paths will create the PR, wait for checks, merge, and signal cleanup
without pausing for confirmation. This does **not** relax hard gates that apply regardless of the
flag: failing `pre_pr`/`pre_finish` hooks and red CI still stop the flow for a human to look at.
If the label is absent, work proceeds as normal — human confirms at PR/merge/cleanup.

This session may add the label to its own leaf, but only when the user in this session tells it
to. Never do it because the work turned out small or the flow would be smoother. When directed,
run `"$TRK" label-add "$LEAF" autonomous-safe` and append the provenance note quoting the user
("…by the worker session at the user's direction: \"…\""), as in
[Autonomous-safe tasks](../../README.md#autonomous-safe-tasks). Push the tracker afterwards. The
same rule holds in `baton:pr` and `baton:finish`.

When the work is done and merged, run **`baton:finish`**.
