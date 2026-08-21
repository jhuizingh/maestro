#!/usr/bin/env bash
# baton — tests for scripts/task-state.sh, the state vocabulary baton:status renders.
#
# Nothing acts on this state, so a wrong answer costs a misleading report rather than lost work.
# It is pinned anyway, because the report's whole job is to be believed without re-deriving it,
# and three of the rows are shapes that look identical from the outside:
#
#   unstarted vs done-outside-git   both are "no commits, no PR, clean tree" — merge-state.sh
#                                   cannot tell them apart, and only the no-pr-needed assertion
#                                   (via cleanup-verdict.sh) does. Reporting finished work as
#                                   `unstarted` is the regression that matters most here.
#   merged-needs-finish vs done     both have a merged branch; only the cleanup label separates
#                                   "the PR landed and nobody closed it out" from "finished".
#   waiting-on-human vs -on-ci      a red check and a green one are both a human's problem; a
#                                   still-running one is not. The red case must never be silent.
#
# The last block chains the REAL cleanup-verdict.sh into task-state.sh, so the two scripts'
# shared vocabulary (the four verdict tokens) is verified rather than assumed. A typo there
# would send every finished task down the evidence ladder and report it as unstarted.
#
# Hermetic: the unit blocks need nothing but bash and jq (the script under test is a pure
# function); the end-to-end block at the bottom builds throwaway git repos under a temp dir and
# stubs `gh`, so it touches nothing outside that dir and never reaches the network. It is skipped
# rather than failed where git is absent. Exit 0 = all passed.

set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
TS="$HERE/task-state.sh"
CV="$HERE/cleanup-verdict.sh"
for f in "$TS" "$CV"; do [ -r "$f" ] || { echo "not found: $f" >&2; exit 2; }; done

PASS=0; FAIL=0
_ok()  { PASS=$((PASS+1)); printf '  ✅ %s\n' "$1"; }
_bad() { FAIL=$((FAIL+1)); printf '  ❌ %s\n     %s\n' "$1" "$2"; }
_eq()  { if [ "$2" = "$3" ]; then _ok "$1"; else _bad "$1" "got '$2', want '$3'"; fi }

READY=ready-for-worktree-delete

_s() { bash "$TS" "$@" | jq -r '.state'; }                        # -> state token
_r() { bash "$TS" "$@" | jq -r '.reason'; }                       # -> reason line
_n() { bash "$TS" "$@" | jq -r '.notes | join(" ~ ")'; }          # -> notes, flattened

# ============================================================================================
echo "the evidence ladder"

_eq "clean tree, no commits, no PR -> unstarted" \
    "$(_s --verdict not-ready --status open --merged no --has-work no --dirty no)" \
    "unstarted"

_eq "commits but no PR -> in-progress" \
    "$(_s --verdict not-ready --status in_progress --merged no --has-work yes --dirty no)" \
    "in-progress"

_eq "no commits but a dirty tree is still in-progress, not unstarted" \
    "$(_s --verdict not-ready --status in_progress --merged no --has-work no --dirty yes)" \
    "in-progress"

_eq "an open PR with checks running -> waiting-on-ci" \
    "$(_s --verdict not-ready --status in_progress --merged no --has-work yes --dirty no \
          --pr-state OPEN --pr-number 42 --pending "validate")" \
    "waiting-on-ci"

_eq "an open PR with everything green -> waiting-on-human" \
    "$(_s --verdict not-ready --status in_progress --merged no --has-work yes --dirty no \
          --pr-state OPEN --pr-number 42)" \
    "waiting-on-human"

_eq "a failing check is waiting-on-human too — the vocabulary has no 'broken' state" \
    "$(_s --verdict not-ready --status in_progress --merged no --has-work yes --dirty no \
          --pr-state OPEN --pr-number 42 --failing "validate")" \
    "waiting-on-human"

