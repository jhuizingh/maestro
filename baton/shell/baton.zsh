# baton — zsh integration.
#
# Sourced from ~/.zsh_profiles (baton:setup symlinks this file there). Provides:
#   • baton-start <context> [--dangerous]   open a home session for a context
#   • <name>-start                          auto-generated per registered context
#   • a chpwd hook that points BEADS_DIR at the active context's tracker — unless cwd is
#     under a repo with its own self-hosted `.beads/`, in which case it defers to that
#     (see _baton_local_beads_dir)
#
# Requires yq (v4) and jq on PATH. Safe to source in any shell; no-ops cleanly
# when nothing is configured yet.

export BATON_REGISTRY="${BATON_REGISTRY:-$HOME/.config/baton/registry.yaml}"

# Locate the plugin's context resolver (installed copy, marketplace copy, or dev checkout).
_baton_resolver() {
  local c
  if [[ -n "${BATON_RESOLVER:-}" && -x "$BATON_RESOLVER" ]]; then print -r -- "$BATON_RESOLVER"; return; fi
  for c in \
    "$HOME/.claude/plugins/cache/"*/baton/*/scripts/resolve-context.sh(N) \
    "$HOME/.claude/plugins/marketplaces/"*/baton/scripts/resolve-context.sh(N) \
    "$HOME/code/maestro/baton/scripts/resolve-context.sh"(N); do
    [[ -x "$c" ]] && { print -r -- "$c"; return; }
  done
}

# Open a home/orchestrator session for a context: pin it, cd to its home, run session-start.
baton-start() {
  emulate -L zsh
  local ctx="${1:-}"; [[ -n "$ctx" ]] && shift
  local dangerous=0 a
  for a in "$@"; do [[ "$a" == "--dangerous" ]] && dangerous=1; done
  if [[ -z "$ctx" ]]; then print -u2 "usage: baton-start <context> [--dangerous]"; return 1; fi

  local reg="$BATON_REGISTRY" ws home="" found=""
  for ws in ${(f)"$(yq -r '.workspaces[]?' "$reg" 2>/dev/null)"}; do
    ws="${ws/#\~/$HOME}"
    [[ -f "$ws/context.yaml" ]] || continue
    if [[ "$(yq -r '.name // ""' "$ws/context.yaml" 2>/dev/null)" == "$ctx" ]]; then
      found="$ws"
      home="$(yq -r '.home // ""' "$ws/context.yaml" 2>/dev/null)"
      break
    fi
  done
  if [[ -z "$found" ]]; then print -u2 "baton-start: unknown context '$ctx' (not in $reg)"; return 1; fi
  [[ -n "$home" && "$home" != "null" ]] || home="$found"
  home="${home/#\~/$HOME}"

  export BATON_CONTEXT="$ctx"
  cd "$home" || return 1
  if (( dangerous )); then
    claude --dangerously-skip-permissions "run baton:session-start"
  else
    claude "run baton:session-start"
  fi
}

# Define a <name>-start function for each registered workspace (picked up in new shells).
_baton_gen_starts() {
  emulate -L zsh
  local reg="$BATON_REGISTRY" ws name
  [[ -f "$reg" ]] || return 0
  for ws in ${(f)"$(yq -r '.workspaces[]?' "$reg" 2>/dev/null)"}; do
    ws="${ws/#\~/$HOME}"
    [[ -f "$ws/context.yaml" ]] || continue
    name="$(yq -r '.name // ""' "$ws/context.yaml" 2>/dev/null)"
    [[ -n "$name" && "$name" != "null" ]] || continue
    # define <name>-start as a function that pins this context
    eval "${name}-start() { baton-start ${(q)name} \"\$@\"; }"
  done
}
_baton_gen_starts

# Walk up from cwd (capped at $HOME) looking for a repo with its own self-hosted bd tracker
# (marked by a tracked .beads/config.yaml — present even in a fresh worktree before
# `bd bootstrap`, unlike the gitignored embeddeddolt/ data dir). Prints its .beads dir and
# returns 0 if found, else returns 1.
_baton_local_beads_dir() {
  emulate -L zsh
  local d="$PWD"
  while true; do
    [[ -f "$d/.beads/config.yaml" ]] && { print -r -- "$d/.beads"; return 0; }
    [[ "$d" == "$HOME" || "$d" == "/" ]] && return 1
    d="${d:h}"
  done
}

# Keep BEADS_DIR pointed at the active context's tracker so bare `bd` targets the right DB —
# unless cwd is under a repo with its own self-hosted tracker, which takes priority (forcing
# the context's central tracker there would silently shadow the repo-local one; this exact bug
# caused a real DB-divergence incident, so the priority order here is deliberate).
_baton_chpwd() {
  emulate -L zsh
  local res dir local_dir
  if local_dir="$(_baton_local_beads_dir)"; then
    export BEADS_DIR="$local_dir"
    return 0
  fi
  res="$(_baton_resolver)"; [[ -n "$res" ]] || return 0
  dir="$("$res" 2>/dev/null | jq -r '.task_tracking.dir // empty' 2>/dev/null)" || return 0
  [[ -n "$dir" ]] && export BEADS_DIR="${dir/#\~/$HOME}"
}
autoload -Uz add-zsh-hook 2>/dev/null && add-zsh-hook chpwd _baton_chpwd
_baton_chpwd
