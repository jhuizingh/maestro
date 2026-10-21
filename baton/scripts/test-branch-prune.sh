#!/usr/bin/env bash
# baton — tests for scripts/branch-prune.sh.
#
# baton:cleanup-worktrees deletes local branches on this script's answer, so the rules are pinned
# here. The load-bearing ones:
#   - "upstream gone" alone NEVER deletes: a gone-but-unmerged branch is confirm-orphaned
#   - only the `prune` bucket is ever deleted automatically
#   - protected branches, origin's default branch and checked-out branches are never touched
#   - merged is judged against origin's default branch, NOT local HEAD: the fixture leaves the
#     local default branch stale on purpose (the 2026-08-06 `git branch -d` failure)
#   - a squash merge (no ancestry at all) is pruned on the PR signal without any `-D`
#
# The fixture's default branch is `trunk`, not main, so a hardcoded main/master would fail here.
#
# Hermetic: a bare repo stands in for origin, so `fetch --prune` produces real "[gone]" state;
# `gh` is stubbed (MERGED for branches listed in a file, "no PR" otherwise). Needs git and jq.

set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
BP="$HERE/branch-prune.sh"
[ -r "$BP" ] || { echo "not found: $BP" >&2; exit 2; }

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
PASS=0; FAIL=0
_ok()  { PASS=$((PASS+1)); printf '  ✅ %s\n' "$1"; }
_bad() { FAIL=$((FAIL+1)); printf '  ❌ %s\n     %s\n' "$1" "$2"; }
_eq()  { if [ "$2" = "$3" ]; then _ok "$1"; else _bad "$1" "got '$2', want '$3'"; fi; }

export GIT_AUTHOR_NAME=T GIT_AUTHOR_EMAIL=t@t.t GIT_COMMITTER_NAME=T GIT_COMMITTER_EMAIL=t@t.t

STUBBIN="$TMP/bin"; mkdir -p "$STUBBIN"
MERGED_LIST="$TMP/gh-merged"; : >"$MERGED_LIST"
cat >"$STUBBIN/gh" <<EOF
#!/usr/bin/env bash
# gh pr view <branch> --json ... — MERGED when listed, else "no pull requests found".
br="\$3"
if grep -qxF -- "\$br" "$MERGED_LIST"; then
  echo '{"state":"MERGED","number":42,"baseRefName":"trunk"}'; exit 0
fi
echo "no pull requests found for branch \"\$br\"" >&2; exit 1
EOF
chmod +x "$STUBBIN/gh"
export PATH="$STUBBIN:$PATH"

# --- fixture --------------------------------------------------------------------------------
ORIGIN="$TMP/origin.git"; SEED="$TMP/seed"; R="$TMP/repo"
git init -q --bare -b trunk "$ORIGIN"
git init -q -b trunk "$SEED"
echo base >"$SEED/f"; git -C "$SEED" add f; git -C "$SEED" commit -qm base
git -C "$SEED" remote add origin "$ORIGIN"; git -C "$SEED" push -q origin trunk
git clone -q "$ORIGIN" "$R"

_branch() { # $1 name, $2 file content — commit on a new branch off trunk and push it
  git -C "$R" checkout -q -b "$1" trunk
  echo "$2" >"$R/$1.txt"; git -C "$R" add "$1.txt"; git -C "$R" commit -qm "$1"
  git -C "$R" push -q -u origin "$1"
  git -C "$R" checkout -q trunk
}
# Land via a merge commit on ORIGIN only (from the seed clone) — local trunk stays stale.
_merge_remote() {
  git -C "$SEED" fetch -q origin "$1"
  git -C "$SEED" merge -q --no-ff --no-edit FETCH_HEAD >/dev/null
  git -C "$SEED" push -q origin trunk
}
_squash_remote() {
  git -C "$SEED" fetch -q origin "$1"
  git -C "$SEED" merge -q --squash FETCH_HEAD >/dev/null 2>&1; git -C "$SEED" commit -qm "squash $1"
  git -C "$SEED" push -q origin trunk
  echo "$1" >>"$MERGED_LIST"
}
_delete_remote() { git -C "$SEED" push -q origin --delete "$1"; }

_branch merged-gone       a;  _merge_remote merged-gone;  _delete_remote merged-gone
_branch squashed-gone     b;  _squash_remote squashed-gone; _delete_remote squashed-gone
_branch unmerged-gone     c;  _delete_remote unmerged-gone
_branch merged-present    d;  _merge_remote merged-present
_branch live              e
_branch develop           f;  _merge_remote develop;      _delete_remote develop
_branch in-worktree       g;  _merge_remote in-worktree;  _delete_remote in-worktree
git -C "$R" worktree add -q "$TMP/wt" in-worktree
git -C "$R" branch fresh trunk                      # never committed to, no upstream

LOCAL_TRUNK_BEFORE="$(git -C "$R" rev-parse trunk)"

_row() { jq -r --arg b "$1" '.branches[] | select(.branch == $b) | .'"$2" <<<"$OUT"; }

