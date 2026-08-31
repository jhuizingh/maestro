#!/usr/bin/env bash
# baton — resolve the active context from the current working directory.
#
# Emits the resolved context (its context.yaml) as JSON on stdout, augmented with:
#   _workspace  absolute path to the workspace repo (the dir holding its context.yaml)
#   _match      how it resolved: "env" | "cwd" | "default"
#
# Resolution order:
#   1. $BATON_CONTEXT set              -> the registered workspace whose context name matches
#   2. cwd inside a context's own home -> that context (the workspace dir holding its
#                                         context.yaml, its `home` when that differs, or either
#                                         one's "-worktrees" sibling)
#   3. cwd inside a member repo        -> that context (member repo or its "-worktrees" sibling)
#   4. a context marked default:true
#
# 2 is checked before 3 so a workspace dir always wins for its OWN context, even when another
# context's member_repos glob happens to cover it. Within a rung, ties go to whichever context is
# registered FIRST — so two contexts whose homes nest (~/code/ws and ~/code/ws/sub) both resolve
# to the one registered earlier, not to the more specific one. Exit 0 with JSON on success; exit
# 1 with a message on stderr if nothing resolves.
#
# Workspaces come from $BATON_WORKSPACES (colon-separated) if set, else the registry
# ($BATON_REGISTRY or ~/.config/baton/registry.yaml). Requires: yq (v4, mikefarah), jq (1.6+).

set -euo pipefail

REGISTRY="${BATON_REGISTRY:-$HOME/.config/baton/registry.yaml}"

_expand() { # expand a leading ~ to $HOME, and drop trailing slashes
  local p
  case "$1" in
    "~") p="$HOME" ;;
    "~/"*) p="$HOME/${1#\~/}" ;;
    *) p="$1" ;;
  esac
  # `~/code/ws/` must match the same paths as `~/code/ws`: _under compares path boundaries, so
  # an unnormalized trailing slash would silently match nothing at all.
  while [[ "$p" == */ && "$p" != "/" ]]; do p="${p%/}"; done
  printf '%s' "$p"
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

dirs=(); cfgs=()
for w in "${workspaces[@]}"; do
  cfg="$(_expand "$w")/context.yaml"
  [[ -f "$cfg" ]] && { dirs+=("$w"); cfgs+=("$cfg"); }
