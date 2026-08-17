#!/usr/bin/env bash
# baton — answer "is this plugin actually current?" for one installed plugin.
#
# THREE versions, not two. A plugin has three that can disagree, and only naming all three
# gives an honest answer:
#
#   RUNNING    the copy whose skills this session is executing right now. Frozen at session
#              start — an update mid-session does NOT change it.
#   INSTALLED  what the on-disk install record says is current. An update moves this
#              immediately, while RUNNING stays behind until the session restarts.
#   LATEST     what the marketplace (the distribution channel) advertises.
#
# INSTALLED != LATEST  → an update is available.
# RUNNING   != INSTALLED → an update already landed; this session is still running the old
#                          skills and needs a restart to pick them up.
# A two-value check cannot express the second case, so it reports "up to date" while stale
# skills are still loaded. That is not hypothetical: it is how jbh-a6w was found.
#
# WHERE EACH VALUE MAY BE READ FROM — the three wrong sources, and why they are wrong:
#
#   * NOT a developer clone (~/code/<repo>). A working clone is not a release channel. On a
#     fresh install it does not exist at all, so a check keyed on it silently no-ops — the
#     reported symptom in jbh-a6w. Even when present it lies: that clone sat 8 commits behind
#     on main, advertising 0.3.0 while the channel was on 0.4.1.
#   * NOT a listing of the plugin cache directory. `cache/<mp>/<plugin>/` retains EVERY version
#     ever downloaded (that machine held nine, 0.1.10 through 0.4.1), so "newest directory" is
#     not "installed" — and picking one is how a session produced a confident false pass.
#   * NOT the marketplace clone's WORKING TREE without checking what it is checked out on. That
#     clone can sit on a feature branch (this one was, on `add-pr-skill-and-doc-check`), in
#     which case its files advertise that branch's version as if it were released.
#
# So LATEST is read from the marketplace's ORIGIN default-branch ref (`git show
# origin/main:...`), never from its checkout. A branch-parked clone then still yields a correct
# LATEST, and the parking is reported as a warning instead of silently trusted.
#
# THIS SCRIPT DOES NOT DECIDE WHETHER TO UPDATE, and no caller should use it that way. Every
# defect above came from trying to be clever about whether an update was needed; `claude
# plugins update` already knows. Run the update unconditionally, then call this to REPORT what
# happened — an `update-available` state after an update means the update did not take, which
# is worth surfacing loudly rather than a comparison worth gating on.
#
# Usage:
#   plugin-freshness.sh [--plugin <name>@<marketplace>] [--running <version|path>]
#                       [--no-fetch] [--format json|env]
#
# Options:
#   --plugin <id>    plugin to ask about, as `<name>@<marketplace>`; default `baton@maestro`
#   --running <v>    the running version, or the plugin root path to read it from. Default:
#                    $CLAUDE_PLUGIN_ROOT, else this script's own location — which IS the
#                    running copy whenever a skill invokes it the documented way.
#   --no-fetch       skip `git fetch` in the marketplace clone (caller already refreshed it)
#   --format         json (default), or `export K='V'` lines for `eval`
#
# JSON fields / env vars:
#   plugin              PLUGIN               `<name>@<marketplace>` as asked
#   running             RUNNING_VERSION      "" when it could not be determined
#   running_source      RUNNING_SOURCE       arg | env | script | dev-clone | unknown
#   installed           INSTALLED_VERSION    "" when the plugin is not installed
#   installed_source    INSTALLED_SOURCE     install-record | cli | none
#   latest              LATEST_VERSION       "" when the channel could not be consulted
#   latest_source       LATEST_SOURCE        marketplace-ref | marketplace-worktree | none
#   marketplace_dir     MARKETPLACE_DIR      the marketplace clone ("" if not installed)
#   marketplace_branch  MARKETPLACE_BRANCH   what that clone is checked out on
#   marketplace_default MARKETPLACE_DEFAULT  its origin default branch
#   state               PLUGIN_STATE         current | update-available | restart-needed
#                                            | not-installed | unknown
#   action              PLUGIN_ACTION        none | update | restart | install | investigate
#   command             PLUGIN_COMMAND       the shell command for `action`, "" for none/restart
#   warnings            PLUGIN_WARNINGS      newline-separated; each one is meant to be shown
#
# `state` reports ONE finding, worst-first: an available update outranks a needed restart
# (the restart is implied by taking the update), which outranks `current`. `warnings` is
# where everything else lands, and is independent of `state` — a `current` plugin with a
# branch-parked marketplace still warns.
#
# Exit status: 0 whenever the question could be asked, INCLUDING when the answer is "unknown"
# — a missing marketplace or an offline machine is a degraded reading, not a failure, and a
# non-zero exit here would break the `align` task it is called from. Non-zero only for a
# usage error.
#
# Requires: jq. Uses git for the marketplace clone, and `claude` only as a fallback.

