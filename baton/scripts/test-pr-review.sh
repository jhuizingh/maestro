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
    f="$GH_FIX/pr-${3##*/}.json"   # a number, or a PR URL ending in one
    [ -n "${GH_CALLS:-}" ] && echo "pr ${3##*/}" >>"$GH_CALLS"
    [ -e "$GH_FIX/pr-${3##*/}.slow" ] && sleep 30
    [ -r "$f" ] || { echo "GraphQL: Could not resolve to a PullRequest with the number of $3." >&2; exit 1; }
    cat "$f" ;;
  "api user") echo "${GH_LOGIN:-me}" ;;
  "api graphql") cat "$GH_FIX/graphql.json" ;;
  "api --paginate")
    k="${3%%\?*}"; k="${k#repos/*/*/}"; k="$(sed -E 's#^(issues|pulls)/[0-9]+/#\1-#' <<<"$k")"
    [ -e "$GH_FIX/api-$k.json" ] && cat "$GH_FIX/api-$k.json" || echo '[]' ;;
  "api --method")
    cat >"$GH_FIX/posted.json"; echo "$4" >"$GH_FIX/posted-path"
    [ -e "$GH_FIX/post-fails" ] && { echo 'gh: Unprocessable Entity (HTTP 422)'; exit 1; }
    echo '{"id":9,"html_url":"https://github.com/o/proj/pull/40#pullrequestreview-9"}' ;;
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

# ============================================================================================
echo "rank — review tasks waiting on the user sort first"
U=https://github.com/o/proj/pull
L='[{"id":"plain","labels":[],"status":"in_progress"},
    {"id":"submitted","labels":["pr-review"],"status":"in_progress","description":"Review '$U'/21 (a → main)"},
    {"id":"requested","labels":["pr-review"],"status":"in_progress","description":"Review '$U'/22"},
    {"id":"started","labels":["pr-review"],"status":"in_progress","description":"Review '$U'/23"},
    {"id":"merged","labels":["pr-review"],"status":"in_progress","description":"Review '$U'/24"},
    {"id":"closed-leaf","labels":["pr-review"],"status":"closed","description":"Review '$U'/23"},
    {"id":"unreadable","labels":["pr-review"],"status":"in_progress","description":"Review '$U'/29"},
    {"id":"draft-only","labels":["pr-review"],"status":"in_progress","notes":"see '$U'/25"}]'
D='{"'$U'/21":{"state":"OPEN","reviewRequests":[],"latestReviews":[{"author":{"login":"Me"},"state":"APPROVED"}]},
   "'$U'/22":{"state":"OPEN","reviewRequests":[{"__typename":"User","login":"me"}],
              "latestReviews":[{"author":{"login":"me"},"state":"CHANGES_REQUESTED"}]},
   "'$U'/23":{"state":"OPEN","reviewRequests":[],"latestReviews":[{"author":{"login":"other"},"state":"APPROVED"}]},
   "'$U'/24":{"state":"MERGED","reviewRequests":[{"login":"me"}],"latestReviews":[]},
   "'$U'/25":{"state":"OPEN","reviewRequests":[],"latestReviews":[{"author":{"login":"me"},"state":"PENDING"}]}}'
printf '%s' "$D" >"$TMP/prdata.json"
OUT="$(bash "$PRR" rank --login me --pr-data "$TMP/prdata.json" <<<"$L")"
_v() { jq -r --arg id "$1" '.[] | select(.id == $id) | "\(.review_pending) \(.review_why // "-")"' <<<"$OUT"; }
_eq "pending, then unknown, then the rest, input order kept within each" "$(jq -c 'map(.id)' <<<"$OUT")" \
    '["requested","started","draft-only","unreadable","plain","submitted","merged","closed-leaf"]'
_eq "a non-review task is never pending"           "$(_v plain)"       "no -"
_eq "submitted review (login case-insensitive) → no" "$(_v submitted)" "no review submitted (APPROVED)"
_eq "re-requested after a review → yes"            "$(_v requested)"   "yes review requested"
_eq "started, nothing submitted yet → yes"         "$(_v started)"     "yes no review submitted yet"
_eq "merged PR → no, even with a request"          "$(_v merged)"      "no PR merged"
_eq "closed leaf → no"                             "$(_v closed-leaf)" "no leaf closed"
_eq "PR missing from --pr-data → unknown, never no" "$(_v unreadable)"  "unknown PR unreadable (no data for it)"
OUT="$(bash "$PRR" rank --login me --pr-data <(jq -c '. + {"'$U'/29": {"error": "HTTP 502"}}' "$TMP/prdata.json") <<<"$L")"
_eq "a caller-reported error → unknown with its reason" "$(_v unreadable)" "unknown PR unreadable (HTTP 502)"
_eq "  and the other tasks rank as before"          "$(_v requested) / $(_v submitted)" \
    "yes review requested / no review submitted (APPROVED)"
