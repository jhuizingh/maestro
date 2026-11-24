#!/usr/bin/env bash
# baton — decisions, for any backend whose substrate is an append-only comment stream.
#
# Sourced by a tracker backend AFTER lib-registry.sh (it reuses that file's _reg_now, _reg_die and
# the two comment primitives). Not executable on its own. The interface it implements — the four
# verbs and the decision object — is documented in references/tracker.md, "Decisions".
#
# WHY DECISIONS ARE RECORDS, NOT NOTES. A decision used to be "the latest DECISION-or-ANSWER note
# while needs-decision is set". That shape had four failures in real use: a second DECISION hid
# the first (the user answered the newer one; the older was never seen again); ANY later ANSWER
# note cleared it (a status note spelled `ANSWER (…): created follow-ups` hid an open decision for
# two hours); a user's question about a decision counted as its answer; and the format itself was
# fragile (the keyword not at the start of a line, a missing label, a hand-typed timestamp). Here
# each decision has an id, several can be open on one task, only these verbs open, answer or
# resolve one, and every event is stamped by the clock, never by the caller.
#
# THE MODEL: AN EVENT LOG, FOLDED ON READ — the same as the branch registry, for the same reason
# (comment streams are append-only in practice everywhere). Three events, one JSON line each:
#
#   {"baton":"decision","v":1,"decision":"d4f2a91","event":"open",
#    "question":"…","options":["…","…"],"recommendation":"2","by":"…","recorded":"…"}
#   {"baton":"decision","v":1,"decision":"d4f2a91","event":"reply",
#    "kind":"answer|question|response","text":"…","by":"…","via":"…","recorded":"…"}
#   {"baton":"decision","v":1,"decision":"d4f2a91","event":"resolve","outcome":"…","by":"…","recorded":"…"}
#
# `"baton":"decision"` is the discriminator. Anything else in the stream — prose, branch-registry
# entries — is skipped silently, exactly as the registry skips decisions.
#
# Requires: jq, and lib-registry.sh already sourced.

DEC_REPLY_KINDS="answer question response"

# A decision id: `d` plus 6–12 lowercase hex. Short enough to type in a `[fermata …]` reply, and
# validated wherever one is accepted so a typo is an error rather than a new decision.
_dec_valid_id() { [[ "${1:-}" =~ ^d[0-9a-f]{6,12}$ ]]; }

_dec_mint_id() {
  local hex
  hex="$(od -An -N4 -tx1 /dev/urandom 2>/dev/null | tr -d ' \n')"
  [ -n "$hex" ] || hex="$(printf '%04x%04x' "$RANDOM" "$RANDOM")"
  printf 'd%s\n' "${hex:0:7}"
}

