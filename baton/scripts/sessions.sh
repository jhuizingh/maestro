#!/usr/bin/env bash
# baton — live sessions on this machine, whatever runs them.
#
#   sessions.sh list
#
# Prints a JSON array of {id, name, cwd, status, waiting_for}; status is working | idle | waiting |
# unknown. Dispatches to scripts/sessions/$BATON_SESSIONS_TYPE.sh (default claude-code).
# The contract is in references/sweep.md.
#
# Exit status: 0 the array (`[]` = none live), 1 cannot tell, 4 no such backend.
#
# Requires: jq.

set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
_die() { echo "sessions: $*" >&2; exit 1; }

case "${1:-}" in
  -h|--help) sed -n '2,/^$/p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
  list) [ $# -eq 1 ] || _die "list takes no arguments" ;;
  *) _die "unknown command '${1:-}'" ;;
esac
command -v jq >/dev/null 2>&1 || _die "jq is not installed"

TYPE="${BATON_SESSIONS_TYPE:-claude-code}"
[[ "$TYPE" =~ ^[a-z0-9-]+$ ]] || _die "bad backend name '$TYPE'"
BACKEND="$HERE/sessions/$TYPE.sh"
[ -x "$BACKEND" ] || { echo "sessions: no backend '$TYPE'" >&2; exit 4; }

OUT="$("$BACKEND" list)" || _die "backend '$TYPE' could not list sessions"
jq -ce 'if type == "array" and all(.[]; type == "object") then
    map({ id: (.id // null), name: (.name // null), cwd: (.cwd // ""),
          status: (.status as $s | if (["working","idle","waiting"] | index($s)) != null
                                   then $s else "unknown" end),
          waiting_for: (.waiting_for // null) })
  else error("not an array of objects") end' 2>/dev/null <<<"$OUT" \
  || _die "backend '$TYPE' did not return a session array"
