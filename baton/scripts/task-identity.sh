#!/usr/bin/env bash
# baton — compute the task identity group for one unit of work.
#
# A unit of work has five names. This script is the ONLY place they are derived, so the
# skill that mints them (baton:start) and the skills that recompute them much later from
# the branch alone (baton:cleanup-worktrees, baton:resume) can never disagree — and no
# context ever has to reimplement the transform in a launcher, a hook, or a template.
#
#   LEAF           bead id                      jbh-zvs
#   SLUG           human-readable half          kids-overnight-hvac
#   BR             "<leaf>-<slug>"              jbh-zvs-kids-overnight-hvac
#   SESSION_NAME   tmux session name            kids-overnight-hvac-jbh-zvs
#   SESSION_TITLE  Claude Code session title    kids overnight hvac (jbh-zvs)
#
# THE INPUT IS THE BRANCH, NEVER THE WORKTREE DIRECTORY NAME. A worktree can be re-pointed
# at a new branch (git switch) while keeping its original directory name, so the directory
# records whatever the *first* branch was. Get the branch from `git worktree list
# --porcelain`, which reports it authoritatively.
#
# Usage:
#   task-identity.sh --leaf <id> --slug <slug>     # mint: baton:start, which writes the slug
#   task-identity.sh --branch <branch>             # recompute: everything downstream
#
# Options:
#   --format json|env   json (default) or `export K='V'` lines for `eval`
#   --context <file|->  pre-resolved context JSON; default runs resolve-context.sh
#
# Formats come from the context's optional `naming:` block; absent it, the defaults below
# apply, so an unconfigured context still gets slug-first names with no config at all:
#
#   naming:
#     slug: written                             # or `slugify`; reported as .slug_mode
#     session_name: "{slug}-{leaf}"
#     session_title: "{slug_prose} ({leaf})"
#
# Placeholders: {leaf} {slug} {slug_prose} {branch}.  Requires: jq (yq only via the resolver).

set -euo pipefail

DEF_SESSION_NAME='{slug}-{leaf}'
DEF_SESSION_TITLE='{slug_prose} ({leaf})'
DEF_SLUG_MODE='written'
SLUG_MAX=40          # hard bound; written slugs should be far shorter (see baton:start)

FORMAT=json
CTX_SRC=""
LEAF=""; SLUG=""; BR=""

_die() { echo "task-identity: $*" >&2; exit 1; }

while [ $# -gt 0 ]; do
  case "$1" in
    --leaf)    LEAF="${2:-}"; shift 2 ;;
    --slug)    SLUG="${2:-}"; shift 2 ;;
    --branch)  BR="${2:-}";   shift 2 ;;
    --format)  FORMAT="${2:-json}"; shift 2 ;;
    --context) CTX_SRC="${2:-}"; shift 2 ;;
    -h|--help) sed -n '2,40p' "$0"; exit 0 ;;
    *) _die "unknown argument '$1'" ;;
  esac
done

# --- normalize a slug: lowercase, a-z0-9- only, no repeats, no edge dashes ---------------
_slugify() {
  printf '%s' "$1" \
    | tr '[:upper:]' '[:lower:]' \
    | sed -E 's/[^a-z0-9]+/-/g; s/-+/-/g; s/^-+//; s/-+$//' \
    | cut -c1-"$SLUG_MAX" \
    | sed -E 's/-+$//'
}

# --- derive leaf + slug ------------------------------------------------------------------
# A branch is "<leaf>-<slug>"; the leaf's hash segment may contain a dot (dotted child bead,
# e.g. jbh-pl4.1). Nothing anywhere parses the slug — only this prefix is load-bearing.
LEAF_RE='^([a-z0-9]+-[a-z0-9.]+)-(.+)$'

if [ -n "$BR" ]; then
  [ -z "$LEAF$SLUG" ] || _die "--branch is exclusive with --leaf/--slug"
  printf '%s' "$BR" | grep -qE "$LEAF_RE" || _die "branch '$BR' is not <leaf>-<slug>"
  LEAF="$(printf '%s' "$BR" | sed -E "s/$LEAF_RE/\1/")"
  SLUG="$(printf '%s' "$BR" | sed -E "s/$LEAF_RE/\2/")"
