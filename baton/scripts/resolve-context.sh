#!/usr/bin/env bash
# baton — resolve the active context from the current working directory.
#
# Emits the resolved context (its context.yaml) as JSON on stdout, augmented with:
#   _workspace  absolute path to the workspace repo
#   _match      how it resolved: "env" | "cwd" | "default"
#
# Resolution order:
#   1. $BATON_CONTEXT set             -> the registered workspace whose context name matches
#   2. cwd inside a context's own home -> that context (the workspace dir holding its
#                                         context.yaml, its `home` when that differs, or either
#                                         one's "-worktrees" sibling)
#   3. cwd inside a member repo        -> that context (member repo or its "-worktrees" sibling)
#   4. a context marked default:true
#
# 2 is checked before 3 so a workspace dir always wins for its OWN context, even when another
# context's member_repos glob happens to cover it. Exit 0 with JSON on success; exit 1 with a
# message on stderr if nothing resolves.
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

# --- read every workspace's context.yaml, once ------------------------------
# This runs from shell/baton.zsh's chpwd hook on every `cd`, and yq's startup dominates the
# cost, so all of them are read in ONE invocation rather than one per workspace per field.
# They must all be read up front: the home pass below outranks the member_repos pass across
# workspaces, so no workspace's members can be judged until every workspace's home is known.
# Layout per file: a marker line, then name, home, default, then the member_repos entries.
MARK=$'\x1e'   # ASCII record separator — cannot occur in a name or a path
FIELDS="(\"$MARK\"), (.name // \"\"), (.home // \"\"), (.default // false), (.member_repos[]?)"

cfgs=(); dirs=()
for w in "${workspaces[@]}"; do
  cfg="$(_expand "$w")/context.yaml"
  [[ -f "$cfg" ]] && { dirs+=("$w"); cfgs+=("$cfg"); }
done
if [[ ${#cfgs[@]} -eq 0 ]]; then
  echo "baton: no context.yaml found in any registered workspace (${workspaces[*]})." >&2
  exit 1
fi

_blocks() { yq -N -r "$FIELDS" "$@" 2>/dev/null; }   # -N: no `---` between the files' outputs
_nmarks() { local n=0 l; while IFS= read -r l; do [[ "$l" == "$MARK" ]] && n=$((n+1)); done; echo "$n"; }

blob="$(_blocks "${cfgs[@]}" || true)"
# yq fails the whole batch on one unparseable file, and a multi-document file would emit an
# extra block — either way the blocks stop lining up with $dirs. Re-read one at a time then,
# substituting an inert block for whichever file is the bad one.
if [[ "$(_nmarks <<<"$blob")" != "${#cfgs[@]}" ]]; then
  blob=""
  for cfg in "${cfgs[@]}"; do
    b="$(_blocks "$cfg")" && [[ "$(_nmarks <<<"$b")" == 1 ]] || b="$MARK"$'\n\n\nfalse'
    blob+="$b"$'\n'
  done
fi

# Split the blob into one parallel entry per workspace.
ws_dirs=(); ws_names=(); ws_homes=(); ws_defaults=(); ws_members=()
i=-1; field=0
while IFS= read -r line; do
  if [[ "$line" == "$MARK" ]]; then i=$((i+1)); field=0
    ws_dirs+=("${dirs[$i]}"); ws_names+=(""); ws_homes+=(""); ws_defaults+=(""); ws_members+=("")
    continue
  fi
  [[ $i -ge 0 ]] || continue
  field=$((field+1))
  case $field in
    1) ws_names[$i]="$line" ;;
    2) ws_homes[$i]="$line" ;;
    3) ws_defaults[$i]="$line" ;;
    *) ws_members[$i]+="$line"$'\n' ;;
  esac
done <<<"$blob"

# --- 1. explicit override ---------------------------------------------------
if [[ -n "${BATON_CONTEXT:-}" ]]; then
  for i in "${!ws_dirs[@]}"; do
    if [[ "${ws_names[$i]}" == "$BATON_CONTEXT" ]]; then _emit "${ws_dirs[$i]}" env; exit 0; fi
  done
  echo "baton: BATON_CONTEXT='$BATON_CONTEXT' not found among registered workspaces." >&2
  exit 1
fi

pwd_abs="$(pwd -P)"
_under() { [[ "$1" == "$2" || "$1" == "$2"/* ]]; }

# --- 2. cwd match: the context's own home -----------------------------------
# A workspace dir is the least ambiguous signal a context has, so it is checked before the
# member_repos pass — otherwise standing in one falls through to the default context, and a
# glob belonging to some *other* context can claim it.
for i in "${!ws_dirs[@]}"; do
  ws="$(_expand "${ws_dirs[$i]}")"
  homes=("$ws")
  h="${ws_homes[$i]}"
  if [[ -n "$h" && "$h" != "null" ]]; then
    h="$(_expand "$h")"
    [[ "$h" != "$ws" ]] && homes+=("$h")
  fi
  for P in "${homes[@]}"; do
    if _under "$pwd_abs" "$P" || _under "$pwd_abs" "${P}-worktrees"; then
      _emit "${ws_dirs[$i]}" cwd; exit 0
    fi
  done
done

# --- 3. cwd match: member repos ---------------------------------------------
for i in "${!ws_dirs[@]}"; do
  [[ -n "${ws_members[$i]}" ]] || continue
  while IFS= read -r entry; do
    [[ -n "$entry" && "$entry" != "null" ]] || continue
    entry="$(_expand "$entry")"
    # glob-expand (e.g. ~/code/myproj-*); unmatched globs stay literal and simply won't match
    for P in $entry; do
      if _under "$pwd_abs" "$P" || _under "$pwd_abs" "${P}-worktrees"; then
        _emit "${ws_dirs[$i]}" cwd; exit 0
      fi
    done
  done <<<"${ws_members[$i]}"
done

# --- 4. default -------------------------------------------------------------
for i in "${!ws_dirs[@]}"; do
  if [[ "${ws_defaults[$i]}" == "true" ]]; then _emit "${ws_dirs[$i]}" default; exit 0; fi
done

echo "baton: no context resolved — cwd is not in any context's workspace or member repos, and no context is marked default:true." >&2
exit 1
