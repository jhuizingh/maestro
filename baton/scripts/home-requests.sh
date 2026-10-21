#!/usr/bin/env bash
# baton — home requests: a session that cannot act (a cloud session) asks the home machine to.
#
#   home-requests.sh scan    [--context <file|->] [--format json|text]
#   home-requests.sh check   [--context <file|->] <id> <request>
#   home-requests.sh merge   [--context <file|->] <id> --head <sha> [--confirmed]
#   home-requests.sh adopt   [--context <file|->] <id>
#   home-requests.sh resolve [--context <file|->] <id> <request> --outcome <word> --summary <text>
#
# A cloud session can write to the tracker through the relay (tracker-relay.sh), but it cannot
# merge a PR, run baton:finish or ask a human anything. It asks by putting a REQUEST LABEL on the
# task. The home machine's session-start finds those labels and runs the fixed procedure each one
# names. The labels are a closed set:
#
#   home:review-merge     review the task's PR; merge it (merge commit) and close the task
#   home:finish           set up a worktree for the task's cloud-built branch, for baton:finish
#   home:needs-decision   nothing runs; the task is shown to the human
#
# A `home:*` label that is not one of these is reported and never acted on.
#
# WHY LABELS AND NOT INSTRUCTIONS. The session handling a request runs with the home machine's
# full permissions, and a task's notes can carry text from anywhere: a PR comment, an issue the
# cloud read. So nothing here reads the notes. A label picks one of the procedures below, and the
# procedure takes its inputs only from the tracker's structured fields and the branch registry:
#
#   - THE PR comes from the task's branch registry (repo + branch, `list-branches`), looked up
#     with `gh pr view <branch> --repo <owner>/<repo>`. A PR number or URL in a note is never
#     consulted. A registry `pr` field that disagrees with the branch's PR is a stop.
#   - THE GATES are the same as baton:finish's. `check` decides, and `merge` re-decides before it
#     merges, so a caller cannot skip one: merge without asking only when the task is
#     autonomous-safe AND every check is green; otherwise only with --confirmed (a human said
#     yes). A red check, a draft, a conflict or a closed PR is a stop that --confirmed does not
#     override. Pending checks, an unknown mergeable state or an unreachable GitHub are a wait:
#     the label stays and the next session-start looks again.
#   - `merge` takes the head commit the reviewer looked at (--head) and merges with
#     --match-head-commit, so a push after the review cannot be merged unreviewed.
#
# `scan` does read the notes, for one thing only: their tail goes in its JSON as `notes_tail`
# for the human-facing summary. No procedure here reads that field.
#
# RESOLVING. A handled request is resolved with `resolve`: a note saying what was done (or why it
# stopped), marked `[home-request:<label>]`, then the label is removed. `merge` resolves on its
# own once it has merged. With the label gone, a re-run finds nothing, and `resolve` on a task
# without the label is a no-op.
#
# A SESSION IN RELAY MODE IS NOT HOME. `scan` prints nothing there and the writing verbs refuse,
# for the same reason tracker-relay.sh apply does nothing in relay mode.
#
# `check` prints one JSON object:
#   {id, request, action, reason, autonomous_safe, repo, branch,
#    pr: {number, url, state, head} | null, failing: [...], pending: [...]}
# action is one of:
#   merge     review-merge: autonomous-safe and green — review, then `merge --head`
#   ask       review-merge: green but not autonomous-safe — review, ask, then `merge --confirmed`
#   close     review-merge: the PR is already merged — close the task, then resolve
#   adopt     finish: `adopt` the branch into a worktree, then run baton:finish there
#   report    needs-decision: show it to the human, leave the label
#   wait      nothing to do yet (checks pending, GitHub unreachable) — leave the label
#   stop      cannot proceed — resolve with the reason
#   resolve   the task is already closed — resolve, nothing else
#   none      the task does not carry this request label
#   unknown   not a request this script knows — report it, leave the label
#
# Exit status: 0 ok, 1 usage or failure, 3 no such task, 5 refused by a gate (merge, adopt).
#
# Requires: jq, git, gh (for review-merge and finish).

