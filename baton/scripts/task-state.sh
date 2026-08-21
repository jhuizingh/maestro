#!/usr/bin/env bash
# baton — reduce everything known about one task to ONE overall state.
#
# `baton:status` renders this; nothing acts on it. It is the read-only twin of
# scripts/cleanup-verdict.sh: that script answers "may I DELETE this worktree", this one answers
# "where does this task STAND". They are deliberately separate — one is safety-critical and
# biased toward keeping things, the other is a report and biased toward saying something useful —
# but they must never disagree about whether a task is DONE, so this script does not re-derive
# that. It takes cleanup-verdict.sh's verdict as an input (`--verdict`) and layers the
# not-yet-done states underneath it.
#
# PURE FUNCTION, like cleanup-verdict.sh: every input is computed fresh by the caller, nothing is
# looked up here, and no `unknown` is ever resolved into a confident answer. That is what makes
# the ladder testable without a git repo, a tracker, or a network (scripts/test-task-state.sh).
#
# THE VOCABULARY — exactly one of these comes out, and the set is closed:
#
#   unstarted                 nothing has been done on this branch yet
#   blocked                   an open blocker bead stands in the way
#   in-progress               commits (or uncommitted changes) exist, no PR yet
#   waiting-on-ci             a PR is open and its checks are still running
#   waiting-on-human          a PR is open and needs a person — to review a green one, to look
#                             at a red one, or to decide about one closed without merging
#   merged-needs-finish       the branch landed but `baton:finish` never ran
#   done-awaiting-delete      finished and flagged; a later cleanup pass removes the worktree
#   done-followups-elsewhere  finished, with follow-up work living on another bead/worktree
#   unknown                   the signals available could not decide
#
# (`baton:status` has one more answer, "not a baton worktree", which never reaches this script —
# it is what the skill reports when task-identity.sh resolves no leaf at all.)
#
# WHY `merged-needs-finish` EXISTS. It is the window between "PR merged" and "`baton:finish`
# ran", which is exactly when a worker session gets abandoned — the interesting work is over —
# so a resumed session is unusually likely to be sitting in it. `baton:resume` already routes on
# it; without its own name here, status would have to call it "done", which it is not: the bead
# is still open, the criteria unverified, and the worktree unflagged.
#
# THE LADDER, first match wins. Order is the whole design, so it is spelled out:
#
#   1. the done family, from --verdict          an explicit "I'm done" plus its cross-checks
#   2. merged                                    the branch landed, whatever the bead says
#   3. a closed bead with nothing outstanding    the tracker says finished and git does not object
#   4. the PR family (open / closed-unmerged)    a PR exists, so the work is out of your hands
#   5. blockers                                  nothing further along has happened
#   6. commits or a dirty tree                   work is underway
#   7. nothing outstanding                       unstarted
#
# Rung 3 exists because a CLOSED bead can never be `unstarted`, and without it the bottom of the
# ladder says exactly that about the shape it fires on most — a task whose work deliberately
# landed outside git, where "nothing outstanding" is the finished state rather than the initial
# one. Observed on a real worktree (jbh-c6l) before the rung was added.
#
# Blockers sit BELOW the PR family on purpose. A blocker edge decides whether you can *start*;
# once a PR is open the work is already out and the edge no longer describes what happens next.
# It is never dropped, though — a signal that loses the headline is reported as a note instead,
# which is the general rule here: this script never discards evidence, it only ranks it.
#
# WHAT `--verdict unknown` MEANS. cleanup-verdict.sh could not be consulted (a stale plugin
# cache, say). The ladder still runs, because rungs 2-7 need none of it — but if the leaf is
# labeled `ready-for-worktree-delete`, the done family is precisely what was unreadable, so the
# answer is `unknown` rather than a confident lower rung. That case matters: a task that
# deliberately produced nothing to merge (`no-pr-needed`) is byte-identical to a fresh worktree
# in git, so a wrong fall-through would report finished work as `unstarted`.
#
# Usage:
#   task-state.sh [--verdict <v>] [--verdict-reason <text>] [--labels <text>] [--status <s>]
#                 [--merged <yes|no|unknown>] [--has-work <yes|no|unknown>]
#                 [--dirty <yes|no|unknown>] [--pr-state <s>] [--pr-number <n>]
#                 [--failing <text>] [--pending <text>]
#                 [--blockers <text>] [--unblocks <text>] [--format json|env]
#
# INPUTS:
#   --verdict         cleanup-verdict.sh's `verdict` field; default `unknown`
#   --verdict-reason  its `reason` field; surfaced as a note when the verdict disagrees with the
#                     evidence (`label-state-mismatch`), and otherwise unused
#   --labels          the leaf's labels (`bd label list <leaf>` output, bullets and all). Only
#                     `ready-for-worktree-delete` and `keep-task-open` are read here
#   --status          the leaf bead's status (open, in_progress, closed, …, or unknown)
#   --merged          from scripts/merge-state.sh
#   --has-work        from scripts/merge-state.sh — commits the base does not already have
#   --dirty           `git status --porcelain` non-empty = yes
#   --pr-state        MERGED | OPEN | CLOSED | "" (from merge-state.sh)
#   --pr-number       for the reason/next lines; cosmetic
#   --failing         failing check-run names, newline-separated (merge-state.sh --checks)
#   --pending         pending check-run names, newline-separated
#   --blockers        ids of OPEN beads blocking this one, whitespace-separated. THE CALLER
#                     FILTERS BY STATUS — a closed blocker is not a blocker, and this script has
#                     no way to look one up
#   --unblocks        ids of OPEN beads this one blocks, whitespace-separated; same contract
#
# JSON fields / env vars:
#   state        STATE          one token from the vocabulary above
#   label        STATE_LABEL    the same thing as a human phrase ("waiting on the human")
#   reason       STATE_REASON   one line naming the signals that decided it
#   next         STATE_NEXT     the suggested next action, or "" when there is nothing to do
#   notes        STATE_NOTES    newline-separated secondary observations (evidence that did not
#                               win the headline, anomalies, absent signals). Never empty-worthy:
#                               report every line
#
# Exit status: 0 whenever a state was reached, INCLUDING `unknown` — "I cannot tell" is an
# answer, not a failure. Non-zero only for a usage error.
#
# Requires: bash, jq (for --format json only).

