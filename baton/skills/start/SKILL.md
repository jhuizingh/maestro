---
description: Start work on a task in the active context — resolve a single leaf bead (pick a child of a parent, use a leaf directly, or create one), create its git worktree and branch, and hand off per the context's work mode (new tmux session by default). One worktree ⇔ one leaf bead. `--review <PR URL|number>` starts a review of somebody else's PR instead — a `pr-review` leaf and a worktree on the PR's head.
argument-hint: "[bead-id | free-text task | --review <PR URL|number>]"
allowed-tools: Bash(*), Read
---

## baton:start

### Step 1 — Resolve context + guidance

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
jq -r '"Context: \(.name)  tracker: \(.task_tracking.dir)  mode: \(.work_mode.default)"' <<<"$CTX"
```

`tracker.sh` is the one seam to the task tracker — `task_tracking.type` picks the backend behind
it, and it pins the tracker location per call, so nothing here needs `BEADS_DIR` and no step
below knows whether the answer came from beads. Never call `bd` directly; the verb set is
documented in `../../references/tracker.md`.

Read `$GUIDE` if it exists and honor its preferences for the rest of this skill.

**`$ARGUMENTS` starts with `--review`?** Then this is a review of somebody else's PR. Follow
[Review mode](#review-mode--baton-start---review-pr) below **instead of Steps 2–6**, then rejoin
at Step 7.

### Step 2 — Resolve the LEAF bead (invariant: one worktree ⇔ one leaf)

Determine `LEAF` (a leaf bead id):

- **`$ARGUMENTS` is a bead id** → fetch it: `"$TRK" get <id>` (exit 3 means no such task here).
  - If it has open children (`"$TRK" children <id>`), it's a **parent** — list the open children
    and ask which to work, or offer to create a new child (Step 3). Do **not** worktree a parent.
  - If it's a **leaf** (no open children) → `LEAF=<id>`.
- **`$ARGUMENTS` is free text** → create a leaf: `LEAF="$("$TRK" create "<text>")"` (`create`
  prints only the id).
- **`$ARGUMENTS` empty** → show ready work (`"$TRK" ready`, or `"$TRK" list --status open`), let
  the user pick a leaf; if they pick a parent, drop into the child picker; if they want something
  ad-hoc, create a stub the same way. Every worktree gets a unique leaf — never proceed without
  one.

Confirm: "Starting `<LEAF>` — <title>." If `"$TRK" get <LEAF>` carries the `autonomous-safe`
label, say so explicitly — e.g. "marked autonomous-safe: this worker will go through PR,
merge, and `baton:finish` cleanup without pausing for confirmation at those gates" — since
that's a meaningfully different hand-off than the default human-gated flow.

Never add the label here on your own judgement. If the user says, while starting it, that the
task is autonomous-safe, apply it with `"$TRK" label-add <LEAF> autonomous-safe` plus the
provenance note quoting their words (see
[Autonomous-safe tasks](../../README.md#autonomous-safe-tasks)). The worker session may also
apply it later, but only when the user in that session directs it.

### Step 3 — (When decomposing) create a child leaf

If the user is carving a chunk off a parent:
```bash
LEAF="$("$TRK" create "<child title>" --parent <PARENT>)"   # inherits parent labels
```
Use the new child id as `LEAF`. (For deeper splitting mid-work, see `baton:split`.)

### Step 4 — Check dependencies

```bash
"$TRK" deps "$LEAF" | jq -r '.[] | select(.type=="blocks" and .status!="closed") | .id'
```

A **blocker** is a `down` edge of type `blocks` whose status isn't `closed` — a `parent-child`
edge is hierarchy, not a gate, so don't treat one as blocking. If any come back, warn the user
and confirm before proceeding.

### Step 5 — Write the slug, then compute the identity group

**Write the slug — don't mechanically slugify the title.** Read the bead's title *and*
description, then choose **2–4 concrete words** (≤24 chars, lowercase, `a-z0-9-` only) saying
what the work actually is. Drop filler, articles and prepositions; keep the domain nouns:

| Bead title | Slug |
|---|---|
| Make Technitium DNS multi-node/HA so a node reboot doesn't take down DNS | `technitium-dns-ha` |
| Raise kids' bedrooms overnight maintain-cool target… | `kids-overnight-hvac` |
| Alexa "turn on main area lights" doesn't turn on… | `alexa-lights-broken` |
| Jellyfin transcode temp files fill up config PVC… | `jellyfin-transcode-pvc` |

This slug is the human-readable half of the branch, the worktree directory, the tmux session
name **and** the Claude Code session title — a mid-word truncation like
`make-technitium-dns-multinodeha-so-a-nod` makes all four unrecognizable in a list, which bites
hardest on mobile/remote clients that truncate further. Writing it is safe precisely because the
slug is generated **once** and recorded in the worktree's identity carrier (Step 6): nothing
recomputes it, so it never has to be reproducible.

Fall back to plain slugify (lowercase → non-alphanumeric to `-` → trim) only when the bead has
no useful description, or when the context sets `naming.slug: slugify` — check
`.slug_mode` in the helper output below.

Now compute the whole identity group in one call. **`baton/scripts/task-identity.sh` is the only
place these names are derived** — `baton:resume`, `baton:pr`, `baton:finish`,
`baton:cleanup-worktrees` and the SessionStart hook all recover them later through the same
script's `--worktree` mode, so nothing can drift:

```bash
# Locate baton: harness env → dev clone → install record → newest cached (references/locating-baton.md)
BATON="${CLAUDE_PLUGIN_ROOT:-}"; [ -n "$BATON" ] && [ -d "$BATON/scripts" ] || BATON="$HOME/code/maestro/baton"
_BP="${CLAUDE_CONFIG_DIR:-$HOME/.claude}/plugins"
[ -d "$BATON/scripts" ] || BATON="$(jq -r '.plugins["baton@maestro"] // [] | ((.[] | select(.scope=="user")) // .[0]) | .installPath // empty' "$_BP/installed_plugins.json" 2>/dev/null)"
[ -d "$BATON/scripts" ] || BATON="$(ls -d "$_BP"/cache/maestro/baton/* 2>/dev/null | sort -V | tail -1)"
[ -d "$BATON/scripts" ] || { echo "baton: cannot locate plugin scripts (set CLAUDE_PLUGIN_ROOT or install the plugin)" >&2; exit 1; }
IDENT="$BATON/scripts/task-identity.sh"
# --jira <key> only when the context's naming.branch uses {jira}; see the table below.
ID="$("$IDENT" --leaf "<LEAF>" --slug "<written-slug>" --format env)" || exit 1
eval "$ID"          # capture first: `eval "$(cmd)"` would swallow cmd's exit status
echo "$LEAF / $SLUG / $BR / $DIR / $SESSION_NAME / $SESSION_TITLE"
```

That exports the identity group for the rest of this skill:

| | default | example |
|---|---|---|
| `LEAF` | bead id | `jbh-zvs` |
| `SLUG` | written, above | `kids-overnight-hvac` |
| `BR` | `naming.branch`, `{leaf}-{slug}` | `jbh-zvs-kids-overnight-hvac` |
| `DIR` | `naming.dir`, `{leaf}-{slug}` | `jbh-zvs-kids-overnight-hvac` |
| `SESSION_NAME` | `{slug}-{leaf}` | `kids-overnight-hvac-jbh-zvs` |
| `SESSION_TITLE` | `{slug_prose} ({leaf})` | `kids overnight hvac (jbh-zvs)` |

**`BR` and `DIR` are independent, and a context may make them differ.** By default both are
`{leaf}-{slug}` and match, as they always did. A context whose organisation dictates branch
naming can set `naming.branch: "{jira}/{slug}"` and get `DOT-1234/kids-overnight-hvac` on disk at
`jbh-zvs-kids-overnight-hvac` — so **use `$DIR` for every path and `$BR` for every git ref, and
never assume the two are the same string**. If the context's `naming.branch` uses `{jira}`, pass
the key with `--jira` (from wherever the context records it on the bead — a label, a field, or
the user); the helper refuses to mint a branch with an unfilled `{jira}`.

Nothing downstream parses `$BR` to recover the bead — that is the identity carrier's job, which
Step 6 writes.

Then:
- Target repo: if the bead carries a `repo-<name>` label, map `<name>` to `<code_root>/<name>`;
  otherwise ask which member repo this work belongs to (offer the context's `member_repos`).
- Worktree base: expand `worktree_base` (`{code_root}` and `{repo}` substituted), e.g.
  `~/code/<repo>-worktrees`.

### Step 6 — Claim + create the worktree

```bash
"$TRK" claim "$LEAF"                          # assign to me + in_progress, in one verb
REPO=<code_root>/<repo>; WT_BASE=<expanded worktree_base>
WT="$WT_BASE/$DIR"                            # DIR for the path, BR for the ref — see Step 5
git -C "$REPO" fetch --all --prune
mkdir -p "$WT_BASE"
git -C "$REPO" worktree add "$WT" -b "$BR" origin/main    # or without -b if branch exists
```

**Then write the identity carrier, immediately** — before the handoff, before hooks, before
anything can fail and leave a worktree nobody can identify:

```bash
"$IDENT" --write-carrier "$WT" --leaf "$LEAF" --slug "$SLUG" --branch "$BR"   # add --jira if used
```

That records leaf + slug (+ the jira key, if any) at
`$(git -C "$WT" rev-parse --git-dir)/baton-identity` — inside the worktree's own git dir, so it
is per-worktree by construction, invisible to `git status`, and removed along with the worktree.
It is what every later skill reads to know which task this directory serves. Do **not** try to
write it with `git config`: at local scope inside a linked worktree that writes to the *shared*
repository config, so the next `baton:start` would silently re-point every live worktree of the
repo at this bead.

**Then record the branch on the task** — the other half of the same fact, written from the other
end:

```bash
"$TRK" record-branch "$LEAF" --repo "<repo-name>" --branch "$BR" --worktree "$WT"
```

`--repo` takes the member repo's **name** (`maestro`), not the path in `$REPO` — it is how a
later reader tells two same-named branches in different repos apart, and a machine-specific
absolute path is no use for that.

The carrier answers "what task is THIS worktree?" offline, with no tracker call. The registry
answers "which branches belong to THIS task?", which the carrier structurally cannot: a task can
own several branches over its life (one bead in the wild had three, across PRs #176/#177/#178),
and nothing recorded that anywhere. `baton:pr` and `baton:finish` move the entry's status;
`baton:cleanup-worktrees` reads it as a **per-branch** readiness signal instead of a task-level
label that every worktree of the task can see.

The two carriers do not compete — they answer different questions and are written from the same
values at the same moment. Failing to write the registry entry is **not fatal**: say so and carry
on. A tracker that is unreachable right now must not block a dispatch, and cleanup falls back to
the task's labels exactly as it does for pre-0.8.0 worktrees.

**Then push the tracker**, once, now that the claim and the registry entry are both written (and
any leaf created in Step 2 with them):

```bash
# Locate baton: harness env → dev clone → install record → newest cached (references/locating-baton.md)
BATON="${CLAUDE_PLUGIN_ROOT:-}"; [ -n "$BATON" ] && [ -d "$BATON/scripts" ] || BATON="$HOME/code/maestro/baton"
_BP="${CLAUDE_CONFIG_DIR:-$HOME/.claude}/plugins"
[ -d "$BATON/scripts" ] || BATON="$(jq -r '.plugins["baton@maestro"] // [] | ((.[] | select(.scope=="user")) // .[0]) | .installPath // empty' "$_BP/installed_plugins.json" 2>/dev/null)"
[ -d "$BATON/scripts" ] || BATON="$(ls -d "$_BP"/cache/maestro/baton/* 2>/dev/null | sort -V | tail -1)"
[ -d "$BATON/scripts" ] || { echo "baton: cannot locate plugin scripts (set CLAUDE_PLUGIN_ROOT or install the plugin)" >&2; exit 1; }
TS="$BATON/scripts/tracker-sync.sh"
[ -x "$TS" ] && "$TS" push
```

Also never fatal: it exits 0 whatever happens, and prints one line only when there is something
to say (no remote configured, or the push failed). Relay that line. A skipped push is silent when
the context sets `task_tracking.sync: false`. See "Keeping a tracker in step with its remote" in
`../../references/tracker.md`.

If the repo has a `package.json` (or other obvious deps), install them in the worktree.

### Review mode — `baton:start --review <PR>`

A review task is a worktree on **somebody else's PR**. It still gets one leaf and one worktree,
but the branch is the author's, so it is never minted from `naming.branch`, never PR'd by us, and
never merged by us. It is marked by the reserved **`pr-review`** label (in
`../../references/tracker.md`, "Labels baton reads") and by `kind=review` in the identity carrier
and the branch registry. `scripts/pr-review.sh` is the one place that knows how the PR is
fetched and tracked; never hand-roll `gh pr checkout` or `git fetch` here.

**R1 — Resolve the PR.** `<PR>` is a URL or a number:

```bash
# Locate baton: harness env → dev clone → install record → newest cached (references/locating-baton.md)
BATON="${CLAUDE_PLUGIN_ROOT:-}"; [ -n "$BATON" ] && [ -d "$BATON/scripts" ] || BATON="$HOME/code/maestro/baton"
_BP="${CLAUDE_CONFIG_DIR:-$HOME/.claude}/plugins"
[ -d "$BATON/scripts" ] || BATON="$(jq -r '.plugins["baton@maestro"] // [] | ((.[] | select(.scope=="user")) // .[0]) | .installPath // empty' "$_BP/installed_plugins.json" 2>/dev/null)"
[ -d "$BATON/scripts" ] || BATON="$(ls -d "$_BP"/cache/maestro/baton/* 2>/dev/null | sort -V | tail -1)"
[ -d "$BATON/scripts" ] || { echo "baton: cannot locate plugin scripts (set CLAUDE_PLUGIN_ROOT or install the plugin)" >&2; exit 1; }
PRR="$BATON/scripts/pr-review.sh"
PRJ="$("$PRR" resolve "<PR>" --context - <<<"$CTX")" || exit 1     # a URL picks its member repo
# A bare number: add --repo <code_root>/<name> (from a repo-<name> label, or ask which member repo)
jq -r '"PR #\(.number) \(.title) — \(.head) → \(.base) in \(.repo_name)\(if .cross_repo then " (fork: \(.head_owner))" else "" end) [\(.state)]"' <<<"$PRJ"
```

A URL names its repo; `resolve` matches it to a member repo by origin's `OWNER/REPO`, and a repo
that is not a member is an **error** — say so and stop rather than cloning anything. A bare number
needs the repo: from a `repo-<name>` label if the user named a task, otherwise ask. If the PR is
already `MERGED` or `CLOSED`, say so and confirm before going on.

**R2 — Find or create the leaf.** One open review leaf per PR; a closed one is never reused:

```bash
# Locate baton: harness env → dev clone → install record → newest cached (references/locating-baton.md)
BATON="${CLAUDE_PLUGIN_ROOT:-}"; [ -n "$BATON" ] && [ -d "$BATON/scripts" ] || BATON="$HOME/code/maestro/baton"
_BP="${CLAUDE_CONFIG_DIR:-$HOME/.claude}/plugins"
[ -d "$BATON/scripts" ] || BATON="$(jq -r '.plugins["baton@maestro"] // [] | ((.[] | select(.scope=="user")) // .[0]) | .installPath // empty' "$_BP/installed_plugins.json" 2>/dev/null)"
[ -d "$BATON/scripts" ] || BATON="$(ls -d "$_BP"/cache/maestro/baton/* 2>/dev/null | sort -V | tail -1)"
[ -d "$BATON/scripts" ] || { echo "baton: cannot locate plugin scripts (set CLAUDE_PLUGIN_ROOT or install the plugin)" >&2; exit 1; }
PRR="$BATON/scripts/pr-review.sh"; TRK="$BATON/scripts/tracker.sh"
PR_URL="$(jq -r .url <<<"$PRJ")"
M="$("$TRK" list --label pr-review --status all | "$PRR" match-leaves --url "$PR_URL")"
OPEN="$(jq -r '.open[0] // empty' <<<"$M")"; PREV="$(jq -r .closed_latest <<<"$M")"
```

- **`$OPEN` set** → `LEAF=$OPEN`. If its registry entry (`"$TRK" list-branches "$LEAF"`) names a
  worktree that still exists, this is a re-run: say so, skip R3–R4, and go straight to the
  handoff (Step 8) on that worktree. (`baton:resume` will catch it up with the author.)
- **Otherwise** create a new leaf:
  ```bash
  LEAF="$("$TRK" create "Review PR #<n>: <title>" --labels "pr-review,repo-<repo_name>" \
          --description "Review $PR_URL (<head> → <base>)")"
  [ -n "$PREV" ] && "$TRK" link "$LEAF" "$PREV" --type relates-to    # re-review: link the last one
  ```
  The PR URL must be in the description — it is how R2 finds this leaf next time. **No ticket
  labels** (no `jira-…`): the ticket is the author's, and a context's ticket hooks must not start,
  assign or close it.

**R3 — Identity, with `--kind review`.** Write a slug as in Step 5 (e.g. `pr-482-retry-backoff`),
then:

```bash
# Locate baton: harness env → dev clone → install record → newest cached (references/locating-baton.md)
BATON="${CLAUDE_PLUGIN_ROOT:-}"; [ -n "$BATON" ] && [ -d "$BATON/scripts" ] || BATON="$HOME/code/maestro/baton"
_BP="${CLAUDE_CONFIG_DIR:-$HOME/.claude}/plugins"
[ -d "$BATON/scripts" ] || BATON="$(jq -r '.plugins["baton@maestro"] // [] | ((.[] | select(.scope=="user")) // .[0]) | .installPath // empty' "$_BP/installed_plugins.json" 2>/dev/null)"
[ -d "$BATON/scripts" ] || BATON="$(ls -d "$_BP"/cache/maestro/baton/* 2>/dev/null | sort -V | tail -1)"
[ -d "$BATON/scripts" ] || { echo "baton: cannot locate plugin scripts (set CLAUDE_PLUGIN_ROOT or install the plugin)" >&2; exit 1; }
IDENT="$BATON/scripts/task-identity.sh"
ID="$("$IDENT" --leaf "$LEAF" --slug "<written-slug>" --kind review \
                --pr "$(jq -r .number <<<"$PRJ")" --head "$(jq -r .head <<<"$PRJ")" --format env)" || exit 1
