#!/usr/bin/env bash
# baton — answer the two questions a REVIEW task's close-out and cleanup need, for one worktree:
#
#   1. What is OUR latest effective review on this PR?          (informational only)
#   2. Does this worktree hold work of ours that exists nowhere else?   (the one safeguard)
#
# It sits beside merge-state.sh and is shaped like it: every caller (baton:finish, baton:status,
# baton:cleanup-worktrees) asks the same script, gh being unavailable is a degraded answer and
# never a failure, and nothing here decides anything — cleanup-verdict.sh does that.
#
# EFFECTIVE REVIEW STATE. Our latest review that is APPROVED, CHANGES_REQUESTED or DISMISSED; a
# later plain COMMENTED review does not revoke an approval, which matches GitHub's own
# reviewDecision. Approved-then-changes-requested is CHANGES_REQUESTED; a dismissed approval reads
# DISMISSED (GitHub rewrites the review's own state). Only COMMENTED reviews → COMMENTED. None →
# NONE. Unsubmitted (PENDING) reviews are ignored. When the login or the reviews cannot be read
# the answer is `unknown` — and because closing a review never depends on it, `unknown` blocks
# nothing.
#
# UNPUSHED. Commits on HEAD that are on neither the author's head (@{u}) nor the author's head as
# of our last sync (branch.<b>.baton-synced, kept by pr-review.sh). The second term is what keeps
# a force-push from making the author's OLD commits look like ours. No upstream at all → unknown.
#
# Usage:
#   review-state.sh [--repo <worktree>] [--pr <n> --gh-repo <OWNER/REPO> | --pr-url <url>]
#                   [--login <user>] [--reviews-json <file|->] [--format json|env]
#
#   --repo          the worktree; default cwd
#   --pr/--gh-repo  which PR to read reviews for; or --pr-url, which names both
#   --login         whose reviews count; default `gh api user`
#   --reviews-json  the PR's reviews array instead of asking GitHub (tests, or a caller that
#                   already has it)
#   With neither --pr nor --pr-url nor --reviews-json, review_state is `unknown`.
#
# JSON fields / env vars:
#   review_state   REVIEW_STATE   APPROVED | CHANGES_REQUESTED | DISMISSED | COMMENTED | NONE | unknown
#   review_login   REVIEW_LOGIN   whose reviews were read ("" if unknown)
#   unpushed       UNPUSHED       yes | no | unknown
#   unpushed_count UNPUSHED_COUNT number, or "" when unknown
#   upstream       UPSTREAM       the ref @{u} resolves to ("" if none)
#   dirty          REVIEW_DIRTY   yes | no | unknown
#
# Exit status: 0 whenever it answered, including with `unknown`s. 1 only for a usage error.
#
# Requires: git, jq; gh unless --reviews-json and --login are both given.

set -uo pipefail

_die() { echo "review-state: $*" >&2; exit 1; }

REPO="."; PR=""; GH_REPO_ARG=""; PR_URL=""; LOGIN=""; REVIEWS_SRC=""; FORMAT=json
while [ $# -gt 0 ]; do
  case "$1" in
    --repo)         REPO="${2:-}";        shift 2 || _die "option '$1' needs a value" ;;
    --pr)           PR="${2:-}";          shift 2 || _die "option '$1' needs a value" ;;
    --gh-repo)      GH_REPO_ARG="${2:-}"; shift 2 || _die "option '$1' needs a value" ;;
    --pr-url)       PR_URL="${2:-}";      shift 2 || _die "option '$1' needs a value" ;;
    --login)        LOGIN="${2:-}";       shift 2 || _die "option '$1' needs a value" ;;
    --reviews-json) REVIEWS_SRC="${2:-}"; shift 2 || _die "option '$1' needs a value" ;;
    --format)       FORMAT="${2:-json}";  shift 2 || _die "option '$1' needs a value" ;;
    -h|--help)      sed -n '2,45p' "$0"; exit 0 ;;
    *) _die "unknown argument '$1'" ;;
  esac
done
case "$FORMAT" in json|env) ;; *) _die "unknown --format '$FORMAT' (want json or env)" ;; esac
case "$PR" in ''|*[!0-9]*) [ -z "$PR" ] || _die "--pr must be a number (got '$PR')" ;; esac
[ -d "$REPO" ] || _die "--repo '$REPO' is not a directory"
_git() { git -C "$REPO" "$@"; }

