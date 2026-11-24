#!/usr/bin/env bash
# baton — read-only check of one context for things that have quietly stopped.
#
#   sweep-check.sh [--context <file|->] [--format text|json] [--backlog each|summary]
#                  [--today YYYY-MM-DD] [--now <ISO UTC>] [--stalled-after <hours>]
#                  [--stale-after <days>]
#
# Prints one line per problem, `<kind> <task id> <detail>`. Kinds, in order: unreadable, answered,
# prompt, overdue, stalled, unknown, backlog. Nothing to report prints nothing. The rules for each
# kind are in references/sweep.md.
#
# Writes nothing: tracker reads only, no sync, no carrier backfill, no session touched. A read that
# fails is an `unreadable` line, never a silence.
#
# Exit status: 0 every check ran, 1 could not run at all, 2 ran but something was unreadable.
#
# Requires: jq, git.

set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
TRK="$HERE/tracker.sh"
SESSIONS="$HERE/sessions.sh"
IDENT="$HERE/task-identity.sh"
PARKED="$HERE/parked.sh"

_die() { echo "sweep-check: $*" >&2; exit 1; }

CTX_SRC=""; FORMAT=text; BACKLOG=each; TODAY=""; NOW=""; STALLED_H=4; STALE_D=14
while [ $# -gt 0 ]; do
  case "$1" in
    --context)       CTX_SRC="${2:-}";   shift 2 || _die "option '$1' needs a value" ;;
    --format)        FORMAT="${2:-}";    shift 2 || _die "option '$1' needs a value" ;;
    --backlog)       BACKLOG="${2:-}";   shift 2 || _die "option '$1' needs a value" ;;
    --today)         TODAY="${2:-}";     shift 2 || _die "option '$1' needs a value" ;;
    --now)           NOW="${2:-}";       shift 2 || _die "option '$1' needs a value" ;;
    --stalled-after) STALLED_H="${2:-}"; shift 2 || _die "option '$1' needs a value" ;;
    --stale-after)   STALE_D="${2:-}";   shift 2 || _die "option '$1' needs a value" ;;
    -h|--help) sed -n '2,/^$/p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) _die "unknown option '$1'" ;;
  esac
done
command -v jq >/dev/null 2>&1 || _die "jq is not installed"
case "$FORMAT" in text|json) ;; *) _die "--format must be text or json" ;; esac
case "$BACKLOG" in each|summary) ;; *) _die "--backlog must be each or summary" ;; esac
[ -n "$TODAY" ] || TODAY="$(date +%Y-%m-%d)"
[ -n "$NOW" ] || NOW="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
[[ "$TODAY" =~ ^[0-9]{4}-[0-9]{2}-[0-9]{2}$ ]] || _die "--today must be YYYY-MM-DD"
[[ "$NOW" =~ ^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z$ ]] || _die "--now must be YYYY-MM-DDTHH:MM:SSZ"
[[ "$STALLED_H" =~ ^[0-9]{1,6}$ ]] || _die "--stalled-after must be a number of hours"
[[ "$STALE_D" =~ ^[0-9]{1,6}$ ]] || _die "--stale-after must be a number of days"

# --- context ----------------------------------------------------------------------------------
if [ "$CTX_SRC" = - ]; then CTX="$(cat)"
elif [ -n "$CTX_SRC" ]; then CTX="$(cat "$CTX_SRC" 2>/dev/null)" || _die "cannot read context '$CTX_SRC'"
else CTX="$("$HERE/resolve-context.sh" 2>/dev/null)" || CTX=""
fi
jq -e 'type == "object"' >/dev/null 2>&1 <<<"$CTX" || _die "no context resolved"
TMPD="$(mktemp -d "${TMPDIR:-/tmp}/baton-sweep.XXXXXX")" || _die "cannot make a temp directory"
trap 'rm -rf "$TMPD"' EXIT
CTXF="$TMPD/context"; FOUND="$TMPD/found"; LIVE="$TMPD/live"; DECS="$TMPD/decisions"; PARK="$TMPD/parked"
printf '%s' "$CTX" >"$CTXF"
: >"$FOUND"; : >"$LIVE"; : >"$DECS"; : >"$PARK"

_T() { "$TRK" --context "$CTXF" "$@" 2>/dev/null </dev/null; }

# _found <kind> <id> <title> <detail>
_found() {
  jq -cn --arg kind "$1" --arg id "$2" --arg title "$3" --arg detail "$4" \
    '{kind:$kind, id:$id, title:$title, detail:$detail}' >>"$FOUND"
}

