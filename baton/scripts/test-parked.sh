#!/usr/bin/env bash
# baton — tests for scripts/parked.sh: what a parked task is waiting on, and whether it is overdue.
#
# baton:status and the session-start `status` step both report this, so the rule is pinned here
# rather than restated in two skills. The failure it exists for: a parked "7-day check" sat three
# days past due with nothing flagging it, because the reason and date were never recorded.
#
# Hermetic: needs only bash and jq. Exit 0 = all passed.

set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
P="$HERE/parked.sh"
[ -r "$P" ] || { echo "not found: $P" >&2; exit 2; }
command -v jq >/dev/null 2>&1 || { echo "jq is required" >&2; exit 2; }

PASS=0; FAIL=0
_ok()  { PASS=$((PASS+1)); printf '  ✅ %s\n' "$1"; }
_bad() { FAIL=$((FAIL+1)); printf '  ❌ %s\n     %s\n' "$1" "$2"; }
_eq()  { if [ "$2" = "$3" ]; then _ok "$1"; else _bad "$1" "got '$2', want '$3'"; fi; }

# $1 = registry JSON, then flags -> "parked|waiting_on|until|overdue|line"
_p() {
  local regs="$1"; shift
  bash "$P" --today 2026-05-10 "$@" <<<"$regs" \
    | jq -r '[.parked, .waiting_on, .until, .overdue, .line] | join("|")'
}

PARK='{"repo":"m","branch":"b1","status":"merged","ready":"yes","keep_task_open":"yes","waiting_on":"the vendor fix","updated":"2026-05-01T00:00:00Z"}'

echo "== reading the reason"
_eq "not parked: nothing to say" \
    "$(_p '[{"repo":"m","branch":"b1","status":"merged","ready":"yes"}]')" "no|||no|"
_eq "parked with a reason and no date" \
    "$(_p "[$PARK]")" "yes|the vendor fix||no|waiting on the vendor fix"
_eq "parked with a future date" \
    "$(_p "[$(jq -c '.until="2026-05-20"' <<<"$PARK")]")" \
    "yes|the vendor fix|2026-05-20|no|waiting on the vendor fix (until 2026-05-20)"
_eq "due today is not yet overdue" \
    "$(_p "[$(jq -c '.until="2026-05-10"' <<<"$PARK")]" | cut -d'|' -f4)" "no"
_eq "a past date is overdue, and the line says so" \
    "$(_p "[$(jq -c '.until="2026-05-07"' <<<"$PARK")]")" \
    "yes|the vendor fix|2026-05-07|yes|waiting on the vendor fix (until 2026-05-07) — overdue"
_eq "parked before reasons were recorded" \
    "$(_p '[{"repo":"m","branch":"b1","keep_task_open":"yes"}]' | cut -d'|' -f1,5)" \
    "yes|waiting on: no reason recorded"
_eq "a malformed date is never called overdue" \
    "$(_p "[$(jq -c '.until="soon"' <<<"$PARK")]" | cut -d'|' -f4)" "no"

echo "== which entry"
OLD='{"repo":"m","branch":"b0","keep_task_open":"yes","waiting_on":"older","updated":"2026-04-01T00:00:00Z"}'
LIVE='{"repo":"m","branch":"b2","status":"open","updated":"2026-05-09T00:00:00Z"}'
_eq "without --branch, the newest parked entry wins" \
    "$(_p "[$OLD,$PARK,$LIVE]" | cut -d'|' -f2)" "the vendor fix"
_eq "with --branch, that branch's entry — even if it is not parked" \
    "$(_p "[$OLD,$PARK,$LIVE]" --branch b2 | cut -d'|' -f1)" "no"
_eq "with --branch on a parked one" \
    "$(_p "[$OLD,$PARK,$LIVE]" --branch b0 | cut -d'|' -f2)" "older"

echo "== input and output"
_eq "garbage in reads as not parked" "$(_p 'not json' | cut -d'|' -f1)" "no"
_eq "empty input reads as not parked" "$(_p '' | cut -d'|' -f1)" "no"
_eq "a bad --today is refused" "$(bash "$P" --today tomorrow <<<'[]' >/dev/null 2>&1; echo $?)" "2"
ENV="$(bash "$P" --today 2026-05-10 --format env <<<"[$(jq -c '.waiting_on="Bob'"'"'s \"ok\"" | .until="2026-05-01"' <<<"$PARK")]")"
eval "$ENV"
_eq "env output survives quotes in the reason" "$WAITING_ON" "Bob's \"ok\""
_eq "…and exports OVERDUE" "$OVERDUE" "yes"
_eq "…and PARKED_LINE" "$PARKED_LINE" "waiting on Bob's \"ok\" (until 2026-05-01) — overdue"

echo
printf 'parked: %d passed, %d failed\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
