#!/usr/bin/env bash
# baton — prune local branches that have landed, so a repo converges to clean on every
# baton:cleanup-worktrees run instead of accumulating orphaned refs forever.
#
# WHY THIS EXISTS. The worktree pass only deletes a branch alongside the worktree it removes. A
# branch whose worktree went some other way, or that never had one, stays local forever: measured
# in homebernetes on 2026-08-06, 41 of 47 local branches had a deleted upstream ("[gone]") — PRs
# merged, GitHub deleted the remote branch, and nothing ever reaped the local ref. This is the
# separate, branch-centric pass that does, run after worktree classification.
#
# This is the ONLY place the branch-pruning rules live, for the same reason cleanup-verdict.sh
# exists: the failure mode is deleted work, so the rules are executable and pinned by
# scripts/test-branch-prune.sh rather than paraphrased from a skill at read time.
#
# THE SAFETY RULE — "upstream is gone" IS NOT PROOF THE WORK LANDED. A remote branch can be
# deleted without ever being merged; pruning on that alone would be silent data loss. So a branch
# is deleted automatically only when its upstream is gone AND a positive merge signal agrees —
# scripts/merge-state.sh's `merged=yes`, which prefers `gh pr view` (MERGED) over ancestry against
# origin's default branch. Never re-derived here: merge-state.sh is the one ladder every skill
# asks, including its first-parent test that stops a never-committed branch reading as merged.
#
# THE BUCKETS (one per local branch):
#   prune             upstream gone + merged=yes + prune_gone_upstream on   → deleted by `apply`
#   confirm-merged    merged=yes, upstream NOT gone (still on the remote, or never pushed)
#                     + prune_merged on                                     → ask first
#   confirm-orphaned  upstream gone, merged=no + prune_gone_upstream on     → ask first; NEVER
#                     automatic. Commits the base lacks may exist only here.
#                     Also: never pushed (upstream none or the base), merged=no, has_work=no
#                     + prune_merged on — a branch with nothing of its own and no worktree,
#                     the shape a `no-pr-needed` task leaves behind. Still asked, never automatic.
#   keep              everything else — with `reason` saying which:
#                       protected      in `protect`, or origin's default branch itself
#                       checked-out    the branch of ANY worktree (the worktree pass owns it)
#                       live           upstream present / none / base, not merged
#                       policy-off     the matching prune_* switch is false
#                       unknown        merge state could not be determined (signal=none)
#
# Only `prune` is ever automatic. The two confirm buckets are the "act automatically only where
# the evidence is unambiguous, ask otherwise" posture baton:cleanup-worktrees already uses: a
# merged branch whose upstream still exists may still be pushed to, and an orphaned one may hold
# the only copy of its commits.
#
# AN UPSTREAM THAT IS THE BASE IS NOT A PUSHED COPY. `git worktree add -b <br> origin/main` (how
# baton:start made branches before 0.16.2) sets the new branch to track origin/main, so a branch
# that was never pushed reads as "upstream present". It is reported as `upstream_state: base` and
# treated as never pushed: the base holds the base's commits, not this branch's.
#
# DELETION IS SHA-PINNED, NEVER `git branch -d`/`-D`. `branch -d` judges "merged" against the
# upstream or the CURRENT HEAD — in a primary clone that is local `main`, which is routinely stale
# (2026-08-06: `-d` refused genuinely merged branches until main was pulled), and it is always
# wrong for squash merges. The decision here is made against origin's default branch and GitHub,
# so the delete is `git update-ref -d refs/heads/<b> <sha>`: it removes the ref only if it still
# points at the commit that was judged, which also closes the race with anything that moved it
# since the scan. Nothing is ever force-deleted on a weaker signal than the bucket names.
#
# Usage:
#   branch-prune.sh scan   [--repo <dir>] [--context <file|->] [--no-fetch] [--format json|summary]
#   branch-prune.sh apply  [--repo <dir>] [--context <file|->] [--no-fetch] [--format json|summary]
#   branch-prune.sh delete --branch <b> [--repo <dir>] [--context <file|->]
#                          (--expect <sha> [--require-no-work] | --require-merged) [--no-fetch]
#
#   scan    classify every local branch; touches nothing. (It does `git fetch --prune origin`
#           first unless --no-fetch: "[gone]" is only true after a prune.)
#   apply   scan, then delete the `prune` bucket. Rows gain `deleted: yes|failed`.
#   delete  delete ONE branch outside the automatic bucket — a human's yes to a confirm-* row
#           (pass the row's sha as --expect, so the yes covers exactly the commit that was shown),
#           or a worktree the worktree pass just removed (--require-merged re-asks merge-state.sh
#           and refuses unless merged=yes). --require-no-work, which needs --expect, is the
#           removed worktree of a `no-pr-needed` task: no merge is ever coming, so it re-asks
#           merge-state.sh and refuses unless has_work=no — nothing on the branch that origin's
#           default branch lacks — at the SHA that was judged. The hard guards are re-checked
#           every time: protected and checked-out branches are refused whatever the flags say.
#
# Policy — the context's optional `cleanup:` block; absent keys take the plugin defaults:
#   cleanup:
#     prune_gone_upstream: true          # the [gone] set (auto when merged, ask when not)
#     prune_merged: true                 # merged branches whose upstream is not gone (ask)
#     protect: [main, master, develop]   # never deleted, whatever their state
# origin's default branch (origin/HEAD) is protected in addition, so a repo whose default is
# `trunk` is safe without anyone remembering to list it. Without --context the defaults apply
# (no resolver run: this script must work on a bare repo, and the policy is all it needs).
#
# JSON (scan/apply):
#   { repo, base, fetched, policy:{prune_gone_upstream, prune_merged, protect[]},
#     branches:[ { branch, sha, upstream, upstream_state: gone|present|none|base,
#                  merged, merge_signal, gh_status, has_work, pr_number,
#                  verdict, reason, deleted } ],
#     counts:{ prune, confirm_merged, confirm_orphaned, keep, deleted, failed } }
#   `deleted` is "" for scan, and for apply: yes | failed | "" (not in the prune bucket).
# delete: { branch, sha, deleted: yes|refused|failed, reason }  — exit 0 only when deleted.
#
# Exit status (scan/apply): 0 whenever the repo was classified — a full `keep` list is an answer.
# Non-zero for a usage error or a non-git directory. An `apply` whose deletes partly failed still
# exits 0; the failures are rows (`deleted: failed`), never a truncated report.
#
# Requires: git, jq. Uses gh through merge-state.sh when present.