set -uo pipefail   # deliberately NOT -e: probing for absent files/commands is normal here

FORMAT=json
PLUGIN_ID="baton@maestro"
RUNNING_ARG=""
FETCH=yes

_die() { echo "plugin-freshness: $*" >&2; exit 1; }

while [ $# -gt 0 ]; do
  case "$1" in
    --plugin)   PLUGIN_ID="${2:-}";  shift 2 ;;
    --running)  RUNNING_ARG="${2:-}"; shift 2 ;;
    --no-fetch) FETCH=no;            shift ;;
    --format)   FORMAT="${2:-json}"; shift 2 ;;
    -h|--help)  sed -n '2,81p' "$0"; exit 0 ;;
    *) _die "unknown argument '$1'" ;;
  esac
done

case "$FORMAT" in json|env) ;; *) _die "unknown --format '$FORMAT' (want json or env)" ;; esac
case "$PLUGIN_ID" in
  *@*) PLUGIN_NAME="${PLUGIN_ID%@*}"; MARKETPLACE="${PLUGIN_ID##*@}" ;;
  *) _die "--plugin '$PLUGIN_ID' is not of the form <name>@<marketplace>" ;;
esac
[ -n "$PLUGIN_NAME" ] && [ -n "$MARKETPLACE" ] || _die "--plugin '$PLUGIN_ID' has an empty half"
command -v jq >/dev/null 2>&1 || _die "jq is required"

PLUGINS_HOME="${CLAUDE_CONFIG_DIR:-$HOME/.claude}/plugins"
WARNINGS=""
_warn() { WARNINGS="${WARNINGS:+$WARNINGS$'\n'}$1"; }

# A version directory is the LAST path segment of a plugin root, and the segment before it is
# the plugin name — that shape is what distinguishes an installed copy from a dev clone.
_is_version() { grep -Eq '^[0-9]+(\.[0-9]+)*([.+-][0-9A-Za-z.+-]+)?$' <<<"$1"; }

# --- RUNNING: which copy is executing right now --------------------------------------------
RUNNING_VERSION=""; RUNNING_SOURCE=unknown

_version_from_root() {   # <plugin-root-path> -> version on stdout, empty if not an install
  local root="$1" ver parent
  root="${root%/}"
  ver="$(basename "$root")"
  parent="$(basename "$(dirname "$root")")"
  if _is_version "$ver" && [ "$parent" = "$PLUGIN_NAME" ]; then printf '%s' "$ver"; fi
}