set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
TRK="$HERE/tracker.sh"
RELAY="$HERE/tracker-relay.sh"
PREFIX="home:"
KNOWN="home:review-merge home:finish home:needs-decision"

_die() { echo "home-requests: $*" >&2; exit 1; }
_refuse() { echo "home-requests: refused — $*" >&2; exit 5; }

command -v jq >/dev/null 2>&1 || _die "jq is not installed"

_relaying() { [ -x "$RELAY" ] && "$RELAY" active; }

# --- arguments: --context may sit anywhere after the command ----------------------------------
CMD="${1:-}"; shift || true
CTX_SRC=""; FORMAT=json; HEAD=""; CONFIRMED=no; OUTCOME=""; SUMMARY=""
POS=()
while [ $# -gt 0 ]; do
  case "$1" in
    --context)   CTX_SRC="${2:-}"; shift 2 || _die "option '$1' needs a value" ;;
    --format)    FORMAT="${2:-}";  shift 2 || _die "option '$1' needs a value" ;;
    --head)      HEAD="${2:-}";    shift 2 || _die "option '$1' needs a value" ;;
    --outcome)   OUTCOME="${2:-}"; shift 2 || _die "option '$1' needs a value" ;;
    --summary)   SUMMARY="${2:-}"; shift 2 || _die "option '$1' needs a value" ;;
    --confirmed) CONFIRMED=yes; shift ;;
    --) shift; POS+=("$@"); break ;;
    -*) _die "unknown option '$1'" ;;
    *) POS+=("$1"); shift ;;
  esac
done

case "$CMD" in
  -h|--help|"") sed -n '2,66p' "$0" | sed 's/^# \{0,1\}//'; [ -n "$CMD" ]; exit ;;
  scan|check|merge|adopt|resolve) ;;
  *) _die "unknown command '$CMD'" ;;
esac

# --- context ----------------------------------------------------------------------------------
if [ "$CTX_SRC" = - ]; then CTX="$(cat)"
elif [ -n "$CTX_SRC" ]; then CTX="$(cat "$CTX_SRC" 2>/dev/null)" || _die "cannot read context '$CTX_SRC'"
else CTX="$("$HERE/resolve-context.sh" 2>/dev/null)" || CTX=""
fi
jq -e 'type == "object"' >/dev/null 2>&1 <<<"$CTX" || _die "no context resolved"
CTXF="$(mktemp "${TMPDIR:-/tmp}/baton-home-requests.XXXXXX")"
trap 'rm -f "$CTXF"' EXIT
printf '%s' "$CTX" >"$CTXF"

_T() { "$TRK" --context "$CTXF" "$@"; }

_expand() { case "$1" in "~") printf '%s' "$HOME" ;; "~/"*) printf '%s/%s' "$HOME" "${1#\~/}" ;; *) printf '%s' "$1" ;; esac; }

# GitHub, pointed at the context's host when it is not github.com.
_gh() {
  local host; host="$(jq -r '.github.host // empty' <<<"$CTX")"
  if [ -n "$host" ] && [ "$host" != github.com ]; then GH_HOST="$host" gh "$@"; else gh "$@"; fi
}

