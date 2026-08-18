#!/usr/bin/env bash
# baton — compute the task identity group for one unit of work.
#
# A unit of work has several names. This script is the ONLY place they are derived, so the
# skill that mints them (baton:start) and the skills that recover them much later
# (baton:cleanup-worktrees, baton:resume, baton:pr, baton:finish, the SessionStart hook) can
# never disagree — and no context ever has to reimplement the transform in a launcher, a hook,
# or a template.
#
#   LEAF           bead id                      jbh-zvs
#   SLUG           human-readable half          kids-overnight-hvac
#   BR             git branch                   jbh-zvs-kids-overnight-hvac
#   DIR            worktree directory name      jbh-zvs-kids-overnight-hvac
#   SESSION_NAME   tmux session name            kids-overnight-hvac-jbh-zvs
#   SESSION_TITLE  Claude Code session title    kids overnight hvac (jbh-zvs)
#
# BRANCH AND DIRECTORY ARE BOTH CONFIGURABLE AND MAY LEGITIMATELY DIFFER. A context whose
# organisation dictates branch naming can set `naming.branch: "{jira}/{slug}"` and get
# DOT-1234/loki-heavy-logql while the worktree directory still carries the bead id. Because of
# that, THE BRANCH IS NO LONGER AN IDENTITY CARRIER — nothing may recover a leaf by parsing it
# except as a documented back-compat fallback (see the ladder below).
#
# THE CARRIER
#
# baton:start writes an identity record into the worktree at creation:
#
#     $(git rev-parse --git-dir)/baton-identity      # .git/worktrees/<name>/baton-identity
#
# It is per-worktree by construction, invisible to `git status` and to the working tree, needs
# no repo-wide git extension, is readable without git, and is removed for free — it lives inside
# the per-worktree git dir that `git worktree remove` and `git worktree prune` delete wholesale.
#
# NOT `git config`. At local scope inside a linked worktree, `git config` writes to the SHARED
# repository config, so the last `baton:start` would overwrite the leaf for every live worktree
# of that repo (verified, git 2.51.0). `git config --worktree` does isolate correctly, but it
# requires flipping the repo-wide `extensions.worktreeConfig` extension AND it fails OPEN: a
# worktree with no value of its own silently reads the shared scope's, so a carrier-less
# pre-existing worktree resolves to a confident WRONG leaf instead of falling through to the
# ladder below. baton:cleanup-worktrees deletes on that answer. A sidecar file fails closed.
#
# RESOLUTION LADDER (--worktree), in order, with the carrier backfilled whenever a fallback answered:
#   1. the carrier file                     authoritative
#   2. the worktree DIR basename, LEAF_RE   back-compat (linked worktrees only)
#   3. the BRANCH, LEAF_RE                  back-compat
#
# Usage:
#   task-identity.sh --leaf <id> --slug <slug> [--jira <key>]   # mint: baton:start
#   task-identity.sh --worktree <path>                          # resolve a worktree (preferred)
#   task-identity.sh --branch <branch>                          # recompute from a branch string
#   task-identity.sh --write-carrier <path> --leaf <id> --slug <slug> [--branch <br>]
#
# Options:
#   --format json|env   json (default) or `export K='V'` lines for `eval`
#   --context <file|->  pre-resolved context JSON; default runs resolve-context.sh
#   --no-backfill       --worktree: resolve via a fallback without writing the carrier
#
# Formats come from the context's optional `naming:` block; absent it, the defaults below
# apply, so an unconfigured context still gets slug-first names with no config at all:
#
#   naming:
#     slug: written                             # or `slugify`; reported as .slug_mode
#     branch: "{leaf}-{slug}"                   # used RAW — git permits '/' and uppercase
#     dir: "{leaf}-{slug}"                      # sanitized to ONE path segment
#     session_name: "{slug}-{leaf}"
#     session_title: "{slug_prose} ({leaf})"
#
# Placeholders: {leaf} {slug} {slug_prose} {jira} everywhere; session_name/session_title
# additionally get {branch} and {dir}.  Requires: jq (yq only via the resolver).

