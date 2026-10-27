#!/usr/bin/env bash
# baton — tests for scripts/cleanup-verdict.sh and the merge-state signals it consumes.
#
# baton:cleanup-worktrees removes a worktree and deletes its branch on this verdict, so the
# rules are pinned here rather than left to a careful reading of the skill's prose. The three
# shapes that matter are indistinguishable to git and must NOT be treated alike:
#
#   c6l shape        work done, but deliberately outside git (an API against a live system):
#                    zero commits, no PR, bead closed and labeled  -> MUST be cleanable
#   fresh-unstarted  a worktree nobody has committed to yet, bead open and unlabeled
#                    -> MUST be kept; this is the regression that matters most
#   real work        commits the base does not have -> MUST never be removed, label or not
#
# The first two produce byte-identical merge-state output (asserted below). That is the whole
# problem: only the deliberate `no-pr-needed` assertion plus the bead's own state separates them,
# which is why the label exists and why it is guarded by has_work.
#
# Hermetic: builds throwaway git repos under a temp dir, stubs `gh` so nothing touches the
# network, touches nothing outside the temp dir, and needs only git and jq. Exit 0 = all passed.

set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
VERDICT="$HERE/cleanup-verdict.sh"
MS="$HERE/merge-state.sh"
for f in "$VERDICT" "$MS"; do [ -r "$f" ] || { echo "not found: $f" >&2; exit 2; }; done

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
PASS=0; FAIL=0

_ok()  { PASS=$((PASS+1)); printf '  ✅ %s\n' "$1"; }
_bad() { FAIL=$((FAIL+1)); printf '  ❌ %s\n     %s\n' "$1" "$2"; }
_eq()  { # $1 = what, $2 = got, $3 = want
  if [ "$2" = "$3" ]; then _ok "$1"; else _bad "$1" "got '$2', want '$3'"; fi
}

# A `gh` that answers "this branch has no PR" — the c6l situation, and offline by construction.
# Without it merge-state.sh would shell out to the real gh against a repo with no remote, making
# the result depend on the machine's network and login state.
STUBBIN="$TMP/bin"; mkdir -p "$STUBBIN"
cat >"$STUBBIN/gh" <<'EOF'
#!/usr/bin/env bash
echo "no pull requests found for branch" >&2
exit 1
EOF
chmod +x "$STUBBIN/gh"

_ms() { # $1 = repo, $2 = branch -> prints "merged/has_work"
  local out
  out="$(PATH="$STUBBIN:$PATH" bash "$MS" --repo "$1" --branch "$2" --no-fetch --format env 2>/dev/null)"
  ( eval "$out"; printf '%s/%s' "${MERGED:-?}" "${HAS_WORK:-?}" )
}

_v() { # verdict flags... -> prints "verdict|relaxed"
  bash "$VERDICT" "$@" | jq -r '.verdict + "|" + .relaxed'
}

_repo() { # $1 = name -> prints its path, with one commit and origin/main pointing at it
  local r="$TMP/$1"
  git init -q "$r"
  git -C "$r" config user.email t@t.t; git -C "$r" config user.name T
  : >"$r/f"; git -C "$r" add f; git -C "$r" commit -qm init
  git -C "$r" branch -M main          # don't inherit the machine's init.defaultBranch
  git -C "$r" update-ref refs/remotes/origin/main HEAD
  printf '%s' "$r"
}

_commit() { # $1 = repo, $2 = branch, $3 = message
  git -C "$1" checkout -q "$2"
  echo "$3" >>"$1/f"; git -C "$1" commit -qam "$3"
}

# ============================================================================================
echo "merge-state signals for the shapes that look alike"

# --- the c6l shape: branch cut, nothing ever committed on it, base moved on without it --------
R="$(_repo c6l)"
git -C "$R" branch nothing-to-merge
_commit "$R" main "someone else's work"
git -C "$R" update-ref refs/remotes/origin/main main
_eq "c6l shape: no commits of its own, base moved on -> merged=no, has_work=no" \
    "$(_ms "$R" nothing-to-merge)" "no/no"

