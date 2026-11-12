#!/usr/bin/env bash
# baton — what a parked task is waiting on, read out of its branch registry.
#
# A task is PARKED when `baton:finish` left it open on purpose (`keep-task-open`): this worktree's
# work landed, and something outside it has to happen before the task can close. Since 0.17.0
# finish records why, as `waiting_on` (short text, required) and `until` (an ISO date, optional)
# on the branch entry it marks `keep_task_open=yes`. Before that the reason was never written
# down, so parked tasks piled up with no way to tell what each was waiting for, and a dated check
# could sit days past due with nothing flagging it.
#
# This is the one reader, so baton:status, the session-start `status` step and any other tool
# agree on what "parked" and "overdue" mean. Pure: no tracker, no git, no network.
#
# INPUT — the task's folded registry (`tracker.sh list-branches <id>`), a JSON array on stdin.
#
# WHICH ENTRY. With --branch, that branch's entry (the newest, if the name occurs in several
# repos). Without it, the most recently updated entry with `keep_task_open == "yes"`: a task can
# own several worktrees, and the latest one parked it last.
#
# Usage:
#   parked.sh [--branch <br>] [--today YYYY-MM-DD] [--format json|env]
#
# JSON fields / env vars:
#   parked      PARKED       yes | no — the chosen entry has keep_task_open=yes
#   waiting_on  WAITING_ON   the recorded reason, or empty (parked before 0.17.0, or not parked)
#   until       UNTIL        the recorded date, or empty
#   overdue     OVERDUE      yes when parked and `until` is before --today (default: today, local)
#   branch      PARKED_BRANCH  the entry's branch, or empty
#   line        PARKED_LINE  "waiting on X (until D)", "… — overdue", or empty when not parked
#
# Requires: jq.

set -uo pipefail

BR=""; TODAY=""; FORMAT=json

_die() { echo "parked: $*" >&2; exit 2; }

while [ $# -gt 0 ]; do
  case "$1" in
    --branch) BR="${2:-}";     shift 2 || _die "option '$1' needs a value" ;;
    --today)  TODAY="${2:-}";  shift 2 || _die "option '$1' needs a value" ;;
    --format) FORMAT="${2:-}"; shift 2 || _die "option '$1' needs a value" ;;
    -h|--help) sed -n '2,/^$/p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) _die "unknown option '$1'" ;;
  esac
done
[ -n "$TODAY" ] || TODAY="$(date +%Y-%m-%d)"
[[ "$TODAY" =~ ^[0-9]{4}-[0-9]{2}-[0-9]{2}$ ]] || _die "--today must be YYYY-MM-DD"
case "$FORMAT" in json|env) ;; *) _die "--format must be json or env" ;; esac
command -v jq >/dev/null 2>&1 || _die "jq is required"

REGS="$(cat)"; [ -n "$REGS" ] || REGS='[]'
jq -e 'type == "array"' >/dev/null 2>&1 <<<"$REGS" || REGS='[]'   # garbage in reads as "no registry"

jq -c --arg br "$BR" --arg today "$TODAY" '
  ( if $br != "" then [ .[]? | select(.branch == $br) ]
    else [ .[]? | select(.keep_task_open == "yes") ] end
    | sort_by(.updated // "") | last // {} ) as $e
  | ($e.keep_task_open == "yes") as $parked
  | ($e.waiting_on // "") as $why
  | ($e.until // "") as $until
  # ISO dates compare as strings. A malformed `until` (hand-written) is never called overdue.
  | ($parked and ($until | test("^[0-9]{4}-[0-9]{2}-[0-9]{2}$")) and $until < $today) as $overdue
  | { parked:     (if $parked then "yes" else "no" end),
      waiting_on: (if $parked then $why else "" end),
      until:      (if $parked then $until else "" end),
      overdue:    (if $overdue then "yes" else "no" end),
      branch:     ($e.branch // ""),
      line: ( if ($parked | not) then ""
              else ( (if $why != "" then "waiting on \($why)" else "waiting on: no reason recorded" end)
                     + (if $until != "" then " (until \($until))" else "" end)
                     + (if $overdue then " — overdue" else "" end) )
              end ) }
' <<<"$REGS" | if [ "$FORMAT" = env ]; then
  jq -r '@sh "export PARKED=\(.parked)",
         @sh "export WAITING_ON=\(.waiting_on)",
         @sh "export UNTIL=\(.until)",
         @sh "export OVERDUE=\(.overdue)",
         @sh "export PARKED_BRANCH=\(.branch)",
         @sh "export PARKED_LINE=\(.line)"'
else
  cat
fi
