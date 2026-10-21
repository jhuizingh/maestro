#!/usr/bin/env bash
# baton — answer "has this branch landed?" for one branch.
#
# This is the ONLY place the merge-state ladder is derived, so every skill that asks the
# question (baton:resume on wake-up, baton:finish Step 7, baton:cleanup-worktrees Step 3)
# gets the same answer from the same evidence, and a fix to the ladder lands once.
#
# THE LADDER, in order:
#   1. `gh pr view <branch>` — GitHub is the authority. `git branch --merged` false-negatives
#      on squash and rebase merges (both put a NEW commit on the base that is not an ancestor
#      of the feature branch), so a squash-merged branch looks unmerged to ancestry forever.
#   2. git ancestry against the base — the fallback when there is no PR at all (in-place work
#      mode, a direct push, or a branch that was never PR'd), and when gh is missing, broken,
#      or unauthenticated. Never fatal: a machine without gh still gets an answer. This rung is
#      ancestry PLUS a first-parent test, because plain `git branch --merged` calls a
#      never-committed-to branch merged — see the long note at the rung itself.
# `signal` reports which rung actually decided it, so a caller can tell the user what it knows.
#
# HAS_WORK IS SEPARATE FROM MERGED, and the distinction is the point. An empty `base..branch`
# range means either "already merged" or "nothing outstanding on this branch" — opposite
# situations wanting opposite responses, and the reason a caller must never read the range itself
# as a verdict. Read `merged` first; consult `has_work` only to describe an UNMERGED branch.
#
# `merged=no` + `has_work=no` means NOTHING IS OUTSTANDING — which is three different situations
# this script cannot tell apart and does not try:
#   (a) nothing has been done here yet (a fresh worktree)
#   (b) the work landed by fast-forward or a direct push with no PR (see the known limit below)
#   (c) the work was real but produced no commits at all — done against a live system through an
#       API, say — so no merge is ever coming
# Only intent separates them, and intent is not in git. baton records it as a label the finishing
# session applies (`no-pr-needed`) and `scripts/cleanup-verdict.sh` consumes, guarded by this
# script's own `has_work=no`. Deriving the exception here instead would erode the bias below for
# every caller without anyone deciding to; assert it per task, never infer it.
#
# Usage:
#   merge-state.sh [--branch <br>] [--pr <number>] [--repo <dir>] [--base <ref>] [--no-fetch]
#                  [--checks] [--format json|env]
#
# Options:
#   --branch <br>   branch to ask about; default = current branch of --repo
#   --pr <number>   ask GitHub about this PR rather than "the PR whose head is <br>". For a branch
#                   whose local name is not the PR's head name — a review branch
#                   (`review/<n>-<head>`), above all on a fork PR — the by-name lookup finds no
#                   PR and the ladder falls to ancestry. Ancestry still uses --branch.
#   --repo <dir>    repo/worktree to run git in; default = cwd
#   --base <ref>    base to compare against; default = the PR's own base, else origin's default
#                   branch (origin/HEAD, else asked of origin), else origin/main or origin/master
#   --no-fetch      skip `git fetch origin` (caller already fetched)
#   --checks        also report the PR's failing/pending check runs (costs nothing extra —
#                   same gh call). Meaningless without an open PR; empty otherwise.
#   --format        json (default), or `export K='V'` lines for `eval`
#
# JSON fields / env vars:
#   merged      MERGED         yes | no
#   signal      MERGE_SIGNAL   pr | ancestry | none   (which rung answered; none = neither could)
#   gh          GH_STATUS      ok | no-pr | unavailable | error   (why the PR rung did/didn't answer)
#   base        MERGE_BASE     the ref ancestry was compared against ("" if none resolved)
#   base_from   MERGE_BASE_FROM  how that base was chosen: flag | pr | origin-head | remote-head
#                              | guess (origin/main or origin/master, nothing better) | ""
#   has_work    HAS_WORK       yes | no | unknown     (commits in base..branch; `no` is exactly
#                                                      "the tip is already an ancestor of base",
#                                                      i.e. nothing outstanding — see above)
#   pr_repo     PR_REPO        the GitHub repo the PR rung asked ("" = left to gh's own guess,
#                              because no remote is GitHub-shaped). Deliberately not GH_REPO:
#                              gh reads that variable, so a caller's `eval` would redirect
#                              every later gh call in its shell.
#   pr_state    PR_STATE       MERGED | OPEN | CLOSED | ""
#   pr_number   PR_NUMBER      number, or "" when there is no PR
#   failing     FAILING        newline-separated check names (--checks only)
#   pending     PENDING        newline-separated check names (--checks only)
#
# Exit status: 0 whenever a question was asked, INCLUDING when the answer is "no" or the
# evidence was weak — gh being absent or logged out is a fallback, not a failure. Non-zero
# only for a usage error or a non-git directory.
#
# Requires: git, jq. Uses gh when present.

