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
IDENT="${CLAUDE_PLUGIN_ROOT:-$HOME/code/maestro/baton}/scripts/task-identity.sh"
[ -x "$IDENT" ] || IDENT="$HOME/code/maestro/baton/scripts/task-identity.sh"
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
bd update "$LEAF" --claim                     # assign to me + in_progress
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

If the repo has a `package.json` (or other obvious deps), install them in the worktree.

### Step 7 — Fire home.on_dispatch hooks

Run each action in `hooks.home.on_dispatch` (from `$CTX`) — shell command or natural-language
step — in the home/orchestrator session (cwd = workspace).

Hook actions see the full identity group plus the paths: **`LEAF`, `SLUG`, `BR`, `DIR`,
`SESSION_NAME`, `SESSION_TITLE`** (already exported by Step 5) and `REPO`, `WT_BASE`, `WT`. Keep
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
  LAUNCHER="$(echo "$CTX" | jq -r '.handoff.launcher // ""')"
  DANGEROUS="$(echo "$CTX" | jq -r '.handoff.dangerous // true')"
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