# --- fresh-unstarted: git cannot tell it from the c6l shape ------------------------------------
git -C "$R" branch fresh-unstarted
_eq "fresh-unstarted shape: identical merge-state output (this is the ambiguity)" \
    "$(_ms "$R" fresh-unstarted)" "$(_ms "$R" nothing-to-merge)"

# --- real unmerged work -------------------------------------------------------------------------
git -C "$R" branch real-work
_commit "$R" real-work "a commit the base does not have"
git -C "$R" checkout -q main
_eq "real work: merged=no, has_work=yes" "$(_ms "$R" real-work)" "no/yes"

# --- merged by a merge commit (the ordinary path) -------------------------------------------------
git -C "$R" branch landed
_commit "$R" landed "work that will land"
git -C "$R" checkout -q main
git -C "$R" merge -q --no-ff -m "merge landed" landed
git -C "$R" update-ref refs/remotes/origin/main main
_eq "merged via merge commit: merged=yes, has_work=no" "$(_ms "$R" landed)" "yes/no"

# --- fast-forwarded with no PR: the documented limit the label exists to resolve --------------------
git -C "$R" branch ff-landed
_commit "$R" ff-landed "work that will fast-forward"
git -C "$R" checkout -q main
git -C "$R" merge -q --ff-only ff-landed
git -C "$R" update-ref refs/remotes/origin/main main
_eq "fast-forwarded, no PR: reported merged=no on purpose (bias toward not-merged)" \
    "$(_ms "$R" ff-landed)" "no/no"

# ============================================================================================
echo "cleanup-verdict — the three shapes end to end"

READY=ready-for-worktree-delete

_eq "c6l shape + closed + labeled + no-pr-needed -> confirmed-ready, merge relaxed" \
    "$(_v --labels "$READY no-pr-needed" --state closed --merged no --has-work no --dirty no)" \
    "confirmed-ready|merged"

_eq "fresh-unstarted (open, unlabeled), same git signals -> KEPT" \
    "$(_v --labels "" --state open --merged no --has-work no --dirty no)" \
    "not-ready|"

_eq "fresh-unstarted, in_progress -> KEPT" \
    "$(_v --labels "" --state in_progress --merged no --has-work no --dirty no)" \
    "not-ready|"

_eq "real unmerged work + BOTH labels -> mismatch, never removable" \
    "$(_v --labels "$READY no-pr-needed" --state closed --merged no --has-work yes --dirty no)" \
    "label-state-mismatch|"

_eq "real unmerged work, unlabeled -> KEPT" \
    "$(_v --labels "" --state open --merged no --has-work yes --dirty no)" \
    "not-ready|"

# ============================================================================================
echo "the guard: no-pr-needed relaxes MERGED and nothing else"

_eq "no-pr-needed + dirty tree -> mismatch (uncommitted work must not be swept)" \
    "$(_v --labels "$READY no-pr-needed" --state closed --merged no --has-work no --dirty yes)" \
    "label-state-mismatch|"

_eq "no-pr-needed + has_work=unknown -> mismatch (an unknown is not a 'no')" \
    "$(_v --labels "$READY no-pr-needed" --state closed --merged no --has-work unknown --dirty no)" \
    "label-state-mismatch|"

_eq "no-pr-needed + merged=unknown (helper missing) -> mismatch" \
    "$(_v --labels "$READY no-pr-needed" --state closed --merged unknown --has-work no --dirty no)" \
    "label-state-mismatch|"

# The merge cross-check is legitimately relaxed here (and `relaxed` says so), but STATE is red
# and no label covers it — proving the relaxation is scoped to MERGED alone.
_eq "no-pr-needed does NOT relax STATE: open bead, no keep-task-open -> mismatch" \
    "$(_v --labels "$READY no-pr-needed" --state open --merged no --has-work no --dirty no)" \
    "label-state-mismatch|merged"
_eq "…and the reason names the open bead as what disagreed" \
    "$(bash "$VERDICT" --labels "$READY no-pr-needed" --state open --merged no --has-work no --dirty no \
       | jq -r '.reason | test("bead is open") | tostring')" "true"

