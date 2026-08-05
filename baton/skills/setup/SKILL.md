---
description: One-time machine bootstrap for baton — wires the shell integration (so <name>-start commands and BEADS_DIR auto-switching work), ensures the config dir exists, checks required tools, and optionally wires your tmux launcher for the new-session handoff. Run once per machine, then baton:configure to create your first context.
allowed-tools: Bash(*), Read, Edit
---

## baton:setup

Bootstrap this machine. Idempotent — safe to re-run. Explain each step as you go and stop on
any hard error.

### Step 1 — Config dir + registry

```bash
mkdir -p "$HOME/.config/baton"
REG="$HOME/.config/baton/registry.yaml"
[ -f "$REG" ] || printf 'workspaces: []\n' > "$REG"
echo "Registry: $REG"; cat "$REG"
```

### Step 2 — Locate the shell integration directory

```bash
SHELL_DIR="${CLAUDE_PLUGIN_ROOT:-$HOME/code/maestro/baton}/shell"
[ -d "$SHELL_DIR" ] || SHELL_DIR="$HOME/code/maestro/baton/shell"
echo "Shell integration: $SHELL_DIR"; [ -d "$SHELL_DIR" ] || { echo "NOT FOUND — is the plugin installed?"; exit 1; }
```

Point at the installed plugin copy (`$CLAUDE_PLUGIN_ROOT`) so it tracks plugin updates, or at
a stable working checkout (`~/code/maestro`) if you develop the plugin locally.

### Step 3 — Symlink into ~/.zsh_profiles and ensure the loader

Symlink the **directory** (a common loader glob uses the `(.)` qualifier, which excludes a
symlinked *file* but traverses a symlinked *directory* to its real files):

```bash
mkdir -p "$HOME/.zsh_profiles"
ln -sfn "$SHELL_DIR" "$HOME/.zsh_profiles/baton"
ls -l "$HOME/.zsh_profiles/baton"
```

Ensure `~/.zshrc` sources `~/.zsh_profiles` recursively. If `grep -q zsh_profiles ~/.zshrc`
finds nothing, append this block (tell the user you're editing `~/.zshrc`):

```bash
# Source all shell scripts in ~/.zsh_profiles and its subdirectories (follows symlinks)
if [ -d "$HOME/.zsh_profiles" ]; then
  for file in "$HOME"/.zsh_profiles/***/*(.N); do
    source "$file"
  done
fi
```

Tell the user to `source ~/.zshrc` or open a new shell for `<name>-start` to appear.

### Step 4 — Tool check

Invoke **`baton:doctor`** and let it report/fix missing tools.

### Step 5 — Optional: wire the tmux handoff

The default work mode opens each task in a fresh **plain tmux** session — `baton:start` creates
it directly, so no tmuxinator project or launcher wrapper is required. Only `tmux` itself needs
to be installed. (A context can point `handoff.launcher` at a custom command instead; that's
opt-in, not the default.)

Two ways a handed-off session orients itself to its task:

- **Automatic (recommended):** baton ships a SessionStart hook that detects a baton worktree
  (branch resolves to a leaf bead) and tells the session to run `baton:resume`. This needs no
  launcher changes and is active whenever the plugin is enabled — it's what makes the plain-tmux
  default work with zero extra setup.
- **Launcher prompt (optional):** *only* relevant if you already use a `tmuxinator` `code`
  project with a `code-launch-claude` script and want to point `handoff.launcher` at it. Its
  default prompt can also run `baton:resume` in a baton worktree. Detect it and offer to help:

```bash
LAUNCHER="${XDG_CONFIG_HOME:-$HOME/.config}/tmuxinator/code-launch-claude"
[ -f "$LAUNCHER" ] && echo "Found launcher: $LAUNCHER" || echo "No tmuxinator code launcher found (that's fine — the SessionStart hook covers handoff, or use a same-session work_mode)."
```

If found and the user wants it, propose editing its `default_prompt` to: *"If this is a baton
worktree, run baton:resume; otherwise <its existing prompt>."* **Warn if the file appears
chezmoi-managed** (contains `chezmoi`) — in that case edit the chezmoi source, not the live
file. Never edit it without the user's confirmation.

### Step 6 — Next step

Print: "Setup complete. Run **baton:configure** to create your first context." If the user
already has workspace repos cloned, mention they can add them to the registry with
`baton:configure` (choose *register existing*).