set -uo pipefail

FORMAT=json
VERDICT=unknown; VERDICT_REASON=""; LABELS=""; BEAD_STATUS=unknown
MERGED=unknown; HAS_WORK=unknown; DIRTY=unknown
PR_STATE=""; PR_NUMBER=""; FAILING=""; PENDING=""; BLOCKERS=""; UNBLOCKS=""

_die() { echo "task-state: $*" >&2; exit 1; }

while [ $# -gt 0 ]; do
  case "$1" in
    --verdict)        VERDICT="${2:-}";        shift 2 ;;
    --verdict-reason) VERDICT_REASON="${2:-}"; shift 2 ;;
    --labels)         LABELS="${2:-}";         shift 2 ;;
    --status)         BEAD_STATUS="${2:-}";    shift 2 ;;
    --merged)         MERGED="${2:-}";         shift 2 ;;
    --has-work)       HAS_WORK="${2:-}";       shift 2 ;;
    --dirty)          DIRTY="${2:-}";          shift 2 ;;
    --pr-state)       PR_STATE="${2:-}";       shift 2 ;;
    --pr-number)      PR_NUMBER="${2:-}";      shift 2 ;;
    --failing)        FAILING="${2:-}";        shift 2 ;;
    --pending)        PENDING="${2:-}";        shift 2 ;;
    --blockers)       BLOCKERS="${2:-}";       shift 2 ;;
    --unblocks)       UNBLOCKS="${2:-}";       shift 2 ;;
    --format)         FORMAT="${2:-json}";     shift 2 ;;
    -h|--help)        sed -n '2,103p' "$0"; exit 0 ;;
    *) _die "unknown argument '$1'" ;;
  esac
done

case "$FORMAT" in json|env) ;; *) _die "unknown --format '$FORMAT' (want json or env)" ;; esac

# An empty value is not a third state — it is a signal the caller failed to compute, which is
# what `unknown` already means. Collapsing it keeps every comparison below a positive test.
[ -n "$VERDICT" ]     || VERDICT=unknown
[ -n "$BEAD_STATUS" ] || BEAD_STATUS=unknown
[ -n "$MERGED" ]      || MERGED=unknown
[ -n "$HAS_WORK" ]    || HAS_WORK=unknown
[ -n "$DIRTY" ]       || DIRTY=unknown

for v in MERGED HAS_WORK DIRTY; do
  case "${!v}" in yes|no|unknown) ;;
    *) _die "--$(echo "$v" | tr 'A-Z_' 'a-z-') must be yes, no or unknown (got '${!v}')" ;;
  esac
