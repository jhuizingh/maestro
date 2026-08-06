---
description: Create or edit a baton context — a small workspace repo holding a context.yaml (task tracker, member repos, GitHub owner, work mode, startup tasks, hooks) plus a guidance.md. Registers it so its <name>-start command appears. Re-run anytime to adjust tasks, tools, hooks, or the retrospective.
argument-hint: "[context-name] | register <path> | edit <name>"
allowed-tools: Bash(*), Read, Write, Edit
---

## baton:configure

Guided setup/editing of a context. All per-context behavior lives in the workspace repo this
creates — never in the plugin.

### Step 0 — Pick a mode

- `$ARGUMENTS` empty or a bare name → **create a new context** (default flow below).
- `register <path>` → the workspace repo already exists; validate it has a `context.yaml`, then
  just add its path to the registry (Step 8) and stop.
- `edit <name>` → load that context's `context.yaml`, ask which field(s) to change, rewrite the
  file, and stop. Re-read it first so edits are against current state.

### Step 1 — Core identity

Ask (offer sensible defaults):
- **name** (slug, e.g. `personal`, `client`) — also the `<name>-start` command and, by
  convention, the workspace repo `~/code/<name>-workspace`.
- **description** — one line.
- **default?** — should this be the fallback when cwd matches no member repo? (Only one
  context should be `default: true`; if another already is, warn.)

### Step 2 — Task tracking

- **type** — default `beads`.
- **dir** — the beads `.beads` dir. Convention: a dedicated repo `~/code/<name>-task-tracking`.
  Offer to **create it now**:
  ```bash
  TT="$HOME/code/<name>-task-tracking"
  mkdir -p "$TT" && ( cd "$TT" && git init -q && bd init )
  ```
  `bd init` is correct **only here** — a brand-new tracker with no history anywhere yet. If the
  user instead points at an **existing** tracker (this machine or another), never re-run `bd
  init` against it and never copy its `.beads/config.yaml` wholesale — validate the dir exists,
  and if it needs setting up fresh in this location, use `bd bootstrap` instead (non-destructive,
  auto-detects the right action). See `baton:beads` for the full init-vs-bootstrap rule and why
  it matters.

### Step 3 — GitHub

- **host** — default `github.com`.
- **owner** — the account/org new repos are created under and PRs target. Ask explicitly.
- **new_repo_prefix** — prefix for new repos this context creates (e.g. `myproj-`); may be empty.
- **signoff_required** — default `true` (baton:new-repo asks before creating).

### Step 4 — Paths

- **home** — where `<name>-start` cd's to. Default `~/code/<name>-workspace`.
- **code_root** — default `~/code`.
- **worktree_base** — default `"{code_root}/{repo}-worktrees"`.

### Step 5 — Member repos

Ask which repos belong to this context — the cwd-based auto-detection key. Accept explicit
paths and globs (e.g. `~/code/myproj-*`). Offer to scan `code_root` and let the user tick which
subdirectories belong here. Store under `member_repos`.

### Step 6 — Work mode, tools, startup tasks

- **work_mode.default** — `worktree-new-session` (default) | `worktree-same-session` | `in-place`.
- **handoff.launcher** — only asked when `work_mode.default` is `worktree-new-session`. Default
  `""`, meaning **plain tmux**: baton creates a detached session itself and switches to it, which
  works out of the box with no wrapper. Offer the alternative only if they want it: any command
  that takes a worktree path (e.g. a `tmuxinator` wrapper).
- **handoff.args** — optional, only worth asking if they named a launcher that needs more than a
  path. Default `["{worktree}"]`. Available placeholders: `{worktree}`, `{branch}`, `{leaf}`,
  `{slug}`, `{session_name}`, `{session_title}`, `{claude_args}`. Mention that a launcher can
  equally read `$SESSION_NAME` / `$SESSION_TITLE` from its environment — baton exports the whole
  identity group — so a launcher never has to derive its own session name or title.
- **handoff.dangerous** — launch worker sessions with `--dangerously-skip-permissions`. Default
  `true`, since a worker session is expected to run unattended in an isolated worktree; say that
  plainly and let them decline.
- **required_tools** — extra tools beyond baton's baseline (e.g. `node`, `docker`); may be empty.
- **startup_tasks** — what `baton:session-start` runs every time. Default (preselected):
  `align, doctor, cleanup, status`. Let the user reorder, drop, or add custom entries (a shell
  command string or a natural-language step).
- **naming** — don't ask; the plugin's defaults are good and the block is optional. Write it only
  if the user brings it up or wants something specific. It overrides how a task's names are built
  (see `scripts/task-identity.sh`):
  ```yaml
  naming:
    slug: written                     # or `slugify` to opt back into mechanical truncation
    session_name: "{slug}-{leaf}"     # tmux session; sanitized to [A-Za-z0-9_-]
    session_title: "{slug_prose} ({leaf})"
  ```
  The branch is always `<leaf>-<slug>` and is not configurable — `baton:resume` and
  `baton:cleanup-worktrees` parse that prefix.

### Step 7 — Hooks, retro, guidance