# THE FOLD. Comment array on stdin -> array of decision objects on stdout, oldest first.
#
# Pure and kept as one jq program so scripts/test-tracker.sh can pin it with hand-written streams.
#
# STATUS, which is the whole point:
#   resolved   a resolve event exists. Nothing reopens a decision; pose a new one instead.
#   answered   else, the user's latest reply (answer or question) is an answer: the user is done,
#              and the orchestrator has not acted yet. "Answered, waiting on the orchestrator".
#   open       else. Waiting on the user. `pending_question` is set when the user's latest reply
#              is a question nobody has responded to since — "you asked …, awaiting reply".
# A `response` (the orchestrator answering the user's question) never changes status on its own:
# it only clears pending_question, handing the decision back to the user.
_dec_fold() {
  jq -c '
    [ .[]
      | . as $c
      | (try (.text | fromjson) catch null) as $e
      | select($e != null and ($e | type) == "object" and $e.baton == "decision"
               and (($e.decision // "") | type) == "string" and ($e.decision // "") != "")
      | $e + {_at: ($c.created_at // $e.recorded // "")}
    ]
    | sort_by(._at)
    | group_by(.decision)
    | map(
        . as $g
        | ([ $g[] | select(.event == "open") ] | first) as $o
        # A reply or resolve with no open event is an orphan (a hand-written comment, a
        # half-replicated stream). It is not a decision: there is no question to show.
        | select($o != null)
        | ([ $g[] | select(.event == "reply")
             | {kind: (.kind // "answer"), text: (.text // ""), at: ._at, by: (.by // null), via: (.via // null)} ]) as $replies
        | ([ $g[] | select(.event == "resolve") ] | first) as $r
        # By position in the (stably sorted) stream, not by timestamp: a question and its response
        # can share a second.
        | ([ $replies | to_entries[] | select(.value.kind == "answer" or .value.kind == "question") ] | last) as $ue
        | ($ue.value // null) as $u
        | ([ $replies[] | select(.kind == "answer") ] | last) as $ans
        | ( if $u != null and $u.kind == "question"
              and ([ $replies[($ue.key + 1):][] | select(.kind == "response") ] | length) == 0
            then $u else null end ) as $pq
        | { id: $o.decision,
            question: ($o.question // ""),
            options: ($o.options // []),
            recommendation: ($o.recommendation // null),
            asked_at: $o._at,
            asked_by: ($o.by // null),
            status: (if $r != null then "resolved"
                     elif $u != null and $u.kind == "answer" then "answered"
                     else "open" end),
            answer: (if $ans != null then {text: $ans.text, at: $ans.at, by: $ans.by} else null end),
            answered_at: (if $u != null and $u.kind == "answer" then $u.at else null end),
            pending_question: (if $r == null and $pq != null then {text: $pq.text, at: $pq.at, by: $pq.by} else null end),
            replies: $replies,
            resolved_at: (if $r != null then $r._at else null end),
            resolved_by: (if $r != null then ($r.by // null) else null end),
            outcome: (if $r != null then ($r.outcome // null) else null end),
            updated_at: ($g[-1]._at) }
      )
    | sort_by(.asked_at)
  '
}

# $1 = task id; $2 = --all to include resolved ones.
_dec_list() {
  local all="${2:-}"
  _reg_comments_json "$1" | _dec_fold | jq -c --arg all "$all" \
    'if $all == "--all" then . else map(select(.status != "resolved")) end'
}

# $1 = task id, $2 = decision id -> the folded decision, or nothing.
_dec_one() {
  _reg_comments_json "$1" | _dec_fold | jq -c --arg d "$2" '.[] | select(.id == $d)'
}

# $1 = task id, then a JSON object of event fields. Appends one event.
_dec_append() {
  local id="$1" ev="$2" payload
  payload="$(jq -c --arg at "$(_reg_now)" '{baton:"decision", v:1} + . + {recorded:$at}' <<<"$ev")" \
    || _reg_die "could not build the decision event"
  _reg_comment_add "$id" "$payload"
}

# --- the verbs ---------------------------------------------------------------------------------
# Each takes the task id first, as every task verb does. A backend calls these from its dispatch
# and supplies _label_add/_label_remove for the needs-decision bookkeeping.

# decide <task> --question <q> [--option <o>]... [--rec <n|text>] [--by <who>] [--id <did>]
# Prints the decision id.
_dec_decide() {
  local tid="$1"; shift
  local q="" rec="" by="" did="" opts=()
  while [ $# -gt 0 ]; do
    case "$1" in
      --question) q="${2:-}";   shift 2 || _reg_die "option '$1' needs a value" ;;
      --option)   opts+=("${2:-}"); shift 2 || _reg_die "option '$1' needs a value" ;;
      --rec)      rec="${2:-}"; shift 2 || _reg_die "option '$1' needs a value" ;;
      --by)       by="${2:-}";  shift 2 || _reg_die "option '$1' needs a value" ;;
      --id)       did="${2:-}"; shift 2 || _reg_die "option '$1' needs a value" ;;
      *) _reg_die "decide: unknown option '$1'" ;;
    esac
  done
  [ -n "$q" ] || _reg_die "decide needs --question"
  local o
  for o in ${opts[@]+"${opts[@]}"}; do [ -n "$o" ] || _reg_die "decide: an --option is empty"; done
  # A numeric recommendation names an option, so it must name one that exists.
  if [[ "$rec" =~ ^[0-9]+$ ]]; then
    [ "$rec" -ge 1 ] && [ "$rec" -le "${#opts[@]}" ] \
      || _reg_die "decide: --rec $rec names no option (there are ${#opts[@]})"
  fi
  local existing
  existing="$(_reg_comments_json "$tid" | _dec_fold)" || existing="[]"
  if [ -n "$did" ]; then
    _dec_valid_id "$did" || _reg_die "decide: --id must look like d + 6-12 hex digits, got '$did'"
    jq -e --arg d "$did" 'any(.[]; .id == $d)' >/dev/null <<<"$existing" \
      && _reg_die "decide: decision $did already exists on $tid"
  else
    local n=0
    did="$(_dec_mint_id)"
    while jq -e --arg d "$did" 'any(.[]; .id == $d)' >/dev/null <<<"$existing"; do
      n=$((n+1)); [ "$n" -lt 20 ] || _reg_die "decide: could not mint an unused decision id"
      did="$(_dec_mint_id)"
    done
  fi
  local ev
  ev="$(jq -cn --arg d "$did" --arg q "$q" --arg rec "$rec" --arg by "$by" \
        --args '{decision:$d, event:"open", question:$q, options:$ARGS.positional}
                + (if $rec != "" then {recommendation:$rec} else {} end)
                + (if $by != "" then {by:$by} else {} end)' ${opts[@]+"${opts[@]}"})"
  _dec_append "$tid" "$ev"
  # Bookkeeping: needs-decision is the index readers gate on, so it is set by the verb and never
  # left to a caller to remember. A failure here is reported but does not undo the decision.
  _label_add "$tid" needs-decision \
    || echo "tracker[$BATON_TRACKER_TYPE]: WARNING decision $did recorded, but needs-decision could not be added to $tid" >&2
  printf '%s\n' "$did"
}

# reply <task> <did> --kind answer|question|response --text <t> [--by <who>] [--via <channel>]
_dec_reply() {
  local tid="$1" did="${2:-}"; shift 2 || _reg_die "reply needs a task id and a decision id"
  local kind="" text="" by="" via=""
  while [ $# -gt 0 ]; do
    case "$1" in
      --kind) kind="${2:-}"; shift 2 || _reg_die "option '$1' needs a value" ;;
      --text) text="${2:-}"; shift 2 || _reg_die "option '$1' needs a value" ;;
      --by)   by="${2:-}";   shift 2 || _reg_die "option '$1' needs a value" ;;
      --via)  via="${2:-}";  shift 2 || _reg_die "option '$1' needs a value" ;;
      *) _reg_die "reply: unknown option '$1'" ;;
    esac
  done
  _dec_valid_id "$did" || _reg_die "reply: '$did' is not a decision id"
  case " $DEC_REPLY_KINDS " in *" $kind "*) ;; *) _reg_die "reply: --kind must be one of: $DEC_REPLY_KINDS" ;; esac
  [ -n "$text" ] || _reg_die "reply needs --text"
  local cur; cur="$(_dec_one "$tid" "$did")"
  [ -n "$cur" ] || { echo "tracker[$BATON_TRACKER_TYPE]: no decision $did on $tid" >&2; exit 3; }
  [ "$(jq -r .status <<<"$cur")" != resolved ] \
    || _reg_die "reply: decision $did on $tid is already resolved — pose a new one"
  _dec_append "$tid" "$(jq -cn --arg d "$did" --arg k "$kind" --arg t "$text" --arg by "$by" --arg via "$via" \
    '{decision:$d, event:"reply", kind:$k, text:$t}
     + (if $by != "" then {by:$by} else {} end) + (if $via != "" then {via:$via} else {} end)')"
}

# resolve <task> <did> [--outcome <text>] [--by <who>]
_dec_resolve() {
  local tid="$1" did="${2:-}"; shift 2 || _reg_die "resolve needs a task id and a decision id"
  local outcome="" by=""
  while [ $# -gt 0 ]; do
    case "$1" in
      --outcome) outcome="${2:-}"; shift 2 || _reg_die "option '$1' needs a value" ;;
      --by)      by="${2:-}";      shift 2 || _reg_die "option '$1' needs a value" ;;
      *) _reg_die "resolve: unknown option '$1'" ;;
    esac
  done
  _dec_valid_id "$did" || _reg_die "resolve: '$did' is not a decision id"
  local cur; cur="$(_dec_one "$tid" "$did")"
  [ -n "$cur" ] || { echo "tracker[$BATON_TRACKER_TYPE]: no decision $did on $tid" >&2; exit 3; }
  if [ "$(jq -r .status <<<"$cur")" = resolved ]; then
    # Idempotent: a re-run (or a relay replay) of a resolve is not an error.
    echo "tracker[$BATON_TRACKER_TYPE]: decision $did on $tid was already resolved" >&2
  else
    _dec_append "$tid" "$(jq -cn --arg d "$did" --arg o "$outcome" --arg by "$by" \
      '{decision:$d, event:"resolve"}
       + (if $o != "" then {outcome:$o} else {} end) + (if $by != "" then {by:$by} else {} end)')"
  fi
  # Leaving the last unresolved decision clears needs-decision. Read back, not inferred, so a
  # decision opened concurrently keeps the label.
  local left; left="$(_dec_list "$tid" | jq 'length')" || left=1
  if [ "${left:-1}" = 0 ]; then
    _label_remove "$tid" needs-decision \
      || echo "tracker[$BATON_TRACKER_TYPE]: WARNING could not remove needs-decision from $tid" >&2
  fi
  return 0
}