_eq "an unsubmitted (PENDING) review does not count; URL from notes" "$(_v draft-only)" "yes no review submitted yet"
_eq "empty list → empty"                           "$(bash "$PRR" rank --login me --pr-data "$TMP/prdata.json" <<<'[]')" "[]"
bash "$PRR" rank <<<'{}' >/dev/null 2>&1; _eq "not an array → usage error" "$?" "1"

# Through gh: each open leaf's PR is read with `gh pr view <url>`, the login from `gh api user`.
jq '."'$U'/22"' "$TMP/prdata.json" >"$GH_FIX/pr-22.json"
jq '."'$U'/21"' "$TMP/prdata.json" >"$GH_FIX/pr-21.json"
OUT="$(GH_LOGIN=me bash "$PRR" rank <<<"$(jq -c '[.[] | select(.id == "submitted" or .id == "requested" or .id == "plain")]' <<<"$L")")"
_eq "via gh: requested first, submitted not"       "$(jq -c 'map([.id, .review_pending])' <<<"$OUT")" \
    '[["requested","yes"],["plain","no"],["submitted","no"]]'


# Partial results: one PR deleted, one hanging; each makes only its own task unknown.
jq '."'$U'/23"' "$TMP/prdata.json" >"$GH_FIX/pr-23.json"
: >"$GH_FIX/pr-31.slow"
L2='[{"id":"ok","labels":["pr-review"],"status":"in_progress","description":"'$U'/22"},
     {"id":"gone","labels":["pr-review"],"status":"in_progress","description":"'$U'/30"},
     {"id":"hangs","labels":["pr-review"],"status":"in_progress","description":"'$U'/31"},
     {"id":"ok2","labels":["pr-review"],"status":"in_progress","description":"'$U'/23"}]'
export GH_CALLS="$TMP/gh-calls"; : >"$GH_CALLS"
T0=$(date +%s)
OUT="$(GH_LOGIN=me bash "$PRR" rank --timeout 1 --jobs 2 <<<"$L2")"
T1=$(date +%s)
_eq "deleted PR → only that task unknown, with gh's reason" "$(_v gone)" \
    "unknown PR unreadable (GraphQL: Could not resolve to a PullRequest with the number of $U/30.)"
_eq "hanging PR → unknown, timed out"               "$(_v hangs)"   "unknown PR unreadable (timed out after 1s)"
_eq "the others rank normally"                      "$(_v ok) / $(_v ok2)" "yes review requested / yes no review submitted yet"
_eq "order: waiting, then unknown"                  "$(jq -c 'map(.id)' <<<"$OUT")" '["ok","ok2","gone","hangs"]'
_eq "each PR read once"                             "$(grep -c '^pr ' "$GH_CALLS")" "4"
[ $((T1 - T0)) -lt 10 ] && _ok "the hang cost the timeout, not 30s ($((T1 - T0))s)" \
  || _bad "the hang cost the timeout, not 30s" "took $((T1 - T0))s"
unset GH_CALLS

# No gh at all: every open review is unknown, nothing is no.
NOGH="$TMP/nogh"; mkdir -p "$NOGH"; for b in jq bash cat mktemp; do ln -sf "$(command -v $b)" "$NOGH/$b"; done
OUT="$(PATH="$NOGH" bash "$PRR" rank <<<"$L2")"
_eq "gh missing → unknown (gh not installed)"       "$(jq -r '[.[].review_pending] | unique | join(",")' <<<"$OUT") $(_v ok)" \
    "unknown unknown PR unreadable (gh not installed)"
bash "$PRR" rank --timeout 0 <<<'[]' >/dev/null 2>&1; _eq "--timeout 0 → usage error" "$?" "1"