eval "$ID"          # … plus TASK_KIND=review, REVIEW_PR
```

`$BR` comes out as **`review/<n>-<head>`** — never the bare head name, which collides with a local
branch of ours, or with `main` on a fork PR. `$DIR` follows `naming.dir` as usual.

**R4 — Claim, worktree, carrier, registry.**

```bash
# Locate baton: harness env → dev clone → install record → newest cached (references/locating-baton.md)
BATON="${CLAUDE_PLUGIN_ROOT:-}"; [ -n "$BATON" ] && [ -d "$BATON/scripts" ] || BATON="$HOME/code/maestro/baton"
_BP="${CLAUDE_CONFIG_DIR:-$HOME/.claude}/plugins"
[ -d "$BATON/scripts" ] || BATON="$(jq -r '.plugins["baton@maestro"] // [] | ((.[] | select(.scope=="user")) // .[0]) | .installPath // empty' "$_BP/installed_plugins.json" 2>/dev/null)"
[ -d "$BATON/scripts" ] || BATON="$(ls -d "$_BP"/cache/maestro/baton/* 2>/dev/null | sort -V | tail -1)"
[ -d "$BATON/scripts" ] || { echo "baton: cannot locate plugin scripts (set CLAUDE_PLUGIN_ROOT or install the plugin)" >&2; exit 1; }
IDENT="$BATON/scripts/task-identity.sh"; PRR="$BATON/scripts/pr-review.sh"; TRK="$BATON/scripts/tracker.sh"
"$TRK" claim "$LEAF"
REPO="$(jq -r .repo_dir <<<"$PRJ")"; WT_BASE=<expanded worktree_base>; WT="$WT_BASE/$DIR"
mkdir -p "$WT_BASE"
"$PRR" checkout --worktree "$WT" --branch "$BR" <<<"$PRJ" || exit 1
"$IDENT" --write-carrier "$WT" --leaf "$LEAF" --slug "$SLUG" --branch "$BR" --kind review --pr "$REVIEW_PR"
"$TRK" record-branch "$LEAF" --repo "<repo_name>" --branch "$BR" --worktree "$WT"
"$TRK" update-branch "$LEAF" "$BR" kind=review "pr=$REVIEW_PR" "pr_url=$PR_URL"
```

`checkout` tracks the author's head: `origin/<head>` for a same-repo PR, and for a fork PR
`refs/pull/<n>/head` fetched to `refs/baton/review/<n>` (outside `refs/remotes`, so `fetch
--prune` can't make it vanish). **Pushing to a fork is out of scope** unless the user asks. It also
records the PR number on the branch (`branch.<b>.baton-pr`), which is what lets cleanup ask GitHub
about the PR by number later.

The registry entry carries `kind`, `pr` and `pr_url`, and every later step reads the PR from
there (or from the carrier's `REVIEW_PR`) — **never from the branch name**. As in Step 6, a failed
registry write is not fatal: say so and carry on. Then push the tracker with
`tracker-sync.sh push`, exactly as Step 6 does. Skip dependency installs unless the user wants to
run the code.

Rejoin at **Step 7** (`on_dispatch` hooks see `TASK_KIND=review`, so a context's ticket glue can
skip it) and **Step 8** (hand-off). In Step 9, say it is a review: read-only by default, and
closing it is the reviewer's call whenever they like — `baton:finish`.

### Step 7 — Fire home.on_dispatch hooks

Run each action in `hooks.home.on_dispatch` (from `$CTX`) — shell command or natural-language
step — in the home/orchestrator session (cwd = workspace).

Hook actions see the full identity group plus the paths: **`LEAF`, `SLUG`, `BR`, `DIR`,
`SESSION_NAME`, `SESSION_TITLE`, `TASK_KIND`** (already exported by Step 5, or R3 for a review)
and `REPO`, `WT_BASE`, `WT`. `TASK_KIND` is `review` for a review task and `task` otherwise. Keep
them exported when running the actions. A hook that builds a path must use `$WT` (or
`$WT_BASE/$DIR`), never `$WT_BASE/$BR` — those are no longer the same string in every context.

### Step 8 — Hand off per work_mode

- **`worktree-new-session`** (default): open a fresh session in the worktree. Nothing further
  needs writing — Step 6 already wrote the identity carrier, which is how the worker discovers
  its bead. (Before 0.5.0 baton wrote nothing at all and the worker parsed the branch name; that
  is now only a back-compat fallback, and it is the reason the branch could not be configured.)

  The handoff is data-driven from `$CTX`'s `handoff` block — `launcher`, `dangerous`, and the
  optional `args`. Never hardcode a specific launcher here.

  ```bash
  WT="$WT_BASE/$DIR"
  LAUNCHER="$(jq -r '.handoff.launcher // ""' <<<"$CTX")"
  DANGEROUS="$(jq -r '.handoff.dangerous // true' <<<"$CTX")"
  if [ "$DANGEROUS" = "true" ]; then CLAUDE_ARGS="--dangerously-skip-permissions"; else CLAUDE_ARGS=""; fi
  export LEAF SLUG BR DIR SESSION_NAME SESSION_TITLE CLAUDE_ARGS   # the launcher's real interface
  ```

  **If `launcher` is set**, invoke it with `handoff.args` (default `["{worktree}"]`, which is
  the plain "command takes a worktree path" shape). Substitute `{worktree}`, `{branch}`,
  `{leaf}`, `{slug}`, `{session_name}`, `{session_title}`, `{claude_args}` in each arg:
  ```bash
  # args default to just the worktree path when handoff.args is absent
  "$LAUNCHER" "$WT"                                   # e.g. args: ["{worktree}"]
  # or, for a launcher that names the session and titles the Claude session itself:
  # args: ["{worktree}", "{session_name}", "{session_title}"]
  ```
  A launcher does **not** have to take args at all — the identity group is exported into its
  environment above, so it can read `$SESSION_NAME` / `$SESSION_TITLE` directly. That's the
  point of computing them here: a launcher should never derive either one itself.

  **If `launcher` is unset (the default)**, use plain tmux — no tmuxinator or wrapper required:
  ```bash
  tmux new-session -d -s "$SESSION_NAME" -c "$WT" "claude $CLAUDE_ARGS"
  if [ -n "$TMUX" ]; then tmux switch-client -t "$SESSION_NAME"; else tmux attach -t "$SESSION_NAME"; fi
  ```
  `hooks.home.on_cleanup` tears the session down by the same `$SESSION_NAME`, recovered from
  the worktree by the same helper — so the two agree by construction, not by convention. If `tmux`
  isn't available, say so and fall back to `worktree-same-session` rather than failing the
  dispatch.

  The new session runs `baton:resume` (via the SessionStart hook / launcher prompt).
- **`worktree-same-session`**: `cd "$WT_BASE/$DIR"` and continue here; then run `baton:resume`
  yourself to orient and begin.
- **`in-place`**: skip the worktree entirely (Step 6 already ran? — for in-place, do NOT create a
  worktree; just work on a branch in the member repo). Orient and begin.

### Step 9 — Orient

Print a short summary: bead id + title, repo, branch, worktree path, session name, whether it's
autonomous-safe, and the first acceptance criterion / next action. Then begin (or, for
new-session handoff, tell the user the worker session is opening — by its `$SESSION_NAME`, so
they can find it in `tmux ls`).
