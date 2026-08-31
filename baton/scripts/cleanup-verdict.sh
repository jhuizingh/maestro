#!/usr/bin/env bash
# baton — decide what baton:cleanup-worktrees may do with ONE worktree.
#
# This is the ONLY place the bucket rules live. They used to be prose in
# skills/cleanup-worktrees/SKILL.md, which is fine for describing intent and wrong for a rule
# whose failure mode is deleting work: the guard below is safety-critical, so it is executable
# and tested (scripts/test-cleanup-verdict.sh) rather than paraphrased at read time.
#
# INPUTS — all computed fresh by the caller, none of them inferred here:
#   --labels    the readiness signals for this worktree, as whitespace-separated tokens (bullet
#               formatting is tolerated). Only three are read: ready-for-worktree-delete,
#               keep-task-open, no-pr-needed.
#   --label-scope  bead (default) | branch | branch-unrecorded — WHERE those signals came from.
#               `bead` means the task's own labels, which are shared by every worktree the task
#               ever had; `branch` means this branch's entry in the task's branch registry, which
#               is not. `branch-unrecorded` means the task owns several branches and THIS one
#               records no readiness, so the task labels were deliberately NOT borrowed — the
#               caller passes no labels with it. See "LABEL SCOPE" below — the scope changes
#               nothing about the verdict, and everything about whether the verdict is about
#               THIS worktree.
#   --state     the leaf bead's status (`closed`, `open`, `in_progress`, … or `unknown` when the
#               lookup itself failed)
#   --merged    yes | no | unknown   from scripts/merge-state.sh
#   --has-work  yes | no | unknown   from scripts/merge-state.sh
#   --dirty     yes | no | unknown   `git status --porcelain` non-empty = yes
#
# Every input defaults to `unknown`, and no `unknown` can ever satisfy a green condition — an
# omitted or failed signal degrades toward keeping the worktree, never toward removing it.
#
# THE TWO SIGNAL FAMILIES:
#   * The LABEL family is intent — what a worker session asserted when it finished.
#   * The DERIVED family (state/merged/has_work/dirty) is evidence — re-read from the tracker and
#     from git by the caller, never inferred here.
# Neither is trusted alone. Labels never substitute for evidence; evidence never overrides an
# absent "I'm done".
#
# TWO LABELS ARE MODIFIERS, NOT EXTRA REQUIREMENTS. Each relaxes exactly one cross-check, and
# only that one:
#   keep-task-open  relaxes STATE   — the bead is open on purpose, so `closed` isn't required.
#   no-pr-needed    relaxes MERGED  — the task deliberately produced nothing to merge (work done
#                                     against a live system via an API, say), so there is no
#                                     merge to observe and `merged=yes` can never arrive.
#
# LABEL SCOPE — WHOSE "I'M DONE" IS THIS?
#
# The three labels live on the TASK. One task can own several worktrees over its life (an earlier
# one that finished and was labeled, then a later one opened against the same still-open task for
# follow-up work), and a task-level label is visible from all of them. skills/cleanup-worktrees
# documented the consequence as a deferred limitation: the still-active later worktree gets
# flagged as an anomaly because of a label the finished one left behind.
#
# Since 0.8.0 baton also records a per-branch registry entry on the task (see
# references/tracker.md), carrying the same three signals scoped to one branch. The caller
# prefers that entry when it exists and passes --label-scope branch; it falls back to the task's
# labels, and --label-scope bead, only for worktrees created before the registry existed.
#
# The scope does NOT change the rules — the same three labels relax the same cross-checks either
# way, and every derived signal is still recomputed from git per worktree. What it changes is
# what a removal can honestly claim. `bead` means "this task was declared done, possibly by
# different work"; `branch` means "this branch was". It is reported in $LABEL_SCOPE and named in
# the reason, so an automatic removal is never silently justified by somebody else's label.
#
# THE no-pr-needed GUARD, which is the whole reason this file is executable:
# the label relaxes MERGED **only when git independently agrees nothing is outstanding** —
# `has_work=no` (the branch adds no commits the base doesn't already have, i.e. its tip is
# already an ancestor of the base) AND a clean tree. merge-state.sh deliberately resolves the
# ambiguous "empty base..branch" case toward "not merged" so real work is never stranded; that
# bias is correct and is NOT weakened here. What the label does is let a human/worker who knows
# the work happened outside git assert the exception per task, rather than have it silently
# derived for every caller. With the guard, a wrong label costs a worktree that should have been
# kept a little longer. Without it, a wrong label would cost commits. has_work=yes is never
# relaxed by any label, so the case the bias exists for is untouched.
#
# Usage:
#   cleanup-verdict.sh [--labels <text>] [--label-scope bead|branch|branch-unrecorded] [--state <s>]
#                      [--merged <yes|no|unknown>] [--has-work <yes|no|unknown>]
#                      [--dirty <yes|no|unknown>] [--format json|env]
#
# JSON fields / env vars:
#   verdict        VERDICT         confirmed-ready | looks-done-unlabeled
#                                  | label-state-mismatch | not-ready
#   reason         VERDICT_REASON  one line, human-readable, naming the signals that decided it
#   relaxed        RELAXED         "" | state | merged | state,merged  (which cross-checks a
#                                  modifier label stood in for; report it so an auto-removal
#                                  says WHY it needed no merge)
#   labeled        LABELED         yes | no
#   keep_open      KEEP_OPEN       yes | no
#   no_pr_needed   NO_PR_NEEDED    yes | no
#   state_ok       STATE_OK        yes | no
#   merged_ok      MERGED_OK       yes | no
#   clean          CLEAN           yes | no
#   label_scope    LABEL_SCOPE     bead | branch | branch-unrecorded — which signal family answered
#
# Exit status: 0 whenever a verdict was reached, INCLUDING `not-ready` — a red signal is an
# answer, not a failure. Non-zero only for a usage error.
#
# Requires: bash, jq (for --format json only).

