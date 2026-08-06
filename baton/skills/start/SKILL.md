---
description: Start work on a task in the active context — resolve a single leaf bead (pick a child of a parent, use a leaf directly, or create one), create its git worktree and branch, and hand off per the context's work mode (new tmux session by default). One worktree ⇔ one leaf bead.
argument-hint: "[bead-id | free-text task]"
allowed-tools: Bash(*), Read
---

## baton:start

### Step 1 — Resolve context + guidance

```bash
RESOLVER="${CLAUDE_PLUGIN_ROOT:-$HOME/code/maestro/baton}/scripts/resolve-context.sh"
[ -x "$RESOLVER" ] || RESOLVER="$HOME/code/maestro/baton/scripts/resolve-context.sh"
CTX="$("$RESOLVER")" || { echo "$CTX"; exit 1; }
export BEADS_DIR="$(echo "$CTX" | jq -r '.task_tracking.dir' | sed "s|^~|$HOME|")"
WS="$(echo "$CTX" | jq -r '._workspace')"
GUIDE="$WS/$(echo "$CTX" | jq -r '.guidance // "guidance.md"')"
echo "$CTX" | jq -r '"Context: \(.name)  tracker: \(.task_tracking.dir)  mode: \(.work_mode.default)"'
```

Read `$GUIDE` if it exists and honor its preferences for the rest of this skill.

### Step 2 — Resolve the LEAF bead (invariant: one worktree ⇔ one leaf)

Determine `LEAF` (a leaf bead id):

- **`$ARGUMENTS` is a bead id** → fetch it: `bd show <id> --json`.
  - If it has open children (`bd children <id>`), it's a **parent** — list the open children and
    ask which to work, or offer to create a new child (Step 3). Do **not** worktree a parent.
  - If it's a **leaf** (no open children) → `LEAF=<id>`.
- **`$ARGUMENTS` is free text** → create a leaf: `bd q "<text>"` (capture the id).
- **`$ARGUMENTS` empty** → show ready work (`bd ready` or `bd list --status open`), let the user
  pick a leaf; if they pick a parent, drop into the child picker; if they want something ad-hoc,
  create a stub (`bd q "<text>"`). Every worktree gets a unique leaf — never proceed without one.

Confirm: "Starting `<LEAF>` — <title>." If `bd show <LEAF> --json` carries the `autonomous-safe`
label, say so explicitly — e.g. "marked autonomous-safe: this worker will go through PR,
merge, and `baton:finish` cleanup without pausing for confirmation at those gates" — since
that's a meaningfully different hand-off than the default human-gated flow.

### Step 3 — (When decomposing) create a child leaf

If the user is carving a chunk off a parent:
```bash
bd create "<child title>" --parent <PARENT> --json   # inherits parent labels
```
Use the new child id as `LEAF`. (For deeper splitting mid-work, see `baton:split`.)

### Step 4 — Check dependencies

If `LEAF` has an unmet `blocked-by` dependency (an open blocker), warn the user and confirm
before proceeding.

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
slug is generated **once** and read back from the branch name forever after: nothing recomputes
it, so it never has to be reproducible.

Fall back to plain slugify (lowercase → non-alphanumeric to `-` → trim) only when the bead has
no useful description, or when the context sets `naming.slug: slugify` — check
`.slug_mode` in the helper output below.

Now compute the whole identity group in one call. **`baton/scripts/task-identity.sh` is the only
place these names are derived** — `baton:cleanup-worktrees` and `baton:resume` recompute them
later from the branch alone using this same script, so nothing can drift:

```bash
IDENT="${CLAUDE_PLUGIN_ROOT:-$HOME/code/maestro/baton}/scripts/task-identity.sh"
[ -x "$IDENT" ] || IDENT="$HOME/code/maestro/baton/scripts/task-identity.sh"
ID="$("$IDENT" --leaf "<LEAF>" --slug "<written-slug>" --format env)" || exit 1
eval "$ID"          # capture first: `eval "$(cmd)"` would swallow cmd's exit status
echo "$LEAF / $SLUG / $BR / $SESSION_NAME / $SESSION_TITLE"
```