# --- scan -------------------------------------------------------------------------------------
_scan() {
  _relaying && { [ "$FORMAT" = text ] || echo '[]'; return 0; }
  local all
  all="$(_T list 2>/dev/null)" || all="[]"
  jq -e 'type == "array"' >/dev/null 2>&1 <<<"$all" || all="[]"
  local found
  found="$(jq -c --arg p "$PREFIX" --arg known "$KNOWN" '
    ($known | split(" ")) as $k
    | [ .[] | select(.status != "closed")
        | ([.labels[]? | select(startswith($p))]) as $req
        | select($req | length > 0)
        | { id, title, status,
            autonomous_safe: ((.labels // []) | index("autonomous-safe") != null),
            requests: [$req[] | select(. as $l | $k | index($l))],
            unknown:  [$req[] | select(. as $l | $k | index($l) | not)],
            notes_tail: ((.notes // "") | if length > 600 then .[-600:] else . end) } ]' <<<"$all")"
  if [ "$FORMAT" = text ]; then
    jq -r '.[] | . as $t
      | ($t.requests[] | "home request \($t.id): \(.) — \($t.title)"),
        ($t.unknown[]  | "home request \($t.id): UNKNOWN label \(.) — reported, not acted on")' <<<"$found"
  else
    printf '%s\n' "$found"
  fi
}

# --- check ------------------------------------------------------------------------------------
# Sets DECISION (the JSON object above). Never reads .notes.
_decide() {
  local id="$1" req="$2"
  local task rc action="" reason="" repo="" branch="" pr="null" failing="[]" pending="[]" auto=false
  task="$(_T get "$id" 2>/dev/null)"; rc=$?
  [ "$rc" -eq 3 ] && { echo "home-requests: no such task $id" >&2; exit 3; }
  [ "$rc" -eq 0 ] || _die "could not read $id"
  jq -e '(.labels // []) | index("autonomous-safe")' >/dev/null <<<"$task" && auto=true

  _out() {
    DECISION="$(jq -cn --arg id "$id" --arg req "$req" --arg action "$action" --arg reason "$reason" \
      --argjson auto "$auto" --arg repo "$repo" --arg branch "$branch" --argjson pr "$pr" \
      --argjson failing "$failing" --argjson pending "$pending" \
      '{id: $id, request: $req, action: $action, reason: $reason, autonomous_safe: $auto,
        repo: $repo, branch: $branch, pr: $pr, failing: $failing, pending: $pending}')"
  }

  case " $KNOWN " in
    *" $req "*) ;;
    *) action=unknown; reason="$req is not a request baton knows — reported, not acted on"; _out; return ;;
  esac
  if ! jq -e --arg l "$req" '(.labels // []) | index($l)' >/dev/null <<<"$task"; then
    action=none; reason="$id has no $req label — nothing to do"; _out; return
  fi
  if [ "$(jq -r .status <<<"$task")" = closed ]; then
    action=resolve; reason="$id is already closed"; _out; return
  fi
  if [ "$req" = home:needs-decision ]; then
    action=report; reason="waiting on a human decision"; _out; return
  fi
  # A review task's PR is somebody else's: never merge it, and never finish it from here.
  if jq -e '(.labels // []) | index("pr-review")' >/dev/null <<<"$task"; then
    action=stop; reason="$id is a pr-review task — the PR belongs to its author, so baton never merges or finishes it on request"; _out; return
  fi

  # The PR: from the registry, never from a note.
  local reg live nlive done_n
  reg="$(_T list-branches "$id" 2>/dev/null)" || reg="[]"
  jq -e 'type == "array"' >/dev/null 2>&1 <<<"$reg" || reg="[]"
  live="$(jq -c '[.[] | select((.status // "open") | IN("merged", "no-change", "abandoned", "reviewed") | not)]' <<<"$reg")"
  nlive="$(jq 'length' <<<"$live")"
  done_n="$(jq '[.[] | select(.status == "merged")] | length' <<<"$reg")"
  if [ "$nlive" -eq 0 ] && [ "$done_n" -gt 0 ] && [ "$req" = home:review-merge ]; then
    action=close; reason="the registry records the branch as merged"
    branch="$(jq -r '[.[] | select(.status == "merged")] | last | .branch' <<<"$reg")"
    repo="$(jq -r '[.[] | select(.status == "merged")] | last | .repo // ""' <<<"$reg")"
    _out; return
  fi
  if [ "$nlive" -eq 0 ]; then
    action=stop; reason="$id has no live branch in its registry, so there is no PR to act on"; _out; return
  fi
  if [ "$nlive" -gt 1 ]; then
    action=stop; reason="$id has $nlive live branches in its registry ($(jq -r 'map(.branch) | join(", ")' <<<"$live")) — which one is meant is a human call"
    _out; return
  fi
  branch="$(jq -r '.[0].branch' <<<"$live")"
  repo="$(jq -r '.[0].repo // "" | sub("/+$"; "") | split("/") | last // ""' <<<"$live")"
  local reg_pr; reg_pr="$(jq -r '.[0].pr // ""' <<<"$live")"
  if [ -z "$repo" ]; then
    action=stop; reason="the registry entry for $branch names no repo"; _out; return
  fi
  local owner; owner="$(jq -r '.github.owner // empty' <<<"$CTX")"
  if [ -z "$owner" ]; then
    action=stop; reason="the context sets no github.owner, so $repo cannot be found on GitHub"; _out; return
  fi
  command -v gh >/dev/null 2>&1 || { action=wait; reason="gh is not installed"; _out; return; }

  local pj err
  err="$(mktemp "${TMPDIR:-/tmp}/baton-home-requests.XXXXXX")"
  pj="$(_gh pr view "$branch" --repo "$owner/$repo" \
          --json number,state,url,isDraft,mergeable,headRefName,headRefOid,statusCheckRollup 2>"$err")" || pj=""
  if [ -z "$pj" ] || ! jq -e 'type == "object"' >/dev/null 2>&1 <<<"$pj"; then
    if grep -qi 'no pull requests found' "$err"; then
      action=stop; reason="branch $branch in $owner/$repo has no PR"
    else
      action=wait; reason="could not ask GitHub about $branch: $(grep . "$err" | tail -1)"
    fi
    rm -f "$err"; _out; return
  fi
  rm -f "$err"
  pr="$(jq -c '{number, url, state, head: .headRefOid}' <<<"$pj")"
  failing="$(jq -c '[.statusCheckRollup[]? | select(.conclusion == "FAILURE" or .conclusion == "ERROR"
                      or .conclusion == "CANCELLED" or .conclusion == "TIMED_OUT"
                      or .state == "FAILURE" or .state == "ERROR") | (.name // .context)]' <<<"$pj")"
  pending="$(jq -c '[.statusCheckRollup[]? | select(.status == "IN_PROGRESS" or .status == "QUEUED"
                      or .status == "PENDING" or .state == "PENDING" or .state == "EXPECTED") | (.name // .context)]' <<<"$pj")"
  local num state
  num="$(jq -r .number <<<"$pj")"; state="$(jq -r .state <<<"$pj")"
  if [ "$(jq -r .headRefName <<<"$pj")" != "$branch" ]; then
    action=stop; reason="GitHub answered with a PR for $(jq -r .headRefName <<<"$pj"), not $branch"; _out; return
  fi
  if [ -n "$reg_pr" ] && [ "${reg_pr##*/}" != "$num" ]; then
    action=stop; reason="the registry records PR $reg_pr for $branch but its PR is #$num"; _out; return
  fi

  if [ "$req" = home:finish ]; then
    case "$state" in
      OPEN|MERGED) action=adopt; reason="PR #$num is $state — set up a worktree and run baton:finish there" ;;
      *) action=stop; reason="PR #$num is $state" ;;
    esac
    _out; return
  fi

  # home:review-merge
  case "$state" in
    MERGED) action=close; reason="PR #$num is already merged"; _out; return ;;
    OPEN) ;;
    *) action=stop; reason="PR #$num is $state"; _out; return ;;
  esac
  if [ "$(jq -r .isDraft <<<"$pj")" = true ]; then action=stop; reason="PR #$num is a draft"; _out; return; fi
  if [ "$(jq 'length' <<<"$failing")" -gt 0 ]; then
    action=stop; reason="PR #$num has failing checks: $(jq -r 'join(", ")' <<<"$failing")"; _out; return
  fi
  if [ "$(jq 'length' <<<"$pending")" -gt 0 ]; then
    action=wait; reason="PR #$num has checks still running: $(jq -r 'join(", ")' <<<"$pending")"; _out; return
  fi
  case "$(jq -r '.mergeable // "UNKNOWN"' <<<"$pj")" in
    CONFLICTING) action=stop; reason="PR #$num conflicts with its base"; _out; return ;;
    MERGEABLE) ;;
    *) action=wait; reason="GitHub has not worked out whether PR #$num can merge yet"; _out; return ;;
  esac
  if [ "$auto" = true ]; then
    action=merge; reason="PR #$num is green and $id is autonomous-safe"
  else
    action=ask; reason="PR #$num is green; $id is not autonomous-safe, so a human says yes first"
  fi
  _out
}