_eq "...but it must LEAD with the failure, not bury it" \
    "$(_r --verdict not-ready --status in_progress --merged no --has-work yes --dirty no \
          --pr-state OPEN --pr-number 42 --failing $'validate\nidentity' \
       | grep -qi 'FAILING: validate, identity' && echo named || echo buried)" \
    "named"

_eq "failing beats pending when both are present" \
    "$(_s --verdict not-ready --merged no --has-work yes --dirty no --pr-state OPEN \
          --failing "validate" --pending "schema")" \
    "waiting-on-human"

_eq "a PR closed without merging needs a person, not a re-start" \
    "$(_s --verdict not-ready --status open --merged no --has-work yes --dirty no --pr-state CLOSED)" \
    "waiting-on-human"

# ============================================================================================
echo
echo "merged, but not finished"

_eq "merged with no cleanup label -> merged-needs-finish (NOT done)" \
    "$(_s --verdict not-ready --status in_progress --merged yes --has-work no --dirty no \
          --pr-state MERGED --pr-number 7)" \
    "merged-needs-finish"

_eq "merged with the bead already closed is still merged-needs-finish" \
    "$(_s --verdict looks-done-unlabeled --status closed --merged yes --has-work no --dirty no)" \
    "merged-needs-finish"

_eq "...and says so, so the reader knows only the label is missing" \
    "$(_r --verdict looks-done-unlabeled --status closed --merged yes --has-work no --dirty no \
       | grep -qi 'already closed' && echo mentioned || echo silent)" \
    "mentioned"

_eq "merged outranks an open PR record that disagrees" \
    "$(_s --verdict not-ready --merged yes --has-work no --dirty no --pr-state OPEN --pending "validate")" \
    "merged-needs-finish"

# ============================================================================================
echo
echo "a closed bead is never unstarted"
# The jbh-c6l shape, caught on a live worktree: bead closed, worktree flagged, zero commits by
# design (the work happened through an API), and the `no-pr-needed` label predating the feature —
# so the verdict is label-state-mismatch and the evidence ladder alone said `unstarted`, i.e.
# finished work reported as not begun.

_eq "closed + labeled + nothing outstanding -> done, not unstarted" \
    "$(_s --verdict label-state-mismatch --labels "$READY" --status closed --merged no \
          --has-work no --dirty no)" \
    "done-awaiting-delete"

_eq "closed + unlabeled + nothing outstanding is also done, not unstarted" \
    "$(_s --verdict not-ready --labels "" --status closed --merged no --has-work no --dirty no)" \
    "done-awaiting-delete"

_eq "...and it points at the missing cleanup signal rather than at the work" \
    "$(bash "$TS" --verdict not-ready --labels "" --status closed --merged no --has-work no \
        --dirty no | jq -r '.next' | grep -q 'baton:finish' && echo pointed || echo lost)" \
    "pointed"

_eq "a closed bead with commits still outstanding is NOT smoothed over" \
    "$(_s --verdict not-ready --status closed --merged no --has-work yes --dirty no)" \
    "in-progress"

_eq "...it is reported as the contradiction it is" \
    "$(_n --verdict not-ready --status closed --merged no --has-work yes --dirty no \
       | grep -q 'CLOSED while the branch still has work' && echo flagged || echo quiet)" \
    "flagged"

_eq "a closed bead with an open PR still waits on the human" \
    "$(_s --verdict not-ready --status closed --merged no --has-work yes --dirty no --pr-state OPEN)" \
    "waiting-on-human"

_eq "closed + nothing outstanding + an open dependent -> follow-ups elsewhere" \
    "$(_s --verdict not-ready --status closed --merged no --has-work no --dirty no --unblocks "jbh-2ri")" \
    "done-followups-elsewhere"

# ============================================================================================
echo
echo "the done family"

_eq "labeled + every cross-check green -> done-awaiting-delete" \
    "$(_s --verdict confirmed-ready --labels "$READY" --status closed --merged yes \
          --has-work no --dirty no)" \
    "done-awaiting-delete"