_eq "no-pr-needed + keep-task-open relaxes both, and says so" \
    "$(_v --labels "$READY no-pr-needed keep-task-open" --state open --merged no --has-work no --dirty no)" \
    "confirmed-ready|state,merged"

_eq "a bare 'no' merge with no label is still not merged" \
    "$(_v --labels "$READY" --state closed --merged no --has-work no --dirty no)" \
    "label-state-mismatch|"

# ============================================================================================
echo "pre-existing buckets are unchanged"

_eq "all four signals agree -> confirmed-ready, nothing relaxed" \
    "$(_v --labels "$READY" --state closed --merged yes --has-work no --dirty no)" \
    "confirmed-ready|"

_eq "keep-task-open covers an open bead" \
    "$(_v --labels "$READY keep-task-open" --state open --merged yes --has-work no --dirty no)" \
    "confirmed-ready|state"

_eq "closed + merged + clean but unlabeled -> looks-done-unlabeled" \
    "$(_v --labels "" --state closed --merged yes --has-work no --dirty no)" \
    "looks-done-unlabeled|"

_eq "labeled but reopened -> mismatch" \
    "$(_v --labels "$READY" --state open --merged yes --has-work no --dirty no)" \
    "label-state-mismatch|"

_eq "labeled but dirty -> mismatch" \
    "$(_v --labels "$READY" --state closed --merged yes --has-work no --dirty yes)" \
    "label-state-mismatch|"

_eq "a failed bead lookup can never be confirmed-ready" \
    "$(_v --labels "$READY" --state unknown --merged yes --has-work no --dirty no)" \
    "label-state-mismatch|"
_eq "…and it says the lookup failed rather than implying the bead is merely open" \
    "$(bash "$VERDICT" --labels "$READY" --state unknown --merged yes --has-work no --dirty no \
       | jq -r '.reason | test("bead lookup FAILED") | tostring')" "true"

# ============================================================================================
echo "input hygiene"

_eq "no flags at all -> everything unknown -> not-ready" \
    "$(_v)" "not-ready|"

_eq "a label that merely CONTAINS a known one does not count" \
    "$(_v --labels "assume-no-pr-needed $READY" --state closed --merged no --has-work no --dirty no)" \
    "label-state-mismatch|"

_eq "bd's bullet output is accepted verbatim" \
    "$(_v --labels "$(printf '🏷 Labels for jbh-c6l:\n  - cleanup\n  - %s\n  - no-pr-needed\n' "$READY")" \
         --state closed --merged no --has-work no --dirty no)" \
    "confirmed-ready|merged"

# ============================================================================================
# LABEL SCOPE. The three labels live on the TASK, so every worktree that task ever had sees
# them; the branch registry records the same signals against ONE branch. --label-scope says
# which family the caller read, and must change what the verdict CLAIMS without changing the
# rules — an identical signal set has to reach an identical verdict either way, or "prefer the
# registry when it exists" would silently mean two different safety policies.
echo "label scope (bead vs branch)"

for scope in bead branch; do
  _eq "scope=$scope: the same signals reach the same verdict (ready + all green)" \
      "$(_v --label-scope "$scope" --labels "$READY" --state closed --merged yes --has-work no --dirty no)" \
      "confirmed-ready|"
  _eq "scope=$scope: real unmerged commits are still never removable" \
      "$(_v --label-scope "$scope" --labels "$READY no-pr-needed" --state closed --merged no --has-work yes --dirty no)" \
      "label-state-mismatch|"
  _eq "scope=$scope: an unlabeled, unstarted branch is still kept" \
      "$(_v --label-scope "$scope" --labels "" --state open --merged no --has-work no --dirty no)" \
      "not-ready|"
done

_eq "the scope is reported so a caller can say which family answered" \
    "$(bash "$VERDICT" --label-scope branch --labels "$READY" --state closed --merged yes \
        --has-work no --dirty no | jq -r .label_scope)" "branch"
_eq "…and defaults to bead, which is the pre-registry behaviour" \
    "$(bash "$VERDICT" --labels "$READY" --state closed --merged yes --has-work no --dirty no \
        | jq -r .label_scope)" "bead"
