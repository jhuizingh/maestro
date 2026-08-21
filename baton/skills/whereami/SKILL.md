---
description: Report which baton context is active for the current directory, and the paths it resolves to (tracker, code root, worktree base). A quick debugging aid for context auto-detection.
allowed-tools: Bash(*)
---

## baton:whereami

Resolve and display the active context for the current working directory.

### Step 1 — Run the resolver

```bash
RESOLVER="${CLAUDE_PLUGIN_ROOT:-$HOME/code/maestro/baton}/scripts/resolve-context.sh"
[ -x "$RESOLVER" ] || RESOLVER="$HOME/code/maestro/baton/scripts/resolve-context.sh"
CTX="$("$RESOLVER")" || { echo "$CTX"; exit 1; }
echo "$CTX" | jq '{
  context: .name,
  resolved_by: ._match,
  workspace: ._workspace,
  tracker: .task_tracking.dir,
  code_root: .code_root,
  worktree_base: .worktree_base,
  member_repos: .member_repos,
  default_work_mode: .work_mode.default
}'
```

### Step 2 — Explain

Print a one-line summary in plain language, e.g.:

> Active context: **personal** (resolved by *cwd* — you're inside a member repo). Tasks
> live in `<tracker>`; new work goes to worktrees under `<worktree_base>`.

If resolution failed (no context), relay the resolver's error and suggest `baton:configure`
(to create a context) or `cd` into a member repo.

If the cwd is a **worktree** rather than a member repo's primary clone, add one line pointing at
`baton:status`: this skill answers "which context am I in", and the follow-up question — "so what
is this worktree's task, and where does it stand?" — is that skill's, also read-only.

### Step 3 — Surface pending worktree cleanup

```bash
BEADS_DIR="$(echo "$CTX" | jq -r '.task_tracking.dir' | sed "s|^~|$HOME|")" \
  bd list --all --label ready-for-worktree-delete --json 2>/dev/null | jq length
```

If this is non-zero, say so explicitly — e.g. "3 worktrees are flagged ready for cleanup; run
`baton:cleanup-worktrees` to review them" — rather than leaving it to be discovered only when a `cleanup`
startup task happens to run.
