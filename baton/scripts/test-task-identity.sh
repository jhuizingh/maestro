#!/usr/bin/env bash
# baton — tests for scripts/task-identity.sh, the identity seam.
#
# This is the one script whose answer other skills ACT on destructively:
# baton:cleanup-worktrees removes a worktree and deletes its branch based on the leaf it
# reports. A wrong answer here is not a cosmetic bug, so the resolution ladder — carrier,
# then directory name, then branch — is pinned down by tests rather than by a careful reading.
#
# Hermetic: builds throwaway git repos under a temp dir, touches nothing outside it, and needs
# only git and jq. The tracker is a stub that knows the ids listed in $TASKS. Run it directly;
# exit 0 = all passed.

set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
IDENT="$HERE/task-identity.sh"
[ -x "$IDENT" ] || { echo "not found: $IDENT" >&2; exit 2; }

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
PASS=0; FAIL=0

# No context file => the plugin defaults, with no dependency on whatever context the machine
# running this happens to resolve.
DEFAULT_CTX="$TMP/default.json"; echo '{}' >"$DEFAULT_CTX"
JIRA_CTX="$TMP/jira.json"
echo '{"naming":{"branch":"{jira}/{slug}","dir":"{leaf}-{slug}"}}' >"$JIRA_CTX"

_ok()   { PASS=$((PASS+1)); printf '  ✅ %s\n' "$1"; }
_bad()  { FAIL=$((FAIL+1)); printf '  ❌ %s\n     %s\n' "$1" "$2"; }
_eq()   { # $1 = what, $2 = got, $3 = want
  if [ "$2" = "$3" ]; then _ok "$1"; else _bad "$1" "got '$2', want '$3'"; fi
}
# Stub tracker seam: `get <id>` succeeds for ids in $TASKS, exits 3 (not found) otherwise.
TASKS="$TMP/tasks"; printf '%s\n' jbh-aaa jbh-bbb jbh-ccc jbh-pl4.1 >"$TASKS"
FAKE_TRK="$TMP/fake-tracker.sh"
cat >"$FAKE_TRK" <<STUB
#!/usr/bin/env bash
while [ "\${1:-}" = --context ]; do shift 2; done
cat >/dev/null
[ "\$1" = get ] && grep -qxF "\$2" "$TASKS" && { echo '{}'; exit 0; }
exit 3
STUB
chmod +x "$FAKE_TRK"
_id()   { "$IDENT" --context "$CTX" --tracker-cmd "$FAKE_TRK" "$@" 2>/dev/null; }

_newrepo() { # $1 = repo name -> prints its path
  local r="$TMP/$1"
  git init -q "$r"
  git -C "$r" config user.email t@t.t; git -C "$r" config user.name T
  : >"$r/f"; git -C "$r" add f; git -C "$r" commit -qm init
  git -C "$r" update-ref refs/remotes/origin/main HEAD
  printf '%s' "$r"
}

echo "task-identity.sh — resolution ladder"
CTX="$DEFAULT_CTX"
REPO="$(_newrepo repo1)"
mkdir -p "$TMP/wts"

# --- the ladder ---------------------------------------------------------------------------
git -C "$REPO" worktree add -q "$TMP/wts/jbh-aaa-thing-one" -b jbh-aaa-thing-one

_eq "no carrier: falls back to the directory name" \
    "$(_id --worktree "$TMP/wts/jbh-aaa-thing-one" | jq -r '.leaf + "/" + .identity_source')" \
    "jbh-aaa/dir"

_eq "a plain read writes nothing" \
    "$(ls "$REPO/.git/worktrees/jbh-aaa-thing-one/baton-identity" 2>/dev/null; echo -n)" ""

_eq "--backfill records a validated fallback" \
    "$(_id --worktree "$TMP/wts/jbh-aaa-thing-one" --backfill | jq -r .carrier_authoritative)" "false"
_eq "…and the next read is authoritative" \
    "$(_id --worktree "$TMP/wts/jbh-aaa-thing-one" | jq -r '.leaf + "/" + .identity_source')" \
    "jbh-aaa/carrier"