# --- resolve ----------------------------------------------------------------------------------
_resolve() {
  local id="$1" req="$2" outcome="$3" summary="$4" task rc
  task="$(_T get "$id" 2>/dev/null)"; rc=$?
  [ "$rc" -eq 3 ] && { echo "home-requests: no such task $id" >&2; exit 3; }
  [ "$rc" -eq 0 ] || _die "could not read $id"
  if ! jq -e --arg l "$req" '(.labels // []) | index($l)' >/dev/null <<<"$task"; then
    echo "home request $id: no $req label — nothing to resolve"
    return 0
  fi
  # Note first: a run that dies between the two leaves the label, and the procedures are
  # idempotent by state, so the next run repeats nothing that matters.
  _T note "$id" "Home request $req: $outcome — $summary [home-request:$req]" >/dev/null \
    || _die "could not write the resolution note on $id"
  _T label-remove "$id" "$req" >/dev/null || _die "could not remove $req from $id"
  echo "home request $id: $req $outcome — $summary"
}

# --- merge ------------------------------------------------------------------------------------
_merge() {
  local id="$1"
  [ -n "$HEAD" ] || _die "merge needs --head <sha>: the commit that was reviewed"
  _decide "$id" home:review-merge
  local action reason num repo branch head owner
  action="$(jq -r .action <<<"$DECISION")"; reason="$(jq -r .reason <<<"$DECISION")"
  case "$action" in
    merge) ;;
    ask) [ "$CONFIRMED" = yes ] || _refuse "$reason (pass --confirmed once a human has said yes)" ;;
    *) _refuse "$action: $reason" ;;
  esac
  num="$(jq -r .pr.number <<<"$DECISION")"; head="$(jq -r .pr.head <<<"$DECISION")"
  repo="$(jq -r .repo <<<"$DECISION")"; branch="$(jq -r .branch <<<"$DECISION")"
  owner="$(jq -r '.github.owner' <<<"$CTX")"
  [ "$HEAD" = "$head" ] || _refuse "PR #$num's head is now $head, not the reviewed $HEAD — review it again"
  local out
  out="$(_gh pr merge "$num" --repo "$owner/$repo" --merge --match-head-commit "$head" 2>&1)" \
    || _die "gh pr merge #$num failed: $(printf '%s\n' "$out" | grep . | tail -1)"
  echo "home request $id: merged PR #$num ($owner/$repo)"
  _T close "$id" --reason "PR #$num merged on request (home:review-merge)" >/dev/null \
    || echo "home-requests: WARNING merged PR #$num but could not close $id" >&2
  _T update-branch "$id" "$branch" status=merged "pr=$num" >/dev/null 2>&1 || true
  _resolve "$id" home:review-merge done "merged PR #$num at $head and closed the task"
}

