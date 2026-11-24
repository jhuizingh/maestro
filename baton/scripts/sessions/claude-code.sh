#!/usr/bin/env bash
# baton — sessions backend: Claude Code, read from `claude agents --json`.
#
# An interactive session reports `status` (busy | shell | idle | waiting, with waitingFor); a
# background one reports `state` (working | blocked | …).
#
# Requires: claude, jq.

set -uo pipefail

_die() { echo "sessions(claude-code): $*" >&2; exit 1; }

[ "${1:-}" = list ] || _die "unknown command '${1:-}'"
command -v claude >/dev/null 2>&1 || _die "claude is not on PATH"

OUT="$(claude agents --json </dev/null 2>/dev/null)" || _die "claude agents --json failed"
jq -ce 'if type == "array" then
    map({ id: (.sessionId // .id), name, cwd,
          status: ((.status // .state) as $s
            | if   $s == "waiting" or $s == "blocked" then "waiting"
              elif $s == "busy" or $s == "working" or $s == "shell" then "working"
              elif $s == "idle" then "idle"
              else "unknown" end),
          waiting_for: (.waitingFor // null) })
  else error("not an array") end' 2>/dev/null <<<"$OUT" \
  || _die "claude agents --json printed something other than a session array"