if [ -n "$PR_URL" ]; then
  # https://<host>/<owner>/<repo>/pull/<n>[/…] — the same shape pr-review.sh resolves.
  _u="${PR_URL#*://}"; _host="${_u%%/*}"; _rest="${_u#*/}"
  _o="${_rest%%/*}"; _rest="${_rest#*/}"; _r="${_rest%%/*}"; _rest="${_rest#*/}"
  _n="${_rest#pull/}"; _n="${_n%%[/#?]*}"
  case "$_rest" in pull/*) ;; *) _die "--pr-url '$PR_URL' is not a PR URL" ;; esac
  case "$_n" in ''|*[!0-9]*) _die "--pr-url '$PR_URL' is not a PR URL" ;; esac
  PR="$_n"
  if [ "$_host" = github.com ]; then GH_REPO_ARG="$_o/$_r"; else GH_REPO_ARG="$_host/$_o/$_r"; fi
fi

# --- 2. work of ours ----------------------------------------------------------------------------
UPSTREAM=""; UNPUSHED=unknown; UNPUSHED_COUNT=""; DIRTY=unknown
if _git rev-parse --git-dir >/dev/null 2>&1; then
  if _git status --porcelain >/dev/null 2>&1; then
    [ -z "$(_git status --porcelain 2>/dev/null)" ] && DIRTY=no || DIRTY=yes
  fi
  UP_SHA="$(_git rev-parse --verify --quiet '@{u}' 2>/dev/null || true)"
  if [ -n "$UP_SHA" ]; then
    UPSTREAM="$(_git rev-parse --symbolic-full-name '@{u}' 2>/dev/null || true)"
    BR="$(_git rev-parse --abbrev-ref HEAD 2>/dev/null || true)"
    SYNCED="$(_git config --get "branch.$BR.baton-synced" 2>/dev/null || true)"
    [ -n "$SYNCED" ] && ! _git rev-parse --verify --quiet "$SYNCED^{commit}" >/dev/null && SYNCED=""
    UNPUSHED_COUNT="$(_git rev-list --count HEAD --not "$UP_SHA" ${SYNCED:+"$SYNCED"} 2>/dev/null || true)"
    case "$UNPUSHED_COUNT" in
      0) UNPUSHED=no ;;
      ''|*[!0-9]*) UNPUSHED_COUNT=""; UNPUSHED=unknown ;;
      *) UNPUSHED=yes ;;
    esac
  fi
fi

# --- 1. our effective review ---------------------------------------------------------------------
REVIEW_STATE=unknown
REVIEWS=""
if [ -n "$REVIEWS_SRC" ]; then
  if [ "$REVIEWS_SRC" = "-" ]; then REVIEWS="$(cat)"; else REVIEWS="$(cat "$REVIEWS_SRC" 2>/dev/null || true)"; fi
elif [ -n "$PR" ] && [ -n "$GH_REPO_ARG" ] && command -v gh >/dev/null 2>&1; then
  # --paginate emits one array per page; slurp and concatenate them.
  REVIEWS="$( (cd "$REPO" && gh api --paginate "repos/$GH_REPO_ARG/pulls/$PR/reviews") 2>/dev/null \
              | jq -cs 'add // []' 2>/dev/null || true)"
fi
if [ -z "$LOGIN" ] && [ -n "$REVIEWS" ] && command -v gh >/dev/null 2>&1; then
  LOGIN="$(gh api user --jq .login 2>/dev/null || true)"
fi
if [ -n "$REVIEWS" ] && [ -n "$LOGIN" ] && jq -e 'type == "array"' >/dev/null 2>&1 <<<"$REVIEWS"; then
  REVIEW_STATE="$(jq -r --arg me "$LOGIN" '
    [ .[] | select((.user.login // "" | ascii_downcase) == ($me | ascii_downcase)) ]
    | to_entries | sort_by([(.value.submitted_at // ""), .key]) | map(.value) as $mine
    | ([ $mine[] | select(.state == "APPROVED" or .state == "CHANGES_REQUESTED" or .state == "DISMISSED") ]
       | last | .state)
      // (if any($mine[]; .state == "COMMENTED") then "COMMENTED" else "NONE" end)' <<<"$REVIEWS" 2>/dev/null || true)"
  [ -n "$REVIEW_STATE" ] || REVIEW_STATE=unknown
fi

case "$FORMAT" in
  json)
    jq -n --arg rs "$REVIEW_STATE" --arg login "$LOGIN" --arg unpushed "$UNPUSHED" \
      --arg count "$UNPUSHED_COUNT" --arg up "$UPSTREAM" --arg dirty "$DIRTY" \
      '{review_state:$rs, review_login:$login, unpushed:$unpushed,
        unpushed_count:($count | if . == "" then null else tonumber end),
        upstream:$up, dirty:$dirty}'
    ;;
  env)
    _q() { printf "'%s'" "$(printf '%s' "$1" | sed "s/'/'\\\\''/g")"; }
    printf 'export REVIEW_STATE=%s\n'   "$(_q "$REVIEW_STATE")"
    printf 'export REVIEW_LOGIN=%s\n'   "$(_q "$LOGIN")"
    printf 'export UNPUSHED=%s\n'       "$(_q "$UNPUSHED")"
    printf 'export UNPUSHED_COUNT=%s\n' "$(_q "$UNPUSHED_COUNT")"
    printf 'export UPSTREAM=%s\n'       "$(_q "$UPSTREAM")"
    printf 'export REVIEW_DIRTY=%s\n'   "$(_q "$DIRTY")"
    ;;
esac