# --- adopt ------------------------------------------------------------------------------------
# What baton:start would have done for a branch the cloud built: a worktree on the home machine
# with the identity carrier, so baton:finish can run there. Idempotent: an existing worktree on
# the same branch is reused.
_adopt() {
  local id="$1"
  _decide "$id" home:finish
  local action reason repo branch
  action="$(jq -r .action <<<"$DECISION")"; reason="$(jq -r .reason <<<"$DECISION")"
  [ "$action" = adopt ] || _refuse "$action: $reason"
  repo="$(jq -r .repo <<<"$DECISION")"; branch="$(jq -r .branch <<<"$DECISION")"

  # The member repo's local clone: a member_repos entry (or glob match) whose basename is $repo.
  local pat d path=""
  while IFS= read -r pat; do
    [ -n "$pat" ] || continue
    pat="$(_expand "$pat")"
    for d in $pat; do
      [ "$(basename "$d")" = "$repo" ] && git -C "$d" rev-parse --git-dir >/dev/null 2>&1 && { path="$d"; break 2; }
    done
  done < <(jq -r '.member_repos[]? // empty' <<<"$CTX")
  [ -n "$path" ] || _die "no member repo named $repo is cloned on this machine"

  local root base wt
  root="$(_expand "$(jq -r '.code_root // "~/code"' <<<"$CTX")")"
  base="$(jq -r '.worktree_base // "{code_root}/{repo}-worktrees"' <<<"$CTX")"
  base="${base//\{code_root\}/$root}"; base="${base//\{repo\}/$repo}"
  wt="$(_expand "$base")/$(printf '%s' "$branch" | tr '/' '-')"

  git -C "$path" fetch -q origin "$branch" 2>/dev/null || _die "could not fetch $branch from $repo's origin"
  if [ -d "$wt" ]; then
    [ "$(git -C "$wt" rev-parse --abbrev-ref HEAD 2>/dev/null)" = "$branch" ] \
      || _die "$wt exists and is not a worktree on $branch"
  elif git -C "$path" show-ref -q --verify "refs/heads/$branch"; then
    git -C "$path" worktree add -q "$wt" "$branch" || _die "git worktree add failed"
  else
    git -C "$path" worktree add -q "$wt" -b "$branch" "origin/$branch" || _die "git worktree add failed"
  fi
  local slug="${branch#"$id"-}"; slug="$(printf '%s' "$slug" | tr '/' '-')"
  "$HERE/task-identity.sh" --write-carrier "$wt" --leaf "$id" --slug "$slug" --branch "$branch" >/dev/null \
    || _die "could not write the identity carrier in $wt"
  _T record-branch "$id" --repo "$repo" --branch "$branch" --worktree "$wt" >/dev/null \
    || echo "home-requests: WARNING could not record $branch in $id's registry" >&2
  [ "$(_T get "$id" | jq -r .status)" = in_progress ] || _T claim "$id" >/dev/null 2>&1 || true
  printf '%s\n' "$wt"
}