set -uo pipefail   # deliberately NOT -e: probing gh/git for absence is normal control flow

FORMAT=json
BR=""; REPO="."; BASE=""; FETCH=yes; WANT_CHECKS=no; PR_ARG=

_die() { echo "merge-state: $*" >&2; exit 1; }

while [ $# -gt 0 ]; do
  case "$1" in
    --branch)   BR="${2:-}";     shift 2 || _die "option '$1' needs a value" ;;
    --pr)       PR_ARG="${2:-}"; shift 2 || _die "option '$1' needs a value" ;;
    --repo)     REPO="${2:-}";   shift 2 || _die "option '$1' needs a value" ;;
    --base)     BASE="${2:-}";   shift 2 || _die "option '$1' needs a value" ;;
    --no-fetch) FETCH=no;        shift ;;
    --checks)   WANT_CHECKS=yes; shift ;;
    --format)   FORMAT="${2:-json}"; shift 2 || _die "option '$1' needs a value" ;;
    -h|--help)  sed -n '2,77p' "$0"; exit 0 ;;
    *) _die "unknown argument '$1'" ;;
  esac
done

case "$FORMAT" in json|env) ;; *) _die "unknown --format '$FORMAT' (want json or env)" ;; esac
case "$PR_ARG" in ''|[1-9]*) ;; *) _die "--pr must be a PR number (got '$PR_ARG')" ;; esac
case "$PR_ARG" in *[!0-9]*) _die "--pr must be a PR number (got '$PR_ARG')" ;; esac
[ -d "$REPO" ] || _die "--repo '$REPO' is not a directory"
_git() { git -C "$REPO" "$@"; }
_git rev-parse --git-dir >/dev/null 2>&1 || _die "'$REPO' is not a git repository"

if [ -z "$BR" ]; then
  BR="$(_git rev-parse --abbrev-ref HEAD 2>/dev/null || true)"
  [ -n "$BR" ] && [ "$BR" != HEAD ] || _die "could not determine the current branch; pass --branch"
fi

[ "$FETCH" = yes ] && _git fetch origin --quiet 2>/dev/null

# --- which GitHub repo holds this branch's PR ---------------------------------------------
# Never let gh guess. In a fork with an `upstream` remote and no `gh repo set-default`, gh picks
# upstream, where the fork's own PRs are invisible — so a merged branch read as "no PR" and fell
# through to ancestry (observed on two qmk_firmware worktrees, jbh-2sg). The rule, in order:
#   1. the remote `gh repo set-default` chose (`remote.<r>.gh-resolved`) — an explicit choice,
#      and the same place an unqualified `gh pr create` sends the PR
#   2. origin — where the branch was pushed, so where baton's own PRs live
# The URL is read raw from config (not `git remote get-url`) so an insteadOf rewrite can't hide
# the GitHub coordinates. A remote that isn't GitHub-shaped leaves PR_REPO empty and gh falls back
# to its own resolution — the old behaviour, no worse.
_gh_slug() { # remote URL -> OWNER/REPO (github.com) or HOST/OWNER/REPO (Enterprise); "" if not one
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
PR_REMOTE=""; PR_REPO=""
RESOLVED="$(_git config --get-regexp '^remote\..*\.gh-resolved$' 2>/dev/null | head -n1 || true)"
if [ -n "$RESOLVED" ]; then
  R_KEY="${RESOLVED%% *}"; R_VAL="${RESOLVED#* }"
  PR_REMOTE="${R_KEY#remote.}"; PR_REMOTE="${PR_REMOTE%.gh-resolved}"
  # Current gh writes `base`; older versions wrote the OWNER/REPO itself.
  case "$R_VAL" in base|'') PR_REPO="$(_gh_slug "$(_git config --get "remote.$PR_REMOTE.url" 2>/dev/null)")" ;;
                   *)        PR_REPO="$R_VAL" ;; esac