_eq "keep-task-open splits it into done-followups-elsewhere" \
    "$(_s --verdict confirmed-ready --labels "$READY keep-task-open" --status open --merged yes \
          --has-work no --dirty no)" \
    "done-followups-elsewhere"

_eq "so does blocking an open bead, with no label involved" \
    "$(_s --verdict confirmed-ready --labels "$READY" --status closed --merged yes \
          --has-work no --dirty no --unblocks "jbh-2ri")" \
    "done-followups-elsewhere"

_eq "...and it names where the follow-up lives" \
    "$(_r --verdict confirmed-ready --labels "$READY" --status closed --merged yes \
          --has-work no --dirty no --unblocks "jbh-2ri" \
       | grep -q 'jbh-2ri' && echo named || echo vague)" \
    "named"

_eq "work done outside git (no-pr-needed) is DONE, not unstarted" \
    "$(_s --verdict confirmed-ready --labels "$READY no-pr-needed" --status closed --merged no \
          --has-work no --dirty no)" \
    "done-awaiting-delete"

_eq "the same git shape without the verdict is unstarted — only intent separates them" \
    "$(_s --verdict not-ready --status open --merged no --has-work no --dirty no)" \
    "unstarted"

# ============================================================================================
echo
echo "blockers rank below anything further along, but are never dropped"

_eq "no PR, no commits, an open blocker -> blocked" \
    "$(_s --verdict not-ready --status open --merged no --has-work no --dirty no --blockers "jbh-du9")" \
    "blocked"

_eq "commits plus an open blocker is still blocked" \
    "$(_s --verdict not-ready --status in_progress --merged no --has-work yes --dirty no --blockers "jbh-du9")" \
    "blocked"

_eq "an open PR outranks the blocker edge" \
    "$(_s --verdict not-ready --merged no --has-work yes --dirty no --pr-state OPEN --blockers "jbh-du9")" \
    "waiting-on-human"

_eq "...but the blocker still shows up as a note" \
    "$(_n --verdict not-ready --merged no --has-work yes --dirty no --pr-state OPEN --blockers "jbh-du9" \
       | grep -q 'jbh-du9' && echo kept || echo dropped)" \
    "kept"

_eq "an unfinished task that blocks another says so in the notes" \
    "$(_n --verdict not-ready --status in_progress --merged no --has-work yes --dirty no --unblocks "jbh-2ri" \
       | grep -q 'unblocks jbh-2ri' && echo kept || echo dropped)" \
    "kept"

# ============================================================================================
echo
echo "degraded signals never become confident answers"

_eq "no flags at all -> unknown" \
    "$(_s)" "unknown"

_eq "a failed bead lookup is reported, not swallowed" \
    "$(_n --verdict not-ready --status unknown --merged no --has-work yes --dirty no \
       | grep -q 'bead lookup FAILED' && echo reported || echo swallowed)" \
    "reported"

_eq "an unknown merge state still yields a usable state, biased to unmerged" \
    "$(_s --verdict not-ready --status open --merged unknown --has-work yes --dirty no)" \
    "in-progress"

_eq "...and admits the bias" \
    "$(_n --verdict not-ready --status open --merged unknown --has-work yes --dirty no \
       | grep -q 'safe direction' && echo admitted || echo hidden)" \
    "admitted"

_eq "verdict unknown + a ready label -> unknown, never a lower rung" \
    "$(_s --verdict unknown --labels "$READY" --status closed --merged no --has-work no --dirty no)" \
    "unknown"

_eq "verdict unknown WITHOUT the label still runs the evidence ladder" \
    "$(_s --verdict unknown --status open --merged no --has-work no --dirty no)" \
    "unstarted"

_eq "a labeled-but-contradicted worktree reports the evidence, not the label" \
    "$(_s --verdict label-state-mismatch --labels "$READY" --status open --merged no \
          --has-work yes --dirty no)" \
    "in-progress"

_eq "...and shouts about the contradiction" \
    "$(_n --verdict label-state-mismatch --verdict-reason "branch not merged (merged=no)" \
          --labels "$READY" --status open --merged no --has-work yes --dirty no \
       | grep -q 'ANOMALY' && echo flagged || echo quiet)" \
    "flagged"

