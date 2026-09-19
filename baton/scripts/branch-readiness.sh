#!/usr/bin/env bash
# baton — read ONE branch's readiness out of a task's branch registry.
#
# This is the ONLY place the "which registry entry is this worktree's, and does it say anything
# about readiness" rule lives. It used to be the same jq expression pasted into
# skills/cleanup-worktrees/SKILL.md and skills/status/SKILL.md with a note that the two must stay
# byte-for-byte identical — and both copies were wrong in the same way for a whole release:
# they compared the entry's `repo` against the member repo's ABSOLUTE PATH, while `baton:start`
# records the repo's NAME. The match never succeeded, so every worktree started since 0.8.0
# silently fell through to the task-label fallback and the per-branch readiness the registry
# exists to provide was never read (jbh-7xb8). A rule two skills must agree on, whose failure is
# silent, belongs in one executable place with a test — not in two prose blocks.
#
# THE KEY. A registry entry's `repo` is the member repo's NAME (its basename — `maestro`, not
# `/Users/x/code/maestro`): the registry is shared through the tracker across machines, and a
# path is only meaningful on the one that wrote it. See references/tracker.md, "Branch entry".
#
# Callers, however, hold the PATH — it is what `git -C` and merge-state.sh take — so --repo
# accepts either and reduces it to the name. The stored side is reduced the same way, so an
# entry written with a path (by hand, or by a caller that got it wrong) still matches. Both
# reductions are `basename`, so `maestro`, `/a/maestro` and `/b/maestro/` are the same repo.
#
# INPUT — the task's folded registry (`tracker.sh list-branches <id>`), a JSON array on stdin.
#
# THREE OUTCOMES, and the middle one is the reason this is not a two-way `if`:
#
#   branch             this branch's entry records readiness (`ready`, `keep_task_open` or
#                      `no_pr_needed`): LABELS is the bead-label spelling of those fields, and
#                      the answer is about THIS branch.
#   bead               no readiness recorded for this branch, and the task owns at most one
#                      recorded branch — so its task labels can only be about this one. LABELS
#                      is --task-labels passed through. This is the documented fallback for a
#                      pre-0.8.0 worktree, a backend with no registry, a freshly-started
#                      worktree, or a `baton:finish` whose `update-branch` write failed.
#   branch-unrecorded  no readiness recorded for this branch, and the task owns several — the
#                      task labels may well have been applied for a sibling, so they are NOT
#                      borrowed and LABELS is empty. The caller lands the worktree somewhere
#                      that asks a human rather than one that deletes.
#
# An entry EXISTING is not the middle question. `record-branch` writes repo/branch/worktree/
# created/status and none of the readiness fields, so every worktree baton has created has an
# entry, and testing for the entry rather than the fields would make the `bead` fallback
# unreachable — exactly the case it is for.
#
# Usage:
#   branch-readiness.sh --branch <br> --repo <path-or-name> [--task-labels <text>] [--format json|env]
#
# JSON fields / env vars:
#   label_scope  LABEL_SCOPE  branch | bead | branch-unrecorded
#   labels       LABELS       whitespace-separated readiness tokens for cleanup-verdict.sh
#   entry        REG_ENTRY    the matched entry (JSON object), or {} / empty when none matched
#   entry_found  REG_FOUND    yes | no — an entry exists for this repo+branch (readiness or not)
#   branches     NBR          how many branches the task has on record, all repos
#   repo_key     REPO_KEY     the name the match was made on
#
# Requires: jq.

set -uo pipefail

BR=""; REPO=""; TASK_LABELS=""; FORMAT=json

_die() { echo "branch-readiness: $*" >&2; exit 2; }

while [ $# -gt 0 ]; do
  case "$1" in
    --branch)      BR="${2:-}";          shift 2 || _die "option '$1' needs a value" ;;
    --repo)        REPO="${2:-}";        shift 2 || _die "option '$1' needs a value" ;;
    --task-labels) TASK_LABELS="${2:-}"; shift 2 || _die "option '$1' needs a value" ;;
    --format)      FORMAT="${2:-}";      shift 2 || _die "option '$1' needs a value" ;;
    -h|--help)     sed -n '2,/^$/p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) _die "unknown option '$1'" ;;
  esac
done
[ -n "$BR" ]   || _die "--branch is required"
[ -n "$REPO" ] || _die "--repo is required"
case "$FORMAT" in json|env) ;; *) _die "--format must be json or env" ;; esac
command -v jq >/dev/null 2>&1 || _die "jq is required"

# The canonical key. `basename` on a bare name is the name, so a caller may pass either form.
REPO_KEY="$(basename "${REPO%/}")"

REGS="$(cat)"; [ -n "$REGS" ] || REGS='[]'
jq -e 'type == "array"' >/dev/null 2>&1 <<<"$REGS" || REGS='[]'   # garbage in reads as "no registry"

jq -c --arg br "$BR" --arg repo "$REPO_KEY" --arg task_labels "$TASK_LABELS" '
  # An entry matches when it names this branch and its repo reduces to the same NAME — or names
  # no repo at all: an entry written by `update-branch` alone, with no `record-branch` before it
  # (a pre-0.8.0 worktree finished by a post-0.8.0 baton:finish), carries none, and it can only
  # be about the one repo the task has a branch of that name in.
  # (`"" | split("/")` is `[]` in jq, so `last` alone would turn a missing repo into null.)
  def repo_name: ((. // "") | rtrimstr("/") | split("/") | last) // "";
  . as $regs
  | [ $regs[]? | select(.branch == $br and ((.repo | repo_name) == $repo or (.repo | repo_name) == "")) ]
  | first // {} | . as $reg
  | ($regs | length) as $nbr
  | ($reg | has("ready") or has("keep_task_open") or has("no_pr_needed")) as $recorded
  | ( if $recorded then "branch"
      elif $nbr <= 1 then "bead"
      else "branch-unrecorded" end ) as $scope
  | ( if $scope == "branch" then
        ( [ ($reg | select(.ready == "yes")          | "ready-for-worktree-delete"),
            ($reg | select(.keep_task_open == "yes") | "keep-task-open"),
            ($reg | select(.no_pr_needed == "yes")   | "no-pr-needed") ] | join(" ") )
      elif $scope == "bead" then $task_labels
      else "" end ) as $labels
  | { label_scope: $scope,
      labels:      $labels,
      entry:       $reg,
      entry_found: (if ($reg | length) > 0 then "yes" else "no" end),
      branches:    $nbr,
      repo_key:    $repo }
' <<<"$REGS" | if [ "$FORMAT" = env ]; then
  jq -r '@sh "export LABEL_SCOPE=\(.label_scope)",
         @sh "export LABELS=\(.labels)",
         @sh "export REG_ENTRY=\(if (.entry | length) > 0 then (.entry | tojson) else "" end)",
         @sh "export REG_FOUND=\(.entry_found)",
         @sh "export NBR=\(.branches | tostring)",
         @sh "export REPO_KEY=\(.repo_key)"'
else
  cat
fi