set -euo pipefail

DEF_BRANCH='{leaf}-{slug}'
DEF_DIR='{leaf}-{slug}'
DEF_SESSION_NAME='{slug}-{leaf}'
DEF_SESSION_TITLE='{slug_prose} ({leaf})'
DEF_SLUG_MODE='written'
SLUG_MAX=40          # hard bound; written slugs should be far shorter (see baton:start)
CARRIER_FILE='baton-identity'

FORMAT=json
CTX_SRC=""
LEAF=""; SLUG=""; BR=""; JIRA=""; WT=""; WRITE_WT=""
BACKFILL=yes
MODE=""

_die() { echo "task-identity: $*" >&2; exit 1; }
_warn() { echo "task-identity: $*" >&2; }

while [ $# -gt 0 ]; do
  case "$1" in
    --leaf)          LEAF="${2:-}"; shift 2 ;;
    --slug)          SLUG="${2:-}"; shift 2 ;;
    --branch)        BR="${2:-}";   shift 2 ;;
    --jira)          JIRA="${2:-}"; shift 2 ;;
    --worktree)      WT="${2:-}";   shift 2 ;;
    --write-carrier) WRITE_WT="${2:-}"; shift 2 ;;
    --no-backfill)   BACKFILL=no; shift ;;
    --format)        FORMAT="${2:-json}"; shift 2 ;;
    --context)       CTX_SRC="${2:-}"; shift 2 ;;
    -h|--help) sed -n '2,69p' "$0"; exit 0 ;;
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

# --- sanitize a rendered directory name to a SINGLE path segment -------------------------
# '/' becomes '-' rather than being dropped, so "{jira}/{slug}" reused as a dir template
# still reads as two parts. Dots survive: dotted child bead ids (jbh-pl4.1) depend on them.
_sanitize_dir() {
  printf '%s' "$1" \
    | tr '\n/' '--' \
    | sed -E 's/[^A-Za-z0-9._-]/-/g; s/-+/-/g; s/^[-.]+//; s/-+$//'
}