# The carrier OUTRANKS both names: rename the branch and the leaf must not move.
git -C "$TMP/wts/jbh-aaa-thing-one" branch -m jbh-zzz-renamed
_eq "carrier outranks a renamed branch" \
    "$(_id --worktree "$TMP/wts/jbh-aaa-thing-one" | jq -r .leaf)" "jbh-aaa"

# Branch rung: a directory name that says nothing, a branch that does.
git -C "$REPO" worktree add -q "$TMP/wts/scratch" -b jbh-bbb-thing-two
_eq "no carrier, unhelpful dir: falls back to the branch" \
    "$(_id --worktree "$TMP/wts/scratch" | jq -r '.leaf + "/" + .identity_source')" \
    "jbh-bbb/branch"
_eq "an unreachable tracker rejects a name-derived leaf" \
    "$("$IDENT" --context "$CTX" --tracker-cmd /bin/false --worktree "$TMP/wts/scratch" >/dev/null 2>&1 \
       && echo resolved || echo rejected)" "rejected"
_eq "so does having no context to ask" \
    "$("$IDENT" --context /dev/null --tracker-cmd "$FAKE_TRK" --worktree "$TMP/wts/scratch" >/dev/null 2>&1 \
       && echo resolved || echo rejected)" "rejected"

# --- the failure the whole design exists to avoid -------------------------------------------
echo "concurrency — one repo, many worktrees"
git -C "$REPO" worktree add -q "$TMP/wts/jbh-ccc-thing-three" -b jbh-ccc-thing-three
for w in jbh-aaa-thing-one jbh-ccc-thing-three; do _id --worktree "$TMP/wts/$w" --backfill >/dev/null; done
GOT="$(for w in jbh-aaa-thing-one jbh-ccc-thing-three; do _id --worktree "$TMP/wts/$w" | jq -r .leaf; done | paste -sd, -)"
_eq "each worktree reports its OWN leaf" "$GOT" "jbh-aaa,jbh-ccc"
# git config would have failed exactly here: at local scope in a linked worktree it writes to
# the SHARED config, so the last writer wins for every worktree of the repo.
git -C "$TMP/wts/jbh-aaa-thing-one" config --local baton.probe AAA
git -C "$TMP/wts/jbh-ccc-thing-three" config --local baton.probe CCC
_eq "(why not git config) --local leaks across worktrees" \
    "$(git -C "$TMP/wts/jbh-aaa-thing-one" config --get baton.probe)" "CCC"

# --- the primary clone must never resolve ---------------------------------------------------
echo "primary clone"
# A repo whose own directory name matches the legacy <leaf>-<slug> shape by accident.
REPO2="$(_newrepo jbh-task-tracking)"
_id --worktree "$REPO2" >/dev/null 2>&1 \
  && _bad "primary clone is rejected" "resolved an identity from the repo's own name" \
  || _ok "primary clone is rejected"
_eq "…and nothing was written to it" \
    "$(ls "$REPO2/.git/baton-identity" 2>/dev/null; echo -n)" ""

# A primary clone on someone's own hyphenated branch — even one naming a REAL task — is not a
# baton worktree: baton never puts a task branch in the primary clone.
REPO3="$(_newrepo qmk_firmware)"
git -C "$REPO3" switch -q -c jbh-aaa-miryoku-keymap
_id --worktree "$REPO3" --backfill >/dev/null 2>&1 \
  && _bad "primary clone on a <leaf>-<slug> branch is rejected" "resolved an identity from its branch" \
  || _ok "primary clone on a <leaf>-<slug> branch is rejected"
_eq "…and nothing was written to it" \
    "$(ls "$REPO3/.git/baton-identity" 2>/dev/null; echo -n)" ""

