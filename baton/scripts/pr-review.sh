#!/usr/bin/env bash
# baton — the git and GitHub half of a REVIEW task: a worktree on somebody else's PR.
#
# A review task (`pr-review` label, kind=review in the identity carrier and the branch registry)
# breaks the "one leaf ⇔ one worktree ⇔ one PR I will merge" model in one place: the branch is
# not ours. This script is the ONLY place that knows how such a branch is fetched, checked out,
# kept in step with the author, and matched back to a task. baton:start --review, baton:resume
# and review-state.sh go through it, so a fork PR is handled the same way everywhere.
#
# VERBS
#   resolve <url|number> [--repo <dir>] [--context <file|->]
#       Ask GitHub about the PR and work out which member repo it belongs to. A URL names its own
#       repo; it is matched against the context's member_repos by origin's OWNER/REPO, and a repo
#       that is not a member is an error. A bare number needs --repo. Emits the PR object below.
#   checkout --worktree <path> --branch <b> [--repo <dir>] [--pr-json <file|->]
#       Create the worktree for a PR object (stdin by default) on local branch <b>, tracking the
#       author's head. Reuses <b> if it already exists. Emits {worktree, branch, upstream, head,
#       cross_repo}.
#   sync [--worktree <dir>] [--no-fetch]
#       Fetch the PR head and bring the worktree up to it without ever discarding work of ours.
#       Emits the sync result below.
#   match-leaves --url <url>
#       Read a tracker `list` array on stdin and say which review tasks are for this PR:
#       {open:[ids], closed_latest:id|""}. Pure — no git, no gh, no tracker.
#   rank [--login <user>] [--pr-data <file>] [--timeout <s>] [--jobs <n>]
#       Read a tracker task array on stdin and re-emit it with the review tasks that are waiting
#       on the user first, then those whose state is unknown, then the rest (stable within each),
#       every task carrying review_pending yes|unknown|no and each pr-review task a review_why.
#       The ONE place the "review waiting on you" rule lives: every listing that ranks tasks
#       (fermata, baton:task-list, session-start's status) asks it.
#       --pr-data is an object {<PR url>: <gh pr view --json state,reviewRequests,latestReviews>}
#       standing in for gh (tests, or a caller that already has it). An entry {"error": "<why>"}
#       says the caller could not read that PR. Without --pr-data each open review leaf's PR is
#       read with gh: --timeout seconds per PR (default 10), at most --jobs at once (default 4).
#       A PR that fails or times out makes only its own tasks unknown, never the whole answer.
#       See REVIEW WAITING ON YOU.
#   post [--worktree <dir> | --url <PR url>] [--payload <file|->] [--since <ISO-8601>]
#        [--login <user>] [--dry-run]
#       Post a review from an exact payload, and only against the PR the user decided on. The
#       payload (stdin by default) is {commit_id, event, body, comments:[{path, line, side, body,
#       start_line?, start_side?}]} plus any helper keys of the drafter's own (note, head_moved,
#       drafted_at, ticket_notes, …); only the GitHub fields listed are sent. ticket_notes is
#       baton:resume's: the review's non-blocking notes, handed to hooks.worker.on_review_notes
#       after a successful post — never sent to GitHub. The PR is the review worktree's
#       (branch.<b>.baton-pr, default the current directory), or --url's. See POSTING A REVIEW.
#       Emits the post result below. Posting is outward-facing: run it only after the user has
#       said yes to this exact payload.
#
# WHERE THE AUTHOR'S HEAD LIVES LOCALLY
#   same-repo PR   origin/<head>, the ordinary remote-tracking branch.
#   fork PR        refs/baton/review/<n>, fetched from origin's refs/pull/<n>/head. Pushing to a
#                  fork is out of scope. NOT under refs/remotes/origin/: `git fetch --prune origin`
#                  deletes anything there that origin has no branch for, which would make every
#                  fork review's upstream read [gone] after the next fetch.
#   The local branch tracks it (branch.<b>.remote/merge; remote "." for the fork ref), so @{u},
#   `git status` and branch-prune.sh's [gone] test all work as for any other branch.
#
# BRANCH CONFIG baton keeps for a review branch (removed with the branch by `git branch -D`):
#   branch.<b>.baton-pr      the PR number — so merge-state.sh can be asked with --pr even after
#                            the worktree is gone (branch-prune.sh reads it)
#   branch.<b>.baton-synced  the author's head as of the last checkout/sync. Commits of OURS are
#                            exactly those on HEAD that are on neither @{u} nor this commit; a
#                            force-push leaves the author's old commits on HEAD but on neither
#                            remote ref, and this is what keeps them from reading as ours.
#
# PR object (resolve, and checkout's input):
#   {number, url, title, state, head, base, head_owner, cross_repo, gh_repo, repo_dir, repo_name}
#
# sync result:
#   {result, head, upstream, ours, behind}
#   result: up-to-date | fast-forward | reset | ahead | diverged | dirty | error
#     fast-forward  the author pushed; we had nothing of our own; HEAD moved forward
#     reset         the author force-pushed and we hold no commits of ours: HEAD reset to theirs
#     ahead         we hold commits of ours on top of the author's current head; nothing moved
#     diverged      the author's head moved AND we hold commits of ours: nothing touched — ask
#     dirty         uncommitted changes: nothing touched — ask
#   `ours` counts the commits of ours (see baton-synced above).
#
# post result:
#   {result, pr, commit_id, event, reasons:[…], warnings:[…], stripped:[…], payload, review_url}
#   result: posted | ready (--dry-run, every check passed) | refused | failed (GitHub rejected
#   the POST; reasons holds its message). `payload` is exactly what was (or would be) sent.
#
# POSTING A REVIEW (post). Every check runs and every failure is reported; any one refuses.
#   payload    event is APPROVE, REQUEST_CHANGES or COMMENT; body is not blank; commit_id is a full
#              sha; each comment has a path, a whole-number line, a non-blank body, and side
#              RIGHT (missing → RIGHT). In a review worktree, HEAD is commit_id: the payload is
#              about the code that was reviewed.
#   diff       each comment's line (and start_line) is a RIGHT-side line of that file's diff at
#              the PR's head. A file GitHub gives no patch for (binary, too large) is a warning.
#   PR         it is OPEN, and its head is still commit_id. Its body was not edited since the
#              draft (the author changed it after the user decided: re-queue the decision).
#   activity   nobody but bots and the reviewer commented or reviewed since the draft: issue
#              comments, inline comments and submitted reviews whose time is after --since (or
#              the payload's drafted_at; one of them is required). A bot is a GitHub Bot
#              account, a login ending in [bot], github-actions or Copilot. The reviewer is
#              --login, else `gh api user`.
#   A refusal means: show the user what changed and draft again. Never retry the same payload.
#
# REVIEW WAITING ON YOU (rank). A task's review_pending is yes when, in order:
#   not labelled pr-review                         → no
#   the leaf is closed                             → no  ("leaf closed")
#   no PR URL in its description or notes          → yes ("leaf open, no PR URL")
#   the PR cannot be read                          → unknown ("PR unreadable (<why>)")
#   the PR is not OPEN (merged or closed)          → no  ("PR merged" / "PR closed")
#   the user is in the PR's reviewRequests         → yes ("review requested")
#   the user has a submitted review on the PR      → no  ("review submitted (<STATE>)")
#   otherwise                                      → yes ("no review submitted yet")
#   An open leaf the user started is itself the instruction to review, so a missing URL counts as
#   yes. A PR that cannot be read (gh missing, offline, timed out, deleted) is unknown: it may be
#   waiting, so it must never read as no, and it is not confirmed either. Callers show it as
#   "review status unknown" and keep it beside the waiting ones. A re-request puts the user back in reviewRequests, which wins over an
#   earlier submitted review. Team requests are not resolved to members. An unknown login skips
#   the two user tests and falls through to yes.
#
# Exit status: 0 when the verb answered (a `diverged` or `dirty` sync is an answer). 1 for usage
# errors and for anything that could not be done (no such PR, not a member repo, fetch failed,
# a failed POST). 2 when post refused: nothing was sent.
#
# Requires: git, jq; gh for resolve.