_report() {
  local all
  all="$(jq -cs --arg backlog "$BACKLOG" '
    def rank: {unreadable:0, answered:1, prompt:2, overdue:3, stalled:4, unknown:5, backlog:6}[.kind];
    def one_line: gsub("[\\x{0000}-\\x{001f}\\x{007f}-\\x{009f}]+"; " ") | gsub(" +"; " ")
      | if length > 200 then .[:199] + "…" else . end;
    [ to_entries[] | .key as $k | .value | . + {_r: rank, _k: $k} ] | sort_by(._r, ._k)
    | map(del(._r, ._k) | .title |= one_line | .detail |= one_line)
    | ([ .[] | select(.kind == "backlog") ]) as $b
    | if $backlog == "summary" and ($b | length) > 1
      then map(select(.kind != "backlog"))
           + [{kind: "backlog", id: "-", title: "",
               detail: "\($b | length) tasks untouched too long; oldest: \([ $b[:3][].id ] | join(", "))"}]
      else . end' "$FOUND")" || _die "could not assemble the report"
  if [ "$FORMAT" = json ]; then printf '%s\n' "$all"
  else jq -r '.[] | "\(.kind) \(.id) \(.detail)" + (if .title == "" then "" else " — \(.title)" end)' <<<"$all"
  fi
  if grep -q '"kind":"unreadable"' "$FOUND"; then exit 2; fi
  exit 0
}

# --- tasks ------------------------------------------------------------------------------------
TASKS="$(_T list --status active)" && jq -e 'type == "array" and all(.[]; type == "object" and (.id | type) == "string")' >/dev/null 2>&1 <<<"$TASKS" || {
  _found unreadable - "" "the task list could not be read; nothing was checked"
  _report
}

ME="$(_T whoami)" && ME="$(jq -r '.user // ""' 2>/dev/null <<<"$ME")" || ME=""

# The tasks to judge: mine, unassigned, or posing a decision.
MINE="$(jq -c --arg me "$ME" '
  def norm: ascii_downcase | gsub("^\\s+|\\s+$"; "");
  map(.labels = (.labels // []))
  | map(select($me == "" or (.assignee // "") == "" or ((.assignee | norm) == ($me | norm))
               or (.labels | index("needs-decision")) != null))' <<<"$TASKS")" \
  || _die "could not read the task list"

# --- live sessions ----------------------------------------------------------------------------
SESS="$("$SESSIONS" list 2>/dev/null </dev/null)" || {
  SESS='[]'
  _found unreadable - "" "live sessions could not be listed; prompts not checked, and stalled/unknown judged as if none were live"
}
while IFS= read -r S; do
  [ -n "$S" ] || continue
  CWD="$(jq -r .cwd <<<"$S")"
  [ -n "$CWD" ] && [ -d "$CWD" ] || continue
  ID_JSON="$("$IDENT" --worktree "$CWD" --context "$CTXF" --no-backfill --format json 2>/dev/null </dev/null)" || continue
  LEAF="$(jq -r '.leaf // empty' 2>/dev/null <<<"$ID_JSON")"
  [ -n "$LEAF" ] || continue
  printf '%s\n' "$LEAF" >>"$LIVE"
  [ "$(jq -r .status <<<"$S")" = waiting ] || continue
  WHAT="$(jq -r '"session " + (.name // .id // "?") + " is waiting at a prompt"
                 + (if .waiting_for then " (\(.waiting_for))" else "" end)' <<<"$S")"
  TITLE="$(jq -r --arg id "$LEAF" '[ .[] | select(.id == $id) ][0] | .title // ""' <<<"$TASKS")"
  if jq -e --arg id "$LEAF" 'any(.[]; .id == $id)' >/dev/null <<<"$TASKS"; then
    _found prompt "$LEAF" "$TITLE" "$WHAT"
  else
    # Not active here: closed in this tracker, or another context's task.
    T="$(_T get "$LEAF")"; RC=$?
    case "$RC" in
      0) _found prompt "$LEAF" "$(jq -r '.title // ""' <<<"$T")" "$WHAT, and the task is closed" ;;
      3) : ;;
      *) _found unreadable "$LEAF" "" "$WHAT, and its task could not be read" ;;
    esac
  fi
done < <(jq -c '.[]' <<<"$SESS")

# --- decisions --------------------------------------------------------------------------------
while IFS= read -r ID; do
  [ -n "$ID" ] || continue
  D="$(_T decisions "$ID")"; RC=$?
  if [ "$RC" -eq 4 ]; then D='[]'; RC=0; fi     # no decision records: only notes to read by hand
  if [ "$RC" -ne 0 ] || ! jq -e 'type == "array"' >/dev/null 2>&1 <<<"$D"; then
    TITLE="$(jq -r --arg id "$ID" '.[] | select(.id == $id) | .title // ""' <<<"$MINE")"
    _found unreadable "$ID" "$TITLE" "its decisions could not be read"
    continue
  fi
  jq -c --arg id "$ID" '{id: $id, decisions: .}' <<<"$D" >>"$DECS"
