#!/bin/sh
# Example fermata ticket adapter: `ticket-info.sh <key>` prints TicketInfo JSON, or an error
# object saying why it could not (see "When `info` fails" in fermata's DESIGN.md).
#
# The ticket source here is GitHub issues via `gh`, with keys shaped `owner/repo#123`. Copy this
# into your workspace and replace `lookup` (and `classify`) for your ticket system; the rest
# is the part worth keeping:
#   - portable: POSIX sh, no GNU `timeout`, no jq;
#   - bounded: finishes inside fermata's budget ($FERMATA_INFO_TIMEOUT), so its own reason
#     reaches the panel instead of fermata's kill;
#   - honest: passes the tool's error text through to stderr and says not_found only when the
#     source said so;
#   - an optional local fallback ($TICKET_INFO_DIR/<key>.json) used only when the file exists.
#
# fermata.yaml:
#   ticket:
#     key_from: label
#     pattern: '^gh-(.+/.+#[0-9]+)$'
#     info: ['./fermata/ticket-info.sh', '{key}']
set -u

key="${1:-}"

# --- output helpers ------------------------------------------------------------------------

# JSON string body: escape \ and ", flatten newlines and tabs to spaces.
json_str() {
    printf '%s' "$1" | tr '\n\t\r' '   ' | sed -e 's/\\/\\\\/g' -e 's/"/\\"/g'
}

# fail <kind> <message>: print the error object, echo the message to stderr, stop.
fail() {
    printf '%s\n' "ticket-info: $2" >&2
    printf '{"error":{"kind":"%s","message":"%s"}}\n' "$1" "$(json_str "$2")"
    exit 1
}

# --- bounded execution ---------------------------------------------------------------------

# Leave a second for this script's own work; fermata kills us at the full budget.
budget="${FERMATA_INFO_TIMEOUT:-5}"
case "$budget" in '' | *[!0-9]*) budget=5 ;; esac
limit=$((budget > 1 ? budget - 1 : 1))

# run_bounded <secs> <cmd...>: run cmd, TERM it after secs. Status 124 means it timed out.
# The watchdog's fds go to /dev/null so a leftover `sleep` never holds fermata's pipes open.
run_bounded() {
    secs="$1"
    shift
    "$@" &
    pid=$!
    flag="${TMPDIR:-/tmp}/ticket-info.$$.timedout"
    (sleep "$secs" && touch "$flag" && kill -TERM "$pid") </dev/null >/dev/null 2>&1 &
    dog=$!
    wait "$pid"
    rc=$?
    kill "$dog" 2>/dev/null
    if [ -e "$flag" ]; then
        rm -f "$flag"
        return 124
    fi
    return "$rc"
}

# --- the ticket source (replace for your system) -------------------------------------------

case "$key" in
*/*#*) ;;
*) fail error "key '$key' is not owner/repo#number" ;;
esac
repo="${key%%#*}"
num="${key##*#}"

# `exec` matters: run_bounded backgrounds this in a subshell, and only exec makes gh itself the
# process it kills. A surviving grandchild would keep the output pipe, and us, open.
# shellcheck disable=SC2329  # invoked through run_bounded
lookup() {
    exec gh issue view "$num" -R "$repo" --json number,title,state,assignees,url --jq "{
        key: \"$key\",
        title: .title,
        status: .state,
        assignee: (.assignees | map(.login) | join(\", \") | if . == \"\" then null else . end),
        url: .url
    }"
}

# classify <stderr>: which kind a failed lookup is. Only an answer from the source is not_found.
classify() {
    case "$1" in
    *"Could not resolve to an issue"* | *"Could not resolve to a Repository"*) echo not_found ;;
    *"error connecting"* | *"timeout"* | *"timed out"* | *"no such host"* | *"connection reset"* | \
        *"HTTP 5"[0-9][0-9]* | *"HTTP 401"* | *"gh auth login"* | *"rate limit"*) echo unavailable ;;
    *) echo error ;;
    esac
}

# --- main ----------------------------------------------------------------------------------

command -v gh >/dev/null 2>&1 || fail error "gh is not installed"

errf="${TMPDIR:-/tmp}/ticket-info.$$.err"
trap 'rm -f "$errf"' EXIT
out="$(run_bounded "$limit" lookup 2>"$errf")"
rc=$?
err="$(cat "$errf" 2>/dev/null)"

if [ "$rc" -eq 0 ] && [ -n "$out" ]; then
    [ -n "$err" ] && printf '%s\n' "$err" >&2
    printf '%s\n' "$out"
    exit 0
fi

# Optional local fallback: only a file that exists counts; a missing one is just "no data".
local_file="${TICKET_INFO_DIR:-}/$repo/$num.json"
if [ -n "${TICKET_INFO_DIR:-}" ] && [ -f "$local_file" ] && [ "$rc" -ne 0 ]; then
    kind="$(classify "$err")"
    if [ "$kind" != not_found ]; then
        printf '%s\n' "ticket-info: $key: live lookup failed, using $local_file: $err" >&2
        cat "$local_file"
        exit 0
    fi
fi

[ "$rc" -eq 124 ] && fail unavailable "gh did not answer within ${limit}s"
[ -n "$err" ] || err="gh exited $rc with no output"
fail "$(classify "$err")" "$err"
