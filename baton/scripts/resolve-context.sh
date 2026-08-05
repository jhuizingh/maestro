#!/usr/bin/env bash
# baton — resolve the active context from the current working directory.
#
# Emits the resolved context (its context.yaml) as JSON on stdout, augmented with:
#   _workspace  absolute path to the workspace repo
#   _match      how it resolved: "env" | "cwd" | "default"
#
# Resolution order:
#   1. $BATON_CONTEXT set          -> the registered workspace whose context name matches
#   2. cwd inside a member repo     -> that context (member repo or its "-worktrees" sibling)
#   3. a context marked default:true
# Exit 0 with JSON on success; exit 1 with a message on stderr if nothing resolves.
#
# Workspaces come from $BATON_WORKSPACES (colon-separated) if set, else the registry
# ($BATON_REGISTRY or ~/.config/baton/registry.yaml). Requires: yq (v4), jq.

set -euo pipefail

REGISTRY="${BATON_REGISTRY:-$HOME/.config/baton/registry.yaml}"

_expand() { # expand a leading ~ to $HOME
  case "$1" in
    "~") printf '%s' "$HOME" ;;
    "~/"*) printf '%s' "$HOME/${1#\~/}" ;;
    *) printf '%s' "$1" ;;
  esac
}

# --- collect workspace dirs -------------------------------------------------
workspaces=()
if [[ -n "${BATON_WORKSPACES:-}" ]]; then
  IFS=':' read -r -a workspaces <<<"$BATON_WORKSPACES"
elif [[ -f "$REGISTRY" ]]; then
  while IFS= read -r w; do [[ -n "$w" && "$w" != "null" ]] && workspaces+=("$w"); done \
    < <(yq -r '.workspaces[]?' "$REGISTRY" 2>/dev/null || true)
fi

if [[ ${#workspaces[@]} -eq 0 ]]; then
  echo "baton: no workspaces registered (looked in $REGISTRY). Run baton:configure." >&2
  exit 1
fi

_emit() { # $1 = workspace dir, $2 = match type
  local ws cfg
  ws="$(_expand "$1")"; cfg="$ws/context.yaml"
  [[ -f "$cfg" ]] || { echo "baton: context.yaml not found in $ws" >&2; return 1; }
  yq -o=json '.' "$cfg" | jq --arg ws "$ws" --arg m "$2" '. + {_workspace:$ws, _match:$m}'
}

_name_of() { yq -r '.name // ""' "$(_expand "$1")/context.yaml" 2>/dev/null || true; }

# --- 1. explicit override ---------------------------------------------------
if [[ -n "${BATON_CONTEXT:-}" ]]; then
  for w in "${workspaces[@]}"; do
    [[ -f "$(_expand "$w")/context.yaml" ]] || continue
    if [[ "$(_name_of "$w")" == "$BATON_CONTEXT" ]]; then _emit "$w" env; exit 0; fi
  done
  echo "baton: BATON_CONTEXT='$BATON_CONTEXT' not found among registered workspaces." >&2
  exit 1
fi

# --- 2. cwd match -----------------------------------------------------------
pwd_abs="$(pwd -P)"
_under() { [[ "$1" == "$2" || "$1" == "$2"/* ]]; }

for w in "${workspaces[@]}"; do
  cfg="$(_expand "$w")/context.yaml"; [[ -f "$cfg" ]] || continue
  while IFS= read -r entry; do
    [[ -n "$entry" && "$entry" != "null" ]] || continue
    entry="$(_expand "$entry")"
    # glob-expand (e.g. ~/code/myproj-*); unmatched globs stay literal and simply won't match
    for P in $entry; do
      if _under "$pwd_abs" "$P" || _under "$pwd_abs" "${P}-worktrees"; then
        _emit "$w" cwd; exit 0
      fi
    done
  done < <(yq -r '.member_repos[]?' "$cfg" 2>/dev/null || true)
done

# --- 3. default -------------------------------------------------------------
for w in "${workspaces[@]}"; do
  cfg="$(_expand "$w")/context.yaml"; [[ -f "$cfg" ]] || continue
  if [[ "$(yq -r '.default // false' "$cfg" 2>/dev/null)" == "true" ]]; then
    _emit "$w" default; exit 0
  fi
done

echo "baton: no context resolved — cwd is not in any member repo and no context is marked default:true." >&2
exit 1