done
if [[ ${#cfgs[@]} -eq 0 ]]; then
  echo "baton: no context.yaml found in any registered workspace (${workspaces[*]})." >&2
  exit 1
fi

# --- read every workspace's context.yaml, once ------------------------------
# This runs from shell/baton.zsh's chpwd hook on every `cd`, and yq's startup dominates the cost,
# so all of them are read in ONE invocation. They must all be read up front: the home rung
# outranks the member_repos rung across workspaces, so no workspace's members can be judged until
# every workspace's home is known.
#
# Each document is tagged with its own filename rather than being paired up by position. That is
# the whole robustness argument: yq emits nothing at all for an empty file, two documents for a
# multi-document one, and aborts the batch outright on a parse error — so its output lines do NOT
# correspond 1:1 to the input files, and any scheme that pairs the Nth block with the Nth file
# will sooner or later hand one context the tracker of another. Tagging makes the pairing exact,
# and lets a file that produced no record be detected precisely rather than inferred from a count.
_records() { # $@ = context.yaml paths -> one compact JSON record per document, tagged by file
  yq -o=json -I0 '{"_f": filename, "c": .}' "$@" 2>/dev/null
}

# One row per requested path, in order, whether or not a record exists for it. jq drives the rows
# from the path list, so the rows cannot desync from $cfgs. Fields come out of parsed JSON rather
# than off line positions, so a value spanning several lines cannot shift the fields after it.
#
# Fields are separated by RS and the member list by US — deliberately NOT tabs. Tab is IFS
# whitespace, so `read` collapses a run of them and drops empty fields entirely: one context with
# no `home:` would silently shift `default` and `member_repos` up a slot. Every value is stripped
# of control characters first, which both keeps a rogue newline from splitting a row and keeps a
# value from forging a separator. A path mangled that way matches nothing, which is the point.
_table() { # stdin = records; $@ = cfg paths in order
  jq -Rrn --args '
    def clean: tostring | gsub("[[:cntrl:]]"; " ");
    [ inputs | fromjson? | select(type == "object") ] as $recs
    | $ARGS.positional[] as $p
    | (first($recs[] | select(._f == $p and (.c | type) == "object")) // null) as $r
    | [ (if $r == null then "0" else "1" end),
        ($r.c.name? // "" | clean),
        ($r.c.home? // "" | clean),
        (if $r.c.default? == true then "1" else "0" end),
        ([ ($r.c.member_repos? // [])[] | select(type == "string") | clean ] | join("\u001f"))
      ] | join("\u001e")' "$@"
}

ok=(); names=(); homes=(); defaults=(); members=()
_load() {
  ok=(); names=(); homes=(); defaults=(); members=()
  local o n h d m
  while IFS=$'\036' read -r o n h d m; do
    ok+=("$o"); names+=("$n"); homes+=("$h"); defaults+=("$d"); members+=("$m")
  done < <(printf '%s\n' "$recs" | _table "${cfgs[@]}")
}

recs="$(_records "${cfgs[@]}" || true)"
_load

# A parse error aborts the batch, so every file after the bad one goes missing too; an empty file
# goes missing on its own. Re-read whatever the batch didn't cover, one at a time, so that one
# broken context cannot blind the resolver to the others. strenv() rather than filename(), so
# this path still works on a yq lacking the latter.
retry=no
for k in "${!cfgs[@]}"; do
  [[ "${ok[$k]:-0}" == 1 ]] && continue
  retry=yes
  r="$(BATON_CFG="${cfgs[$k]}" yq -o=json -I0 '{"_f": strenv(BATON_CFG), "c": .}' "${cfgs[$k]}" \
       2>/dev/null || true)"
  [[ -n "$r" ]] && recs+=$'\n'"$r"
done
[[ "$retry" == yes ]] && _load

if [[ ${#ok[@]} -ne ${#cfgs[@]} ]]; then
  echo "baton: could not read the registered contexts — check that yq (v4) and jq are installed." >&2
  exit 1
fi

readable=no
for k in "${!cfgs[@]}"; do
  if [[ "${ok[$k]}" == 1 ]]; then readable=yes
  else echo "baton: ignoring ${cfgs[$k]} — not readable as a context.yaml." >&2
  fi
done
if [[ "$readable" == no ]]; then
  echo "baton: no registered context.yaml could be parsed — check them, and that yq is mikefarah's v4." >&2
  exit 1
fi

_emit() { # $1 = workspace dir, $2 = match type
  # Served out of the records already in hand rather than re-reading the file: it saves a yq
  # spawn on the hot path, and it guarantees the emitted JSON is the very document the rungs
  # above matched on. `first` is what makes a multi-document context.yaml emit one object rather
  # than one per document, which a caller doing `jq -r .task_tracking.dir` would read as two.
  local ws cfg
  ws="$(_expand "$1")"; cfg="$ws/context.yaml"
  printf '%s\n' "$recs" | jq -Rn --arg f "$cfg" --arg ws "$ws" --arg m "$2" '
    first(inputs | fromjson? | select(type == "object" and ._f == $f and (.c | type) == "object"))
    | .c + {_workspace: $ws, _match: $m}'
}

# --- 1. explicit override ---------------------------------------------------
if [[ -n "${BATON_CONTEXT:-}" ]]; then
  for k in "${!dirs[@]}"; do
    if [[ "${ok[$k]}" == 1 && "${names[$k]}" == "$BATON_CONTEXT" ]]; then
      _emit "${dirs[$k]}" env; exit 0
    fi
  done
  echo "baton: BATON_CONTEXT='$BATON_CONTEXT' not found among registered workspaces." >&2
  exit 1
fi

pwd_abs="$(pwd -P)"
_under() { [[ "$1" == "$2" || "$1" == "$2"/* ]]; }

# --- 2. cwd match: the context's own home -----------------------------------
# A workspace dir is the least ambiguous signal a context has, so it is checked before the
# member_repos rung — otherwise standing in one falls through to the default context, and a glob
# belonging to some *other* context can claim it. Gated on the config being readable: this rung
# matches on the REGISTERED directory, which is known whether or not the file parses, so without
# the gate a half-edited context.yaml would be claimed here and then fail to emit — turning a
# graceful fall-through into a hard error, which shell/baton.zsh's chpwd hook in turn translates
# into leaving BEADS_DIR pointed at whatever context the shell was in before.
for k in "${!dirs[@]}"; do
  [[ "${ok[$k]}" == 1 ]] || continue
  ws="$(_expand "${dirs[$k]}")"
  cands=("$ws")
  h="${homes[$k]}"
  if [[ -n "$h" && "$h" != "null" ]]; then
    h="$(_expand "$h")"
    [[ "$h" != "$ws" ]] && cands+=("$h")
  fi
  for P in "${cands[@]}"; do
    if _under "$pwd_abs" "$P" || _under "$pwd_abs" "${P}-worktrees"; then
      _emit "${dirs[$k]}" cwd; exit 0
    fi
  done
done

# --- 3. cwd match: member repos ---------------------------------------------
for k in "${!dirs[@]}"; do
  [[ "${ok[$k]}" == 1 && -n "${members[$k]}" ]] || continue
  while IFS= read -r entry; do
    [[ -n "$entry" && "$entry" != "null" ]] || continue
    entry="$(_expand "$entry")"
    # glob-expand (e.g. ~/code/myproj-*); unmatched globs stay literal and simply won't match
    for P in $entry; do
      if _under "$pwd_abs" "$P" || _under "$pwd_abs" "${P}-worktrees"; then
        _emit "${dirs[$k]}" cwd; exit 0
      fi
    done
    # printf '%s\n', not '%s': without the trailing newline `read` returns false on the last
    # entry and the loop drops it before the body ever runs.
  done < <(printf '%s\n' "${members[$k]}" | tr '\037' '\n')
done

# --- 4. default -------------------------------------------------------------
for k in "${!dirs[@]}"; do
  if [[ "${ok[$k]}" == 1 && "${defaults[$k]}" == 1 ]]; then
    _emit "${dirs[$k]}" default; exit 0
  fi
done

echo "baton: no context resolved — cwd is not in any context's workspace or member repos, and no context is marked default:true." >&2
exit 1
