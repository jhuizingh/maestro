---
description: Verify this machine has every tool baton needs — its baseline (git, bd, gh, yq, jq), plus tmux and any custom handoff.launcher when the context uses the worktree-new-session work mode, plus the active context's required_tools — and offer to install or fix anything missing. Runs at setup and, by default, on every session start so environment drift is caught over time.
allowed-tools: Bash(*)
---

## baton:doctor

Check the environment and report what's missing, then offer to fix it. Non-destructive:
never installs anything without asking.

### Step 1 — Resolve context (best-effort)

```bash
RESOLVER="${CLAUDE_PLUGIN_ROOT:-$HOME/code/maestro/baton}/scripts/resolve-context.sh"
[ -x "$RESOLVER" ] || RESOLVER="$HOME/code/maestro/baton/scripts/resolve-context.sh"
CTX="$("$RESOLVER" 2>/dev/null || true)"
```

If `CTX` is empty (no context yet — e.g. during first-run setup), continue with just the
baseline checks below.

### Step 2 — Determine the tool list

Baseline (always required): `git`, `bd`, `gh`, `yq`, `jq`.

Conditional — only required for the `worktree-new-session` work mode:

- `tmux` — needed for the default plain-tmux handoff.
- The context's `handoff.launcher`, **if** it sets one (e.g. `tmuxinator`). Check the launcher
  command itself, not `tmuxinator` specifically — the default is plain tmux with no wrapper, so
  a missing `tmuxinator` is only a problem for contexts that actually ask for it.

```bash
COND=""
if [ -n "$CTX" ]; then
  [ "$(echo "$CTX" | jq -r '.work_mode.default // "worktree-new-session"')" = "worktree-new-session" ] && COND="tmux"
  L="$(echo "$CTX" | jq -r '.handoff.launcher // ""')"
  [ -n "$L" ] && COND="$COND ${L%% *}"
fi
```

Context extras: if `CTX` is non-empty, add `.required_tools[]`:

```bash
EXTRA=""
[ -n "$CTX" ] && EXTRA="$(echo "$CTX" | jq -r '.required_tools[]?' 2>/dev/null | tr '\n' ' ')"
echo "Extra tools for this context: ${EXTRA:-<none>}"
```

### Step 3 — Check presence

For each tool in `git bd gh yq jq $COND $EXTRA`, run `command -v <tool>` and record
present/missing. Print a checklist, marking conditional tools with what needs them:

```
baton doctor — <context or "no context">
  ✅ git        ✅ bd         ✅ gh        ✅ yq        ✅ jq
  ✅ tmux       (worktree-new-session handoff)
  ❌ node       ✅ docker
```

With no context resolved, check the baseline only and note that handoff tools depend on the
context's work mode.

### Step 4 — Offer to fix

If everything is present, print "All required tools present." and stop.

Otherwise list what's missing and offer to install it. Prefer Homebrew on macOS
(`brew install <formula>`) where the tool has a known formula (`gh`, `tmux`, `tmuxinator`,
`yq`, `jq`, `node`; beads = `brew install beads`). For anything without an obvious install
path (or a context-specific tool), tell the user the tool is missing and ask how they'd like
to handle it — do **not** guess an installer. Only run an install command after the user
confirms.

### Step 5 — Re-check

After any install, re-run the presence check for the affected tools and report the final state.

### See also

`bd` being present doesn't mean it's pointed at the right database. For ambient `BEADS_DIR`
drift, `config.yaml` sync.remote verification, and init-vs-bootstrap safety, run `baton:beads`.