done
case "$VERDICT" in
  confirmed-ready|looks-done-unlabeled|label-state-mismatch|not-ready|unknown) ;;
  *) _die "--verdict must be a cleanup-verdict.sh verdict or unknown (got '$VERDICT')" ;;
esac
case "$PR_STATE" in
  MERGED|OPEN|CLOSED|"") ;;
  *) _die "--pr-state must be MERGED, OPEN, CLOSED or empty (got '$PR_STATE')" ;;
esac

# --- read the two labels this script cares about ---------------------------------------------
# Exact token match, never a substring — the same rule as cleanup-verdict.sh, for the same
# reason: `grep -w` treats `-` as a word separator, so `keep-task-open` would also match inside
# `dont-keep-task-open`. Splitting on whitespace and comparing whole tokens cannot. bd's bullet
# formatting ("  - <label>") splits into a bare `-` plus the label, so its output passes verbatim.
_has() {
  local want="$1" tok
  for tok in $LABELS; do [ "$tok" = "$want" ] && return 0; done
  return 1
}
LABELED=no;   _has ready-for-worktree-delete && LABELED=yes
KEEP_OPEN=no; _has keep-task-open            && KEEP_OPEN=yes

# --- normalise the list inputs ----------------------------------------------------------------
_join() { # collapse whitespace/newlines to a ", "-separated list
  local out="" tok
  for tok in $1; do out="${out:+$out, }$tok"; done
  printf '%s' "$out"
}
_count() { local n=0 tok; for tok in $1; do n=$((n+1)); done; printf '%s' "$n"; }

FAILING_LIST="$(_join "$FAILING")"; FAILING_N="$(_count "$FAILING")"
PENDING_LIST="$(_join "$PENDING")"; PENDING_N="$(_count "$PENDING")"
BLOCKERS_LIST="$(_join "$BLOCKERS")"; BLOCKERS_N="$(_count "$BLOCKERS")"
UNBLOCKS_LIST="$(_join "$UNBLOCKS")"; UNBLOCKS_N="$(_count "$UNBLOCKS")"

NOTES=""
_note() { NOTES="${NOTES:+$NOTES
}$1"; }

# --- the ladder -------------------------------------------------------------------------------
STATE=""; REASON=""; NEXT=""

# Both rungs that conclude "finished" split the same way, so the split lives here once: a task
# with deferred work of its own (keep-task-open) or an open bead waiting on it is done *here*
# but not done *everywhere*, and saying so is the difference between "forget this" and "the
# thread continues over there".
_done_family() { # $1 = why it is finished, $2 = what cleanup will actually do
  if [ "$KEEP_OPEN" = yes ] || [ -n "$UNBLOCKS_LIST" ]; then
    STATE=done-followups-elsewhere
    if [ "$KEEP_OPEN" = yes ] && [ -n "$UNBLOCKS_LIST" ]; then
      REASON="$1; the bead is open on purpose (keep-task-open) and unblocks $UNBLOCKS_LIST"
    elif [ "$KEEP_OPEN" = yes ]; then
      REASON="$1; the bead is open on purpose (keep-task-open), so follow-up work is expected"
    else
      REASON="$1; it unblocks $UNBLOCKS_LIST, which live elsewhere"
    fi
    if [ -n "$UNBLOCKS_LIST" ]; then
      NEXT="nothing to do here — the follow-up work is $UNBLOCKS_LIST"
    else
      NEXT="nothing to do here — the deferred work stays tracked on this bead"
    fi
  else
    STATE=done-awaiting-delete
    REASON="$1"
    NEXT="$2"
  fi
}

# Rung 1 — the done family. cleanup-verdict.sh owns "is this finished", so status never
# second-guesses it; it only splits `confirmed-ready` in two, which cleanup has no reason to.
case "$VERDICT" in
  confirmed-ready)
    _done_family "finished: labeled ready-for-worktree-delete and every cross-check agrees" \
                 "nothing to do here — a later home session's baton:cleanup-worktrees removes this worktree"
    ;;
  label-state-mismatch)
    # The label says done and the evidence disagrees. Do NOT let either side win silently:
    # fall through to what the evidence actually shows, and carry the disagreement as a note.
    # cleanup-verdict.sh's reason already opens with "labeled ready but …", so it is quoted as-is.
    _note "ANOMALY: ${VERDICT_REASON:-labeled ready but the cross-checks disagree} — baton:cleanup-worktrees will flag this worktree rather than remove it"
    ;;
  unknown)
    if [ "$LABELED" = yes ]; then
      STATE=unknown
      REASON="the leaf is labeled ready-for-worktree-delete, but cleanup-verdict.sh could not be consulted, so whether it is genuinely finished is exactly what is unreadable"
      NEXT="check that the baton plugin is current (baton:doctor), then re-run"
    else
      _note "cleanup-verdict.sh was not consulted; the finished states could not be evaluated"
    fi
    ;;