_eq "a branch-scoped removal says so in its reason, so it never borrows another worktree's label" \
    "$(bash "$VERDICT" --label-scope branch --labels "$READY" --state closed --merged yes \
        --has-work no --dirty no | jq -r '.reason | test("THIS branch") | tostring')" "true"
_eq "…and the bead-scoped default stays unannotated (a line on every row would be noise)" \
    "$(bash "$VERDICT" --labels "$READY" --state closed --merged yes --has-work no --dirty no \
        | jq -r '.reason | test("THIS branch") | tostring')" "false"
_eq "an unknown scope is a usage error, not a silent fallback to bead" \
    "$(bash "$VERDICT" --label-scope worktree >/dev/null 2>&1 && echo accepted || echo rejected)" "rejected"

# ============================================================================================
# REVIEW TASKS. A worktree on somebody else's PR: its commits are the author's (has_work=yes,
# merged=no for as long as the PR is open), and the reviewer decides when the review is over.
# Removal must therefore not wait on the merge or on approval — but must never take work of OURS
# with it: a dirty tree, or commits that exist only here (unpushed).
echo "review tasks (--kind review)"

_rv() { _v --kind review --merged no --has-work yes "$@"; }

for pr in OPEN MERGED CLOSED ""; do
  _eq "closed + ready + clean + nothing unpushed -> removed, PR '${pr:-none}'" \
      "$(_rv --labels "$READY" --state closed --dirty no --unpushed no --pr-state "$pr")" \
      "confirmed-ready|"
done
_eq "an OPEN PR we requested changes on is still removed once the reviewer closed it" \
    "$(_rv --labels "$READY" --state closed --dirty no --unpushed no --pr-state OPEN)" \
    "confirmed-ready|"
_eq "ready, but commits of ours only here -> flagged, never removed" \
    "$(_rv --labels "$READY" --state closed --dirty no --unpushed yes --pr-state OPEN)" \
    "label-state-mismatch|"
_eq "ready, but a dirty tree -> flagged" \
    "$(_rv --labels "$READY" --state closed --dirty yes --unpushed no --pr-state MERGED)" \
    "label-state-mismatch|"
_eq "ready, but unpushed could not be read -> flagged (unknown never removes)" \
    "$(_rv --labels "$READY" --state closed --dirty no --pr-state MERGED)" \
    "label-state-mismatch|"
_eq "ready, but the bead was reopened -> flagged" \
    "$(_rv --labels "$READY" --state open --dirty no --unpushed no --pr-state OPEN)" \
    "label-state-mismatch|"
for pr in MERGED CLOSED; do
  _eq "unlabeled, PR $pr, nothing of ours -> offered, never auto-removed" \
      "$(_rv --labels "" --state open --dirty no --unpushed no --pr-state "$pr")" \
      "looks-done-unlabeled|"
done
_eq "unlabeled, PR MERGED, but commits of ours -> kept" \
    "$(_rv --labels "" --state open --dirty no --unpushed yes --pr-state MERGED)" \
    "not-ready|"
_eq "unlabeled, PR CLOSED, dirty -> kept" \
    "$(_rv --labels "" --state open --dirty yes --unpushed no --pr-state CLOSED)" \
    "not-ready|"
_eq "unlabeled, PR OPEN -> kept" \
    "$(_rv --labels "" --state in_progress --dirty no --unpushed no --pr-state OPEN)" \
    "not-ready|"
_eq "no-pr-needed relaxes nothing for a review" \
    "$(_rv --labels "$READY no-pr-needed" --state closed --dirty no --unpushed yes --pr-state OPEN)" \
    "label-state-mismatch|"
_eq "a merged PR does not stand in for unpushed=no" \
    "$(_v --kind review --merged yes --has-work no --labels "$READY" --state closed --dirty no --pr-state MERGED)" \
    "label-state-mismatch|"
_eq "the kind is reported" \
    "$(bash "$VERDICT" --kind review | jq -r .kind)" "review"