fi
if [ -z "$PR_REPO" ]; then
  PR_REMOTE=origin
  PR_REPO="$(_gh_slug "$(_git config --get remote.origin.url 2>/dev/null)")"
fi
[ -n "$PR_REPO" ] || PR_REMOTE=""
# A PR into another repo (a fork's branch PR'd upstream) is headed `<fork-owner>:<branch>` there;
# asking by bare branch name could match someone else's same-named branch in that repo.
PR_SELECTOR="$BR"
ORIGIN_REPO="$(_gh_slug "$(_git config --get remote.origin.url 2>/dev/null)")"
if [ -n "$PR_REPO" ] && [ -n "$ORIGIN_REPO" ] && [ "$PR_REPO" != "$ORIGIN_REPO" ]; then
  O_OWNER="${ORIGIN_REPO%/*}"; O_OWNER="${O_OWNER##*/}"
  PR_SELECTOR="$O_OWNER:$BR"
fi
# A number names exactly one PR in PR_REPO, with no head-name or fork-owner guessing at all.
[ -n "$PR_ARG" ] && PR_SELECTOR="$PR_ARG"

# --- rung 1: ask GitHub -------------------------------------------------------------------
GH_STATUS=unavailable
PR_JSON=""; PR_STATE=""; PR_NUMBER=""; PR_BASE=""
FAILING=""; PENDING=""

if command -v gh >/dev/null 2>&1; then
  FIELDS=state,number,baseRefName
  [ "$WANT_CHECKS" = yes ] && FIELDS="$FIELDS,statusCheckRollup"
  GH_ARGS=(pr view "$PR_SELECTOR" --json "$FIELDS")
  [ -n "$PR_REPO" ] && GH_ARGS+=(--repo "$PR_REPO")
  # Spelled out rather than `mktemp -t`: BSD mktemp treats -t's argument as a prefix, GNU
  # treats it as a template and rejects one with no X's — this form works on both.
  ERRF="$(mktemp "${TMPDIR:-/tmp}/baton-merge-state.XXXXXX")"
  PR_JSON="$( (cd "$REPO" && gh "${GH_ARGS[@]}") 2>"$ERRF" )" || PR_JSON=""
  GH_ERR="$(cat "$ERRF" 2>/dev/null || true)"; rm -f "$ERRF"
  # Herestrings, not pipes, throughout: under `pipefail` a reader that exits early (grep -q)
  # SIGPIPEs its writer and the pipeline reports failure — see the note at the ancestry rung.
  if [ -n "$PR_JSON" ] && jq -e . >/dev/null 2>&1 <<<"$PR_JSON"; then
    GH_STATUS=ok
    PR_STATE="$(jq -r '.state // empty'       <<<"$PR_JSON")"
    PR_NUMBER="$(jq -r '.number // empty'     <<<"$PR_JSON")"
    PR_BASE="$(jq -r '.baseRefName // empty'  <<<"$PR_JSON")"
  elif grep -qi 'no pull requests found' <<<"$GH_ERR"; then
    # A real answer, not a failure: this branch has no PR. Ancestry is the only rung left.
    GH_STATUS=no-pr
  else
    # Logged out, no network, not a GitHub remote, rate-limited — all the same to us.
    GH_STATUS=error
  fi
fi