# ============================================================================================
echo "post — exact payloads, only against the PR the user decided on"
_push rev r1; _pr 40 rev false
WT4="$TMP/wt-40"
bash "$PRR" resolve 40 --repo "$R" | bash "$PRR" checkout --worktree "$WT4" --branch review/40-rev >/dev/null
H="$(git -C "$WT4" rev-parse HEAD)"
_gq() { # $1 state, $2 head, $3 lastEditedAt|null
  jq -cn --arg s "$1" --arg h "$2" --argjson e "${3:-null}" \
    '{data:{repository:{pullRequest:{url:"https://github.com/o/proj/pull/40",state:$s,headRefOid:$h,lastEditedAt:$e,editor:{login:"author"}}}}}' \
    >"$GH_FIX/graphql.json"
}
_gq OPEN "$H"
printf '%s' '[{"filename":"rev.txt","patch":"@@ -0,0 +1,3 @@\n+a\n+b\n+c"},{"filename":"bin.png"}]' >"$GH_FIX/api-pulls-files.json"
DRAFT=2026-11-25T10:00:00Z
P="$(jq -cn --arg h "$H" --arg d "$DRAFT" '{commit_id:$h, event:"APPROVE", body:"Looks good.", drafted_at:$d,
      note:"helper", head_moved:false, ticket_notes:"Non-blocking: rename x later.",
      comments:[{path:"rev.txt", line:2, side:"RIGHT", body:"Non-blocking: …"},
                {path:"rev.txt", start_line:1, line:3, body:"range"}]}')"
_post() { (cd "$WT4" && bash "$PRR" post "$@"); }
rm -f "$GH_FIX/posted.json"
OUT="$(_post --dry-run <<<"$P")"; RC=$?
_eq "dry run: every check passes → ready, exit 0"  "$(jq -r .result <<<"$OUT") $RC" "ready 0"
_eq "…and nothing was sent"                        "$([ -e "$GH_FIX/posted.json" ] && echo sent || echo none)" "none"
_eq "helper keys are stripped, and named"          "$(jq -c '[(.payload | keys), .stripped]' <<<"$OUT")" '[["body","comments","commit_id","event"],["drafted_at","head_moved","note","ticket_notes"]]'
_eq "comments carry RIGHT, a range keeps start_side" "$(jq -c '.payload.comments | map([.side, .start_side])' <<<"$OUT")" '[["RIGHT",null],["RIGHT","RIGHT"]]'
OUT="$(_post <<<"$P")"; RC=$?
_eq "posted, with the review's URL"                "$(jq -r '"\(.result) \(.review_url) \(.event)"' <<<"$OUT") $RC" \
    "posted https://github.com/o/proj/pull/40#pullrequestreview-9 APPROVE 0"
_eq "…to the PR's reviews endpoint"                "$(cat "$GH_FIX/posted-path")" "repos/o/proj/pulls/40/reviews"
_eq "…exactly the cleaned payload"                 "$(jq -c . "$GH_FIX/posted.json")" "$(jq -c .payload <<<"$OUT")"
_eq "…so ticket_notes never reach GitHub"           "$(jq 'has("ticket_notes")' "$GH_FIX/posted.json")" "false"
_eq "…and the result names the PR (on_review_notes' PR_URL)" "$(jq -r .pr <<<"$OUT")" "https://github.com/o/proj/pull/40"

_refused() { # $1 = payload, rest = args; prints the reasons joined, and checks nothing was sent
  rm -f "$GH_FIX/posted.json"
  local o rc; o="$(_post "${@:2}" <<<"$1")"; rc=$?
  [ "$rc" = 2 ] && [ "$(jq -r .result <<<"$o")" = refused ] && [ ! -e "$GH_FIX/posted.json" ] \
    || echo "NOT REFUSED (exit $rc, sent: $([ -e "$GH_FIX/posted.json" ] && echo yes || echo no))"
  jq -r '.reasons | join(" | ")' <<<"$o"
}
_has() { case "$2" in *"$3"*) _ok "$1" ;; *) _bad "$1" "got '$2', want it to contain '$3'" ;; esac; }
_has "an empty body is refused"                   "$(_refused "$(jq -c '.body = "  "' <<<"$P")")" "the review body is empty"
_has "a missing event is refused (it would leave a PENDING review)" "$(_refused "$(jq -c 'del(.event)' <<<"$P")")" "event must be"
_has "an inline comment with an empty body"       "$(_refused "$(jq -c '.comments[0].body = ""' <<<"$P")")" "comment 1 (rev.txt:2): empty body"
_has "a LEFT-side comment"                        "$(_refused "$(jq -c '.comments[0].side = "LEFT"' <<<"$P")")" "side must be RIGHT"
_has "a line outside the diff"                    "$(_refused "$(jq -c '.comments[0].line = 7' <<<"$P")")" "rev.txt:7 is not a RIGHT-side diff line"
_has "a file outside the diff"                    "$(_refused "$(jq -c '.comments[0].path = "nope.txt"' <<<"$P")")" "nope.txt is not in the PR diff"
OUT="$(_post --dry-run <<<"$(jq -c '.comments = [{path:"bin.png", line:1, body:"x"}]' <<<"$P")")"
_eq "a file GitHub gives no patch for → a warning, not a refusal" "$(jq -r '"\(.result) \(.warnings | length)"' <<<"$OUT")" "ready 1"
_has "no draft time is a usage error"             "$(_post <<<"$(jq -c 'del(.drafted_at)' <<<"$P")" 2>&1; echo "exit $?")" "exit 1"
_has "a commit_id that is not the worktree's HEAD" "$(_refused "$(jq -c '.commit_id = "'"$(git -C "$R" rev-parse main)"'"' <<<"$P")")" "not the reviewed worktree's HEAD"
_has "several problems are all reported"          "$(_refused "$(jq -c '.body = "" | .comments[0].line = 7' <<<"$P")")" "empty | comment 1: rev.txt:7"