set -uo pipefail

_die() { echo "pr-review: $*" >&2; exit 1; }
_git() { git -C "$REPO" "$@"; }

VERB="${1:-}"; [ $# -gt 0 ] && shift
case "$VERB" in
  resolve|checkout|sync|match-leaves|rank|post) ;;
  -h|--help|"") sed -n '2,/^set -uo pipefail/p' "$0" | sed '$d'; exit 0 ;;
  *) _die "unknown verb '$VERB' (want resolve, checkout, sync, match-leaves, rank or post)" ;;
esac

TARGET=""; REPO=""; CTX_SRC=""; WT=""; BR=""; PR_JSON_SRC="-"; FETCH=yes; URL=""; LOGIN=""; PR_DATA_SRC=""; TIMEOUT=10; JOBS=4
PAYLOAD_SRC="-"; SINCE=""; DRY=no; WT_GIVEN=no
while [ $# -gt 0 ]; do
  case "$1" in
    --repo)     REPO="${2:-}";        shift 2 || _die "option '$1' needs a value" ;;
    --context)  CTX_SRC="${2:-}";     shift 2 || _die "option '$1' needs a value" ;;
    --worktree) WT="${2:-}"; WT_GIVEN=yes; shift 2 || _die "option '$1' needs a value" ;;
    --branch)   BR="${2:-}";          shift 2 || _die "option '$1' needs a value" ;;
    --pr-json)  PR_JSON_SRC="${2:-}"; shift 2 || _die "option '$1' needs a value" ;;
    --url)      URL="${2:-}";         shift 2 || _die "option '$1' needs a value" ;;
    --login)    LOGIN="${2:-}";       shift 2 || _die "option '$1' needs a value" ;;
    --pr-data)  PR_DATA_SRC="${2:-}"; shift 2 || _die "option '$1' needs a value" ;;
    --timeout)  TIMEOUT="${2:-}";     shift 2 || _die "option '$1' needs a value" ;;
    --jobs)     JOBS="${2:-}";        shift 2 || _die "option '$1' needs a value" ;;
    --payload)  PAYLOAD_SRC="${2:-}"; shift 2 || _die "option '$1' needs a value" ;;
    --since)    SINCE="${2:-}";       shift 2 || _die "option '$1' needs a value" ;;
    --dry-run)  DRY=yes; shift ;;
    --no-fetch) FETCH=no; shift ;;
    -*) _die "unknown option '$1'" ;;
    *) [ -z "$TARGET" ] || _die "unexpected argument '$1'"; TARGET="$1"; shift ;;
  esac