if [ "$WANT_CHECKS" = yes ] && [ "$GH_STATUS" = ok ]; then
  FAILING="$(jq -r '
    .statusCheckRollup[]? | select(.conclusion=="FAILURE" or .conclusion=="ERROR"
      or .conclusion=="CANCELLED" or .conclusion=="TIMED_OUT") | .name' <<<"$PR_JSON")"
  PENDING="$(jq -r '
    .statusCheckRollup[]? | select(.status=="IN_PROGRESS" or .status=="QUEUED"
      or .status=="PENDING") | .name' <<<"$PR_JSON")"
fi

# --- resolve the base ---------------------------------------------------------------------
# The PR's own base beats a guess: a branch may target a release branch, not the default one.
_ref_exists() { _git rev-parse --verify --quiet "$1" >/dev/null 2>&1; }

# Never assume `main`: a fork's default can be anything (qmk_firmware's is jh-miryoku-keymap), and
# a guessed base that doesn't exist silently disables this whole rung. origin/HEAD is only set by
# `git clone` (or `git remote set-head`), so when it's missing ask origin itself before guessing.
# BASE_FROM records which of these answered, so a wrong base is visible rather than silent.
BASE_FROM=""
if [ -n "$BASE" ]; then
  BASE_FROM=flag
else
  if [ -n "$PR_BASE" ] && [ -n "$PR_REMOTE" ] && _ref_exists "$PR_REMOTE/$PR_BASE"; then
    BASE="$PR_REMOTE/$PR_BASE"; BASE_FROM=pr
  elif [ -n "$PR_BASE" ] && _ref_exists "origin/$PR_BASE"; then
    BASE="origin/$PR_BASE"; BASE_FROM=pr
  else
    HEADREF="$(_git symbolic-ref --quiet --short refs/remotes/origin/HEAD 2>/dev/null || true)"
    REMOTE_HEAD=""
    if [ -z "$HEADREF" ] && [ "$FETCH" = yes ]; then
      # `ref: refs/heads/<default>\tHEAD` — the remote's own answer. Network, so only when the
      # caller allowed a fetch; --no-fetch callers have already resolved what they can.
      REMOTE_HEAD="$(_git ls-remote --symref origin HEAD 2>/dev/null \
        | sed -n 's|^ref: refs/heads/\(.*\)[[:space:]]HEAD$|\1|p' | head -n1 || true)"
    fi
    if [ -n "$HEADREF" ] && _ref_exists "$HEADREF"; then
      BASE="$HEADREF"; BASE_FROM=origin-head
    elif [ -n "$REMOTE_HEAD" ] && _ref_exists "origin/$REMOTE_HEAD"; then
      BASE="origin/$REMOTE_HEAD"; BASE_FROM=remote-head
    elif _ref_exists origin/main; then
      BASE=origin/main; BASE_FROM=guess
    elif _ref_exists origin/master; then
      BASE=origin/master; BASE_FROM=guess
    fi
  fi
fi
[ -n "$BASE" ] && _ref_exists "$BASE" || { BASE=""; BASE_FROM=""; }

