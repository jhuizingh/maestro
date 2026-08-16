# baton — zsh integration.
#
# Sourced from ~/.zsh_profiles (baton:setup symlinks this file there). Provides:
#   • baton-start <context> [--dangerous]   open a home session for a context — inline, or in
#                                           its own tmux session per work_mode.home
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

# The orchestrator command, defined in exactly one place. Both the inline path and the
# tmux-session path below use it, so there is no second copy to keep in sync — the same
# reason task-identity.sh owns the session name rather than each caller deriving it.
_baton_home_cmd() {
  emulate -L zsh
  if (( ${1:-0} )); then
    print -r -- 'claude --dangerously-skip-permissions "run baton:session-start"'
  else
    print -r -- 'claude "run baton:session-start"'
  fi
}

# Open a home/orchestrator session for a context: pin it, cd to its home, run session-start.
#
# `work_mode.home` decides where that happens:
#   inline (default) — run claude right here, in the calling shell.
#   tmux-session     — get-or-create one dedicated tmux session per context. Without this,
#                      starting several contexts from one terminal piles every orchestrator
#                      into whichever session you happened to be sitting in.
#
# The tmux path is deliberately get-or-**create**: if the session already exists you are just
# put back into it, and baton:session-start is NOT re-run. Re-running the startup tasks over a
# session that is already open is never what "take me back to my jbh window" means.
baton-start() {
  emulate -L zsh
  local ctx="${1:-}"; [[ -n "$ctx" ]] && shift
  local dangerous=0 a
  for a in "$@"; do [[ "$a" == "--dangerous" ]] && dangerous=1; done
  if [[ -z "$ctx" ]]; then print -u2 "usage: baton-start <context> [--dangerous]"; return 1; fi

  local reg="$BATON_REGISTRY" ws home="" found="" cfg=""
  for ws in ${(f)"$(yq -r '.workspaces[]?' "$reg" 2>/dev/null)"}; do
    ws="${ws/#\~/$HOME}"
    [[ -f "$ws/context.yaml" ]] || continue
    if [[ "$(yq -r '.name // ""' "$ws/context.yaml" 2>/dev/null)" == "$ctx" ]]; then
      found="$ws"; cfg="$ws/context.yaml"
      home="$(yq -r '.home // ""' "$cfg" 2>/dev/null)"
      break
    fi
  done
  if [[ -z "$found" ]]; then print -u2 "baton-start: unknown context '$ctx' (not in $reg)"; return 1; fi
  [[ -n "$home" && "$home" != "null" ]] || home="$found"
  home="${home/#\~/$HOME}"

  local mode; mode="$(yq -r '.work_mode.home // "inline"' "$cfg" 2>/dev/null)"
  [[ -n "$mode" && "$mode" != "null" ]] || mode="inline"
  if [[ "$mode" == "tmux-session" ]] && ! command -v tmux >/dev/null 2>&1; then
    print -u2 "baton-start: work_mode.home is tmux-session but tmux is not on PATH — running inline."
    mode="inline"
  fi

  if [[ "$mode" != "tmux-session" ]]; then
    export BATON_CONTEXT="$ctx"
    cd "$home" || return 1
    eval "$(_baton_home_cmd $dangerous)"
    return
  fi

  # Session name: {context} is the only placeholder — a home session is per context, and
  # unlike a worker session there is no bead/branch to name it after. Sanitized for the same
  # reason task-identity.sh sanitizes: tmux reads "." and ":" as window/pane target separators,
  # so a name containing either creates fine and then fails every later -t lookup.
  local fmt sess
  fmt="$(yq -r '.naming.home_session // ""' "$cfg" 2>/dev/null)"
  [[ -n "$fmt" && "$fmt" != "null" ]] || fmt='{context}-home'
  sess="${fmt//\{context\}/$ctx}"
  sess="${sess//[^A-Za-z0-9_-]/_}"

  # `-t =name` is an exact match. Bare `-t name` is a prefix/fnmatch match, so a plain
  # `has-session -t jbh-home` would also be satisfied by an unrelated `jbh-home-2` and we would
  # attach to the wrong session instead of creating ours.
  if ! tmux has-session -t "=$sess" 2>/dev/null; then
    # -e puts BATON_CONTEXT in the session environment, so the orchestrator (and anything else
    # opened in that session later) is pinned to this context regardless of cwd.
    #
    # -P -F '#{pane_id}' hands back the new pane's id (%N) to aim send-keys at. Reusing the
    # session name there does not work: send-keys takes a *pane* target, where a bare
    # "=jbh-home" is read as a pane spec and fails with "can't find pane" — the command silently
    # never runs and you get an empty session. A pane id has no such ambiguity.
    local pane
    pane="$(tmux new-session -d -s "$sess" -c "$home" -e "BATON_CONTEXT=$ctx" -P -F '#{pane_id}')" || return 1
    # send-keys rather than passing the command to new-session: when claude exits we want the
    # shell to survive so the session can be reused, not the whole session to disappear.
    tmux send-keys -t "$pane" "$(_baton_home_cmd $dangerous)" C-m
  fi

  if [[ -n "${TMUX:-}" ]]; then
    tmux switch-client -t "=$sess"
  else
    tmux attach-session -t "=$sess"
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
