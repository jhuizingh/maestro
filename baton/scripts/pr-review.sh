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
# Exit status: 0 when the verb answered (a `diverged` or `dirty` sync is an answer). 1 for usage
# errors and for anything that could not be done (no such PR, not a member repo, fetch failed).
#
# Requires: git, jq; gh for resolve.

set -uo pipefail

_die() { echo "pr-review: $*" >&2; exit 1; }
_git() { git -C "$REPO" "$@"; }

VERB="${1:-}"; [ $# -gt 0 ] && shift
case "$VERB" in
  resolve|checkout|sync|match-leaves) ;;
  -h|--help|"") sed -n '2,64p' "$0"; exit 0 ;;
  *) _die "unknown verb '$VERB' (want resolve, checkout, sync or match-leaves)" ;;
esac

TARGET=""; REPO=""; CTX_SRC=""; WT=""; BR=""; PR_JSON_SRC="-"; FETCH=yes; URL=""
while [ $# -gt 0 ]; do
  case "$1" in
    --repo)     REPO="${2:-}";        shift 2 || _die "option '$1' needs a value" ;;
    --context)  CTX_SRC="${2:-}";     shift 2 || _die "option '$1' needs a value" ;;
    --worktree) WT="${2:-}";          shift 2 || _die "option '$1' needs a value" ;;
    --branch)   BR="${2:-}";          shift 2 || _die "option '$1' needs a value" ;;
    --pr-json)  PR_JSON_SRC="${2:-}"; shift 2 || _die "option '$1' needs a value" ;;
    --url)      URL="${2:-}";         shift 2 || _die "option '$1' needs a value" ;;
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

case "$VERB" in
  resolve)      _resolve ;;
  checkout)     _checkout ;;
  sync)         _sync ;;
  match-leaves) _match_leaves ;;
esac