set -uo pipefail

FORMAT=json
LABELS=""; STATE=unknown; MERGED=unknown; HAS_WORK=unknown; DIRTY=unknown
LABEL_SCOPE=bead

_die() { echo "cleanup-verdict: $*" >&2; exit 1; }

while [ $# -gt 0 ]; do
  case "$1" in
    --labels)   LABELS="${2:-}";      shift 2 || _die "option '$1' needs a value" ;;
    --label-scope) LABEL_SCOPE="${2:-}"; shift 2 || _die "option '$1' needs a value" ;;
    --state)    STATE="${2:-}";       shift 2 || _die "option '$1' needs a value" ;;
    --merged)   MERGED="${2:-}";      shift 2 || _die "option '$1' needs a value" ;;
    --has-work) HAS_WORK="${2:-}";    shift 2 || _die "option '$1' needs a value" ;;
    --dirty)    DIRTY="${2:-}";       shift 2 || _die "option '$1' needs a value" ;;
    --format)   FORMAT="${2:-json}";  shift 2 || _die "option '$1' needs a value" ;;
    -h|--help)  sed -n '2,93p' "$0"; exit 0 ;;
    *) _die "unknown argument '$1'" ;;
  esac
done

case "$FORMAT" in json|env) ;; *) _die "unknown --format '$FORMAT' (want json or env)" ;; esac
[ -n "$LABEL_SCOPE" ] || LABEL_SCOPE=bead
case "$LABEL_SCOPE" in bead|branch|branch-unrecorded) ;;
  *) _die "unknown --label-scope '$LABEL_SCOPE' (want bead, branch or branch-unrecorded)" ;;
esac

# An empty value is not a third state — it is a signal the caller failed to compute, which is
# exactly what `unknown` means. Collapsing it here keeps every comparison below a positive test.
[ -n "$STATE" ]    || STATE=unknown
[ -n "$MERGED" ]   || MERGED=unknown
[ -n "$HAS_WORK" ] || HAS_WORK=unknown
[ -n "$DIRTY" ]    || DIRTY=unknown

for v in MERGED HAS_WORK DIRTY; do
  case "${!v}" in yes|no|unknown) ;; *) _die "--$(echo "$v" | tr 'A-Z_' 'a-z-') must be yes, no or unknown (got '${!v}')" ;; esac
done

# --- read the label family -----------------------------------------------------------------
# Exact token match, never a substring: `grep -w` treats `-` as a word separator, so a pattern
# like `no-pr-needed` would also match inside a hypothetical `assume-no-pr-needed`. Splitting on
# whitespace and comparing whole tokens can't do that. bd's bullet formatting ("  - <label>")
# splits into a bare `-` plus the label, so its output can be passed through verbatim.
_has() {
  local want="$1" tok rc=1
  # `set -f` around the split: word splitting is wanted here, globbing is not. Unquoted, a label
  # containing `*` or `?` expands against the current directory before the comparison — which
  # loses a real label rather than inventing one, but loses it silently and only on some machines.
  set -f
  for tok in $LABELS; do [ "$tok" = "$want" ] && { rc=0; break; }; done
  set +f
  return $rc
}

LABELED=no;      _has ready-for-worktree-delete && LABELED=yes
KEEP_OPEN=no;    _has keep-task-open            && KEEP_OPEN=yes
NO_PR_NEEDED=no; _has no-pr-needed              && NO_PR_NEEDED=yes

# --- evaluate the three cross-checks ---------------------------------------------------------
RELAXED=""
_relax() { RELAXED="${RELAXED:+$RELAXED,}$1"; }

CLEAN=no
[ "$DIRTY" = no ] && CLEAN=yes

STATE_OK=no
if [ "$STATE" = closed ]; then
  STATE_OK=yes
elif [ "$KEEP_OPEN" = yes ]; then
  STATE_OK=yes; _relax state
fi

MERGED_OK=no
if [ "$MERGED" = yes ]; then
  MERGED_OK=yes