else
  [ -n "$LEAF" ] || _die "need --branch, or --leaf with --slug"
  [ -n "$SLUG" ] || _die "need --slug when passing --leaf"
  SLUG="$(_slugify "$SLUG")"
  [ -n "$SLUG" ] || _die "slug is empty after normalization"
  BR="$LEAF-$SLUG"
fi

# --- read the context's naming overrides (absent context => plugin defaults) --------------
_context_json() {
  if [ "$CTX_SRC" = "-" ]; then cat
  elif [ -n "$CTX_SRC" ]; then cat "$CTX_SRC"
  else
    local r="${CLAUDE_PLUGIN_ROOT:-}"
    local resolver="${r:+$r/scripts/resolve-context.sh}"
    [ -n "$resolver" ] && [ -x "$resolver" ] || resolver="$(dirname "$0")/resolve-context.sh"
    [ -x "$resolver" ] || return 0
    "$resolver" 2>/dev/null || true
  fi
}

# A context is a nicety here, never a requirement: with no resolvable (or no valid) context,
# the plugin defaults above apply, so an unconfigured setup still gets sensible names.
CTX="$(_context_json)"
printf '%s' "$CTX" | jq -e . >/dev/null 2>&1 || CTX=""

_naming() { # $1 = key, $2 = default
  local v
  [ -n "$CTX" ] || { printf '%s' "$2"; return; }
  v="$(printf '%s' "$CTX" | jq -r --arg k "$1" '.naming[$k] // empty' 2>/dev/null || true)"
  printf '%s' "${v:-$2}"
}

FMT_NAME="$(_naming session_name "$DEF_SESSION_NAME")"
FMT_TITLE="$(_naming session_title "$DEF_SESSION_TITLE")"
SLUG_MODE="$(_naming slug "$DEF_SLUG_MODE")"

# --- render -------------------------------------------------------------------------------
SLUG_PROSE="${SLUG//-/ }"

_render() {
  local f="$1"
  f="${f//\{leaf\}/$LEAF}"
  f="${f//\{slug_prose\}/$SLUG_PROSE}"
  f="${f//\{slug\}/$SLUG}"
  f="${f//\{branch\}/$BR}"
  printf '%s' "$f"
}

# tmux treats "." and ":" as target syntax and rejects them in session names; anything else
# outside [A-Za-z0-9_-] is sanitized for the same reason. Dotted child ids hit this directly.
SESSION_NAME="$(_render "$FMT_NAME" | tr -d '\n' | sed 's/[^A-Za-z0-9_-]/_/g')"
SESSION_TITLE="$(_render "$FMT_TITLE" | tr -d '\n')"

# Transitional only: the session name baton used before the identity group existed. Worktrees
# started under the old scheme are still named this way, and are never renamed in flight, so a
# context migrating its on_cleanup can tear both down until the last one is gone.
SESSION_NAME_LEGACY="$(printf 'baton-%s' "$BR" | sed 's/[^A-Za-z0-9_-]/_/g')"

case "$FORMAT" in
  json)
    jq -n \
      --arg leaf "$LEAF" --arg slug "$SLUG" --arg slug_prose "$SLUG_PROSE" \
      --arg branch "$BR" --arg session_name "$SESSION_NAME" \
      --arg session_title "$SESSION_TITLE" --arg legacy "$SESSION_NAME_LEGACY" \
      --arg slug_mode "$SLUG_MODE" \
      '{leaf:$leaf, slug:$slug, slug_prose:$slug_prose, branch:$branch,
        session_name:$session_name, session_title:$session_title,
        session_name_legacy:$legacy, slug_mode:$slug_mode}'
    ;;
  env)
    _q() { printf "'%s'" "$(printf '%s' "$1" | sed "s/'/'\\\\''/g")"; }
    printf 'export LEAF=%s\n'                "$(_q "$LEAF")"
    printf 'export SLUG=%s\n'                "$(_q "$SLUG")"
    printf 'export BR=%s\n'                  "$(_q "$BR")"
    printf 'export SESSION_NAME=%s\n'        "$(_q "$SESSION_NAME")"
    printf 'export SESSION_TITLE=%s\n'       "$(_q "$SESSION_TITLE")"
    printf 'export SESSION_NAME_LEGACY=%s\n' "$(_q "$SESSION_NAME_LEGACY")"
    ;;
  *) _die "unknown --format '$FORMAT' (want json or env)" ;;
esac