_gq OPEN 1111111111111111111111111111111111111111
_has "the PR head moved since the review"         "$(_refused "$P")" "the PR head moved: it is 111111111 now"
_gq MERGED "$H"
_has "the PR is no longer open"                   "$(_refused "$P")" "the PR is merged, not open"
_gq OPEN "$H" '"2026-11-25T11:00:00Z"'
_has "the author edited the PR body after the draft" "$(_refused "$P")" "the PR description was edited at 2026-11-25T11:00:00Z by author"
_gq OPEN "$H" '"2026-11-25T09:00:00Z"'
_eq "…an edit before the draft is fine"           "$(_post --dry-run <<<"$P" | jq -r .result)" "ready"

c() { jq -cn --arg l "$1" --arg t "${3:-User}" --arg at "$2" '{user:{login:$l, type:$t}, updated_at:$at, submitted_at:$at, state:"COMMENTED"}'; }
printf '[%s,%s,%s,%s,%s]' "$(c me 2026-11-25T11:00:00Z)" "$(c github-actions[bot] 2026-11-25T11:00:00Z Bot)" \
  "$(c Copilot 2026-11-25T11:00:00Z)" "$(c author 2026-11-25T09:59:59Z)" "$(c Me 2026-11-25T12:00:00.123Z)" >"$GH_FIX/api-issues-comments.json"
_eq "after the draft: only the reviewer, bots and Copilot; others before it → ready" \
    "$(GH_LOGIN=me _post --dry-run <<<"$P" | jq -r .result)" "ready"
printf '[%s]' "$(c author 2026-11-25T10:30:00Z)" >"$GH_FIX/api-pulls-comments.json"
_has "the author commented inline after the draft" "$(GH_LOGIN=me _refused "$P")" "author commented inline at 2026-11-25T10:30:00Z"
rm -f "$GH_FIX/api-pulls-comments.json"
printf '[%s]' "$(c other 2026-11-25T10:30:00Z)" >"$GH_FIX/api-pulls-reviews.json"
_has "another reviewer reviewed after the draft"  "$(GH_LOGIN=me _refused "$P")" "other reviewed (COMMENTED) at"
_eq "…unless it is before --since, which wins over drafted_at" \
    "$(GH_LOGIN=me _post --dry-run --since 2026-11-25T10:45:00Z <<<"$P" | jq -r .result)" "ready"
rm -f "$GH_FIX/api-pulls-reviews.json" "$GH_FIX/api-issues-comments.json"

: >"$GH_FIX/post-fails"
OUT="$(_post <<<"$P")"; RC=$?
_eq "GitHub rejects the POST → failed, exit 1"     "$(jq -r .result <<<"$OUT") $RC" "failed 1"
_has "…with GitHub's message"                      "$(jq -r '.reasons[0]' <<<"$OUT")" "HTTP 422"
rm -f "$GH_FIX/post-fails"

cp "$GH_FIX/api-pulls-files.json" "$TMP/files.bak"; echo 'not json' >"$GH_FIX/api-pulls-files.json"
rm -f "$GH_FIX/posted.json"; _post <<<"$P" >/dev/null 2>&1; RC=$?
_eq "a check that cannot run fails closed: exit 1, nothing sent" \
    "$RC $([ -e "$GH_FIX/posted.json" ] && echo sent || echo none)" "1 none"
cp "$TMP/files.bak" "$GH_FIX/api-pulls-files.json"

OUT="$(bash "$PRR" post --url https://github.com/o/proj/pull/40 --payload <(printf '%s' "$P") --dry-run)"
_eq "--url without a worktree: no HEAD check"      "$(jq -r .result <<<"$OUT")" "ready"
bash "$PRR" post --url https://github.com/o/proj/pull/41 --worktree "$WT4" <<<"$P" >/dev/null 2>&1
_eq "--url disagreeing with the review worktree → usage error" "$?" "1"
(cd "$R" && bash "$PRR" post <<<"$P" >/dev/null 2>&1)
_eq "not a review branch and no --url → usage error" "$?" "1"

echo
echo "passed: $PASS  failed: $FAIL"
[ "$FAIL" -eq 0 ]
