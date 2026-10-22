#!/usr/bin/env bash
# baton — tests for scripts/pr-review.sh and scripts/review-state.sh, the git/GitHub half of a
# review task.
#
# What matters most here is the line between the author's work and ours. A review worktree holds
# the author's commits, which come and go as they push and force-push; the reviewer may also have
# commits or edits of their own, which baton must never discard. So the tests attack exactly that
# line: a force-push with nothing of ours (reset, and the old commits must NOT read as ours), a
# force-push WITH something of ours (stop), and the fork case, whose head lives outside
# refs/remotes so `fetch --prune` cannot make it vanish.
#
# Hermetic: a bare repo stands in for origin, mapped from a GitHub-shaped URL with insteadOf; `gh`
# is a stub that answers `pr view` from fixture files and `api user` with a fixed login. Needs git
# and jq. Exit 0 = all passed.

set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
PRR="$HERE/pr-review.sh"; RS="$HERE/review-state.sh"
for f in "$PRR" "$RS"; do [ -r "$f" ] || { echo "not found: $f" >&2; exit 2; }; done

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
PASS=0; FAIL=0
_ok()  { PASS=$((PASS+1)); printf '  ✅ %s\n' "$1"; }
_bad() { FAIL=$((FAIL+1)); printf '  ❌ %s\n     %s\n' "$1" "$2"; }
_eq()  { if [ "$2" = "$3" ]; then _ok "$1"; else _bad "$1" "got '$2', want '$3'"; fi; }

export GIT_CONFIG_GLOBAL="$TMP/gitconfig" GIT_CONFIG_NOSYSTEM=1
git config --global user.name t; git config --global user.email t@t
git config --global init.defaultBranch main

STUBBIN="$TMP/bin"; mkdir -p "$STUBBIN"
cat >"$STUBBIN/gh" <<'EOF'
#!/usr/bin/env bash
case "$1 $2" in
  "pr view")
    f="$GH_FIX/pr-$3.json"
    [ -r "$f" ] || { echo "GraphQL: Could not resolve to a PullRequest with the number of $3." >&2; exit 1; }
    cat "$f" ;;
  "api user") echo "${GH_LOGIN:-me}" ;;
  *) echo "gh stub: unexpected $*" >&2; exit 1 ;;
esac
EOF
chmod +x "$STUBBIN/gh"
export PATH="$STUBBIN:$PATH" GH_FIX="$TMP/fix"; mkdir -p "$GH_FIX"

# --- fixture: origin (o/proj on GitHub), the author's clone, and our member repo ---------------
ORIGIN="$TMP/origin.git"; AUTHOR="$TMP/author"
git init -q --bare "$ORIGIN"
git config --global "url.$ORIGIN.insteadOf" "git@github.com:o/proj.git"
git clone -q "git@github.com:o/proj.git" "$AUTHOR" 2>/dev/null
git -C "$AUTHOR" commit -q --allow-empty -m root; git -C "$AUTHOR" push -q origin main
mkdir -p "$TMP/code"; R="$TMP/code/proj"
git clone -q "git@github.com:o/proj.git" "$R" 2>/dev/null
git -C "$R" config remote.origin.url "git@github.com:o/proj.git"