done < <(jq -r '.[] | select(.labels | index("needs-decision")) | .id' <<<"$MINE")

# --- parked tasks -----------------------------------------------------------------------------
while IFS= read -r ID; do
  [ -n "$ID" ] || continue
  if R="$(_T list-branches "$ID")" && jq -e 'type == "array"' >/dev/null 2>&1 <<<"$R" \
     && P="$("$PARKED" --today "$TODAY" <<<"$R")"; then
    jq -c --arg id "$ID" '{id: $id} + .' <<<"$P" >>"$PARK"
  else
    TITLE="$(jq -r --arg id "$ID" '.[] | select(.id == $id) | .title // ""' <<<"$MINE")"
    _found unreadable "$ID" "$TITLE" "parked, but its branch registry could not be read"
  fi
done < <(jq -r '.[] | select(.labels | index("keep-task-open")) | .id' <<<"$MINE")

# --- the rules --------------------------------------------------------------------------------
jq -c --arg today "$TODAY" --arg now "$NOW" --argjson stalled_h "$STALLED_H" --argjson stale_d "$STALE_D" \
  --rawfile live "$LIVE" --slurpfile decs "$DECS" --slurpfile park "$PARK" '
  def ts: if type != "string" then null
    else (sub("\\.[0-9]+"; "") | sub("\\+00:00$"; "Z")) as $s
      | try ($s | fromdateiso8601) catch null end;
  def ago($t): if $t == null then "at an unreadable time"
    else (($now | fromdateiso8601) - $t) as $s
      | if $s < 3600 then "\(($s / 60 | floor) | if . < 0 then 0 else . end)m ago"
        elif $s < 172800 then "\($s / 3600 | floor)h ago"
        else "\($s / 86400 | floor)d ago" end end;
  def has($l): (.labels | index($l)) != null;
  ($now | fromdateiso8601) as $n
  | ($live | split("\n")) as $live
  | (reduce $decs[] as $d ({}; .[$d.id] = $d.decisions)) as $decs
  | (reduce $park[] as $p ({}; .[$p.id] = $p)) as $park
  | [ .[]
      | . as $t
      | (.updated_at | ts) as $upd
      | ((.state // {}).state // null) as $st
      | (($live | index($t.id)) != null) as $is_live
      | ($decs[$t.id]) as $ds
      | (has("keep-task-open") or has("ready-for-worktree-delete")) as $is_parked
      | {id, title: (.title // "")} as $base
      | ( ($ds // [])[] | select(.status == "answered")
          | $base + {kind: "answered", _s: 0,
              detail: "decision \(.id) answered \(ago(.answered_at | ts)), not resolved: \(.question)"} ),
        ( if ($st == "blocked" or $st == "waiting-until") and ((.state.until // "") | test("^[0-9]{4}-[0-9]{2}-[0-9]{2}$"))
             and .state.until < $today
          then $base + {kind: "overdue", _s: 0, detail: "\($st) until \(.state.until), which has passed"}
          elif ($park[$t.id].overdue // "no") == "yes"
          then $base + {kind: "overdue", _s: 0, detail: "parked, \($park[$t.id].line)"}
          else empty end ),
        ( if $st == "working" and ($is_live | not)
          then ([$upd, (.state.at | ts)] | map(select(. != null)) | max) as $last
            | if $last == null or ($n - $last) > ($stalled_h * 3600)
              then $base + {kind: "stalled", _s: 0,
                detail: "declared working, no live session, last change \(ago($last))"}
              else empty end
          else empty end ),
        ( if $st == "unknown" then $base + {kind: "unknown", _s: 0, detail: "its declared state cannot be read"}
          elif .status == "unknown" then $base + {kind: "unknown", _s: 0, detail: "its tracker status cannot be read"}
          elif has("needs-decision") and $ds == [] then
            $base + {kind: "unknown", _s: 0, detail: "needs-decision, but no decision is recorded; read its notes"}
          elif $st == null and .status == "in_progress" and ($is_live | not) and ($is_parked | not)
               and (has("needs-decision") | not)
          then $base + {kind: "unknown", _s: 0, detail: "in progress, no declared state, no live session"}
          else empty end ),
        ( if $st == null and (.status == "open" or .status == "blocked")
             and (has("needs-decision") | not) and ($is_parked | not)
             and ($upd == null or ($n - $upd) > ($stale_d * 86400))
          then $base + {kind: "backlog", _s: ($upd // 0), detail: "untouched, last change \(ago($upd))"}
          else empty end )
    ]
  | sort_by(._s)[] | {kind, id, title, detail}' <<<"$MINE" >>"$FOUND" \
  || _die "could not evaluate the tasks"

_report