# --- dispatch ---------------------------------------------------------------------------------
case "$CMD" in
  scan) _scan ;;
  check)
    [ "${#POS[@]}" -eq 2 ] || _die "check needs <id> <request>"
    _decide "${POS[0]}" "${POS[1]}"; printf '%s\n' "$DECISION" ;;
  merge|adopt|resolve)
    _relaying && _die "this session relays its tracker writes, so it is not the home machine"
    case "$CMD" in
      merge) [ "${#POS[@]}" -eq 1 ] || _die "merge needs <id>"; _merge "${POS[0]}" ;;
      adopt) [ "${#POS[@]}" -eq 1 ] || _die "adopt needs <id>"; _adopt "${POS[0]}" ;;
      resolve)
        [ "${#POS[@]}" -eq 2 ] || _die "resolve needs <id> <request>"
        printf '%s' "$OUTCOME" | grep -Eq '^[a-z][a-z-]{0,30}$' || _die "resolve needs --outcome <word> (e.g. done, stopped, declined)"
        [ -n "$SUMMARY" ] || _die "resolve needs --summary <text>"
        case "$SUMMARY" in -*) _die "--summary may not start with '-'" ;; esac
        _resolve "${POS[0]}" "${POS[1]}" "$OUTCOME" "$SUMMARY" ;;
    esac ;;
esac