# --- rung 2: ancestry, and the has_work reading of the same range --------------------------
#
# `git branch --merged` is NOT enough on its own, and this is the trap the whole rung exists to
# avoid: a branch that has never been committed to is trivially an ancestor of its base, so
# plain ancestry calls a brand-new worktree "merged". Both a landed branch and an untouched one
# produce an empty `base..branch`; ancestry alone cannot separate them.
#
# What does separate them is WHERE the tip sits. A branch merged by a merge commit has its tip
# as the merge's SECOND parent, so the tip is an ancestor of the base but is NOT on the base's
# first-parent chain. A never-committed branch's tip IS on that chain — it's just the base
# commit the branch was cut from. So:
#
#   ancestor + off the first-parent chain  → landed via a merge commit
#   ancestor + on the first-parent chain   → no commits of its own yet
#   not an ancestor                        → has unlanded commits (or was squash-merged, which
#                                            only the PR rung can see)
#
# Known limit, deliberately resolved toward "not merged": a fast-forward merge leaves the tip
# ON the first-parent chain, so an FF-merged branch with no PR is indistinguishable from a fresh
# one. Reading that as "not merged" costs a redundant look at the work; reading it as "merged"
# would strand real work. The PR rung answers it whenever a PR exists, and for the branches where
# it never will, the `no-pr-needed` label described above lets a caller set the verdict aside
# deliberately rather than have this rung guess.
ANCESTRY=no
HAS_WORK=unknown
TIP="$(_git rev-parse --verify --quiet "$BR" 2>/dev/null || true)"
if [ -n "$BASE" ] && [ -n "$TIP" ]; then
  COUNT="$(_git rev-list --count "$BASE..$BR" 2>/dev/null || true)"
  if [ -n "$COUNT" ]; then
    if [ "$COUNT" -gt 0 ]; then HAS_WORK=yes; else HAS_WORK=no; fi
  fi
  if _git merge-base --is-ancestor "$TIP" "$BASE" 2>/dev/null; then
    # Herestring, not a pipe: `grep -q` exits on the first hit, and under `pipefail` the
    # SIGPIPE'd rev-list would make the whole pipeline report failure — i.e. a found tip would
    # read as "not found" and invert this test. (Observed doing exactly that.)
    FIRST_PARENTS="$(_git rev-list --first-parent "$BASE" 2>/dev/null || true)"
    if grep -qxF "$TIP" <<<"$FIRST_PARENTS"; then
      ANCESTRY=no      # the tip is just the branch point — nothing has been done here
    else
      ANCESTRY=yes
    fi
  fi
fi

# --- combine ------------------------------------------------------------------------------
if [ "$PR_STATE" = MERGED ]; then
  MERGED=yes; SIGNAL=pr
elif [ "$ANCESTRY" = yes ]; then
  MERGED=yes; SIGNAL=ancestry
elif [ "$GH_STATUS" = ok ]; then
  MERGED=no;  SIGNAL=pr           # an open/closed-unmerged PR is a definitive "not landed"
elif [ -n "$BASE" ]; then
  MERGED=no;  SIGNAL=ancestry     # no PR to ask, but ancestry was actually checkable
else
  MERGED=no;  SIGNAL=none         # nothing could be consulted — say so rather than imply "no"
fi

# --- render -------------------------------------------------------------------------------
case "$FORMAT" in
  json)
    jq -n \
      --arg branch "$BR" --arg merged "$MERGED" --arg signal "$SIGNAL" \
      --arg gh "$GH_STATUS" --arg base "$BASE" --arg base_from "$BASE_FROM" \
      --arg has_work "$HAS_WORK" --arg pr_repo "$PR_REPO" \
      --arg pr_state "$PR_STATE" --arg pr_number "$PR_NUMBER" \
      --arg failing "$FAILING" --arg pending "$PENDING" \
      '{branch:$branch, merged:$merged, signal:$signal, gh:$gh, base:$base,
        base_from:$base_from, has_work:$has_work, pr_repo:$pr_repo, pr_state:$pr_state, pr_number:$pr_number,
        failing:($failing | if . == "" then [] else split("\n") end),
        pending:($pending | if . == "" then [] else split("\n") end)}'
    ;;
  env)
    _q() { printf "'%s'" "$(printf '%s' "$1" | sed "s/'/'\\\\''/g")"; }
    printf 'export MERGED=%s\n'       "$(_q "$MERGED")"
    printf 'export MERGE_SIGNAL=%s\n' "$(_q "$SIGNAL")"
    printf 'export GH_STATUS=%s\n'    "$(_q "$GH_STATUS")"
    printf 'export MERGE_BASE=%s\n'   "$(_q "$BASE")"
    printf 'export MERGE_BASE_FROM=%s\n' "$(_q "$BASE_FROM")"
    printf 'export HAS_WORK=%s\n'     "$(_q "$HAS_WORK")"
    printf 'export PR_REPO=%s\n'      "$(_q "$PR_REPO")"
    printf 'export PR_STATE=%s\n'     "$(_q "$PR_STATE")"
    printf 'export PR_NUMBER=%s\n'    "$(_q "$PR_NUMBER")"
    printf 'export FAILING=%s\n'      "$(_q "$FAILING")"
    printf 'export PENDING=%s\n'      "$(_q "$PENDING")"
    ;;
esac