_eq "keep-task-open on its own means nothing and is called out" \
    "$(_n --verdict not-ready --labels "keep-task-open" --status open --merged no \
          --has-work yes --dirty no \
       | grep -q 'only means something alongside' && echo flagged || echo quiet)" \
    "flagged"

# ============================================================================================
echo
echo "input hygiene"

_eq "a label that merely CONTAINS a known one does not count" \
    "$(_s --verdict confirmed-ready --labels "dont-keep-task-open $READY" --status closed \
          --merged yes --has-work no --dirty no)" \
    "done-awaiting-delete"

_eq "bd's bullet output is accepted verbatim" \
    "$(_s --verdict confirmed-ready \
          --labels "$(printf '🏷 Labels for jbh-c6l:\n  - cleanup\n  - %s\n  - keep-task-open\n' "$READY")" \
          --status open --merged yes --has-work no --dirty no)" \
    "done-followups-elsewhere"

_eq "an out-of-range --merged is a usage error, not a silent pass" \
    "$(bash "$TS" --merged maybe >/dev/null 2>&1 && echo accepted || echo rejected)" "rejected"

_eq "an unrecognised --verdict is a usage error" \
    "$(bash "$TS" --verdict probably-fine >/dev/null 2>&1 && echo accepted || echo rejected)" "rejected"

_eq "an unrecognised --pr-state is a usage error" \
    "$(bash "$TS" --pr-state DRAFT >/dev/null 2>&1 && echo accepted || echo rejected)" "rejected"

_eq "unknown still exits 0 ('I cannot tell' is an answer, not a failure)" \
    "$(bash "$TS" >/dev/null 2>&1 && echo 0 || echo nonzero)" "0"

_eq "every state carries a human label" \
    "$(bash "$TS" --verdict confirmed-ready --labels "$READY" --status closed --merged yes \
        --has-work no --dirty no | jq -r '.label')" \
    "done, waiting on worktree delete"

# ============================================================================================
echo
echo "the seam: cleanup-verdict.sh's real output drives the done family"
# Not a mock. If either script renames a verdict token, these four rows break — which is the
# point: the whole done family hangs off strings the two files have to agree on.

_chain() { # $1=labels $2=bead status $3=merged $4=has_work $5=dirty -> "verdict/state"
  # NB the bead's status is `--state` to cleanup-verdict.sh and `--status` to task-state.sh:
  # in task-state.sh "state" is already taken by the answer it returns.
  local v s
  v="$(bash "$CV" --labels "$1" --state "$2" --merged "$3" --has-work "$4" --dirty "$5" \
       | jq -r '.verdict')"
  s="$(bash "$TS" --verdict "$v" --labels "$1" --status "$2" --merged "$3" --has-work "$4" \
       --dirty "$5" | jq -r '.state')"
  printf '%s/%s' "$v" "$s"
}

_eq "an ordinary finished worktree" \
    "$(_chain "$READY" closed yes no no)" \
    "confirmed-ready/done-awaiting-delete"

_eq "one finished outside git" \
    "$(_chain "$READY no-pr-needed" closed no no no)" \
    "confirmed-ready/done-awaiting-delete"

_eq "a fresh worktree, identical to the row above in git's eyes" \
    "$(_chain "" open no no no)" \
    "not-ready/unstarted"

_eq "merged and closed but never labeled" \
    "$(_chain "" closed yes no no)" \
    "looks-done-unlabeled/merged-needs-finish"

_eq "real unmerged commits under a ready label" \
    "$(_chain "$READY" open no yes no)" \
    "label-state-mismatch/in-progress"

# ============================================================================================
echo
echo "end to end: real repos through merge-state.sh -> cleanup-verdict.sh -> task-state.sh"
# The blocks above feed task-state.sh by hand. These build actual git repos, so the signals come
# from merge-state.sh reading real history — which is the only way to prove that a fresh branch
# and a merged one, whose `base..branch` ranges are both empty, come out the opposite ends of
# this ladder. `gh` is stubbed to "no PR", so nothing touches the network or a login.