set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
MS="$HERE/merge-state.sh"

_die() { echo "branch-prune: $*" >&2; exit 1; }

VERB="${1:-}"; [ $# -gt 0 ] && shift
case "$VERB" in
  scan|apply|delete) ;;
  -h|--help|"") sed -n '2,96p' "$0"; [ -n "$VERB" ] && exit 0; exit 1 ;;
  *) _die "unknown verb '$VERB' (want scan, apply or delete)" ;;
esac

REPO="."; CTX_SRC=""; FETCH=yes; FORMAT=json; BR=""; EXPECT=""; REQUIRE_MERGED=no; REQUIRE_NO_WORK=no
while [ $# -gt 0 ]; do
  case "$1" in
    --repo)           REPO="${2:-}";    shift 2 || _die "option '$1' needs a value" ;;
    --context)        CTX_SRC="${2:-}"; shift 2 || _die "option '$1' needs a value" ;;
    --no-fetch)       FETCH=no;         shift ;;
    --format)         FORMAT="${2:-}";  shift 2 || _die "option '$1' needs a value" ;;
    --branch)         BR="${2:-}";      shift 2 || _die "option '$1' needs a value" ;;
    --expect)         EXPECT="${2:-}";  shift 2 || _die "option '$1' needs a value" ;;
    --require-merged) REQUIRE_MERGED=yes; shift ;;
    --require-no-work) REQUIRE_NO_WORK=yes; shift ;;
    *) _die "unknown argument '$1'" ;;
  esac
done

case "$FORMAT" in json|summary) ;; *) _die "unknown --format '$FORMAT' (want json or summary)" ;; esac
[ -d "$REPO" ] || _die "--repo '$REPO' is not a directory"
_git() { git -C "$REPO" "$@"; }
_git rev-parse --git-dir >/dev/null 2>&1 || _die "'$REPO' is not a git repository"
[ -r "$MS" ] || _die "merge-state.sh not found next to this script ($MS)"