That exports the five names for the rest of this skill:

| | default | example |
|---|---|---|
| `LEAF` | bead id | `jbh-zvs` |
| `SLUG` | written, above | `kids-overnight-hvac` |
| `BR` | `<LEAF>-<SLUG>` | `jbh-zvs-kids-overnight-hvac` |
| `SESSION_NAME` | `{slug}-{leaf}` | `kids-overnight-hvac-jbh-zvs` |
| `SESSION_TITLE` | `{slug_prose} ({leaf})` | `kids overnight hvac (jbh-zvs)` |

`BR` stays `<LEAF>-<SLUG>` — load-bearing, since `baton:resume` and `baton:cleanup-worktrees`
parse that leaf prefix. A context can override the last two formats via its `naming:` block;
absent one, these defaults apply.

Then:
- Target repo: if the bead carries a `repo-<name>` label, map `<name>` to `<code_root>/<name>`;
  otherwise ask which member repo this work belongs to (offer the context's `member_repos`).
- Worktree base: expand `worktree_base` (`{code_root}` and `{repo}` substituted), e.g.
  `~/code/<repo>-worktrees`.

### Step 6 — Claim + create the worktree

```bash
bd update "$LEAF" --claim                     # assign to me + in_progress
REPO=<code_root>/<repo>; WT_BASE=<expanded worktree_base>
git -C "$REPO" fetch --all --prune
mkdir -p "$WT_BASE"
git -C "$REPO" worktree add "$WT_BASE/$BR" -b "$BR" origin/main    # or without -b if branch exists
```
If the repo has a `package.json` (or other obvious deps), install them in the worktree.

### Step 7 — Fire home.on_dispatch hooks

Run each action in `hooks.home.on_dispatch` (from `$CTX`) — shell command or natural-language
step — in the home/orchestrator session (cwd = workspace).

Hook actions see the full identity group plus the paths: **`LEAF`, `SLUG`, `BR`, `SESSION_NAME`,
`SESSION_TITLE`** (already exported by Step 5) and `REPO`, `WT_BASE`. Keep them exported when
running the actions.

### Step 8 — Hand off per work_mode

- **`worktree-new-session`** (default): open a fresh session in the worktree. **Write no file** —
  the worker discovers its bead from the branch.

  The handoff is data-driven from `$CTX`'s `handoff` block — `launcher`, `dangerous`, and the
  optional `args`. Never hardcode a specific launcher here.

  ```bash
  WT="$WT_BASE/$BR"
  LAUNCHER="$(echo "$CTX" | jq -r '.handoff.launcher // ""')"
  DANGEROUS="$(echo "$CTX" | jq -r '.handoff.dangerous // true')"
  if [ "$DANGEROUS" = "true" ]; then CLAUDE_ARGS="--dangerously-skip-permissions"; else CLAUDE_ARGS=""; fi
  export LEAF SLUG BR SESSION_NAME SESSION_TITLE CLAUDE_ARGS   # the launcher's real interface
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
  `hooks.home.on_cleanup` tears the session down by the same `$SESSION_NAME`, recomputed from
  the branch by the same helper — so the two agree by construction, not by convention. If `tmux`
  isn't available, say so and fall back to `worktree-same-session` rather than failing the
  dispatch.

  The new session runs `baton:resume` (via the SessionStart hook / launcher prompt).
- **`worktree-same-session`**: `cd "$WT_BASE/$BR"` and continue here; then run `baton:resume`
  yourself to orient and begin.
- **`in-place`**: skip the worktree entirely (Step 6 already ran? — for in-place, do NOT create a
  worktree; just work on a branch in the member repo). Orient and begin.

### Step 9 — Orient

Print a short summary: bead id + title, repo, branch, worktree path, session name, whether it's
autonomous-safe, and the first acceptance criterion / next action. Then begin (or, for
new-session handoff, tell the user the worker session is opening — by its `$SESSION_NAME`, so
they can find it in `tmux ls`).