# ============================================================================================
echo "scan: classification with plugin defaults"
OUT="$(bash "$BP" scan --repo "$R")"
_eq "base derived from origin/HEAD, not hardcoded"         "$(jq -r .base <<<"$OUT")" "origin/trunk"
_eq "fetch --prune ran"                                     "$(jq -r .fetched <<<"$OUT")" "yes"
_eq "merged-gone: upstream gone"                            "$(_row merged-gone upstream_state)" "gone"
_eq "merged-gone → prune (ancestry, local trunk stale)"     "$(_row merged-gone verdict)" "prune"
_eq "squashed-gone → prune on the PR signal"                "$(_row squashed-gone verdict)" "prune"
_eq "squashed-gone signal is pr"                            "$(_row squashed-gone merge_signal)" "pr"
_eq "unmerged-gone → confirm-orphaned, never prune"         "$(_row unmerged-gone verdict)" "confirm-orphaned"
_eq "merged-present → confirm-merged (upstream not gone)"   "$(_row merged-present verdict)" "confirm-merged"
_eq "live → keep"                                           "$(_row live verdict)" "keep"
_eq "develop (merged+gone) → keep, protected"               "$(_row develop reason)" "protected"
_eq "trunk (origin default, not in protect) → protected"    "$(_row trunk reason)" "protected"
_eq "in-worktree → keep"                                    "$(_row in-worktree verdict)" "keep"
case "$(_row in-worktree reason)" in checked-out*) _ok "in-worktree reason is checked-out" ;;
  *) _bad "in-worktree reason is checked-out" "got '$(_row in-worktree reason)'" ;; esac
_eq "fresh (never committed) → keep, not merged"            "$(_row fresh verdict)" "keep"
_eq "scan deletes nothing"                                  "$(git -C "$R" rev-parse --verify --quiet merged-gone >/dev/null && echo kept)" "kept"
_eq "counts.prune"                                          "$(jq -r .counts.prune <<<"$OUT")" "2"

# ============================================================================================
echo "policy: switches and protect list"
OUT="$(bash "$BP" scan --repo "$R" --no-fetch --context - <<<'{"cleanup":{"prune_gone_upstream":false}}')"
case "$(_row merged-gone reason)" in policy-off*) _ok "prune_gone_upstream:false keeps gone branches" ;;
  *) _bad "prune_gone_upstream:false keeps gone branches" "got '$(_row merged-gone reason)'" ;; esac
_eq "…and merged-present still offered"                     "$(_row merged-present verdict)" "confirm-merged"
OUT="$(bash "$BP" scan --repo "$R" --no-fetch --context - <<<'{"cleanup":{"prune_merged":false,"protect":["merged-gone"]}}')"
_eq "prune_merged:false → merged-present kept"              "$(_row merged-present verdict)" "keep"
_eq "custom protect list honored"                           "$(_row merged-gone reason)" "protected"
_eq "custom protect replaces defaults (develop now prunable)" "$(_row develop verdict)" "prune"
_eq "origin default still protected under a custom list"    "$(_row trunk reason)" "protected"

# ============================================================================================
echo "apply: deletes the prune bucket only, without -D"
OUT="$(bash "$BP" apply --repo "$R" --no-fetch)"
_gone() { git -C "$R" rev-parse --verify --quiet "refs/heads/$1" >/dev/null && echo present || echo deleted; }
_eq "merged-gone deleted"                                   "$(_gone merged-gone)" "deleted"
_eq "squashed-gone deleted (no ancestry, PR signal)"        "$(_gone squashed-gone)" "deleted"
_eq "row reports deleted=yes"                               "$(_row squashed-gone deleted)" "yes"
for b in unmerged-gone merged-present live develop in-worktree fresh trunk; do
  _eq "$b untouched"                                        "$(_gone "$b")" "present"
done
_eq "local trunk was never updated (judged against origin)" "$(git -C "$R" rev-parse trunk)" "$LOCAL_TRUNK_BEFORE"
_eq "counts.deleted"                                        "$(jq -r .counts.deleted <<<"$OUT")" "2"
_eq "skipped set still reported in full"                    "$(jq -r '.branches | length' <<<"$OUT")" "9"

