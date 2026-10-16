#!/usr/bin/env bash
# baton — tests for how scripts/merge-state.sh picks the repo it asks GitHub and the base it
# compares against.
#
# Both were guesses once, and each guess alone sent merged branches into cleanup's
# `label-state-mismatch` bucket forever (jbh-2sg, on a qmk_firmware fork):
#   - gh resolved `upstream` instead of the fork, so the fork's own merged PRs were invisible
#   - the ancestry fallback compared against `main`, which that fork does not have
# So the ladder is pinned here: which repo `gh pr view` is pointed at, with which selector, and
# which base (and `base_from`) each repo shape resolves to.
#
# Hermetic: throwaway repos under a temp dir; `gh` is a stub that records its arguments and knows
# one merged PR in one repo; GitHub-shaped remote URLs are mapped to local bare repos with
# url.insteadOf, so fetch and ls-remote work offline. Exit 0 = all passed.

set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
MS="$HERE/merge-state.sh"
[ -r "$MS" ] || { echo "not found: $MS" >&2; exit 2; }

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
PASS=0; FAIL=0
_ok()  { PASS=$((PASS+1)); printf '  ✅ %s\n' "$1"; }
_bad() { FAIL=$((FAIL+1)); printf '  ❌ %s\n     %s\n' "$1" "$2"; }
_eq()  { if [ "$2" = "$3" ]; then _ok "$1"; else _bad "$1" "got '$2', want '$3'"; fi; }

# Keep the user's global git config (and any insteadOf in it) out of these repos.
export GIT_CONFIG_GLOBAL="$TMP/gitconfig" GIT_CONFIG_NOSYSTEM=1
git config --global user.name t; git config --global user.email t@t
git config --global init.defaultBranch main