# --- policy ---------------------------------------------------------------------------------
CTX='{}'
if [ -n "$CTX_SRC" ]; then
  if [ "$CTX_SRC" = - ]; then CTX="$(cat)"; else CTX="$(cat "$CTX_SRC")" || _die "cannot read --context '$CTX_SRC'"; fi
  jq -e 'type == "object"' >/dev/null 2>&1 <<<"$CTX" || _die "--context is not a JSON object"
fi
# `false` must survive: jq's `//` treats false as absent, so test for null explicitly.
POLICY="$(jq -c '
  (.cleanup // {}) as $c
  | { prune_gone_upstream: (if $c.prune_gone_upstream == null then true else $c.prune_gone_upstream end),
      prune_merged:        (if $c.prune_merged        == null then true else $c.prune_merged        end),
      protect:             ($c.protect // ["main", "master", "develop"]) }' <<<"$CTX")"
P_GONE="$(jq -r '.prune_gone_upstream' <<<"$POLICY")"
P_MERGED="$(jq -r '.prune_merged' <<<"$POLICY")"

# --- shared facts ---------------------------------------------------------------------------
FETCHED=no
if [ "$FETCH" = yes ]; then
  # --prune is what makes "[gone]" true: without it a deleted remote branch keeps its stale
  # remote-tracking ref and the local branch never reads as gone.
  _git fetch --prune origin --quiet 2>/dev/null && FETCHED=yes
fi

# origin's default branch, never a hardcoded main/master — it is protected and is the base.
BASE="$(_git symbolic-ref --quiet --short refs/remotes/origin/HEAD 2>/dev/null || true)"
DEFAULT_NAME="${BASE#origin/}"

# Every branch checked out in ANY worktree of this repo, primary included. The worktree pass
# owns those; deleting one out from under a worktree would detach it.
CHECKED_OUT="$(_git worktree list --porcelain 2>/dev/null | sed -n 's|^branch refs/heads/||p')"

_is_protected() {
  [ -n "$DEFAULT_NAME" ] && [ "$1" = "$DEFAULT_NAME" ] && return 0
  jq -e --arg b "$1" 'index($b) != null' >/dev/null 2>&1 <<<"$(jq -c '.protect' <<<"$POLICY")"
}
_is_checked_out() { grep -qxF -- "$1" <<<"$CHECKED_OUT"; }

# merge-state.sh for one branch → sets MERGED MERGE_SIGNAL GH_STATUS HAS_WORK PR_NUMBER.
#
# A review branch (`review/<n>-<head>`, made by pr-review.sh) is not named after its PR's head, so
# asking GitHub by branch name finds nothing and a squash-merged review would only ever reach
# confirm-orphaned. pr-review.sh records the PR number in the branch's own config; ask by number
# when it is there. Its upstream is the author's head, so [gone] still means "the author's branch
# was deleted" — prune when that PR merged, ask otherwise, exactly as for any branch.
_merge_state() {
  local out pr
  MERGED=""; MERGE_SIGNAL=""; GH_STATUS=""; HAS_WORK=""; PR_NUMBER=""
  pr="$(git -C "$REPO" config --get "branch.$1.baton-pr" 2>/dev/null || true)"
  case "$pr" in ''|*[!0-9]*) pr="" ;; esac
  out="$(bash "$MS" --repo "$REPO" --branch "$1" ${pr:+--pr "$pr"} --no-fetch --format env 2>/dev/null)" || out=""
  eval "$out"
  # A missing answer must never read as "merged".
  [ -n "$MERGED" ]       || MERGED=unknown
  [ -n "$MERGE_SIGNAL" ] || MERGE_SIGNAL=none
  [ -n "$HAS_WORK" ]     || HAS_WORK=unknown
}

_sha_delete() { # $1 branch, $2 sha — removes the ref only if it still points at $2
  _git update-ref -d "refs/heads/$1" "$2" 2>/dev/null
}

# ================================================================================ delete ====
if [ "$VERB" = delete ]; then
  [ -n "$BR" ] || _die "delete needs --branch"
  [ -n "$EXPECT" ] || [ "$REQUIRE_MERGED" = yes ] \
    || _die "delete needs --expect <sha> (a confirmed row) or --require-merged (a removed worktree's branch)"
  # Without a merge to re-ask, the judged SHA is the only thing tying the delete to the verdict.
  [ "$REQUIRE_NO_WORK" = no ] || [ -n "$EXPECT" ] \
    || _die "--require-no-work needs --expect <sha> (the commit the verdict judged)"

  _out() { jq -n --arg branch "$BR" --arg sha "${SHA:-}" --arg deleted "$1" --arg reason "$2" \
             '{branch:$branch, sha:$sha, deleted:$deleted, reason:$reason}'; }

  SHA="$(_git rev-parse --verify --quiet "refs/heads/$BR" 2>/dev/null || true)"
  [ -n "$SHA" ] || { _out refused "no such local branch"; exit 1; }
  _is_protected "$BR"   && { _out refused "protected"; exit 1; }
  _is_checked_out "$BR" && { _out refused "checked out in a worktree"; exit 1; }
  if [ -n "$EXPECT" ]; then
    FULL="$(_git rev-parse --verify --quiet "$EXPECT^{commit}" 2>/dev/null || true)"
    [ "$FULL" = "$SHA" ] || { _out refused "branch moved since it was shown (now $SHA, expected $EXPECT)"; exit 1; }
  fi
  if [ "$REQUIRE_MERGED" = yes ]; then
    _merge_state "$BR"
    [ "$MERGED" = yes ] || { _out refused "not merged (merged=$MERGED, signal=$MERGE_SIGNAL)"; exit 1; }
  fi
  if [ "$REQUIRE_NO_WORK" = yes ]; then
    _merge_state "$BR"
    [ "$HAS_WORK" = no ] || { _out refused "has commits of its own (has_work=$HAS_WORK)"; exit 1; }
  fi
  if _sha_delete "$BR" "$SHA"; then _out yes "deleted at $SHA"; exit 0; fi
  _out failed "git update-ref -d refused"; exit 1
fi

# ========================================================================== scan / apply ====
ROWS=()
while IFS=$'\t' read -r b sha up track; do
  [ -n "$b" ] || continue
  if [ -z "$up" ]; then STATE_UP=none
  elif [ "$track" = "[gone]" ]; then STATE_UP=gone
  elif [ -n "$BASE" ] && [ "$up" = "$BASE" ]; then STATE_UP=base
  else STATE_UP=present
  fi

  MERGED=""; MERGE_SIGNAL=""; GH_STATUS=""; HAS_WORK=""; PR_NUMBER=""
  VERDICT=keep; REASON=""

  if _is_protected "$b"; then
    REASON="protected"
  elif _is_checked_out "$b"; then
    REASON="checked-out: a worktree has it; the worktree pass decides"
  elif [ "$STATE_UP" = gone ] && [ "$P_GONE" != true ]; then
    REASON="policy-off: upstream gone, prune_gone_upstream is false"
  elif [ "$STATE_UP" != gone ] && [ "$P_MERGED" != true ]; then
    # Nothing in this bucket could ever be acted on, so don't spend a gh call per branch on it.
    REASON="policy-off: upstream $STATE_UP, prune_merged is false"
  else
    _merge_state "$b"
    if [ "$MERGE_SIGNAL" = none ] && [ "$MERGED" != yes ]; then
      REASON="unknown: merge state could not be determined (gh=$GH_STATUS, no base)"
    elif [ "$STATE_UP" = gone ]; then
      if [ "$MERGED" = yes ]; then
        VERDICT=prune; REASON="upstream gone; merged ($MERGE_SIGNAL${PR_NUMBER:+, PR #$PR_NUMBER})"
      else
        VERDICT=confirm-orphaned
        if [ "$HAS_WORK" = no ]; then
          REASON="upstream gone but no merge observed; adds no commits of its own (has_work=no)"
        else
          REASON="upstream gone but NOT merged ($MERGE_SIGNAL, has_work=$HAS_WORK) — may hold the only copy of its commits"
        fi
      fi
    elif [ "$MERGED" = yes ]; then
      VERDICT=confirm-merged
      REASON="merged ($MERGE_SIGNAL${PR_NUMBER:+, PR #$PR_NUMBER}); upstream $STATE_UP, not gone"
    elif [ "$STATE_UP" != present ] && [ "$HAS_WORK" = no ]; then
      # Not checked out, never pushed, nothing the base lacks: deleting it loses only a name.
      # Asked rather than automatic, since a name can be all somebody wanted kept.
      VERDICT=confirm-orphaned
      REASON="never pushed (upstream $STATE_UP); adds no commits of its own (has_work=no)"
    else
      REASON="live: upstream $STATE_UP, not merged"
    fi
  fi

  ROWS+=("$(jq -nc --arg branch "$b" --arg sha "$sha" --arg upstream "$up" --arg us "$STATE_UP" \
    --arg merged "${MERGED:-}" --arg signal "${MERGE_SIGNAL:-}" --arg gh "${GH_STATUS:-}" \
    --arg has_work "${HAS_WORK:-}" --arg pr "${PR_NUMBER:-}" --arg verdict "$VERDICT" --arg reason "$REASON" \
    '{branch:$branch, sha:$sha, upstream:$upstream, upstream_state:$us, merged:$merged,
      merge_signal:$signal, gh_status:$gh, has_work:$has_work, pr_number:$pr,
      verdict:$verdict, reason:$reason, deleted:""}')")
done < <(_git for-each-ref --format='%(refname:short)%09%(objectname)%09%(upstream:short)%09%(upstream:track)' refs/heads)

# Deletion happens after the whole scan, so a failure part-way never truncates the report.
if [ "$VERB" = apply ]; then
  for i in "${!ROWS[@]}"; do
    [ "$(jq -r .verdict <<<"${ROWS[$i]}")" = prune ] || continue
    b="$(jq -r .branch <<<"${ROWS[$i]}")"; sha="$(jq -r .sha <<<"${ROWS[$i]}")"
    # Re-check the hard guards at delete time, not only at classification: `update-ref -d` will
    # happily delete a branch a worktree has checked out, so this guard must not live in one place.
    if _is_protected "$b" || _is_checked_out "$b"; then d=failed
    elif _sha_delete "$b" "$sha"; then d=yes; else d=failed; fi
    ROWS[$i]="$(jq -c --arg d "$d" '.deleted = $d' <<<"${ROWS[$i]}")"
  done
fi

if [ ${#ROWS[@]} -gt 0 ]; then BRANCHES="$(printf '%s\n' "${ROWS[@]}" | jq -sc .)"; else BRANCHES='[]'; fi
REPORT="$(jq -n --arg repo "$(cd "$REPO" && pwd)" --arg base "$BASE" --arg fetched "$FETCHED" \
  --argjson policy "$POLICY" --argjson branches "$BRANCHES" '
  {repo:$repo, base:$base, fetched:$fetched, policy:$policy, branches:$branches,
   counts:{
     prune:            ([$branches[] | select(.verdict == "prune")] | length),
     confirm_merged:   ([$branches[] | select(.verdict == "confirm-merged")] | length),
     confirm_orphaned: ([$branches[] | select(.verdict == "confirm-orphaned")] | length),
     keep:             ([$branches[] | select(.verdict == "keep")] | length),
     deleted:          ([$branches[] | select(.deleted == "yes")] | length),
     failed:           ([$branches[] | select(.deleted == "failed")] | length) }}')"

case "$FORMAT" in
  json) printf '%s\n' "$REPORT" ;;
  summary)
    # Every row, grouped — the skipped set is part of the answer, never truncated away.
    jq -r '
      "branch-prune: \(.repo)  base=\(.base // "?")  fetched=\(.fetched)",
      "  \(.counts.deleted) deleted, \(.counts.failed) failed, \(.counts.prune) prune, \(.counts.confirm_merged) confirm-merged, \(.counts.confirm_orphaned) confirm-orphaned, \(.counts.keep) kept",
      (.branches | sort_by(.verdict, .branch)[]
        | "  [\(.verdict)\(if .deleted != "" then " → " + .deleted else "" end)] \(.branch)  \(.sha[0:9])  — \(.reason)")
    ' <<<"$REPORT" ;;
esac