esac

# Rungs 2-6 — the evidence ladder, reached whenever the done family did not answer.
if [ -z "$STATE" ]; then
  if [ "$MERGED" = yes ]; then
    # Before `baton:finish` labels it. Note the bead's own status either way: a closed bead here
    # means finish half-ran (or someone closed it by hand) and the cleanup signal is still absent.
    STATE=merged-needs-finish
    REASON="the branch has landed${PR_NUMBER:+ (PR #$PR_NUMBER)}, but no ready-for-worktree-delete label — baton:finish has not run"
    [ "$BEAD_STATUS" = closed ] && REASON="$REASON; the bead is already closed, so only the cleanup signal is missing"
    NEXT="run baton:finish — it verifies the acceptance criteria, closes the leaf, and flags the worktree for cleanup"

  elif [ "$BEAD_STATUS" = closed ] && [ "$HAS_WORK" != yes ] && [ "$DIRTY" != yes ] \
       && [ "$PR_STATE" != OPEN ]; then
    # A CLOSED BEAD IS NEVER unstarted, blocked, OR in-progress. Without this rung the ladder
    # below reads "nothing outstanding in git" as "nobody has started", which is exactly wrong
    # for the shape it fires on most: a task whose work deliberately landed outside git. Observed
    # on jbh-c6l — bead closed, worktree flagged, zero commits by design — reported as
    # `unstarted`, i.e. finished work described as not begun.
    #
    # It sits BELOW the merged rung so an ordinary merged-and-closed task still gets the more
    # specific `merged-needs-finish`, and it does not fire while anything is outstanding
    # (commits, uncommitted changes, an open PR) — a closed bead with real work still in flight
    # is a contradiction to report, not to smooth over, so it falls through and is noted below.
    if [ "$LABELED" = yes ]; then
      _done_family "the bead is closed and nothing is outstanding on this branch, and the worktree is flagged ready-for-worktree-delete" \
                   "nothing to do here — baton:cleanup-worktrees will pick this worktree up"
    else
      _done_family "the bead is closed and nothing is outstanding on this branch, but no ready-for-worktree-delete label was ever applied" \
                   "run baton:finish to apply the cleanup signal, or leave it for baton:cleanup-worktrees to ask about"
    fi

  elif [ "$PR_STATE" = OPEN ]; then
    if [ -n "$FAILING_LIST" ]; then
      # Deliberately still "waiting on the human": a red check needs a person, exactly like a
      # green one waiting for review. The distinction lives in the reason, which leads with it.
      STATE=waiting-on-human
      REASON="PR${PR_NUMBER:+ #$PR_NUMBER} is open and $FAILING_N check(s) are FAILING: $FAILING_LIST"
      [ -n "$PENDING_LIST" ] && REASON="$REASON (plus $PENDING_N still running)"
      NEXT="look at the failing checks (gh run view --log-failed), fix, and push — never merge over red"
    elif [ -n "$PENDING_LIST" ]; then
      STATE=waiting-on-ci
      REASON="PR${PR_NUMBER:+ #$PR_NUMBER} is open and $PENDING_N check(s) are still running: $PENDING_LIST"
      NEXT="wait for the checks; baton:finish once they are green (it waits by itself for an autonomous-safe leaf)"
    else
      STATE=waiting-on-human
      REASON="PR${PR_NUMBER:+ #$PR_NUMBER} is open with no failing or pending checks"
      NEXT="review and merge the PR, then run baton:finish"
    fi

  elif [ "$PR_STATE" = CLOSED ]; then
    STATE=waiting-on-human
    REASON="PR${PR_NUMBER:+ #$PR_NUMBER} was closed without merging"
    NEXT="decide what happens to this branch — reopen the PR, open a new one, or abandon the worktree"

  elif [ -n "$BLOCKERS_LIST" ]; then
    STATE=blocked
    REASON="$BLOCKERS_N open blocker(s): $BLOCKERS_LIST"
    if [ "$HAS_WORK" = yes ]; then
      REASON="$REASON; this branch already has commits of its own"
    fi
    NEXT="finish $BLOCKERS_LIST first (each has its own worktree), or re-check whether the dependency still holds"

  elif [ "$HAS_WORK" = yes ] || [ "$DIRTY" = yes ]; then
    STATE=in-progress
    if [ "$HAS_WORK" = yes ] && [ "$DIRTY" = yes ]; then
      REASON="commits on this branch that the base does not have, plus uncommitted changes; no PR yet"
    elif [ "$HAS_WORK" = yes ]; then
      REASON="commits on this branch that the base does not have; no PR yet"
    else
      REASON="uncommitted changes in the worktree; nothing committed yet"
    fi
    NEXT="keep going; run baton:pr once the acceptance criteria are met"

  elif [ "$HAS_WORK" = no ] && [ "$DIRTY" = no ] && [ -z "$PR_STATE" ]; then
    STATE=unstarted
    REASON="no commits of this branch's own, no PR, clean tree"
    NEXT="run baton:resume to pick the task up and start on the acceptance criteria"

  else
    STATE=unknown
    REASON="not enough signal to decide (merged=$MERGED, has_work=$HAS_WORK, dirty=$DIRTY, pr_state=${PR_STATE:-none})"
    NEXT="re-run from inside the worktree; if git or gh was unavailable, that is why"
  fi