elif [ "$NO_PR_NEEDED" = yes ] && [ "$MERGED" = no ] && [ "$HAS_WORK" = no ] && [ "$CLEAN" = yes ]; then
  # THE GUARD. All four conjuncts matter:
  #   no-pr-needed  — somebody asserted the exception deliberately
  #   merged=no     — a definite answer, not `unknown` (a missing/stale merge-state.sh helper
  #                   reports unknown, and must not be relaxed into a removal)
  #   has_work=no   — git agrees the branch adds nothing the base doesn't already have
  #   clean         — nothing uncommitted is about to be thrown away with the worktree
  MERGED_OK=yes; _relax merged
fi

# --- classify --------------------------------------------------------------------------------
_why() { # collect the signals that are red, for the reason line
  local out=""
  [ "$STATE_OK"  = yes ] || out="$out; bead is $STATE (no keep-task-open)"
  [ "$MERGED_OK" = yes ] || {
    if [ "$NO_PR_NEEDED" = yes ]; then
      out="$out; no-pr-needed did not apply (merged=$MERGED, has_work=$HAS_WORK, dirty=$DIRTY)"
    else
      out="$out; branch not merged (merged=$MERGED)"
    fi
  }
  [ "$CLEAN"     = yes ] || out="$out; working tree dirty"
  printf '%s' "${out#; }"
}

RED="$(_why)"
if [ "$LABELED" = yes ]; then
  if [ -z "$RED" ]; then
    VERDICT=confirmed-ready
    case "$RELAXED" in
      *merged*) VERDICT_REASON="labeled ready; nothing to merge by assertion (no-pr-needed) and git agrees (has_work=no); tree clean" ;;
      *)        VERDICT_REASON="labeled ready; bead $STATE, branch merged, tree clean" ;;
    esac
    [ "$KEEP_OPEN" = yes ] && VERDICT_REASON="$VERDICT_REASON; open on purpose (keep-task-open)"
  else
    VERDICT=label-state-mismatch
    VERDICT_REASON="labeled ready but $RED"
  fi
elif [ "$STATE" = closed ] && [ "$MERGED_OK" = yes ] && [ "$CLEAN" = yes ]; then
  VERDICT=looks-done-unlabeled
  VERDICT_REASON="bead closed and nothing outstanding, but no ready-for-worktree-delete label"
  case "$RELAXED" in *merged*) VERDICT_REASON="$VERDICT_REASON (merge relaxed by no-pr-needed)" ;; esac
else
  VERDICT=not-ready
  VERDICT_REASON="${RED:-nothing to flag}"
  [ "$LABELED" = no ] && VERDICT_REASON="$VERDICT_REASON; unlabeled"
fi

# Say whose "I'm done" this was whenever it was the branch's own. The bead-scoped case is the
# default and the old behaviour, so it stays unannotated — a line on every row would be noise.
[ "$LABEL_SCOPE" = branch ] && [ "$LABELED" = yes ] \
  && VERDICT_REASON="$VERDICT_REASON [readiness recorded against THIS branch, not the bead]"

# A failed bead lookup is not an ordinary "still open" — say so wherever it lands, because every
# other classification for this worktree is unreliable too. It is safe by construction (`unknown`
# can never equal `closed`), but it must never be reported as though the bead were merely open.
[ "$STATE" = unknown ] && VERDICT_REASON="$VERDICT_REASON [bead lookup FAILED — state unknown]"

# --- render -----------------------------------------------------------------------------------
case "$FORMAT" in
  json)
    jq -n \
      --arg verdict "$VERDICT" --arg reason "$VERDICT_REASON" --arg relaxed "$RELAXED" \
      --arg labeled "$LABELED" --arg keep_open "$KEEP_OPEN" --arg no_pr_needed "$NO_PR_NEEDED" \
      --arg state_ok "$STATE_OK" --arg merged_ok "$MERGED_OK" --arg clean "$CLEAN" \
      --arg label_scope "$LABEL_SCOPE" \
      '{verdict:$verdict, reason:$reason, relaxed:$relaxed, labeled:$labeled,
        keep_open:$keep_open, no_pr_needed:$no_pr_needed,
        state_ok:$state_ok, merged_ok:$merged_ok, clean:$clean, label_scope:$label_scope}'
    ;;
  env)
    _q() { printf "'%s'" "$(printf '%s' "$1" | sed "s/'/'\\\\''/g")"; }
    printf 'export VERDICT=%s\n'        "$(_q "$VERDICT")"
    printf 'export VERDICT_REASON=%s\n' "$(_q "$VERDICT_REASON")"
    printf 'export RELAXED=%s\n'        "$(_q "$RELAXED")"
    printf 'export LABELED=%s\n'        "$(_q "$LABELED")"
    printf 'export KEEP_OPEN=%s\n'      "$(_q "$KEEP_OPEN")"
    printf 'export NO_PR_NEEDED=%s\n'   "$(_q "$NO_PR_NEEDED")"
    printf 'export STATE_OK=%s\n'       "$(_q "$STATE_OK")"
    printf 'export MERGED_OK=%s\n'      "$(_q "$MERGED_OK")"
    printf 'export CLEAN=%s\n'          "$(_q "$CLEAN")"
    printf 'export LABEL_SCOPE=%s\n'    "$(_q "$LABEL_SCOPE")"
    ;;
esac