# The stub answers MERGED only for $GH_KNOWS_REPO + $GH_KNOWS_SEL; anything else is "no PR", the
# exact symptom of asking the wrong repo.
STUBBIN="$TMP/bin"; mkdir -p "$STUBBIN"
cat >"$STUBBIN/gh" <<'EOF'
#!/usr/bin/env bash
printf '%s\n' "$*" >>"$GH_LOG"
sel="$3"; repo=""
while [ $# -gt 0 ]; do [ "$1" = --repo ] && repo="$2"; shift; done
if [ "$repo" = "${GH_KNOWS_REPO:-}" ] && [ "$sel" = "${GH_KNOWS_SEL:-}" ]; then
  printf '{"state":"MERGED","number":7,"baseRefName":"%s"}\n' "$GH_KNOWS_BASE"; exit 0
fi
echo "no pull requests found for branch \"$sel\"" >&2; exit 1
EOF
chmod +x "$STUBBIN/gh"
export GH_LOG="$TMP/gh.log"

# _fork <name> <default-branch>: a bare "origin" whose HEAD is <default-branch>, holding a
# feature branch `feat` merged into it with a merge commit, and a clone of it at $TMP/<name>.
# The clone's origin URL is git@github.com:me/<name>.git, rewritten to the bare repo.
_fork() {
  local n="$1" def="$2" src="$TMP/$1-src" bare="$TMP/$1.git"
  git init -q -b "$def" "$src"
  git -C "$src" commit -q --allow-empty -m root
  git -C "$src" checkout -q -b feat; git -C "$src" commit -q --allow-empty -m work
  git -C "$src" checkout -q "$def"; git -C "$src" merge -q --no-ff -m merge feat
  git clone -q --bare "$src" "$bare"
  git config --global "url.$bare.insteadOf" "git@github.com:me/$n.git"
  git clone -q "git@github.com:me/$n.git" "$TMP/$n" 2>/dev/null
  git -C "$TMP/$n" config remote.origin.url "git@github.com:me/$n.git"
  git -C "$TMP/$n" branch -q feat origin/feat
  git -C "$TMP/$n" remote add upstream "https://github.com/them/$n.git"
}

_run() { # repo, extra args... -> env output
  local r="$1"; shift
  : >"$GH_LOG"
  PATH="$STUBBIN:$PATH" bash "$MS" --repo "$r" --branch feat --format env "$@" 2>/dev/null
}
_get() { ( eval "$1"; eval "printf '%s' \"\${$2:-}\"" ); }

echo "fork with an upstream remote, no gh-resolved"
_fork qmk jh-keymap
export GH_KNOWS_REPO=me/qmk GH_KNOWS_SEL=feat GH_KNOWS_BASE=jh-keymap
OUT="$(_run "$TMP/qmk")"
_eq "asks the fork, not upstream"       "$(_get "$OUT" PR_REPO)" "me/qmk"
_eq "merged PR is found"                "$(_get "$OUT" MERGED)/$(_get "$OUT" MERGE_SIGNAL)" "yes/pr"
_eq "base is the PR's own base"         "$(_get "$OUT" MERGE_BASE)/$(_get "$OUT" MERGE_BASE_FROM)" "origin/jh-keymap/pr"
_eq "gh got --repo me/qmk"              "$(grep -c -- '--repo me/qmk' "$GH_LOG")" "1"
_eq "GH_REPO is never exported"         "$(grep -c '^export GH_REPO=' <<<"$OUT")" "0"

echo "no PR anywhere: ancestry against the real default branch, not main"
export GH_KNOWS_REPO=nobody
OUT="$(_run "$TMP/qmk")"
_eq "origin/HEAD answers"               "$(_get "$OUT" MERGE_BASE)/$(_get "$OUT" MERGE_BASE_FROM)" "origin/jh-keymap/origin-head"
_eq "merged by ancestry"                "$(_get "$OUT" MERGED)/$(_get "$OUT" MERGE_SIGNAL)" "yes/ancestry"

echo "origin/HEAD unset (not a git clone): ask origin itself"
# git 2.48+ re-creates origin/HEAD on fetch; pin that off so this rung is what answers on any git.
git -C "$TMP/qmk" config remote.origin.followRemoteHEAD never
git -C "$TMP/qmk" remote set-head origin --delete
OUT="$(_run "$TMP/qmk")"
_eq "ls-remote answers"                 "$(_get "$OUT" MERGE_BASE)/$(_get "$OUT" MERGE_BASE_FROM)" "origin/jh-keymap/remote-head"
_eq "still merged by ancestry"          "$(_get "$OUT" MERGED)" "yes"
OUT="$(_run "$TMP/qmk" --no-fetch)"
_eq "--no-fetch never touches the network, so no base" "$(_get "$OUT" MERGE_BASE)/$(_get "$OUT" MERGE_SIGNAL)" "/none"

echo "--base wins and says so"
OUT="$(_run "$TMP/qmk" --base origin/jh-keymap --no-fetch)"
_eq "base_from=flag"                    "$(_get "$OUT" MERGE_BASE_FROM)" "flag"

echo "gh repo set-default chose upstream: ask upstream, headed by the fork owner"
git -C "$TMP/qmk" config remote.upstream.gh-resolved base
export GH_KNOWS_REPO=them/qmk GH_KNOWS_SEL=me:feat
OUT="$(_run "$TMP/qmk")"
_eq "asks upstream"                     "$(_get "$OUT" PR_REPO)" "them/qmk"
_eq "selector is me:feat"               "$(grep -c '^pr view me:feat ' "$GH_LOG")" "1"
_eq "merged PR is found"                "$(_get "$OUT" MERGED)/$(_get "$OUT" MERGE_SIGNAL)" "yes/pr"
git -C "$TMP/qmk" config --unset remote.upstream.gh-resolved

echo "gh-resolved=base on origin (what qmk_firmware has today)"
git -C "$TMP/qmk" config remote.origin.gh-resolved base
export GH_KNOWS_REPO=me/qmk GH_KNOWS_SEL=feat
OUT="$(_run "$TMP/qmk")"
_eq "asks origin with a bare selector"  "$(_get "$OUT" PR_REPO)|$(grep -c '^pr view feat ' "$GH_LOG")" "me/qmk|1"

echo "older gh stored OWNER/REPO as the value"
git -C "$TMP/qmk" config remote.origin.gh-resolved me/qmk
OUT="$(_run "$TMP/qmk")"
_eq "value used as the repo"            "$(_get "$OUT" PR_REPO)" "me/qmk"
git -C "$TMP/qmk" config --unset remote.origin.gh-resolved

echo "remote URL shapes"
for u in "git@github.com:me/x.git|me/x" "https://github.com/me/x|me/x" \
         "https://github.com/me/x.git/|me/x" "ssh://git@github.com/me/x.git|me/x" \
         "https://tok@github.com/me/x.git|me/x" "git@ghe.corp:me/x.git|ghe.corp/me/x" \
         "ssh://git@ghe.corp:2222/me/x.git|ghe.corp/me/x" "/srv/git/x.git|" "../x|"; do
  git -C "$TMP/qmk" config remote.origin.url "${u%%|*}"
  OUT="$(_run "$TMP/qmk" --no-fetch)"
  _eq "${u%%|*}"                        "$(_get "$OUT" PR_REPO)" "${u#*|}"
done
git -C "$TMP/qmk" config remote.upstream.url /srv/git/up.git
git -C "$TMP/qmk" config remote.origin.url /srv/git/x.git
OUT="$(_run "$TMP/qmk" --no-fetch)"
_eq "no GitHub remote: gh left to its own resolution" "$(grep -c -- '--repo' "$GH_LOG")" "0"

echo "json carries the same fields"
J="$(PATH="$STUBBIN:$PATH" bash "$MS" --repo "$TMP/qmk" --branch feat --no-fetch --base origin/jh-keymap)"
_eq "json has pr_repo and base_from"    "$(jq -r 'has("pr_repo") and .base_from == "flag"' <<<"$J")" "true"

echo
echo "passed: $PASS  failed: $FAIL"
[ "$FAIL" -eq 0 ]
