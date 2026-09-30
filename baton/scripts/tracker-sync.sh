#!/usr/bin/env bash
# baton — keep a context's tracker and its remote in step. The one place the sync POLICY lives.
#
#   tracker-sync.sh [--context <file|->] pull|push|check
#
# tracker.sh's `sync` and `sync-status` verbs are mechanism: they do what they are told to one
# tracker. This script decides whether to call them at all and what to say about the answer, so
# every skill gets the same rules without restating them:
#
#   OPT-OUT.   `task_tracking.sync: false` means "this tracker has no remote". Every mode then
#              does nothing and prints nothing. The default is to sync.
#   VERIFY.    `remote` is read before any pull or push. A backend with no remote concept (exit 4)
#              is skipped silently; a backend that could have one but has none configured gets a
#              WARNING — pushing into nothing without saying so is how a tracker ends up living on
#              one laptop.
#   FAIL SOFT. pull and push always exit 0. A failure is one line on stderr, never a failed skill.
#   NO FORCE.  A rejected push is followed by one pull (which merges) and one more push. Never
#              --force.
#
# Modes:
#   pull   sync --pull. Prints "tracker: pulled from <url>" so the caller can see which remote.
#   push   sync --push. Silent on success.
#   check  sync-status. Prints one line and exits:
#            0  in sync, or nothing to check (opt-out, backend has no sync)
#            2  drift — local commits not on the remote, and/or remote commits not merged here
#            3  unknown — no remote configured, or the backend could not tell
#          Counts are as of the last pull or push, so run it after `pull`.
#
# Exit 1 is a usage error only.
#
# Requires: jq (and whatever tracker.sh needs).

set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
TRK="$HERE/tracker.sh"

_die() { echo "tracker-sync: $*" >&2; exit 1; }

CTX_SRC=""
while [ $# -gt 0 ]; do
  case "$1" in
    --context) CTX_SRC="${2:-}"; shift 2 || _die "option '$1' needs a value" ;;
    -h|--help) sed -n '2,31p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    -*) _die "unknown option '$1'" ;;
    *) break ;;
  esac
done
MODE="${1:-}"
case "$MODE" in pull|push|check) ;; *) _die "usage: tracker-sync.sh [--context <file|->] pull|push|check" ;; esac

command -v jq >/dev/null 2>&1 || _die "jq is not installed"

# Resolve the context once and hand the same JSON to every tracker.sh call, so all of them hit
# the same tracker even if cwd-based resolution would have answered differently mid-run.
if [ "$CTX_SRC" = "-" ]; then CTX="$(cat)"
elif [ -n "$CTX_SRC" ]; then
  [ -r "$CTX_SRC" ] || _die "context file '$CTX_SRC' is not readable"
  CTX="$(cat "$CTX_SRC")"
else
  RESOLVER="$HERE/resolve-context.sh"
  CTX="$("$RESOLVER" 2>/dev/null)" || CTX=""
fi
printf '%s' "$CTX" | jq -e 'type == "object"' >/dev/null 2>&1 \
  || { echo "tracker: no context resolved — tracker not synced" >&2; [ "$MODE" = check ] && exit 3; exit 0; }

# `// true` would turn an explicit false into true; test for false itself.
[ "$(printf '%s' "$CTX" | jq -r '.task_tracking.sync == false')" = true ] && exit 0

T() { printf '%s' "$CTX" | "$TRK" --context - "$@"; }

REMOTE_JSON="$(T remote 2>/dev/null)"; RC=$?
[ "$RC" -eq 4 ] && exit 0
if [ "$RC" -ne 0 ]; then
  echo "tracker: could not read the tracker's remote — not synced" >&2
  [ "$MODE" = check ] && exit 3; exit 0
fi
URL="$(printf '%s' "$REMOTE_JSON" | jq -r 'if .configured == true then .remote else "" end' 2>/dev/null)"
if [ -z "$URL" ]; then
  echo "tracker: WARNING no sync remote is configured — tracker changes stay on this machine only." \
       "Add one, or set task_tracking.sync: false if that is intended." >&2
  [ "$MODE" = check ] && exit 3; exit 0
fi

# The cause, not the wrapper: the backend's own "tracker[beads]: … failed" line comes last and
# says nothing the caller doesn't already know, so prefer the last line before it.
_cause() {
  printf '%s\n' "$1" | awk 'NF { l=$0; if ($0 !~ /^tracker(\[[^]]*\])?: /) c=$0 } END { print (c != "" ? c : l) }'
}

case "$MODE" in
  pull)
    OUT="$(T sync --pull 2>&1)"; RC=$?
    case "$RC" in
      0) echo "tracker: pulled from $URL" ;;
      4) ;;
      *) echo "tracker: pull from $URL failed — $(_cause "$OUT")" >&2 ;;
    esac
    exit 0
    ;;

  push)
    OUT="$(T sync --push 2>&1)"; RC=$?
    case "$RC" in 0|4) exit 0 ;; esac
    # Most often the remote moved on (non-fast-forward). Merge it in and try once more.
    if PULL="$(T sync --pull 2>&1)"; then
      if OUT="$(T sync --push 2>&1)"; then
        echo "tracker: push to $URL was rejected; pulled, merged and pushed"
        exit 0
      fi
    else
      OUT="$PULL"
    fi
    echo "tracker: push to $URL FAILED — changes are on this machine only until a later push" \
         "succeeds ($(_cause "$OUT"))" >&2
    exit 0
    ;;

  check)
    S="$(T sync-status 2>&1)"; RC=$?
    [ "$RC" -eq 4 ] && exit 0
    AHEAD="$(printf '%s' "$S" | jq -r '.ahead | numbers' 2>/dev/null)"
    BEHIND="$(printf '%s' "$S" | jq -r '.behind | numbers' 2>/dev/null)"
    if [ "$RC" -ne 0 ] || [ -z "$AHEAD" ] || [ -z "$BEHIND" ]; then
      echo "tracker: drift against $URL unknown — $(_cause "$S")"
      exit 3
    fi
    if [ "$AHEAD" -eq 0 ] && [ "$BEHIND" -eq 0 ]; then
      echo "tracker: in sync with $URL"
      exit 0
    fi
    MSG=""
    [ "$AHEAD" -gt 0 ]  && MSG="$AHEAD commit(s) not on the remote"
    [ "$BEHIND" -gt 0 ] && MSG="${MSG:+$MSG; }remote has $BEHIND commit(s) not merged here"
    echo "tracker: DRIFT against $URL — $MSG"
    exit 2
    ;;
esac