# --- a worktree some other tool made ---------------------------------------------------------
echo "foreign worktrees"
# Hyphenated dir, branch with a slash, no carrier, and the guessed leaf is no task anywhere.
git -C "$REPO" worktree add -q "$TMP/wts/clear-spec-schema" -b docs/clear-spec-schema
_id --worktree "$TMP/wts/clear-spec-schema" --backfill >/dev/null 2>&1 \
  && _bad "a dir name whose leaf is not a task is rejected" "invented leaf 'clear-spec'" \
  || _ok "a dir name whose leaf is not a task is rejected"
_eq "…and --backfill wrote nothing" \
    "$(ls "$REPO/.git/worktrees/clear-spec-schema/baton-identity" 2>/dev/null; echo -n)" ""
git -C "$REPO" worktree add -q "$TMP/wts/plain" -b pr-prep-real-gates
_id --worktree "$TMP/wts/plain" --backfill >/dev/null 2>&1 \
  && _bad "a branch name whose leaf is not a task is rejected" "invented leaf 'pr-prep'" \
  || _ok "a branch name whose leaf is not a task is rejected"
_eq "a carrier-backed worktree needs no tracker lookup" \
    "$("$IDENT" --context "$CTX" --tracker-cmd /bin/false --worktree "$TMP/wts/jbh-ccc-thing-three" >/dev/null 2>&1 \
       && echo resolved || echo rejected)" "resolved"

# --- configurable names ---------------------------------------------------------------------
echo "naming.branch / naming.dir"
CTX="$JIRA_CTX"
_eq "mint: branch from {jira}, dir from {leaf}" \
    "$(_id --leaf art-xyz --slug my-thing --jira DOT-1234 | jq -r '.branch + " @ " + .dir')" \
    "DOT-1234/my-thing @ art-xyz-my-thing"

_id --leaf art-xyz --slug my-thing >/dev/null 2>&1 \
  && _bad "mint refuses an unfilled {jira}" "minted a branch anyway" \
  || _ok "mint refuses an unfilled {jira}"

git -C "$REPO" worktree add -q "$TMP/wts/art-xyz-my-thing" -b DOT-1234/my-thing
"$IDENT" --context "$CTX" --write-carrier "$TMP/wts/art-xyz-my-thing" \
         --leaf art-xyz --slug my-thing --branch DOT-1234/my-thing --jira DOT-1234 >/dev/null
_eq "a free-form branch still resolves, via the carrier" \
    "$(_id --worktree "$TMP/wts/art-xyz-my-thing" | jq -r '.leaf + " on " + .branch')" \
    "art-xyz on DOT-1234/my-thing"

CTX="$DEFAULT_CTX"
_eq "naming.dir is sanitized to one path segment" \
    "$("$IDENT" --context <(echo '{"naming":{"dir":"{slug}/sub dir"}}') --leaf a-b --slug x | jq -r .dir)" \
    "x-sub-dir"

# --- back-compat ------------------------------------------------------------------------------
echo "back-compat"
_eq "--branch still works" "$(_id --branch jbh-zvs-kids-overnight-hvac | jq -r .leaf)" "jbh-zvs"
_eq "--branch rejects a non-baton branch" \
    "$(_id --branch main >/dev/null 2>&1 && echo resolved || echo rejected)" "rejected"
_eq "--leaf/--slug still works" \
    "$(_id --leaf jbh-zvs --slug 'Kids Overnight HVAC' | jq -r '.slug + " / " + .branch')" \
    "kids-overnight-hvac / jbh-zvs-kids-overnight-hvac"
_eq "dotted child ids survive the ladder" \
    "$(git -C "$REPO" worktree add -q "$TMP/wts/jbh-pl4.1-dotted" -b jbh-pl4.1-dotted \
       && _id --worktree "$TMP/wts/jbh-pl4.1-dotted" | jq -r .leaf)" "jbh-pl4.1"

# --- mutually exclusive inputs -----------------------------------------------------------------
_eq "--worktree rejects --leaf alongside it" \
    "$(_id --worktree "$TMP/wts/jbh-aaa-thing-one" --leaf x >/dev/null 2>&1 && echo accepted || echo rejected)" \
    "rejected"

echo
echo "$PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