# ============================================================================================
echo "delete: confirmed and worktree-freed branches"
SHA="$(git -C "$R" rev-parse unmerged-gone)"
D="$(bash "$BP" delete --repo "$R" --no-fetch --branch unmerged-gone 2>&1)"; rc=$?
_eq "delete with no --expect/--require-merged is a usage error" "$rc" "1"
D="$(bash "$BP" delete --repo "$R" --no-fetch --branch unmerged-gone --require-merged)"
_eq "--require-merged refuses an unmerged branch"           "$(jq -r .deleted <<<"$D")" "refused"
D="$(bash "$BP" delete --repo "$R" --no-fetch --branch unmerged-gone --expect "$(git -C "$R" rev-parse trunk)")"
_eq "--expect with the wrong sha refuses"                   "$(jq -r .deleted <<<"$D")" "refused"
_eq "…and leaves the branch"                                "$(_gone unmerged-gone)" "present"
D="$(bash "$BP" delete --repo "$R" --no-fetch --branch unmerged-gone --expect "${SHA:0:9}")"
_eq "--expect with the shown (short) sha deletes"           "$(jq -r .deleted <<<"$D")" "yes"
D="$(bash "$BP" delete --repo "$R" --no-fetch --branch merged-present --require-merged)"
_eq "--require-merged deletes a merged branch"              "$(jq -r .deleted <<<"$D")" "yes"
D="$(bash "$BP" delete --repo "$R" --no-fetch --branch develop --expect "$(git -C "$R" rev-parse develop)")"
_eq "protected branch refused even with a matching sha"     "$(jq -r .reason <<<"$D")" "protected"
D="$(bash "$BP" delete --repo "$R" --no-fetch --branch in-worktree --require-merged)"
_eq "checked-out branch refused"                            "$(jq -r .deleted <<<"$D")" "refused"
D="$(bash "$BP" delete --repo "$R" --no-fetch --branch trunk --expect "$(git -C "$R" rev-parse trunk)")"
_eq "origin default branch refused"                         "$(jq -r .reason <<<"$D")" "protected"

# ============================================================================================
# Review branches (pr-review.sh): local name review/<n>-<head>, upstream = the author's head,
# PR number in branch.<b>.baton-pr. Only "author's branch deleted after the PR merged" prunes.
echo "review branches"
_author() { # $1 head — the author pushes a branch to origin from the seed clone
  git -C "$SEED" checkout -q -b "$1" trunk
  echo "$1" >"$SEED/$1.txt"; git -C "$SEED" add "$1.txt"; git -C "$SEED" commit -qm "$1"
  git -C "$SEED" push -q origin "$1"; git -C "$SEED" checkout -q trunk
}
_review() { # $1 n, $2 head — our local review branch tracking origin/<head>
  git -C "$R" fetch -q origin "+refs/heads/$2:refs/remotes/origin/$2"
  git -C "$R" branch -q "review/$1-$2" "origin/$2"
  git -C "$R" config "branch.review/$1-$2.remote" origin
  git -C "$R" config "branch.review/$1-$2.merge" "refs/heads/$2"
  git -C "$R" config "branch.review/$1-$2.baton-pr" "$1"
}
_author feat-a; _review 50 feat-a; _squash_remote feat-a; echo 50 >>"$MERGED_LIST"; _delete_remote feat-a
_author feat-b; _review 51 feat-b; _delete_remote feat-b
_author feat-c; _review 53 feat-c
# A fork PR: the author's head lives at refs/baton/review/<n>, which no fetch --prune touches.
_author fork-main
git -C "$R" fetch -q origin "+refs/heads/fork-main:refs/baton/review/52"
git -C "$R" branch -q review/52-main refs/baton/review/52
git -C "$R" config branch.review/52-main.remote .
git -C "$R" config branch.review/52-main.merge refs/baton/review/52
git -C "$R" config branch.review/52-main.baton-pr 52
_squash_remote fork-main; echo 52 >>"$MERGED_LIST"
git -C "$R" remote set-head origin trunk
OUT="$(bash "$BP" scan --repo "$R")"
_eq "squash-merged PR, author's branch deleted → prune, asked by PR number" \
    "$(_row review/50-feat-a verdict)/$(_row review/50-feat-a merge_signal)" "prune/pr"
_eq "author's branch deleted, PR NOT merged → confirm-orphaned"  "$(_row review/51-feat-b verdict)" "confirm-orphaned"
_eq "fork PR merged, its ref still here → confirm-merged, never prune" "$(_row review/52-main verdict)" "confirm-merged"
_eq "fork PR's upstream survives fetch --prune"                 "$(_row review/52-main upstream_state)" "present"
_eq "open PR, author's branch live → keep"                      "$(_row review/53-feat-c verdict)" "keep"
OUT="$(bash "$BP" apply --repo "$R" --no-fetch)"
_eq "apply deletes only the merged-and-gone review"             "$(_gone review/50-feat-a)/$(_gone review/51-feat-b)/$(_gone review/52-main)/$(_gone review/53-feat-c)" \
    "deleted/present/present/present"

# ============================================================================================
echo "degraded: no gh, no origin/HEAD"
NOGH="$TMP/nogh"; mkdir -p "$NOGH"
for t in git jq sed grep cat bash mktemp rm head tr env dirname; do
  p="$(command -v "$t")" && ln -sf "$p" "$NOGH/$t"
done
git -C "$R" remote set-head origin -d
OUT="$(PATH="$NOGH" bash "$BP" scan --repo "$R" --no-fetch)"
_eq "no origin/HEAD → base empty, reported"                 "$(jq -r .base <<<"$OUT")" ""
_eq "live branch stays keep with no gh and no base"         "$(_row live verdict)" "keep"

echo
echo "passed: $PASS  failed: $FAIL"
[ "$FAIL" -eq 0 ]