_push() { # $1 branch, $2 msg — the author commits and pushes
  git -C "$AUTHOR" checkout -q "$1" 2>/dev/null || git -C "$AUTHOR" checkout -q -b "$1" main
  echo "$2" >>"$AUTHOR/$1.txt"; git -C "$AUTHOR" add -A; git -C "$AUTHOR" commit -qm "$2"
  git -C "$AUTHOR" push -q -f origin "HEAD:$1"
}
_pr() { # $1 n, $2 head, $3 cross(true|false)
  printf '{"number":%s,"url":"https://github.com/o/proj/pull/%s","state":"OPEN","headRefName":"%s","headRepositoryOwner":{"login":"%s"},"isCrossRepository":%s,"baseRefName":"main"}\n' \
    "$1" "$1" "$2" "$([ "$3" = true ] && echo forker || echo o)" "$3" >"$GH_FIX/pr-$1.json"
}
CTX="$(jq -cn --arg g "$TMP/code/*" '{member_repos: [$g, "~/definitely-not-here"]}')"

# ============================================================================================
echo "resolve"
_pr 12 feat false
OUT="$(bash "$PRR" resolve https://github.com/o/proj/pull/12/files --context - <<<"$CTX")"
_eq "a URL finds its member repo through a glob"   "$(jq -r .repo_dir <<<"$OUT")" "$R"
_eq "PR fields are carried"                        "$(jq -r '"\(.number) \(.head) \(.cross_repo) \(.gh_repo) \(.repo_name)"' <<<"$OUT")" "12 feat false o/proj proj"
OUT="$(bash "$PRR" resolve https://github.com/O/Proj/pull/12 --context - <<<"$CTX" | jq -r .repo_dir)"
_eq "owner/repo match is case-insensitive"         "$OUT" "$R"
bash "$PRR" resolve https://github.com/x/other/pull/1 --context - <<<"$CTX" >/dev/null 2>&1
_eq "a PR in a non-member repo is an error"        "$?" "1"
bash "$PRR" resolve 12 --context - <<<"$CTX" >/dev/null 2>&1
_eq "a bare number with no --repo is an error"     "$?" "1"
_eq "a bare number with --repo resolves"           "$(bash "$PRR" resolve 12 --repo "$R" | jq -r .gh_repo)" "o/proj"
bash "$PRR" resolve https://github.com/x/other/pull/12 --repo "$R" >/dev/null 2>&1
_eq "a URL that disagrees with --repo is an error" "$?" "1"
bash "$PRR" resolve 99 --repo "$R" >/dev/null 2>&1
_eq "no such PR is an error"                       "$?" "1"
bash "$PRR" resolve not-a-pr --repo "$R" >/dev/null 2>&1
_eq "garbage is an error"                          "$?" "1"

# ============================================================================================
echo "checkout + sync, same-repo PR"
_push feat a1
WT="$TMP/wt-12"
OUT="$(bash "$PRR" resolve 12 --repo "$R" | bash "$PRR" checkout --worktree "$WT" --branch review/12-feat)"
_eq "worktree on review/12-feat"                   "$(git -C "$WT" rev-parse --abbrev-ref HEAD)" "review/12-feat"
_eq "tracks origin/feat"                           "$(git -C "$WT" rev-parse --abbrev-ref '@{u}')" "origin/feat"
_eq "PR number recorded on the branch"             "$(git -C "$R" config branch.review/12-feat.baton-pr)" "12"
_eq "at the author's head"                         "$(git -C "$WT" rev-parse HEAD)" "$(git -C "$AUTHOR" rev-parse feat)"
OUT="$(bash "$PRR" resolve 12 --repo "$R" | bash "$PRR" checkout --worktree "$WT" --branch review/12-feat)"
_eq "a second checkout reuses the worktree"        "$(jq -r .worktree <<<"$OUT")" "$WT"
_s() { bash "$PRR" sync --worktree "$WT" | jq -r .result; }
_u() { bash "$RS" --repo "$WT" --reviews-json - --login me <<<'[]' | jq -r .unpushed; }
_eq "nothing new → up-to-date"                     "$(_s)" "up-to-date"
_push feat a2
_eq "author pushed → fast-forward"                 "$(_s)" "fast-forward"
_eq "…to the author's head"                        "$(git -C "$WT" rev-parse HEAD)" "$(git -C "$AUTHOR" rev-parse feat)"
_eq "nothing of ours"                              "$(_u)" "no"
# The author rewrites history. Fetch it WITHOUT syncing first: the old commits are still on HEAD
# and on neither remote ref, and must not be mistaken for ours.
git -C "$AUTHOR" reset -q --hard HEAD~1; _push feat a2-rewritten
git -C "$R" fetch -q origin
_eq "force-pushed, not yet synced: old author commits are not ours" "$(_u)" "no"
_eq "force-push, nothing of ours → reset"          "$(_s)" "reset"
_eq "…to the rewritten head"                       "$(git -C "$WT" rev-parse HEAD)" "$(git -C "$AUTHOR" rev-parse feat)"
echo mine >"$WT/mine.txt"
_eq "a dirty tree → dirty, nothing touched"        "$(_s)" "dirty"
_eq "dirty is reported by review-state"            "$(bash "$RS" --repo "$WT" | jq -r .dirty)" "yes"
git -C "$WT" add mine.txt; git -C "$WT" commit -qm mine
_eq "a commit of ours → unpushed"                  "$(_u)" "yes"
_eq "…ahead, nothing moved"                        "$(_s)" "ahead"
BEFORE="$(git -C "$WT" rev-parse HEAD)"
git -C "$AUTHOR" reset -q --hard HEAD~1; _push feat a3-rewritten-again
_eq "force-push WITH a commit of ours → diverged"  "$(_s)" "diverged"
_eq "…and nothing was moved or discarded"          "$(git -C "$WT" rev-parse HEAD)" "$BEFORE"
_eq "…the commit of ours is still on HEAD"         "$(git -C "$WT" log --format=%s -1)" "mine"

# ============================================================================================
echo "checkout + sync, fork PR"
# The fork's head is named `main` — reviewing it on a branch called main would collide with ours.
git -C "$AUTHOR" checkout -q -b forkwork main; echo f1 >"$AUTHOR/f"; git -C "$AUTHOR" add f; git -C "$AUTHOR" commit -qm f1
git -C "$AUTHOR" push -q origin "HEAD:refs/pull/13/head"
_pr 13 main true
WT2="$TMP/wt-13"
bash "$PRR" resolve 13 --repo "$R" | bash "$PRR" checkout --worktree "$WT2" --branch review/13-main >/dev/null
_eq "worktree on review/13-main, not main"         "$(git -C "$WT2" rev-parse --abbrev-ref HEAD)" "review/13-main"
_eq "upstream is the fetched PR ref"               "$(git -C "$WT2" rev-parse --symbolic-full-name '@{u}')" "refs/baton/review/13"
git -C "$R" fetch -q --prune origin
_eq "the PR ref survives fetch --prune"            "$(git -C "$R" rev-parse --verify --quiet refs/baton/review/13 >/dev/null && echo kept)" "kept"
echo f2 >>"$AUTHOR/f"; git -C "$AUTHOR" commit -qam f2; git -C "$AUTHOR" push -q -f origin "HEAD:refs/pull/13/head"
_eq "fork pushed → fast-forward"                   "$(bash "$PRR" sync --worktree "$WT2" | jq -r .result)" "fast-forward"
_eq "nothing of ours on the fork review"           "$(bash "$RS" --repo "$WT2" --reviews-json - --login me <<<'[]' | jq -r .unpushed)" "no"
_eq "our own main is untouched"                    "$(git -C "$R" rev-parse main)" "$(git -C "$R" rev-parse origin/main)"

# ============================================================================================
echo "review-state: our effective review"
_rs() { bash "$RS" --repo "$WT" --login "${2:-me}" --reviews-json - <<<"$1" | jq -r .review_state; }
r() { printf '{"user":{"login":"%s"},"state":"%s","submitted_at":"2026-01-0%sT00:00:00Z"}' "$1" "$2" "$3"; }
_eq "approved, then a plain comment → APPROVED"    "$(_rs "[$(r me APPROVED 1),$(r me COMMENTED 2)]")" "APPROVED"
_eq "approved, then changes requested → CHANGES_REQUESTED" "$(_rs "[$(r me APPROVED 1),$(r me CHANGES_REQUESTED 2)]")" "CHANGES_REQUESTED"
_eq "changes requested, then approved → APPROVED"  "$(_rs "[$(r me CHANGES_REQUESTED 1),$(r me APPROVED 2)]")" "APPROVED"
_eq "a dismissed approval → DISMISSED"             "$(_rs "[$(r me DISMISSED 1),$(r me COMMENTED 2)]")" "DISMISSED"
_eq "only comments → COMMENTED"                    "$(_rs "[$(r me COMMENTED 1)]")" "COMMENTED"
_eq "only other people's reviews → NONE"           "$(_rs "[$(r other APPROVED 1)]")" "NONE"
_eq "an unsubmitted review is ignored"             "$(_rs '[{"user":{"login":"me"},"state":"PENDING"}]')" "NONE"
_eq "order is by submission, not array position"   "$(_rs "[$(r me CHANGES_REQUESTED 2),$(r me APPROVED 1)]")" "CHANGES_REQUESTED"
_eq "login matching ignores case"                  "$(_rs "[$(r Me APPROVED 1)]" ME)" "APPROVED"
_eq "the login defaults to gh api user"            "$(bash "$RS" --repo "$WT" --reviews-json - <<<"[$(r me APPROVED 1)]" | jq -r .review_login)" "me"
_eq "unreadable reviews → unknown"                 "$(_rs 'not json')" "unknown"
_eq "no PR given → unknown"                        "$(bash "$RS" --repo "$WT" | jq -r .review_state)" "unknown"
bash "$RS" --repo "$WT" --pr-url https://github.com/o/proj/pull/x >/dev/null 2>&1
_eq "a --pr-url that is not a PR URL is a usage error" "$?" "1"

# ============================================================================================
echo "review-state: the author's branch is gone"
# The usual end of a review: the PR merges, GitHub deletes the head branch, the next prune drops
# @{u}. baton-synced still names the author's head and must answer on its own.
VERDICT="$HERE/cleanup-verdict.sh"
_push gone g1; _pr 14 gone false
WT3="$TMP/wt-14"
bash "$PRR" resolve 14 --repo "$R" | bash "$PRR" checkout --worktree "$WT3" --branch review/14-gone >/dev/null
bash "$PRR" sync --worktree "$WT3" >/dev/null
git -C "$AUTHOR" push -q origin --delete gone; git -C "$R" fetch -q --prune origin
_eq "the upstream is really gone"                  "$(git -C "$WT3" rev-parse --verify --quiet '@{u}' >/dev/null 2>&1 && echo still || echo gone)" "gone"
OUT="$(bash "$RS" --repo "$WT3" --reviews-json - --login me <<<'[]')"
_eq "synced covers HEAD → unpushed no"             "$(jq -r '"\(.unpushed) \(.unpushed_count) \(.upstream)"' <<<"$OUT")" "no 0 "
_cv() { bash "$VERDICT" --kind review --merged yes --has-work no --labels ready-for-worktree-delete \
          --label-scope branch --state closed --pr-state MERGED --dirty no --unpushed "$1" | jq -r .verdict; }
_eq "…a closed, labelled, clean review → confirmed-ready" "$(_cv "$(jq -r .unpushed <<<"$OUT")")" "confirmed-ready"
echo mine >"$WT3/mine.txt"; git -C "$WT3" add mine.txt; git -C "$WT3" commit -qm mine
OUT="$(bash "$RS" --repo "$WT3" --reviews-json - --login me <<<'[]')"
_eq "a commit of ours on top → unpushed yes"       "$(jq -r '"\(.unpushed) \(.unpushed_count)"' <<<"$OUT")" "yes 1"
_eq "…and the worktree is kept"                    "$(_cv "$(jq -r .unpushed <<<"$OUT")")" "label-state-mismatch"
git -C "$R" config --unset branch.review/14-gone.baton-synced
git -C "$WT3" reset -q --hard HEAD~1
_eq "never synced: g1 is not on main, so no baseline covers it → yes" \
    "$(bash "$RS" --repo "$WT3" | jq -r .unpushed)" "yes"
git -C "$AUTHOR" checkout -q main; git -C "$AUTHOR" merge -q --no-edit "$(git -C "$WT3" rev-parse HEAD)"
git -C "$AUTHOR" push -q origin main; git -C "$R" fetch -q origin
_eq "never synced, but merged into the default branch → no" \
    "$(bash "$RS" --repo "$WT3" | jq -r .unpushed)" "no"

NOUP="$TMP/noup"; git -C "$R" worktree add -q -b no-upstream "$NOUP" main
_eq "no upstream, at the default branch → unpushed no" "$(bash "$RS" --repo "$NOUP" | jq -r .unpushed)" "no"
BARE="$TMP/bare"; git init -q "$BARE"; git -C "$BARE" commit -q --allow-empty -m x
_eq "no baseline at all → unpushed unknown"        "$(bash "$RS" --repo "$BARE" | jq -r .unpushed)" "unknown"

# ============================================================================================
echo "match-leaves"
L='[{"id":"a","labels":["pr-review"],"status":"closed","closed_at":"2026-01-01","description":"Review https://github.com/o/proj/pull/123"},
    {"id":"b","labels":["pr-review"],"status":"closed","closed_at":"2026-01-03","description":"Review https://github.com/o/proj/pull/12"},
    {"id":"c","labels":["pr-review"],"status":"closed","closed_at":"2026-01-02","description":"https://github.com/o/proj/pull/12/files"},
    {"id":"d","labels":["pr-review"],"status":"in_progress","description":"https://github.com/o/proj/pull/12"},
    {"id":"e","labels":[],"status":"open","description":"https://github.com/o/proj/pull/12"}]'
OUT="$(bash "$PRR" match-leaves --url https://github.com/o/proj/pull/12/ <<<"$L")"
_eq "open review leaves for the PR, pr-review only" "$(jq -c .open <<<"$OUT")" '["d"]'
_eq "the most recently closed one, never /123"      "$(jq -r .closed_latest <<<"$OUT")" "b"
_eq "no match → empty"                              "$(bash "$PRR" match-leaves --url https://github.com/o/proj/pull/7 <<<"$L" | jq -c .)" '{"open":[],"closed_latest":""}'

echo
echo "passed: $PASS  failed: $FAIL"
[ "$FAIL" -eq 0 ]