# --- the carrier -------------------------------------------------------------------------
# Path of the identity record for a worktree, or non-zero if that path can't be determined.
_carrier_path() {
  local wt="$1" gd
  gd="$(git -C "$wt" rev-parse --path-format=absolute --git-dir 2>/dev/null || true)"
  if [ -z "$gd" ]; then          # git < 2.31 has no --path-format
    gd="$(git -C "$wt" rev-parse --git-dir 2>/dev/null || true)"
    [ -n "$gd" ] || return 1
    case "$gd" in /*) ;; *) gd="$wt/$gd" ;; esac
  fi
  printf '%s/%s' "$gd" "$CARRIER_FILE"
}

_carrier_get() { # $1 = carrier file, $2 = key
  [ -f "$1" ] || return 1
  sed -n "s/^$2=//p" "$1" | head -1
}

_carrier_write() { # $1 = carrier file, $2 = leaf, $3 = slug, $4 = branch, $5 = source, $6 = jira
  local f="$1" tmp="$1.tmp.$$"
  {
    printf '# baton worktree identity — written by baton. Do not edit by hand.\n'
    printf '# The authoritative record of which task this worktree serves. Removed with the\n'
    printf '# worktree, since it lives in the per-worktree git dir.\n'
    printf 'schema=1\n'
    printf 'leaf=%s\n'    "$2"
    printf 'slug=%s\n'    "$3"
    printf 'branch=%s\n'  "$4"
    printf 'jira=%s\n'    "${6:-}"
    printf 'source=%s\n'  "$5"
    printf 'written=%s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  } >"$tmp" 2>/dev/null && mv -f "$tmp" "$f" 2>/dev/null && return 0
  rm -f "$tmp" 2>/dev/null || true
  return 1
}

# --- derive leaf + slug ------------------------------------------------------------------
# A legacy branch/dir is "<leaf>-<slug>"; the leaf's hash segment may contain a dot (dotted
# child bead, e.g. jbh-pl4.1). This regex is BACK-COMPAT ONLY — it is how identity was carried
# before the carrier existed, and it is the last two rungs of the --worktree ladder. Nothing
# should reach for it as a primary path.
LEAF_RE='^([a-z0-9]+-[a-z0-9.]+)-(.+)$'

_leaf_of() { printf '%s' "$1" | sed -E "s/$LEAF_RE/\1/"; }
_slug_of() { printf '%s' "$1" | sed -E "s/$LEAF_RE/\2/"; }
_matches()  { printf '%s' "$1" | grep -qE "$LEAF_RE"; }

# Exactly one input mode.
_n=0
[ -n "$WT" ]       && { MODE=worktree;      _n=$((_n+1)); }
[ -n "$WRITE_WT" ] && { MODE=write-carrier; _n=$((_n+1)); }
[ -n "$BR$LEAF$SLUG" ] && [ -z "$WT$WRITE_WT" ] && { _n=$((_n+1)); MODE="${BR:+branch}"; MODE="${MODE:-mint}"; }
[ "$_n" -le 1 ] || _die "--worktree, --write-carrier and --branch/--leaf are mutually exclusive"
[ "$_n" -eq 1 ] || _die "need one of --worktree, --branch, --leaf/--slug, --write-carrier"
# --worktree reads identity, it never takes it as input. Silently ignoring a --leaf passed
# alongside it would hand the caller a confident answer about a different task.
[ "$MODE" != worktree ] || [ -z "$LEAF$SLUG$BR" ] || _die "--worktree takes no --leaf/--slug/--branch (it reads them)"

IDENTITY_SOURCE=""
CARRIER_AUTHORITATIVE=""
CARRIER_PATH=""
DIR=""

case "$MODE" in
  write-carrier)
    [ -n "$LEAF" ] || _die "--write-carrier needs --leaf"
    [ -n "$SLUG" ] || _die "--write-carrier needs --slug"
    CARRIER_PATH="$(_carrier_path "$WRITE_WT")" \
      || _die "'$WRITE_WT' is not a git worktree — cannot write the identity carrier"
    _carrier_write "$CARRIER_PATH" "$LEAF" "$SLUG" "${BR:-$(git -C "$WRITE_WT" rev-parse --abbrev-ref HEAD 2>/dev/null || true)}" mint "$JIRA" \
      || _die "could not write the identity carrier at $CARRIER_PATH"
    echo "$CARRIER_PATH"
    exit 0
    ;;

  worktree)
    [ -d "$WT" ] || _die "no such worktree directory: $WT"
    TOP="$(git -C "$WT" rev-parse --show-toplevel 2>/dev/null || true)"
    [ -n "$TOP" ] || _die "'$WT' is not inside a git worktree"
    DIR="$(basename "$TOP")"
    BR="$(git -C "$WT" rev-parse --abbrev-ref HEAD 2>/dev/null || true)"
    [ "$BR" = "HEAD" ] && BR=""          # detached: no branch to fall back to
    CARRIER_PATH="$(_carrier_path "$WT" || true)"

    # A linked worktree's git dir differs from the common one. Only there is the DIRECTORY
    # NAME a task name: a main worktree's directory is the repository's, and repo names like
    # `jbh-task-tracking` match LEAF_RE by accident, which would invent a leaf out of nothing.
    GD="$(git -C "$WT" rev-parse --path-format=absolute --git-dir 2>/dev/null || true)"
    GCD="$(git -C "$WT" rev-parse --path-format=absolute --git-common-dir 2>/dev/null || true)"
    LINKED=no
    [ -n "$GD" ] && [ -n "$GCD" ] && [ "$GD" != "$GCD" ] && LINKED=yes

    C_LEAF=""; C_SLUG=""
    if [ -n "$CARRIER_PATH" ]; then
      C_LEAF="$(_carrier_get "$CARRIER_PATH" leaf || true)"
      C_SLUG="$(_carrier_get "$CARRIER_PATH" slug || true)"
      [ -n "$JIRA" ] || JIRA="$(_carrier_get "$CARRIER_PATH" jira || true)"
    fi

    if [ -n "$C_LEAF" ] && [ -n "$C_SLUG" ]; then
      LEAF="$C_LEAF"; SLUG="$C_SLUG"
      IDENTITY_SOURCE=carrier; CARRIER_AUTHORITATIVE=yes
    elif [ "$LINKED" = yes ] && _matches "$DIR"; then
      LEAF="$(_leaf_of "$DIR")"; SLUG="$(_slug_of "$DIR")"
      IDENTITY_SOURCE=dir; CARRIER_AUTHORITATIVE=no
    elif [ -n "$BR" ] && _matches "$BR"; then
      LEAF="$(_leaf_of "$BR")"; SLUG="$(_slug_of "$BR")"
      IDENTITY_SOURCE=branch; CARRIER_AUTHORITATIVE=no
    else
      _die "worktree '$WT' has no identity carrier and neither its directory ('$DIR') nor its branch ('${BR:-detached}') is <leaf>-<slug>"
    fi

    # Backfill so the next reader gets an authoritative answer. Never fatal: a read-only or
    # otherwise unwritable git dir must degrade to "resolved, not recorded", not to a failure.
    if [ "$CARRIER_AUTHORITATIVE" = no ] && [ "$BACKFILL" = yes ] && [ -n "$CARRIER_PATH" ]; then
      if _carrier_write "$CARRIER_PATH" "$LEAF" "$SLUG" "$BR" "$IDENTITY_SOURCE" "$JIRA"; then
        _warn "backfilled identity carrier from $IDENTITY_SOURCE at $CARRIER_PATH"
      else
        _warn "could not backfill the identity carrier at $CARRIER_PATH (continuing)"
      fi
    fi
    ;;

  branch)
    [ -z "$LEAF$SLUG" ] || _die "--branch is exclusive with --leaf/--slug"
    _matches "$BR" || _die "branch '$BR' is not <leaf>-<slug>"
    LEAF="$(_leaf_of "$BR")"
    SLUG="$(_slug_of "$BR")"
    IDENTITY_SOURCE=branch; CARRIER_AUTHORITATIVE=no
    ;;

  mint)
    [ -n "$LEAF" ] || _die "need --branch, --worktree, or --leaf with --slug"
    [ -n "$SLUG" ] || _die "need --slug when passing --leaf"
    SLUG="$(_slugify "$SLUG")"
    [ -n "$SLUG" ] || _die "slug is empty after normalization"
    IDENTITY_SOURCE=mint; CARRIER_AUTHORITATIVE=no
    ;;
esac

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

FMT_BRANCH="$(_naming branch "$DEF_BRANCH")"
FMT_DIR="$(_naming dir "$DEF_DIR")"
FMT_NAME="$(_naming session_name "$DEF_SESSION_NAME")"
FMT_TITLE="$(_naming session_title "$DEF_SESSION_TITLE")"
SLUG_MODE="$(_naming slug "$DEF_SLUG_MODE")"

# --- render -------------------------------------------------------------------------------
SLUG_PROSE="${SLUG//-/ }"

# {jira} is available to every template, but it can only be SUPPLIED at mint time (--jira) or
# recovered from the carrier. So minting is strict — a template asking for a key nobody passed
# is a config error the user can still fix — while every later re-derivation degrades to an
# empty substitution and a warning. Failing a resume or a cleanup scan over a cosmetic session
# name would strand real work; a slightly-wrong session name will not.
_render_base() {   # {leaf} {slug} {slug_prose} {jira} — no {branch}/{dir}, which don't exist yet
  local f="$1"
  case "$f" in
    *'{jira}'*)
      if [ -z "$JIRA" ]; then
        [ "$MODE" != mint ] || _die "template '$f' uses {jira} but no --jira was given"
        _warn "template '$f' uses {jira}, which this worktree has no record of — substituting empty"
      fi ;;
  esac
  f="${f//\{leaf\}/$LEAF}"
  f="${f//\{slug_prose\}/$SLUG_PROSE}"
  f="${f//\{slug\}/$SLUG}"
  f="${f//\{jira\}/$JIRA}"
  printf '%s' "$f"
}

# Only mint mode invents a branch and a directory. --worktree reports what git and the
# filesystem actually say; --branch was handed the branch and renders only the directory.
if [ "$MODE" = mint ]; then
  BR="$(_render_base "$FMT_BRANCH" | tr -d '\n')"
  [ -n "$BR" ] || _die "naming.branch rendered empty"
  # The branch is used RAW — '/' and uppercase are legal and are the point of the template.
  # Reject only what git itself would, and only when git is here to say so.
  if command -v git >/dev/null 2>&1; then
    git check-ref-format "refs/heads/$BR" 2>/dev/null \
      || _die "naming.branch rendered '$BR', which git rejects as a branch name"
  fi
fi
if [ "$MODE" = mint ] || [ "$MODE" = branch ]; then
  DIR="$(_sanitize_dir "$(_render_base "$FMT_DIR")")"
  case "$DIR" in
    ""|.|..)
      [ "$MODE" != mint ] || _die "naming.dir rendered '$DIR', which is not a usable directory name"
      _warn "naming.dir rendered '$DIR', which is not a usable directory name — reporting DIR as empty"
      DIR="" ;;
  esac
fi

_render() {   # full placeholder set, for the session names
  local f
  f="$(_render_base "$1")"
  f="${f//\{branch\}/$BR}"
  f="${f//\{dir\}/$DIR}"
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
      --arg branch "$BR" --arg dir "$DIR" --arg session_name "$SESSION_NAME" \
      --arg session_title "$SESSION_TITLE" --arg legacy "$SESSION_NAME_LEGACY" \
      --arg slug_mode "$SLUG_MODE" --arg identity_source "$IDENTITY_SOURCE" \
      --arg carrier_path "$CARRIER_PATH" --arg authoritative "$CARRIER_AUTHORITATIVE" \
      '{leaf:$leaf, slug:$slug, slug_prose:$slug_prose, branch:$branch, dir:$dir,
        session_name:$session_name, session_title:$session_title,
        session_name_legacy:$legacy, slug_mode:$slug_mode,
        identity_source:$identity_source,
        carrier_authoritative:($authoritative == "yes"),
        carrier_path:$carrier_path}'
    ;;
  env)
    _q() { printf "'%s'" "$(printf '%s' "$1" | sed "s/'/'\\\\''/g")"; }
    printf 'export LEAF=%s\n'                  "$(_q "$LEAF")"
    printf 'export SLUG=%s\n'                  "$(_q "$SLUG")"
    printf 'export BR=%s\n'                    "$(_q "$BR")"
    printf 'export DIR=%s\n'                   "$(_q "$DIR")"
    printf 'export SESSION_NAME=%s\n'          "$(_q "$SESSION_NAME")"
    printf 'export SESSION_TITLE=%s\n'         "$(_q "$SESSION_TITLE")"
    printf 'export SESSION_NAME_LEGACY=%s\n'   "$(_q "$SESSION_NAME_LEGACY")"
    printf 'export IDENTITY_SOURCE=%s\n'       "$(_q "$IDENTITY_SOURCE")"
    printf 'export CARRIER_AUTHORITATIVE=%s\n' "$(_q "$CARRIER_AUTHORITATIVE")"
    printf 'export CARRIER_PATH=%s\n'          "$(_q "$CARRIER_PATH")"
    ;;
  *) _die "unknown --format '$FORMAT' (want json or env)" ;;
esac
