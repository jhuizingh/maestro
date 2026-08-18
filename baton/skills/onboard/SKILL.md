---
description: Get oriented on how baton works — a plain-language briefing covering contexts, the beads task model, the worktree handoff, and the full baton skill catalog. Reports your registered contexts and which one is active. Run this on your first session.
allowed-tools: Bash(*), Read
---

## baton:onboard

Produce a short, plain-language briefing. Keep it scannable.

### Step 1 — Gather state

```bash
REG="${BATON_REGISTRY:-$HOME/.config/baton/registry.yaml}"
echo "Registry: $REG"
[ -f "$REG" ] && yq -r '.workspaces[]?' "$REG" || echo "(no contexts registered yet)"

RESOLVER="${CLAUDE_PLUGIN_ROOT:-$HOME/code/maestro/baton}/scripts/resolve-context.sh"
[ -x "$RESOLVER" ] || RESOLVER="$HOME/code/maestro/baton/scripts/resolve-context.sh"
"$RESOLVER" 2>/dev/null | jq -r '"Active here: \(.name) (via \(._match))"' || echo "Active here: (none — cd into a member repo, or configure a context)"
```

For each registered workspace, read its `context.yaml` `name` and `description`.

### Step 2 — Brief the user

Explain, in a few short sections:

1. **Contexts** — baton serves multiple project sets from one plugin. A context is a small
   workspace repo (`context.yaml` + `guidance.md`). The active context is auto-detected from
   your current directory (its `member_repos`), falling back to the one marked `default`.
2. **Task model** — work is tracked in beads (`bd`). Parents are planning containers; each
   worktree maps to exactly one **leaf bead**, recorded in an identity carrier written into the
   worktree at creation (`.git/worktrees/<name>/baton-identity`) rather than inferred from a name.
3. **The flow** — `baton:start` picks/creates a leaf and opens a worker session in a worktree;
   the worker runs `baton:resume` and works; `baton:finish` closes it; `baton:cleanup-worktrees`
   reviews finished worktrees and removes them with your confirmation.
4. **Per-context session start** — `<name>-start` (e.g. `personal-start`) opens a home session
   that runs the context's startup tasks (align, tool check, cleanup review, status). It uses the
   current shell by default; set `work_mode.home: tmux-session` and each context instead gets its
   own tmux session that later `<name>-start`s reattach to rather than re-running startup.
5. **Customization** — startup tasks, lifecycle hooks, and preferences (`guidance.md`) all live
   in *your* workspace repo, edited via `baton:configure` or the retrospective — never the plugin.
   There are six lifecycle hooks (`home.on_dispatch`/`on_cleanup`,
   `worker.on_resume`/`pre_pr`/`pre_finish`/`post_finish`); if the user wants the detail, point
   them at `references/hooks.md` rather than explaining each one here.

### Step 3 — List the skills

Print the catalog (one line each): `setup, doctor, configure, onboard, whereami, start,
resume, finish, worktrees, session-start, split, new-repo, task-add, task-list, remember`.

### Step 4 — Suggest a next step

- No contexts registered → suggest `baton:configure` to create the first one.
- Contexts exist → suggest `<name>-start` or `baton:start`.