done

# remote URL -> OWNER/REPO (github.com) or HOST/OWNER/REPO; "" if not GitHub-shaped. The same
# reduction merge-state.sh applies, so a repo is named the same way by both.
_gh_slug() {
  local u="${1%/}" host path
  u="${u%.git}"
  case "$u" in
    *://*) u="${u#*://}"; u="${u#*@}"; host="${u%%/*}"; path="${u#*/}"; host="${host%%:*}" ;;
    *@*:*) u="${u#*@}";   host="${u%%:*}"; path="${u#*:}" ;;
    *) return 0 ;;
  esac
  case "$path" in */*/*|/*|*/|'') return 0 ;; */*) ;; *) return 0 ;; esac
  if [ "$host" = github.com ]; then printf '%s' "$path"; else printf '%s/%s' "$host" "$path"; fi
}

# A PR URL -> "<gh_repo> <number>", or non-zero. Accepts trailing /files, /commits, #anchors.
_parse_url() {
  local u="$1" host rest owner name num
  u="${u%%#*}"; u="${u%%\?*}"
  case "$u" in https://*|http://*) u="${u#*://}" ;; *) return 1 ;; esac
  host="${u%%/*}"; rest="${u#*/}"
  owner="${rest%%/*}"; rest="${rest#*/}"
  name="${rest%%/*}";  rest="${rest#*/}"
  case "$rest" in pull/*) rest="${rest#pull/}" ;; *) return 1 ;; esac
  num="${rest%%/*}"
  case "$num" in ''|*[!0-9]*) return 1 ;; esac
  [ -n "$owner" ] && [ -n "$name" ] || return 1
  if [ "$host" = github.com ]; then printf '%s/%s %s' "$owner" "$name" "$num"
  else printf '%s/%s/%s %s' "$host" "$owner" "$name" "$num"; fi
}

_context_json() {
  if [ "$CTX_SRC" = "-" ]; then cat
  elif [ -n "$CTX_SRC" ]; then cat "$CTX_SRC" 2>/dev/null
  else "$(dirname "$0")/resolve-context.sh" 2>/dev/null
  fi
}

# Every member repo directory (globs expanded), one per line.
_member_dirs() {
  local ctx="$1" pat p
  while IFS= read -r pat; do
    [ -n "$pat" ] || continue
    pat="${pat/#\~/$HOME}"
    # Word splitting is wanted for the glob, nothing else: member_repos entries are paths.
    for p in $pat; do [ -d "$p" ] && printf '%s\n' "${p%/}"; done
  done < <(jq -r '.member_repos // [] | .[]' <<<"$ctx" 2>/dev/null)
}

# ------------------------------------------------------------------------------------------------
_resolve() {
  [ -n "$TARGET" ] || _die "resolve needs a PR URL or number"
  local gh_repo="" num="" parsed ctx d slug
  if parsed="$(_parse_url "$TARGET")"; then
    gh_repo="${parsed% *}"; num="${parsed#* }"
  else
    case "$TARGET" in '#'*) TARGET="${TARGET#\#}" ;; esac
    case "$TARGET" in ''|*[!0-9]*) _die "'$TARGET' is neither a PR URL nor a PR number" ;; esac
    num="$TARGET"
  fi

  if [ -n "$REPO" ]; then
    [ -d "$REPO" ] || _die "--repo '$REPO' is not a directory"
    slug="$(_gh_slug "$(_git config --get remote.origin.url 2>/dev/null)")"
    if [ -n "$gh_repo" ] && [ "$slug" != "$gh_repo" ]; then
      _die "the PR is in $gh_repo but $REPO's origin is ${slug:-not a GitHub repo}"
    fi
    [ -n "$gh_repo" ] || gh_repo="$slug"
    [ -n "$gh_repo" ] || _die "$REPO's origin is not a GitHub repo, so PR #$num cannot be looked up"
  elif [ -n "$gh_repo" ]; then
    ctx="$(_context_json)"
    jq -e . >/dev/null 2>&1 <<<"$ctx" || _die "no context resolves, so $gh_repo cannot be matched to a member repo"
    while IFS= read -r d; do
      slug="$(git -C "$d" config --get remote.origin.url 2>/dev/null)" || continue
      slug="$(_gh_slug "$slug")"
      # GitHub names are case-insensitive.
      if [ "$(tr 'A-Z' 'a-z' <<<"$slug")" = "$(tr 'A-Z' 'a-z' <<<"$gh_repo")" ]; then REPO="$d"; break; fi
    done < <(_member_dirs "$ctx")
    [ -n "$REPO" ] || _die "$gh_repo is not a member repo of this context (no member repo's origin is it)"
  else
    _die "PR #$num: a bare number needs --repo (from the task's repo-<name> label, or ask)"
  fi

  command -v gh >/dev/null 2>&1 || _die "gh is not installed"
  local out
  out="$( (cd "$REPO" && gh pr view "$num" --repo "$gh_repo" \
            --json number,url,title,state,headRefName,headRepositoryOwner,isCrossRepository,baseRefName) 2>&1 )" \
    || _die "gh could not read PR #$num in $gh_repo: $(head -1 <<<"$out")"
  jq -e . >/dev/null 2>&1 <<<"$out" || _die "gh returned something that is not JSON for PR #$num"
  jq -c --arg gh_repo "$gh_repo" --arg repo_dir "$REPO" --arg repo_name "$(basename "$REPO")" '
    {number: .number, url: .url, title: (.title // ""), state: .state, head: .headRefName, base: .baseRefName,
     head_owner: (.headRepositoryOwner.login // ""), cross_repo: (.isCrossRepository == true),
     gh_repo: $gh_repo, repo_dir: $repo_dir, repo_name: $repo_name}' <<<"$out"
}

# ------------------------------------------------------------------------------------------------
# Fetch the author's head for PR <n>; prints the local ref it lives at.
_fetch_head() { # $1 = number, $2 = head, $3 = cross_repo (true|false)
  local n="$1" head="$2" cross="$3"
  if [ "$cross" = true ]; then
    _git fetch --quiet origin "+refs/pull/$n/head:refs/baton/review/$n" 2>/dev/null \
      || _die "could not fetch refs/pull/$n/head from origin"
    printf 'refs/baton/review/%s' "$n"
  else
    _git fetch --quiet origin "+refs/heads/$head:refs/remotes/origin/$head" 2>/dev/null \
      || _die "could not fetch $head from origin (deleted after merge?)"
    printf 'refs/remotes/origin/%s' "$head"
  fi
}

_checkout() {
  [ -n "$WT" ] || _die "checkout needs --worktree"
  [ -n "$BR" ] || _die "checkout needs --branch"
  local pr n head cross ref sha
  if [ "$PR_JSON_SRC" = "-" ]; then pr="$(cat)"; else pr="$(cat "$PR_JSON_SRC" 2>/dev/null)"; fi
  jq -e '.number and .head' >/dev/null 2>&1 <<<"$pr" || _die "checkout needs a PR object (from resolve) on stdin or --pr-json"
  n="$(jq -r .number <<<"$pr")"; head="$(jq -r .head <<<"$pr")"; cross="$(jq -r '.cross_repo // false' <<<"$pr")"
  [ -n "$REPO" ] || REPO="$(jq -r '.repo_dir // empty' <<<"$pr")"
  [ -n "$REPO" ] && [ -d "$REPO" ] || _die "checkout needs --repo (or a PR object with repo_dir)"
  git check-ref-format "refs/heads/$BR" 2>/dev/null || _die "'$BR' is not a valid branch name"

  ref="$(_fetch_head "$n" "$head" "$cross")" || exit 1
  sha="$(_git rev-parse --verify --quiet "$ref^{commit}")" || _die "fetched $ref but cannot read it"

  if [ -e "$WT" ]; then
    [ "$(git -C "$WT" rev-parse --abbrev-ref HEAD 2>/dev/null)" = "$BR" ] \
      || _die "$WT already exists and is not a worktree on $BR"
  elif _git rev-parse --verify --quiet "refs/heads/$BR" >/dev/null; then
    _git worktree add --quiet "$WT" "$BR" 2>/dev/null || _die "could not add a worktree for existing branch $BR"
  else
    _git worktree add --quiet -b "$BR" "$WT" "$sha" 2>/dev/null || _die "could not create worktree $WT on $BR"
    _git config "branch.$BR.baton-synced" "$sha"
  fi

  if [ "$cross" = true ]; then
    _git config "branch.$BR.remote" .
    _git config "branch.$BR.merge" "$ref"
  else
    _git config "branch.$BR.remote" origin
    _git config "branch.$BR.merge" "refs/heads/$head"
  fi
  _git config "branch.$BR.baton-pr" "$n"

  jq -cn --arg wt "$WT" --arg br "$BR" --arg up "$ref" --arg head "$(git -C "$WT" rev-parse HEAD)" \
    --argjson cross "$cross" \
    '{worktree:$wt, branch:$br, upstream:$up, head:$head, cross_repo:$cross}'
}

# ------------------------------------------------------------------------------------------------
_sync() {
  WT="${WT:-.}"
  REPO="$WT"
  local br n merge remote cross=false head ref up synced ours behind result
  br="$(_git rev-parse --abbrev-ref HEAD 2>/dev/null)" || _die "'$WT' is not a git worktree"
  [ "$br" != HEAD ] || _die "'$WT' is on a detached HEAD"
  n="$(_git config --get "branch.$br.baton-pr" 2>/dev/null || true)"
  [ -n "$n" ] || _die "$br is not a review branch (no branch.$br.baton-pr)"
  remote="$(_git config --get "branch.$br.remote" 2>/dev/null || true)"
  merge="$(_git config --get "branch.$br.merge" 2>/dev/null || true)"
  [ "$remote" = . ] && cross=true
  if [ "$FETCH" = yes ]; then
    head="${merge#refs/heads/}"
    ref="$(_fetch_head "$n" "$head" "$cross")" || {
      jq -cn '{result:"error", head:"", upstream:"", ours:null, behind:null}'; return 0; }
  fi
  up="$(_git rev-parse --verify --quiet '@{u}' 2>/dev/null)" \
    || _die "$br has no upstream — was it created by pr-review.sh checkout?"
  head="$(_git rev-parse HEAD)"
  synced="$(_git config --get "branch.$br.baton-synced" 2>/dev/null || true)"
  [ -n "$synced" ] && ! _git rev-parse --verify --quiet "$synced^{commit}" >/dev/null && synced=""

  # Ours = on HEAD, on neither the author's current head nor where it was when we last synced.
  ours="$(_git rev-list --count HEAD --not "$up" ${synced:+"$synced"} 2>/dev/null)" || ours=""
  behind="$(_git rev-list --count "HEAD..$up" 2>/dev/null)" || behind=""

  if [ -n "$(_git status --porcelain 2>/dev/null)" ]; then
    result=dirty
  elif [ "$head" = "$up" ]; then
    result=up-to-date
  elif _git merge-base --is-ancestor HEAD "$up" 2>/dev/null; then
    _git merge --quiet --ff-only "$up" >/dev/null 2>&1 && result=fast-forward || result=error
  elif [ "$ours" = 0 ]; then
    # A force-push, with nothing of ours to lose: what HEAD has beyond the new head is the
    # author's own rewritten history.
    _git reset --quiet --hard "$up" >/dev/null 2>&1 && result=reset || result=error
  elif [ "$behind" = 0 ]; then
    result=ahead
  else
    result=diverged
  fi
  case "$result" in fast-forward|reset|up-to-date) _git config "branch.$br.baton-synced" "$up" ;; esac

  jq -cn --arg result "$result" --arg head "$(_git rev-parse HEAD)" --arg up "$up" \
    --arg ours "$ours" --arg behind "$behind" \
    '{result:$result, head:$head, upstream:$up,
      ours:($ours | if . == "" then null else tonumber end),
      behind:($behind | if . == "" then null else tonumber end)}'
}

# ------------------------------------------------------------------------------------------------
_match_leaves() {
  [ -n "$URL" ] || _die "match-leaves needs --url"
  local u="${URL%/}"
  # The description holds the URL; match it as a whole token so .../pull/12 never matches
  # .../pull/123. Only tasks labelled pr-review count.
  jq -c --arg u "$u" '
    def esc: gsub("(?<c>[.?*+^$|()\\[\\]{}\\\\])"; "\\\(.c)");
    ($u | esc) as $re
    | [ .[]? | select((.labels // []) | index("pr-review"))
             | select(((.description // "") + " " + (.notes // "")) | test($re + "(?![0-9])")) ]
    | { open: [ .[] | select(.status != "closed") | .id ],
        closed_latest: ([ .[] | select(.status == "closed") ] | sort_by(.closed_at // "") | last | .id // "") }'
}

# ------------------------------------------------------------------------------------------------
# The first PR URL in a task's description, then notes — what match-leaves keys on too.
_JQ_PR_URL='def pr_url: [ ((.description // "") + "\n" + (.notes // ""))
                       | scan("https?://[^\\s/]+/[^\\s/]+/[^\\s/]+/pull/[0-9]+") ] | first // "";'

# Run "$@" for at most $1 seconds (portable: no coreutils timeout on macOS). Touches $2 when it
# had to kill the command. The watchdog's fds go to /dev/null so it never holds a caller's pipe.
_timed() {
  local s="$1" mark="$2" p w rc; shift 2
  "$@" & p=$!
  ( sleep "$s"; touch "$mark"; kill -TERM "$p" ) >/dev/null 2>&1 </dev/null & w=$!
  wait "$p"; rc=$?
  kill "$w" 2>/dev/null; wait "$w" 2>/dev/null
  return "$rc"
}

# Read one PR into $2.json, or say why not in $2.why.
_rank_fetch() { # $1 = url, $2 = path prefix
  if _timed "$TIMEOUT" "$2.to" gh pr view "$1" --json state,reviewRequests,latestReviews >"$2.json" 2>"$2.err" \
     && jq -e 'type == "object"' >/dev/null 2>&1 <"$2.json"; then return 0; fi
  if [ -e "$2.to" ]; then echo "timed out after ${TIMEOUT}s" >"$2.why"
  elif [ -s "$2.err" ]; then head -1 "$2.err" >"$2.why"
  else echo "gh returned no PR" >"$2.why"; fi
}

_rank() {
  local tasks data url i dir urls=()
  tasks="$(cat)"
  jq -e 'type == "array"' >/dev/null 2>&1 <<<"$tasks" || _die "rank needs a tracker task array on stdin"
  case "$TIMEOUT" in ''|*[!0-9]*|0) _die "--timeout needs a whole number of seconds" ;; esac
  case "$JOBS" in ''|*[!0-9]*|0) _die "--jobs needs a positive whole number" ;; esac
  if [ -n "$PR_DATA_SRC" ]; then
    if [ "$PR_DATA_SRC" = "-" ]; then data="$(cat)"; else data="$(cat "$PR_DATA_SRC" 2>/dev/null)"; fi
    jq -e 'type == "object"' >/dev/null 2>&1 <<<"$data" || _die "--pr-data must be a JSON object keyed by PR URL"
  else
    # Only open review leaves need GitHub; each PR is read once.
    while IFS= read -r url; do [ -n "$url" ] && urls+=("$url"); done < <(jq -r "$_JQ_PR_URL"'
      [ .[] | select((.labels // []) | index("pr-review")) | select(.status != "closed") | pr_url ]
      | unique | .[]' <<<"$tasks")
    data='{}'
    if [ "${#urls[@]}" -gt 0 ] && ! command -v gh >/dev/null 2>&1; then
      data="$(printf '%s\n' "${urls[@]}" | jq -Rn '[inputs | {(.): {error: "gh not installed"}}] | add')"
    elif [ "${#urls[@]}" -gt 0 ]; then
      dir="$(mktemp -d)" || _die "cannot make a temporary directory"
      # In batches of --jobs: `wait -n` is not in the bash 3.2 macOS ships.
      for i in "${!urls[@]}"; do
        _rank_fetch "${urls[$i]}" "$dir/$i" &
        [ $(( (i + 1) % JOBS )) -ne 0 ] || wait
      done
      wait
      for i in "${!urls[@]}"; do
        if [ -e "$dir/$i.why" ]; then
          data="$(jq -c --arg u "${urls[$i]}" --arg why "$(cat "$dir/$i.why")" '. + {($u): {error: $why}}' <<<"$data")"
        else
          data="$(jq -c --arg u "${urls[$i]}" --slurpfile p "$dir/$i.json" '. + {($u): $p[0]}' <<<"$data")"
        fi
      done
      rm -rf "$dir"
      if [ -z "$LOGIN" ] && jq -e 'any(.[]; has("error") | not)' >/dev/null <<<"$data"; then
        LOGIN="$(_timed "$TIMEOUT" /dev/null gh api user --jq .login 2>/dev/null || true)"
      fi
    fi
  fi
  jq -c --argjson data "$data" --arg me "$LOGIN" "$_JQ_PR_URL"'
    def lc: ascii_downcase;
    def verdict:
      if ((.labels // []) | index("pr-review")) == null then null
      elif .status == "closed" then ["no", "leaf closed"]
      else pr_url as $u
        | if $u == "" then ["yes", "leaf open, no PR URL"]
          else $data[$u] as $p
          | if $p == null then ["unknown", "PR unreadable (no data for it)"]
            elif ($p | has("error")) then ["unknown", "PR unreadable (\($p.error))"]
            elif ($p.state // "") != "OPEN" then ["no", "PR \(($p.state // "unknown") | lc)"]
            elif $me != "" and any($p.reviewRequests[]?; (.login // "" | lc) == ($me | lc))
              then ["yes", "review requested"]
            elif $me != "" and any($p.latestReviews[]?;
                   (.author.login // "" | lc) == ($me | lc) and (.state // "") != "PENDING")
              then ["no", "review submitted (\([ $p.latestReviews[]
                     | select((.author.login // "" | lc) == ($me | lc)) | .state ] | last))"]
            else ["yes", "no review submitted yet"]
            end
          end
      end;
    map(verdict as $v
        | if $v == null then . + {review_pending: "no"}
          else . + {review_pending: $v[0], review_why: $v[1]} end)
    | sort_by({yes: 0, unknown: 1}[.review_pending] // 2)' <<<"$tasks"
}

# ------------------------------------------------------------------------------------------------
# post. Each check appends to the reasons array; nothing is sent unless it stays empty.
_JQ_TIME='def t: sub("\\.[0-9]+Z$"; "Z") | fromdateiso8601;'

# The payload's own problems, before GitHub is asked anything: [reasons].
_JQ_PAYLOAD_CHECKS='
  def blank: (. // "") | tostring | test("^\\s*$");
  [ (if (.event // "") | IN("APPROVE", "REQUEST_CHANGES", "COMMENT") | not
       then "event must be APPROVE, REQUEST_CHANGES or COMMENT (got \(.event // "none"))" else empty end),
    (if .body | blank then "the review body is empty" else empty end),
    (if (.commit_id // "") | test("^[0-9a-f]{40}$") | not
       then "commit_id must be the full sha of the reviewed head (got \(.commit_id // "none"))" else empty end),
    (if (.comments // []) | type != "array" then "comments must be an array" else empty end),
    ( (.comments // []) | if type == "array" then . else [] end | to_entries[] | .key as $i | .value
      | (if (.path // "") | blank then "comment \($i + 1): no path" else empty end),
        (if (.line | type) != "number" or .line != (.line | floor) or .line < 1
           then "comment \($i + 1): line must be a positive whole number" else empty end),
        (if .body | blank then "comment \($i + 1) (\(.path // "?"):\(.line // "?")): empty body" else empty end),
        (if (.side // "RIGHT") != "RIGHT" then "comment \($i + 1) (\(.path // "?"):\(.line // "?")): side must be RIGHT" else empty end),
        (if .start_line != null and ((.start_side // "RIGHT") != "RIGHT" or (.start_line | type) != "number" or .start_line >= (.line // 0))
           then "comment \($i + 1) (\(.path // "?"):\(.line // "?")): start_line must be a RIGHT-side line before line" else empty end) ) ]'

# What is sent: the GitHub fields only, every comment on the RIGHT side.
_JQ_CLEAN='{commit_id, event, body,
            comments: [ (.comments // [])[]
                        | {path, line, side: "RIGHT", body}
                          + (if .start_line != null then {start_line, start_side: "RIGHT"} else {} end) ]}'

_post() {
  local payload since gh_repo="" n="" parsed slug br wt_head reasons warnings clean stripped login
  local q pr out files since_items resp
  command -v gh >/dev/null 2>&1 || _die "gh is not installed"
  if [ "$PAYLOAD_SRC" = "-" ]; then payload="$(cat)"; else payload="$(cat "$PAYLOAD_SRC" 2>/dev/null)"; fi
  jq -e 'type == "object"' >/dev/null 2>&1 <<<"$payload" || _die "post needs a JSON payload object on stdin or --payload"

  since="${SINCE:-$(jq -r '.drafted_at // empty' <<<"$payload")}"
  [ -n "$since" ] || _die "post needs the draft's time: --since <ISO-8601> or drafted_at in the payload"
  jq -en --arg s "$since" "$_JQ_TIME"' $s | t' >/dev/null 2>&1 \
    || _die "'$since' is not a UTC ISO-8601 time (YYYY-MM-DDTHH:MM:SSZ)"

  # Which PR.
  if [ -n "$URL" ]; then
    parsed="$(_parse_url "$URL")" || _die "'$URL' is not a PR URL"
    gh_repo="${parsed% *}"; n="${parsed#* }"
  fi
  if [ -z "$URL" ] || [ "$WT_GIVEN" = yes ]; then
    WT="${WT:-.}"; REPO="$WT"
    br="$(_git rev-parse --abbrev-ref HEAD 2>/dev/null)" || _die "'$WT' is not a git worktree"
    slug="$(_gh_slug "$(_git config --get remote.origin.url 2>/dev/null)")"
    local wn; wn="$(_git config --get "branch.$br.baton-pr" 2>/dev/null || true)"
    if [ -z "$URL" ]; then
      [ -n "$wn" ] || _die "$br is not a review branch (no branch.$br.baton-pr); pass --url"
      [ -n "$slug" ] || _die "$WT's origin is not a GitHub repo"
      gh_repo="$slug"; n="$wn"
    elif [ -n "$wn" ] && { [ "$wn" != "$n" ] || [ "$(tr 'A-Z' 'a-z' <<<"$slug")" != "$(tr 'A-Z' 'a-z' <<<"$gh_repo")" ]; }; then
      _die "--url names $gh_repo#$n but $WT reviews ${slug:-?}#$wn"
    fi
    wt_head="$(_git rev-parse HEAD)"
  fi
  case "$gh_repo" in */*/*) _die "post supports github.com PRs only (got $gh_repo)" ;; esac

  reasons="$(jq -c "$_JQ_PAYLOAD_CHECKS" <<<"$payload")"
  _reason() { reasons="$(jq -c --arg r "$1" '. + [$r]' <<<"$reasons")"; }
  warnings='[]'
  [ -z "${wt_head:-}" ] || [ "$wt_head" = "$(jq -r '.commit_id // ""' <<<"$payload")" ] \
    || _reason "the payload's commit_id is not the reviewed worktree's HEAD (${wt_head:0:9}): it is not about the code that was reviewed"
  clean="$(jq -c "$_JQ_CLEAN" <<<"$payload")"
  stripped="$(jq -c '[keys[] | select(IN("commit_id", "event", "body", "comments") | not)]' <<<"$payload")"

  # The PR as it is now: state, head, and when its body was last edited.
  q='query($owner:String!,$name:String!,$number:Int!){repository(owner:$owner,name:$name){pullRequest(number:$number){url state headRefOid lastEditedAt editor{login}}}}'
  out="$(gh api graphql -f query="$q" -F owner="${gh_repo%/*}" -F name="${gh_repo#*/}" -F number="$n" 2>&1)" \
    || _die "gh could not read PR #$n in $gh_repo: $(head -1 <<<"$out")"
  pr="$(jq -c '.data.repository.pullRequest // empty' <<<"$out" 2>/dev/null)"
  [ -n "$pr" ] || _die "no PR #$n in $gh_repo"

  login="$LOGIN"
  [ -n "$login" ] || login="$(gh api user --jq .login 2>/dev/null || true)"
  [ -n "$login" ] || warnings="$(jq -c '. + ["could not learn your GitHub login: your own comments count as activity"]' <<<"$warnings")"

  # Activity since the draft. `since` filters issue/inline comments by update time; reviews by
  # submission time, filtered here.
  _ghlist() { # $1 = path; prints one merged array, or fails
    local o; o="$(gh api --paginate "$1" 2>/dev/null)" || return 1
    jq -sc 'add // []' <<<"$o" 2>/dev/null
  }
  local ic rc rv
  ic="$(_ghlist "repos/$gh_repo/issues/$n/comments?per_page=100&since=$since")" || _die "gh could not list PR #$n's comments"
  rc="$(_ghlist "repos/$gh_repo/pulls/$n/comments?per_page=100&since=$since")" || _die "gh could not list PR #$n's inline comments"
  rv="$(_ghlist "repos/$gh_repo/pulls/$n/reviews?per_page=100")" || _die "gh could not list PR #$n's reviews"
  files="$(_ghlist "repos/$gh_repo/pulls/$n/files?per_page=100")" || _die "gh could not list PR #$n's files"

  reasons="$(jq -c --argjson pr "$pr" --arg since "$since" --arg me "$login" \
      --argjson ic "$ic" --argjson rc "$rc" --argjson rv "$rv" --argjson p "$payload" "$_JQ_TIME"'
    def lc: ascii_downcase;
    def bot: (.user.type // "") == "Bot" or ((.user.login // "") | lc | (endswith("[bot]") or IN("github-actions", "copilot")));
    def mine: $me != "" and ((.user.login // "") | lc) == ($me | lc);
    def after($k): (.[$k] // "") as $v | $v != "" and ($v | t) > ($since | t);
    . + [ (if $pr.state != "OPEN" then "the PR is \($pr.state | lc), not open" else empty end),
          (if ($p.commit_id // "") != $pr.headRefOid
             then "the PR head moved: it is \($pr.headRefOid[0:9]) now, the payload reviewed \(($p.commit_id // "?")[0:9])" else empty end),
          (if ($pr.lastEditedAt // "") != "" and ($pr.lastEditedAt | t) > ($since | t)
             then "the PR description was edited at \($pr.lastEditedAt) by \($pr.editor.login // "someone"), after the draft" else empty end),
          ( [ ($ic[] | select(after("updated_at")) | {who: .user.login, what: "commented", at: .updated_at, o: .}),
              ($rc[] | select(after("updated_at")) | {who: .user.login, what: "commented inline", at: .updated_at, o: .}),
              ($rv[] | select((.state // "") != "PENDING" and after("submitted_at"))
                     | {who: .user.login, what: "reviewed (\(.state))", at: .submitted_at, o: .}) ]
            | map(select(.o | (bot or mine) | not)) | sort_by(.at)[]
            | "\(.who // "someone") \(.what) at \(.at), after the draft" ) ]' <<<"$reasons")"

  # Inline comments against the diff at the head.
  local lines
  lines="$(jq -c --argjson files "$files" '
    def right_lines:
      reduce (split("\n")[]) as $l ({n: null, s: []};
        if ($l | startswith("@@")) then .n = ($l | capture("\\+(?<c>[0-9]+)") | .c | tonumber)
        elif .n == null or ($l | startswith("-")) or ($l | startswith("\\")) then .
        else .s += [.n] | .n += 1 end) | .s;
    ($files | map({key: .filename, value: (if .patch then (.patch | right_lines) else null end)}) | from_entries) as $d
    | [ (.comments // []) | if type == "array" then . else [] end | to_entries[] | .key as $i | .value
        | select((.line | type) == "number" and (.path | type) == "string") | .path as $f
        | if ($d | has($f)) | not then {r: "comment \($i + 1): \(.path) is not in the PR diff"}
          elif $d[$f] == null then {w: "comment \($i + 1): GitHub gave no diff for \(.path), so its line \(.line) is unchecked"}
          else ([.line, (.start_line // empty)] - $d[$f]) as $miss
            | if $miss == [] then empty
              else {r: "comment \($i + 1): \(.path):\($miss | map(tostring) | join(",")) is not a RIGHT-side diff line at the head"} end
          end ]' <<<"$payload")"
  reasons="$(jq -c --argjson l "$lines" '. + [$l[] | .r // empty]' <<<"$reasons")"
  warnings="$(jq -c --argjson l "$lines" '. + [$l[] | .w // empty]' <<<"$warnings")"

  _emit() { # $1 = result, $2 = review url
    jq -cn --arg result "$1" --arg pr "$(jq -r .url <<<"$pr")" --argjson clean "$clean" \
      --argjson reasons "$reasons" --argjson warnings "$warnings" --argjson stripped "$stripped" --arg url "${2:-}" \
      '{result: $result, pr: $pr, commit_id: $clean.commit_id, event: $clean.event, reasons: $reasons,
        warnings: $warnings, stripped: $stripped, payload: $clean, review_url: $url}'
  }
  # Fail closed: a check that could not run (bad JSON from gh, a jq error) must never read as
  # "no reasons".
  local v
  for v in "$reasons" "$warnings" "$clean" "$stripped"; do
    jq -e 'type == "array" or type == "object"' >/dev/null 2>&1 <<<"$v" \
      || _die "a check could not be completed, so nothing was posted"
  done
  if ! jq -e 'length == 0' >/dev/null <<<"$reasons"; then _emit refused; exit 2; fi
  if [ "$DRY" = yes ]; then _emit ready; return 0; fi

  if resp="$(gh api --method POST "repos/$gh_repo/pulls/$n/reviews" --input - <<<"$clean" 2>&1)" \
     && jq -e '.id' >/dev/null 2>&1 <<<"$resp"; then
    _emit posted "$(jq -r '.html_url // ""' <<<"$resp")"
  else
    reasons="$(jq -c --arg r "GitHub rejected the review: $(head -c 400 <<<"$resp" | tr '\n' ' ')" '. + [$r]' <<<"$reasons")"
    _emit failed; exit 1
  fi
}

case "$VERB" in
  post)         _post ;;
  rank)         _rank ;;
  resolve)      _resolve ;;
  checkout)     _checkout ;;
  sync)         _sync ;;
  match-leaves) _match_leaves ;;
esac
