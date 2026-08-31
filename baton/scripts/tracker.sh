#!/usr/bin/env bash
# baton — the task tracker seam. ONE dispatch point; the backend is chosen here and nowhere else.
#
# Every skill, script and hook that needs to read or write a task goes through this script. None
# of them may invoke `bd` (or any other backend tool) directly — the single exception is
# `baton:beads`, which IS the beads backend's own audit skill and says so.
#
#   tracker.sh [--context <file|->] [--tracker <dir>] [--type <name>] <verb> [args...]
#
# The full contract — the verb set, the shapes that cross the seam, the branch registry, and
# what a new backend must implement — lives in references/tracker.md. This header covers only
# what dispatching itself does.
#
# WHY THIS EXISTS. `task_tracking.type` shipped in the first context.yaml and was never
# dispatched on: fourteen skills read `task_tracking.dir` and ran `bd`, so the declared extension
# point did nothing and each new skill added more direct calls. It also gives the branch registry
# somewhere to live that is not beads-specific.
#
# BACKEND SELECTION, in order:
#   1. --type <name>            explicit; only baton:configure needs it (no context exists yet)
#   2. .task_tracking.type      from the resolved context
#   3. beads                    the default, so an old context with no `type` keeps working
#
# The backend is `scripts/tracker/<type>.sh`, invoked with BATON_TRACKER_DIR exported. A backend
# is NOT sourced — it runs as its own process, so a backend cannot corrupt a caller's shell and
# an `exit` inside it means what it says.
#
# EXIT STATUS is part of the contract:
#   0  the verb succeeded
#   1  usage error, unknown verb, unreadable context, missing backend
#   3  NOT FOUND — the id does not exist in this tracker
#   4  UNSUPPORTED — this backend does not implement this verb
# 3 and 4 are separated from 1 because a caller may reasonably continue past either. Collapsing
# them into "it failed" is how "no such bead in this context" becomes an error report.
#
# ON $BEADS_DIR. Callers used to `export BEADS_DIR=...` after resolving the context, and
# baton:beads Step 2 exists because an ambient BEADS_DIR silently redirects every `bd` write with
# no error. Going through this seam removes that class of bug: the tracker directory is resolved
# here and handed to the backend explicitly, which pins it per invocation. A caller does not need
# to export anything.
#
# Requires: jq (yq only via resolve-context.sh).

set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
BACKEND_DIR="$HERE/tracker"
DEFAULT_TYPE=beads

CTX_SRC=""
TRACKER_DIR=""
TYPE=""

_die() { echo "tracker: $*" >&2; exit 1; }

while [ $# -gt 0 ]; do
  case "$1" in
    --context) CTX_SRC="${2:-}";     shift 2 || _die "option '$1' needs a value" ;;
    --tracker) TRACKER_DIR="${2:-}"; shift 2 || _die "option '$1' needs a value" ;;
    --type)    TYPE="${2:-}";        shift 2 || _die "option '$1' needs a value" ;;
    -h|--help) sed -n '2,45p' "$0"; echo; echo "See references/tracker.md for the verb set."; exit 0 ;;
    --) shift; break ;;
    -*) _die "unknown option '$1'" ;;
    *) break ;;
  esac
done

[ $# -ge 1 ] || _die "no verb given (try --help, or see references/tracker.md)"

# --- resolve the context, but only when something still needs it ------------------------------
# --type and --tracker together fully determine the dispatch, so baton:configure can set up a
# tracker before any context exists. Resolving anyway would fail there for no reason.
_context_json() {
  if [ "$CTX_SRC" = "-" ]; then cat
  elif [ -n "$CTX_SRC" ]; then cat "$CTX_SRC" 2>/dev/null
  else
    local r="${CLAUDE_PLUGIN_ROOT:-}" resolver=""
    [ -n "$r" ] && resolver="$r/scripts/resolve-context.sh"
    [ -x "${resolver:-}" ] || resolver="$HERE/resolve-context.sh"
    [ -x "$resolver" ] || return 0
    "$resolver" 2>/dev/null || true
  fi
}

if [ -z "$TYPE" ] || [ -z "$TRACKER_DIR" ]; then
  # AN EXPLICIT --context IS STRICT. A caller passing one is asserting it has the context in
  # hand, so an unreadable file or a non-JSON stream is a usage error (exit 1), not something to
  # shrug off. Swallowing it left TRACKER_DIR empty and the verb ran on anyway — surfacing as
  # exit 3 "no such task", a confident answer about a tracker that was never opened.
  if [ -n "$CTX_SRC" ] && [ "$CTX_SRC" != "-" ] && [ ! -r "$CTX_SRC" ]; then
    _die "context file '$CTX_SRC' is not readable"
  fi
  CTX="$(_context_json)"
  if [ -n "$CTX_SRC" ]; then
    printf '%s' "$CTX" | jq -e . >/dev/null 2>&1 \
      || _die "context from '$CTX_SRC' is not valid JSON"
  else
    # Auto-resolution stays tolerant: baton:configure runs `--type beads init <dir>` before any
    # context exists, and that must not need one. The backend refuses to touch a tracker it has
    # no directory for, so a silently-failed resolve still cannot misdirect a read or a write.
    printf '%s' "$CTX" | jq -e . >/dev/null 2>&1 || CTX=""
  fi
  if [ -n "$CTX" ]; then
    [ -n "$TYPE" ] || TYPE="$(printf '%s' "$CTX" | jq -r '.task_tracking.type // empty')"
    [ -n "$TRACKER_DIR" ] || TRACKER_DIR="$(printf '%s' "$CTX" | jq -r '.task_tracking.dir // empty')"
  fi
fi

[ -n "$TYPE" ] || TYPE="$DEFAULT_TYPE"
case "$TRACKER_DIR" in "~") TRACKER_DIR="$HOME" ;; "~/"*) TRACKER_DIR="$HOME/${TRACKER_DIR#\~/}" ;; esac

# A type is a filename here, so it must not be able to escape the backend directory. Contexts are
# the user's own files, but a validated-elsewhere assumption is not a check.
case "$TYPE" in
  *[!a-z0-9-]*|"") _die "invalid task_tracking.type '$TYPE' (want lowercase letters, digits, '-')" ;;
esac

BACKEND="$BACKEND_DIR/$TYPE.sh"
if [ ! -r "$BACKEND" ]; then
  echo "tracker: no backend for task_tracking.type '$TYPE' (looked for $BACKEND)." >&2
  echo "tracker: implemented backends: $(cd "$BACKEND_DIR" 2>/dev/null && ls -1 *.sh 2>/dev/null \
        | grep -v '^lib-' | sed 's/\.sh$//' | tr '\n' ' ')" >&2
  echo "tracker: see references/tracker.md to add one." >&2
  exit 1
fi

export BATON_TRACKER_DIR="$TRACKER_DIR"
export BATON_TRACKER_TYPE="$TYPE"
export BATON_TRACKER_LIB="$BACKEND_DIR"

exec bash "$BACKEND" "$@"