MS="$HERE/merge-state.sh"
if [ ! -r "$MS" ] || ! command -v git >/dev/null 2>&1; then
  echo "  ⏭  skipped (needs git and merge-state.sh)"
else
  TMP="$(mktemp -d)"
  trap 'rm -rf "$TMP"' EXIT
  STUBBIN="$TMP/bin"; mkdir -p "$STUBBIN"
  cat >"$STUBBIN/gh" <<'EOF'
#!/usr/bin/env bash
echo "no pull requests found for branch" >&2
exit 1
EOF
  chmod +x "$STUBBIN/gh"

  _repo() { # $1 = name -> a repo with one commit on main and origin/main pointing at it
    local r="$TMP/$1"
    mkdir -p "$r" && git -C "$r" init -q -b main
    git -C "$r" config user.email t@e && git -C "$r" config user.name t
    echo base >"$r/f" && git -C "$r" add f && git -C "$r" commit -qm base
    git -C "$r" update-ref refs/remotes/origin/main refs/heads/main
    printf '%s' "$r"
  }

  # TS_STATE / TS_LABELS stand in for the bead lookup the skill does with bd; everything else
  # below is derived from the repo. Labels reach BOTH scripts, exactly as the skill passes them.
  _e2e() { # $1 = repo, $2 = branch, $3.. = extra task-state flags -> state token
    local repo="$1" br="$2"; shift 2
    local m dirty
    m="$(PATH="$STUBBIN:$PATH" bash "$MS" --repo "$repo" --branch "$br" --no-fetch --format env 2>/dev/null)"
    dirty="$([ -z "$(git -C "$repo" status --porcelain)" ] && echo no || echo yes)"
    (
      eval "$m"
      eval "$(bash "$CV" --labels "${TS_LABELS:-}" --state "${TS_STATE:-open}" --merged "$MERGED" \
                --has-work "$HAS_WORK" --dirty "$dirty" --format env)"
      bash "$TS" --verdict "$VERDICT" --verdict-reason "$VERDICT_REASON" \
        --labels "${TS_LABELS:-}" --status "${TS_STATE:-open}" --merged "$MERGED" \
        --has-work "$HAS_WORK" --dirty "$dirty" \
        --pr-state "$PR_STATE" --pr-number "$PR_NUMBER" "$@" | jq -r '.state'
    )
  }

  R="$(_repo fresh)"
  git -C "$R" branch feature main
  _eq "a worktree nobody has committed to -> unstarted" "$(_e2e "$R" feature)" "unstarted"
  _eq "...and the same branch with an open blocker -> blocked" \
      "$(_e2e "$R" feature --blockers "jbh-du9")" "blocked"

  R="$(_repo working)"
  git -C "$R" checkout -q -b feature main
  echo work >>"$R/f" && git -C "$R" commit -qam work
  git -C "$R" checkout -q main
  _eq "real commits the base does not have -> in-progress" "$(_e2e "$R" feature)" "in-progress"

  R="$(_repo landed)"
  git -C "$R" checkout -q -b feature main
  echo work >>"$R/f" && git -C "$R" commit -qam work
  git -C "$R" checkout -q main
  git -C "$R" merge -q --no-ff -m merge feature
  git -C "$R" update-ref refs/remotes/origin/main refs/heads/main
  _eq "merged, bead still open -> merged-needs-finish (the abandoned-session case)" \
      "$(_e2e "$R" feature)" "merged-needs-finish"
  _eq "...still merged-needs-finish once the bead is closed, until the label lands" \
      "$(TS_STATE=closed _e2e "$R" feature)" "merged-needs-finish"
  _eq "...and done once it is labeled" \
      "$(TS_STATE=closed TS_LABELS="$READY" _e2e "$R" feature)" "done-awaiting-delete"
fi

echo
echo "$PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
