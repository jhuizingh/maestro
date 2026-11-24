#!/usr/bin/env bash
# baton — declared task state: the closed vocabulary, its validation and the stored shape.
#
# Sourced by a tracker backend; not executable on its own. The vocabulary is documented in
# references/declared-state.md; this file is the one place that enforces it, so every backend accepts
# and refuses exactly the same `state` calls.
#
# WHAT A BACKEND PROVIDES around it: storage of one JSON object per task (beads: issue metadata),
# the `state` object in every task it emits (via _state_norm), and the bookkeeping in
# references/declared-state.md ("Bookkeeping").
#
# Requires: jq.

# The closed vocabulary. `clear` is not a state: it removes the declared one.
STATE_VALUES="waiting-on-me working waiting-on-review blocked paused waiting-until done-pending-cleanup"

_state_die() { echo "tracker[${BATON_TRACKER_TYPE:-?}]: $*" >&2; exit 1; }

_state_known() { # $1 = state name
  local s
  for s in $STATE_VALUES; do [ "$s" = "$1" ] && return 0; done
  return 1
}

# $1 = state, then --on/--until/--why/--next/--by pairs. Prints the stored object (one line), or
# dies with the reason. With no --by, `by` is $BATON_STATE_BY_DEFAULT (the backend's whoami).
_state_build() {
  local st="${1:-}"; shift || true
  [ -n "$st" ] || _state_die "state needs a state name (one of: $STATE_VALUES, or clear)"
  _state_known "$st" || _state_die "unknown state '$st' (want one of: $STATE_VALUES, or clear)"
  local on="" until="" why="" next="" by=""
  while [ $# -gt 0 ]; do
    case "$1" in
      --on)    on="${2:-}";    shift 2 || _state_die "option '$1' needs a value" ;;
      --until) until="${2:-}"; shift 2 || _state_die "option '$1' needs a value" ;;
      --why)   why="${2:-}";   shift 2 || _state_die "option '$1' needs a value" ;;
      --next)  next="${2:-}";  shift 2 || _state_die "option '$1' needs a value" ;;
      --by)    by="${2:-}";    shift 2 || _state_die "option '$1' needs a value" ;;
      *) _state_die "state: unknown option '$1'" ;;
    esac
  done
  # `until` is compared as a string to call a state overdue; anything else would never be.
  [ -z "$until" ] || [[ "$until" =~ ^[0-9]{4}-[0-9]{2}-[0-9]{2}$ ]] \
    || _state_die "--until must be an ISO date (YYYY-MM-DD), got '$until'"
  case "$st" in
    blocked)       [ -n "$on" ]    || _state_die "blocked needs --on (what it is blocked on)" ;;
    paused)        [ -n "$why" ]   || _state_die "paused needs --why (why it was stopped)" ;;
    waiting-until) [ -n "$until" ] || _state_die "waiting-until needs --until YYYY-MM-DD" ;;
  esac
  [ -n "$by" ] || by="${BATON_STATE_BY_DEFAULT:-}"
  jq -cn --arg s "$st" --arg on "$on" --arg u "$until" --arg w "$why" --arg n "$next" \
    --arg by "$by" --arg at "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
    '{state:$s, on:$on, until:$u, why:$w, next:$n, by:$by, at:$at}
     | with_entries(if .value == "" then .value = null else . end)'
}

# jq definition: a stored value (string or object, possibly absent) -> the normalized `state`
# object or null. Unparseable or unknown-named values read as state "unknown", never as no state.
_STATE_NORM_DEF='
  def norm_state:
    (if . == null or . == "" then null
     elif type == "string" then (try fromjson catch {state:"unknown"})
     elif type == "object" then .
     else {state:"unknown"} end) as $v
    | if $v == null then null
      elif ($v | type) != "object" then {state:"unknown"}
      else
        ( ["waiting-on-me","working","waiting-on-review","blocked","paused","waiting-until",
           "done-pending-cleanup"] | index($v.state // "") ) as $k
        | { state: (if $k == null then "unknown" else $v.state end),
            on: ($v.on // null), until: ($v.until // null), why: ($v.why // null),
            next: ($v.next // null), by: ($v.by // null), at: ($v.at // null) }
      end;'