- **hooks** — scaffold empty lists the user can fill later. Two groups by session context:
  `home` (`on_dispatch`, `on_cleanup`) and `worker` (`on_resume`, `pre_pr`, `pre_finish`,
  `post_finish`). `pre_pr` runs from `baton:pr`, right before a PR is opened — the extension point
  for whatever should gate a PR in this context (typecheck, lint, a changelog script, a
  project-specific check).
  Ask if they want to seed any now (e.g. `worker.pre_finish: ["npm test"]`).
  Seed `home.on_cleanup` by default (rather than empty) with a tmux teardown, since the
  new-session handoff leaves a stale session behind otherwise — `baton:cleanup-worktrees` runs
  these actions per removal with the identity group (`$LEAF`, `$SLUG`, `$BR`, `$SESSION_NAME`,
  `$SESSION_TITLE`) and `$WT` (worktree path) set in the environment:
  ```yaml
  on_cleanup:
    - 'tmux kill-session -t "$SESSION_NAME" 2>/dev/null || true'
  ```
  `$SESSION_NAME` is the exact string `baton:start` used to create the session — both come from
  `scripts/task-identity.sh`, so this needs no adjustment for a custom `handoff.launcher`
  (a launcher is *handed* `$SESSION_NAME` rather than deriving one).
  If any member repo picked in Step 5 has an `.envrc` (check with `[ -f <repo>/.envrc ]`) or the
  user says they use direnv, offer to seed `home.on_dispatch` with the copy-then-allow pattern —
  `git worktree add` never checks out gitignored files (`.envrc` is always gitignored) and
  `post-checkout` doesn't reliably fire for worktrees to compensate (confirmed on git 2.51.0), so
  the copy must be an explicit dispatch step; and direnv treats a copy as new/unapproved content
  at its new path even when identical to an already-allowed `.envrc` elsewhere, so the `allow`
  must immediately follow it or the new worker session opens blocked (confirmed 2026-07-27):
  ```yaml
  on_dispatch:
    - "Copy .envrc from the repo root into the new worktree if the repo has one and the worktree doesn't yet: cp \"$REPO/.envrc\" \"$WT_BASE/$BR/.envrc\" only when [ -f \"$REPO/.envrc\" ] && [ ! -f \"$WT_BASE/$BR/.envrc\" ]."
    - "Immediately after the .envrc copy above, pre-approve it so direnv doesn't block the new session: (cd \"$WT_BASE/$BR\" && direnv allow) only when [ -f \"$WT_BASE/$BR/.envrc\" ] && command -v direnv >/dev/null 2>&1."
  ```
- **retro** — enable the "what could have gone better?" prompt? Default `enabled: true`,
  `when: [finish]`.
- **guidance** — filename for the evolving preferences doc, default `guidance.md`.

### Step 8 — Write the workspace repo

Create `~/code/<name>-workspace` (git init if new) and write these files.

`context.yaml` (fill from the answers; this is the exact schema skills read):

```yaml
name: <name>
default: <true|false>
description: <description>

task_tracking:
  type: beads
  dir: ~/code/<name>-task-tracking/.beads

github:
  host: github.com
  owner: <owner>
  new_repo_prefix: <prefix>
  signoff_required: true

home: ~/code/<name>-workspace
code_root: ~/code
worktree_base: "{code_root}/{repo}-worktrees"

member_repos:
  - <path-or-glob>

work_mode:
  default: worktree-new-session

required_tools: []

startup_tasks:
  - align
  - doctor
  - cleanup
  - status

handoff:
  launcher: ""          # empty = plain tmux (no wrapper needed); or a command taking a path
  args: ["{worktree}"]  # only if the launcher needs more than a path
  dangerous: true

guidance: guidance.md

hooks:
  home:
    on_dispatch: []
    on_cleanup:
      - 'tmux kill-session -t "$SESSION_NAME" 2>/dev/null || true'
  worker:
    on_resume: []
    pre_pr: []
    pre_finish: []
    post_finish: []

retro:
  enabled: true
  when: [finish]
```

`guidance.md` — seed with a header:

```markdown
# Guidance — <name>

Evolving preferences and conventions for this context. baton skills read and honor this on
every run. `baton:remember` and the retrospective append here.
```

`CLAUDE.md` — a short note that this is a baton workspace repo and that `baton:session-start`
runs the context's startup tasks.

`.claude/settings.json` — pre-approve the session-start script if you add one:
```json
{ "permissions": { "allow": [] } }
```

### Step 9 — Register + validate

Add the workspace path to the registry (idempotent):

```bash
REG="$HOME/.config/baton/registry.yaml"
WS="$HOME/code/<name>-workspace"
yq -i '.workspaces += ["'"$WS"'"] | .workspaces |= unique' "$REG"
yq '.workspaces' "$REG"
```

Then validate by running the resolver from inside a member repo and from `~/work`:

```bash
RESOLVER="${CLAUDE_PLUGIN_ROOT:-$HOME/code/maestro/baton}/scripts/resolve-context.sh"
[ -x "$RESOLVER" ] || RESOLVER="$HOME/code/maestro/baton/scripts/resolve-context.sh"
"$RESOLVER" | jq '{name,_match}'
```

Tell the user to open a new shell (or `source ~/.zshrc`) for the `<name>-start` command to
appear. Suggest `<name>-start` or `baton:start` as the next step.