if [ -n "$RUNNING_ARG" ]; then
  RUNNING_SOURCE=arg
  case "$RUNNING_ARG" in
    /*|~*|./*|../*) RUNNING_VERSION="$(_version_from_root "${RUNNING_ARG/#\~/$HOME}")" ;;
    *)              RUNNING_VERSION="$RUNNING_ARG" ;;
  esac
  [ -n "$RUNNING_VERSION" ] || { RUNNING_SOURCE=dev-clone
    _warn "--running '$RUNNING_ARG' is not an installed plugin root, so the running version is unknown." ; }
else
  # $CLAUDE_PLUGIN_ROOT is set for a skill's own shell but NOT reliably inherited by every
  # tool-spawned subshell, so the script's own path is the more dependable reading of the two
  # — it is literally the copy being executed. Both are tried; neither is fatal.
  CAND_ROOT="${CLAUDE_PLUGIN_ROOT:-}"; CAND_SRC="env"
  if [ -n "$CAND_ROOT" ]; then
    RUNNING_VERSION="$(_version_from_root "$CAND_ROOT")"
  fi
  if [ -z "$RUNNING_VERSION" ]; then
    SELF="$0"
    # One `cd -P` rather than `readlink -f`, which BSD/macOS lacks in the GNU spelling.
    SELF_DIR="$(cd -P "$(dirname "$SELF")" 2>/dev/null && pwd)" || SELF_DIR=""
    if [ -n "$SELF_DIR" ]; then
      CAND_ROOT="$(dirname "$SELF_DIR")"; CAND_SRC=script
      RUNNING_VERSION="$(_version_from_root "$CAND_ROOT")"
    fi
  fi
  if [ -n "$RUNNING_VERSION" ]; then
    RUNNING_SOURCE="$CAND_SRC"
  elif [ -n "$CAND_ROOT" ]; then
    # A real, useful finding rather than a gap: the skills being run are a working clone's,
    # so no version number describes them and `claude plugins update` will not change them.
    RUNNING_SOURCE=dev-clone
    _warn "Running baton from a working clone ($CAND_ROOT), not the installed plugin — its skills are whatever that clone has checked out, and a plugin update will not change them."
  fi
fi

# --- INSTALLED: what the install record says -----------------------------------------------
INSTALLED_VERSION=""; INSTALLED_SOURCE=none
RECORD="$PLUGINS_HOME/installed_plugins.json"
if [ -r "$RECORD" ]; then
  # `.plugins[<id>]` is an ARRAY — one entry per install scope (user/project/local/managed).
  ENTRIES="$(jq -r --arg id "$PLUGIN_ID" '.plugins[$id] // [] | length' "$RECORD" 2>/dev/null || echo 0)"
  if [ "${ENTRIES:-0}" -gt 0 ]; then
    # Prefer the `user` scope (what `claude plugins update` defaults to); else the first.
    INSTALLED_VERSION="$(jq -r --arg id "$PLUGIN_ID" '
      .plugins[$id] as $e | (($e[] | select(.scope=="user")) // $e[0]) | .version // empty' \
      "$RECORD" 2>/dev/null)"
    [ -n "$INSTALLED_VERSION" ] && INSTALLED_SOURCE=install-record
    if [ "$ENTRIES" -gt 1 ]; then
      _warn "$PLUGIN_ID is installed in $ENTRIES scopes; reporting the user-scope version ($INSTALLED_VERSION). A per-scope update may be needed."
    fi
  fi
fi
if [ -z "$INSTALLED_VERSION" ] && command -v claude >/dev/null 2>&1; then
  LIST="$(claude plugins list --json 2>/dev/null || true)"
  if [ -n "$LIST" ]; then
    INSTALLED_VERSION="$(jq -r --arg id "$PLUGIN_ID" \
      '.installed[]? | select(.id==$id) | .version' <<<"$LIST" 2>/dev/null | head -1)"
    [ -n "$INSTALLED_VERSION" ] && INSTALLED_SOURCE=cli
  fi
fi

# --- LATEST: what the channel advertises ---------------------------------------------------
LATEST_VERSION=""; LATEST_SOURCE=none
MARKETPLACE_DIR=""; MARKETPLACE_BRANCH=""; MARKETPLACE_DEFAULT=""

KNOWN="$PLUGINS_HOME/known_marketplaces.json"
if [ -r "$KNOWN" ]; then
  MARKETPLACE_DIR="$(jq -r --arg m "$MARKETPLACE" '.[$m].installLocation // empty' "$KNOWN" 2>/dev/null)"
fi
[ -n "$MARKETPLACE_DIR" ] || MARKETPLACE_DIR="$PLUGINS_HOME/marketplaces/$MARKETPLACE"
[ -d "$MARKETPLACE_DIR" ] || MARKETPLACE_DIR=""

if [ -z "$MARKETPLACE_DIR" ]; then
  _warn "Marketplace '$MARKETPLACE' is not installed on this machine, so there is no channel to compare against. Add it with: claude plugins marketplace add <source>"
elif ! command -v git >/dev/null 2>&1; then
  _warn "git is not available, so the '$MARKETPLACE' marketplace could not be read."
else
  _mg() { git -C "$MARKETPLACE_DIR" "$@"; }
  if ! _mg rev-parse --git-dir >/dev/null 2>&1; then
    _warn "Marketplace '$MARKETPLACE' at $MARKETPLACE_DIR is not a git clone; its advertised version could not be read from a release ref."
  else
    [ "$FETCH" = yes ] && _mg fetch origin --quiet 2>/dev/null
    MARKETPLACE_BRANCH="$(_mg rev-parse --abbrev-ref HEAD 2>/dev/null || true)"
    HEADREF="$(_mg symbolic-ref --quiet --short refs/remotes/origin/HEAD 2>/dev/null || true)"
    for cand in "$HEADREF" origin/main origin/master; do
      [ -n "$cand" ] || continue
      if _mg rev-parse --verify --quiet "$cand" >/dev/null 2>&1; then
        MARKETPLACE_DEFAULT="$cand"; break
      fi
    done

    # Read the manifests out of the default-branch REF, never the checkout — see the header.
    _read_at() {   # <ref-or-empty> <path-in-repo> -> file contents on stdout
      if [ -n "$1" ]; then _mg show "$1:$2" 2>/dev/null
      elif [ -r "$MARKETPLACE_DIR/$2" ]; then cat "$MARKETPLACE_DIR/$2"
      fi
    }

    REF="$MARKETPLACE_DEFAULT"
    if [ -n "$REF" ]; then LATEST_SOURCE=marketplace-ref; else LATEST_SOURCE=marketplace-worktree; fi

    MANIFEST="$(_read_at "$REF" ".claude-plugin/marketplace.json")"
    if [ -n "$MANIFEST" ]; then
      SRC="$(jq -r --arg n "$PLUGIN_NAME" \
        '.plugins[]? | select(.name==$n) | .source // empty' <<<"$MANIFEST" 2>/dev/null | head -1)"
      case "$SRC" in ./*) SRC="${SRC#./}" ;; esac
      if [ -n "$SRC" ]; then
        PJ="$(_read_at "$REF" "$SRC/.claude-plugin/plugin.json")"
        [ -n "$PJ" ] && LATEST_VERSION="$(jq -r '.version // empty' <<<"$PJ" 2>/dev/null)"
      fi
      # The marketplace entry carries its own `version`; `claude plugin tag` validates that it
      # agrees with plugin.json, so a disagreement means the release was cut inconsistently.
      ENTRY_VERSION="$(jq -r --arg n "$PLUGIN_NAME" \
        '.plugins[]? | select(.name==$n) | .version // empty' <<<"$MANIFEST" 2>/dev/null | head -1)"
      if [ -z "$LATEST_VERSION" ]; then
        LATEST_VERSION="$ENTRY_VERSION"
      elif [ -n "$ENTRY_VERSION" ] && [ "$ENTRY_VERSION" != "$LATEST_VERSION" ]; then
        _warn "Marketplace '$MARKETPLACE' disagrees with itself about $PLUGIN_NAME: marketplace.json says $ENTRY_VERSION, plugin.json says $LATEST_VERSION. Using plugin.json."
      fi
    fi

    [ -n "$LATEST_VERSION" ] || { LATEST_SOURCE=none
      _warn "Could not read $PLUGIN_NAME's advertised version from marketplace '$MARKETPLACE'${REF:+ at $REF}." ; }

    # Surface a checkout that isn't the release ref, rather than trusting it. LATEST is read
    # from the ref either way, so this never corrupts the answer — but the CHECKOUT is what an
    # update installs from, so a wrong one silently pins the whole machine.
    #
    # Two distinct ways to be wrong, and the second hides behind the first. A clone parked on a
    # feature branch is the obvious one. The subtle one is a clone sitting on a branch with the
    # RIGHT NAME whose commits are not the ref's: `main` can be stale, ahead, or an orphaned
    # lineage entirely. Both were live on the machine that prompted this check — it was parked
    # on a feature branch, AND its local `main` was a 5-commit lineage reachable from no remote
    # branch at all, with a working tree advertising 0.1.2 against the channel's 0.4.1. Checking
    # the branch NAME alone would have called that second one clean.
    if [ -n "$MARKETPLACE_DEFAULT" ] && [ -n "$MARKETPLACE_BRANCH" ]; then
      DEFAULT_SHORT="${MARKETPLACE_DEFAULT#origin/}"
      if [ "$MARKETPLACE_BRANCH" != "$DEFAULT_SHORT" ]; then
        _warn "Marketplace '$MARKETPLACE' is checked out on '$MARKETPLACE_BRANCH', not '$DEFAULT_SHORT' — every plugin update from it is pinned to that branch. LATEST was read from $MARKETPLACE_DEFAULT instead. Fix with: git -C $MARKETPLACE_DIR checkout $DEFAULT_SHORT"
      else
        HEAD_SHA="$(_mg rev-parse HEAD 2>/dev/null || true)"
        DEF_SHA="$(_mg rev-parse "$MARKETPLACE_DEFAULT" 2>/dev/null || true)"
        if [ -n "$HEAD_SHA" ] && [ -n "$DEF_SHA" ] && [ "$HEAD_SHA" != "$DEF_SHA" ]; then
          AHEAD="$(_mg rev-list --count "$MARKETPLACE_DEFAULT..HEAD" 2>/dev/null || echo '?')"
          BEHIND="$(_mg rev-list --count "HEAD..$MARKETPLACE_DEFAULT" 2>/dev/null || echo '?')"
          _warn "Marketplace '$MARKETPLACE' is on '$DEFAULT_SHORT' but its checkout does not match $MARKETPLACE_DEFAULT ($AHEAD commit(s) ahead, $BEHIND behind) — an update installs from the checkout, so it can install something the channel never released. LATEST was read from $MARKETPLACE_DEFAULT instead. Fix with: claude plugins marketplace update $MARKETPLACE; if that cannot fast-forward, git -C $MARKETPLACE_DIR reset --hard $MARKETPLACE_DEFAULT discards the $AHEAD local commit(s)."
        fi
      fi
    elif [ -z "$MARKETPLACE_DEFAULT" ]; then
      ON_BRANCH="${MARKETPLACE_BRANCH:+ on branch $MARKETPLACE_BRANCH}"
      _warn "Marketplace '$MARKETPLACE' has no reachable origin default branch, so its advertised version was read from the working tree$ON_BRANCH — treat it as unverified."
    fi
  fi
fi

# --- combine: one finding, worst-first ------------------------------------------------------
if [ -z "$INSTALLED_VERSION" ]; then
  STATE=not-installed; ACTION=install; COMMAND="claude plugins install $PLUGIN_ID"
elif [ -n "$LATEST_VERSION" ] && [ "$LATEST_VERSION" != "$INSTALLED_VERSION" ]; then
  STATE=update-available; ACTION=update; COMMAND="claude plugins update $PLUGIN_ID"
elif [ -n "$RUNNING_VERSION" ] && [ "$RUNNING_VERSION" != "$INSTALLED_VERSION" ]; then
  STATE=restart-needed; ACTION=restart; COMMAND=""
elif [ -z "$LATEST_VERSION" ]; then
  # Installed and self-consistent, but nothing to compare against — say so rather than
  # calling it current, which is the false pass this whole script exists to prevent.
  STATE=unknown; ACTION=investigate; COMMAND=""
else
  STATE=current; ACTION=none; COMMAND=""
fi

# --- render ---------------------------------------------------------------------------------
case "$FORMAT" in
  json)
    jq -n \
      --arg plugin "$PLUGIN_ID" \
      --arg running "$RUNNING_VERSION" --arg running_source "$RUNNING_SOURCE" \
      --arg installed "$INSTALLED_VERSION" --arg installed_source "$INSTALLED_SOURCE" \
      --arg latest "$LATEST_VERSION" --arg latest_source "$LATEST_SOURCE" \
      --arg mp_dir "$MARKETPLACE_DIR" --arg mp_branch "$MARKETPLACE_BRANCH" \
      --arg mp_default "$MARKETPLACE_DEFAULT" \
      --arg state "$STATE" --arg action "$ACTION" --arg command "$COMMAND" \
      --arg warnings "$WARNINGS" \
      '{plugin:$plugin, running:$running, running_source:$running_source,
        installed:$installed, installed_source:$installed_source,
        latest:$latest, latest_source:$latest_source,
        marketplace_dir:$mp_dir, marketplace_branch:$mp_branch,
        marketplace_default:$mp_default,
        state:$state, action:$action, command:$command,
        warnings:($warnings | if . == "" then [] else split("\n") end)}'
    ;;
  env)
    _q() { printf "'%s'" "$(printf '%s' "$1" | sed "s/'/'\\\\''/g")"; }
    printf 'export PLUGIN=%s\n'               "$(_q "$PLUGIN_ID")"
    printf 'export RUNNING_VERSION=%s\n'      "$(_q "$RUNNING_VERSION")"
    printf 'export RUNNING_SOURCE=%s\n'       "$(_q "$RUNNING_SOURCE")"
    printf 'export INSTALLED_VERSION=%s\n'    "$(_q "$INSTALLED_VERSION")"
    printf 'export INSTALLED_SOURCE=%s\n'     "$(_q "$INSTALLED_SOURCE")"
    printf 'export LATEST_VERSION=%s\n'       "$(_q "$LATEST_VERSION")"
    printf 'export LATEST_SOURCE=%s\n'        "$(_q "$LATEST_SOURCE")"
    printf 'export MARKETPLACE_DIR=%s\n'      "$(_q "$MARKETPLACE_DIR")"
    printf 'export MARKETPLACE_BRANCH=%s\n'   "$(_q "$MARKETPLACE_BRANCH")"
    printf 'export MARKETPLACE_DEFAULT=%s\n'  "$(_q "$MARKETPLACE_DEFAULT")"
    printf 'export PLUGIN_STATE=%s\n'         "$(_q "$STATE")"
    printf 'export PLUGIN_ACTION=%s\n'        "$(_q "$ACTION")"
    printf 'export PLUGIN_COMMAND=%s\n'       "$(_q "$COMMAND")"
    printf 'export PLUGIN_WARNINGS=%s\n'      "$(_q "$WARNINGS")"
    ;;
esac