_eq "an unknown --kind is a usage error" \
    "$(bash "$VERDICT" --kind pr >/dev/null 2>&1 && echo accepted || echo rejected)" "rejected"
_eq "an unknown --pr-state is a usage error" \
    "$(bash "$VERDICT" --kind review --pr-state merged >/dev/null 2>&1 && echo accepted || echo rejected)" "rejected"

# A context that never starts a review must see exactly the old rules: --kind task (the default)
# ignores --unpushed and --pr-state, whatever they say.
echo "review flags change nothing for an ordinary task"
SAME=yes
for lbl in "" "$READY" "$READY no-pr-needed" "$READY keep-task-open"; do
  for st in closed open; do
    for m in "yes no" "no no" "no yes" "unknown unknown"; do
      for d in no yes; do
        set -- $m
        base="$(_v --labels "$lbl" --state "$st" --merged "$1" --has-work "$2" --dirty "$d")"
        for extra in "--unpushed yes --pr-state MERGED" "--unpushed no --pr-state CLOSED" "--kind task --unpushed unknown"; do
          # shellcheck disable=SC2086
          got="$(_v --labels "$lbl" --state "$st" --merged "$1" --has-work "$2" --dirty "$d" $extra)"
          [ "$got" = "$base" ] || { SAME=no; _bad "task verdict moved with '$extra'" "$lbl/$st/$m/$d: $base -> $got"; }
        done
      done
    done
  done
done
_eq "every task signal combination reaches the same verdict with or without review flags" "$SAME" "yes"

# ============================================================================================
echo "input hygiene (continued)"

_eq "an out-of-range --merged is a usage error, not a silent pass" \
    "$(bash "$VERDICT" --merged maybe >/dev/null 2>&1 && echo accepted || echo rejected)" "rejected"

_eq "not-ready still exits 0 (a red signal is an answer, not a failure)" \
    "$(bash "$VERDICT" >/dev/null 2>&1 && echo 0 || echo nonzero)" "0"

echo "branch guard: the delete asks for what the verdict relaxed, never for a merge it knew was not coming (jbh-p686)"
_g() { bash "$VERDICT" "$@" | jq -r '.branch_guard'; }
_eq "ordinary merged removal -> merged" \
    "$(_g --labels "$READY" --state closed --merged yes --has-work no --dirty no)" "merged"
_eq "no-pr-needed relaxed the merge -> no-work" \
    "$(_g --labels "$READY no-pr-needed" --state closed --merged no --has-work no --dirty no)" "no-work"
_eq "no-pr-needed + keep-task-open (state,merged) -> no-work" \
    "$(_g --labels "$READY no-pr-needed keep-task-open" --state open --merged no --has-work no --dirty no)" "no-work"
_eq "no-pr-needed but real work: label did not apply -> merged, never no-work" \
    "$(_g --labels "$READY no-pr-needed" --state closed --merged no --has-work yes --dirty no)" "merged"
_eq "keep-task-open alone (state relaxed, merge not) -> merged" \
    "$(_g --labels "$READY keep-task-open" --state open --merged yes --has-work no --dirty no)" "merged"
_eq "review -> pinned" \
    "$(_g --kind review --labels "$READY" --state closed --merged no --has-work yes --dirty no --unpushed no)" "pinned"
_eq "env format carries BRANCH_GUARD" \
    "$(bash "$VERDICT" --labels "$READY no-pr-needed" --state closed --merged no --has-work no --dirty no --format env | grep '^export BRANCH_GUARD=')" \
    "export BRANCH_GUARD='no-work'"
# The skill must hand the guard through, not pick one itself — the 0.16.1 bug lived in that prose.
SKILL="$HERE/../skills/cleanup-worktrees/SKILL.md"
_eq "cleanup Step 4 deletes with the verdict's guard" \
    "$(grep -c -- '--guard "$BRANCH_GUARD" --expect "$BR_SHA"' "$SKILL")" "1"
_eq "…and no longer hard-codes --require-merged for a removed worktree's branch" \
    "$(grep -c -- 'delete --repo <repo> --branch "$BR" --require-merged' "$SKILL")" "0"

echo
echo "$PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