fi

# --- notes: evidence that did not win the headline, and absent signals -------------------------
# Nothing observed is discarded. A blocker that lost to an open PR, a follow-up bead on a task
# that is not done yet, and any signal the caller could not compute all surface here.
case "$STATE" in
  blocked|done-followups-elsewhere) ;;
  *) [ -n "$BLOCKERS_LIST" ] && _note "$BLOCKERS_N open blocker(s) on this bead ($BLOCKERS_LIST) — not what decides the state here, but still open" ;;
esac
case "$STATE" in
  done-followups-elsewhere) ;;
  *) [ -n "$UNBLOCKS_LIST" ] && _note "finishing this unblocks $UNBLOCKS_LIST" ;;
esac
[ "$BEAD_STATUS" = closed ] && case "$STATE" in
  done-awaiting-delete|done-followups-elsewhere|merged-needs-finish) ;;
  *) _note "the bead is CLOSED while the branch still has work outstanding — one of the two is wrong" ;;
esac
[ "$BEAD_STATUS" = unknown ] && _note "the bead lookup FAILED — its status, labels and criteria are unknown, so everything above rests on git alone"
[ "$MERGED" = unknown ] && [ "$STATE" != unknown ] && _note "the merge state could not be determined; this reads the branch as unmerged, which is the safe direction"
[ "$KEEP_OPEN" = yes ] && [ "$STATE" != done-followups-elsewhere ] && _note "keep-task-open is set without a finished worktree — that label only means something alongside ready-for-worktree-delete"

# --- human phrase for the token ---------------------------------------------------------------
case "$STATE" in
  unstarted)                LABEL="unstarted" ;;
  blocked)                  LABEL="blocked" ;;
  in-progress)              LABEL="in progress" ;;
  waiting-on-ci)            LABEL="waiting on CI" ;;
  waiting-on-human)         LABEL="waiting on the human" ;;
  merged-needs-finish)      LABEL="merged, not finished" ;;
  done-awaiting-delete)     LABEL="done, waiting on worktree delete" ;;
  done-followups-elsewhere) LABEL="done, follow-up work elsewhere" ;;
  *)                        LABEL="unknown" ;;
esac

# --- render ------------------------------------------------------------------------------------
case "$FORMAT" in
  json)
    jq -n \
      --arg state "$STATE" --arg label "$LABEL" --arg reason "$REASON" \
      --arg next "$NEXT" --arg notes "$NOTES" \
      '{state:$state, label:$label, reason:$reason, next:$next,
        notes:($notes | if . == "" then [] else split("\n") end)}'
    ;;
  env)
    _q() { printf "'%s'" "$(printf '%s' "$1" | sed "s/'/'\\\\''/g")"; }
    printf 'export STATE=%s\n'        "$(_q "$STATE")"
    printf 'export STATE_LABEL=%s\n'  "$(_q "$LABEL")"
    printf 'export STATE_REASON=%s\n' "$(_q "$REASON")"
    printf 'export STATE_NEXT=%s\n'   "$(_q "$NEXT")"
    printf 'export STATE_NOTES=%s\n'  "$(_q "$NOTES")"
    ;;
esac
