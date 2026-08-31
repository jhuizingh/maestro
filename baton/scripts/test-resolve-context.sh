#!/usr/bin/env bash
# baton — tests for scripts/resolve-context.sh, the context seam.
#
# Every skill starts by asking this script which context it is in, and the answer picks the
# task tracker: shell/baton.zsh's chpwd hook sets BEADS_DIR from it, so a wrong answer means a
# bare `bd list` reads — and `bd create` writes — the wrong database. That is the bug this file
# exists to pin: a context's own workspace dir used to match nothing, so standing in it fell
# through to whichever context is `default: true`.
#
# Hermetic: builds throwaway workspaces under a temp dir, points the resolver at them with
# $BATON_WORKSPACES, touches nothing outside it, and needs only yq and jq. Run it directly;
# exit 0 = all passed.

set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
RESOLVE="$HERE/resolve-context.sh"
[ -x "$RESOLVE" ] || { echo "not found: $RESOLVE" >&2; exit 2; }

# (On macOS $TMPDIR lives under the /var -> /private/var symlink, hence the pwd -P.)
# Check mktemp BEFORE the cd: `cd ""` is a silent no-op, so folding them together would set TMP
# to $PWD whenever mktemp fails — and the EXIT trap below would then delete the working tree.
TMP="$(mktemp -d)" || { echo "mktemp -d failed" >&2; exit 2; }
[ -n "$TMP" ] && [ -d "$TMP" ] || { echo "mktemp -d gave no directory" >&2; exit 2; }
TMP="$(cd "$TMP" && pwd -P)"   # physical path: the resolver compares against `pwd -P`
trap 'rm -rf "$TMP"' EXIT
PASS=0; FAIL=0

_ok()  { PASS=$((PASS+1)); printf '  ✅ %s\n' "$1"; }
_bad() { FAIL=$((FAIL+1)); printf '  ❌ %s\n     %s\n' "$1" "$2"; }
_eq()  { # $1 = what, $2 = got, $3 = want
  if [ "$2" = "$3" ]; then _ok "$1"; else _bad "$1" "got '$2', want '$3'"; fi
}
_at() { # $1 = dir to resolve from -> prints "<name>/<_match>"
  ( cd "$1" && "$RESOLVE" 2>/dev/null | jq -r '.name + "/" + ._match' ) || echo "ERROR"
}

# --- fixtures ---------------------------------------------------------------------------
# Two contexts, mirroring the real shape of the bug:
#   alpha — the `default: true` one, whose member glob $TMP/code/*-workspace incidentally covers
#           BOTH its own workspace dir and zed's. It is registered FIRST, so before the home pass
#           existed its member_repos pass claimed zed's workspace dir as well as its own.
#   zed   — lists only its app repo; its workspace dir is in nobody's member_repos.
#   gamma — registered at a dir that is NOT its `home:`, so the two are matched separately.
mkdir -p "$TMP/code/alpha-workspace" "$TMP/code/alpha-app" \
         "$TMP/code/zed-workspace" "$TMP/code/zed-app" \
         "$TMP/code/zed-app-worktrees/zed-1-a-task" "$TMP/code/zed-workspace-worktrees/zed-2-b" \
         "$TMP/reg/gamma" "$TMP/code/gamma-elsewhere/sub" "$TMP/code/gamma-elsewhere-worktrees/g-1" \
         "$TMP/elsewhere"

cat >"$TMP/code/alpha-workspace/context.yaml" <<YAML
name: alpha
default: true
task_tracking:
  dir: $TMP/alpha-tracker
home: $TMP/code/alpha-workspace
member_repos:
  - $TMP/code/*-workspace
  - $TMP/code/alpha-app
YAML

cat >"$TMP/code/zed-workspace/context.yaml" <<YAML
name: zed
home: $TMP/code/zed-workspace
member_repos:
  - $TMP/code/zed-app
YAML

cat >"$TMP/reg/gamma/context.yaml" <<YAML
name: gamma
home: $TMP/code/gamma-elsewhere
member_repos: []
YAML

export BATON_WORKSPACES="$TMP/code/alpha-workspace:$TMP/code/zed-workspace:$TMP/reg/gamma"
export BATON_REGISTRY="$TMP/no-such-registry.yaml"   # never read; a stray one must not leak in
unset BATON_CONTEXT

# --- the bug ------------------------------------------------------------------------------
echo "resolve-context.sh — a context resolves from its own home"
_eq "workspace dir resolves to ITS OWN context" "$(_at "$TMP/code/zed-workspace")" "zed/cwd"
_eq "…and beats another context's overlapping glob" \
    "$(cd "$TMP/code/zed-workspace" && "$RESOLVE" | jq -r .name)" "zed"
_eq "a subdirectory of the workspace dir counts too" \
    "$(mkdir -p "$TMP/code/zed-workspace/docs/deep" && _at "$TMP/code/zed-workspace/docs/deep")" \
    "zed/cwd"
_eq "the workspace dir's -worktrees sibling resolves the same way" \
    "$(_at "$TMP/code/zed-workspace-worktrees/zed-2-b")" "zed/cwd"
# The default context's own workspace resolved before too, but only incidentally — via its own
# glob, or, failing that, the default fallback landing on the right answer by coincidence.
_eq "the default context's own workspace still resolves to itself" \
    "$(_at "$TMP/code/alpha-workspace")" "alpha/cwd"

# `home:` pointing somewhere other than the registered workspace dir is matched as well.
echo "home: pointing away from the registered dir"
_eq "the declared home matches" "$(_at "$TMP/code/gamma-elsewhere/sub")" "gamma/cwd"
_eq "…as does its -worktrees sibling" "$(_at "$TMP/code/gamma-elsewhere-worktrees/g-1")" "gamma/cwd"
_eq "…and the registered dir still matches too" "$(_at "$TMP/reg/gamma")" "gamma/cwd"

# --- everything that worked before still works --------------------------------------------
echo "unregressed: member repos, \$BATON_CONTEXT, default"
_eq "a member repo resolves by cwd" "$(_at "$TMP/code/zed-app")" "zed/cwd"
_eq "a member repo's worktree base resolves by cwd" \
    "$(_at "$TMP/code/zed-app-worktrees/zed-1-a-task")" "zed/cwd"
_eq "an explicitly listed member repo resolves by cwd" "$(_at "$TMP/code/alpha-app")" "alpha/cwd"
_eq "BATON_CONTEXT wins over cwd" \
    "$(cd "$TMP/code/zed-workspace" && BATON_CONTEXT=alpha "$RESOLVE" | jq -r '.name + "/" + ._match')" \
    "alpha/env"
_eq "outside everything: the default context" "$(_at "$TMP/elsewhere")" "alpha/default"
_eq "_workspace points at the resolved workspace dir" \
    "$(cd "$TMP/code/zed-workspace" && "$RESOLVE" | jq -r ._workspace)" "$TMP/code/zed-workspace"

# --- failure modes --------------------------------------------------------------------------
echo "failure modes"
_eq "BATON_CONTEXT naming nothing is an error, not a fallback" \
    "$(cd "$TMP/elsewhere" && BATON_CONTEXT=nope "$RESOLVE" >/dev/null 2>&1 && echo resolved || echo rejected)" \
    "rejected"
_eq "no default and no match is an error" \
    "$(cd "$TMP/elsewhere" && BATON_WORKSPACES="$TMP/code/zed-workspace" "$RESOLVE" >/dev/null 2>&1 \
       && echo resolved || echo rejected)" "rejected"

# All the context.yamls are read in one batched yq call, which yq fails as a whole if any single
# file is unparseable. The per-file fallback is what keeps one broken context from blinding the
# resolver to every other one.
echo "a malformed context.yaml doesn't blind the others"
mkdir -p "$TMP/code/broken-workspace"
printf 'name: broken\n  bad: [indent\n' >"$TMP/code/broken-workspace/context.yaml"
BROKEN_WS="$TMP/code/broken-workspace:$BATON_WORKSPACES"
_eq "a member repo still resolves" \
    "$(BATON_WORKSPACES="$BROKEN_WS" _at "$TMP/code/zed-app")" "zed/cwd"
_eq "a workspace dir still resolves" \
    "$(BATON_WORKSPACES="$BROKEN_WS" _at "$TMP/code/zed-workspace")" "zed/cwd"
_eq "the default fallback still works" \
    "$(BATON_WORKSPACES="$BROKEN_WS" _at "$TMP/elsewhere")" "alpha/default"
_eq "a registered dir with no context.yaml is skipped, not fatal" \
    "$(mkdir -p "$TMP/code/empty-ws" && BATON_WORKSPACES="$TMP/code/empty-ws:$BATON_WORKSPACES" \
       _at "$TMP/code/zed-app")" "zed/cwd"

# Every context.yaml is read in one batched yq call. yq emits NO document for an empty file, TWO
# for a multi-document one, and aborts the whole batch on a parse error — so its output lines do
# not correspond 1:1 to the input files. Pairing the Nth output with the Nth file therefore hands
# one context another context's tracker, and a count-based guard cannot see it: one empty file
# (-1) and one multi-document file (+1) cancel exactly.
echo "the reader survives files that don't yield one document each"
mkdir -p "$TMP/code/void-workspace" "$TMP/code/twice-workspace" "$TMP/code/void-mem"
: >"$TMP/code/void-workspace/context.yaml"                                   # no documents
printf 'name: twice\nmember_repos: []\n---\nname: ghost\n' >"$TMP/code/twice-workspace/context.yaml"
ODD_WS="$TMP/code/void-workspace:$TMP/code/twice-workspace:$BATON_WORKSPACES"
_eq "an empty file and a multi-doc file don't swap the others' identities" \
    "$(BATON_WORKSPACES="$ODD_WS" _at "$TMP/code/zed-app")" "zed/cwd"
_eq "a multi-doc context resolves as its FIRST document" \
    "$(BATON_WORKSPACES="$ODD_WS" _at "$TMP/code/twice-workspace")" "twice/cwd"
_eq "…and emits exactly one JSON object" \
    "$(cd "$TMP/code/twice-workspace" && BATON_WORKSPACES="$ODD_WS" "$RESOLVE" 2>/dev/null \
       | jq -s 'length')" "1"
_eq "an empty context.yaml is inert — its own dir falls through to another context" \
    "$(BATON_WORKSPACES="$ODD_WS" _at "$TMP/code/void-workspace")" "alpha/cwd"

# Rung 2 matches on the REGISTERED directory, which is known whether or not the file parses. Left
# ungated, a half-edited context.yaml gets claimed there and then fails to emit — and because
# shell/baton.zsh keeps the previous BEADS_DIR when the resolver prints nothing, the shell would
# silently stay pointed at the tracker of whatever context it came from.
echo "an unreadable context.yaml falls through instead of hard-failing"
_eq "standing in a broken context's own workspace dir still resolves" \
    "$(BATON_WORKSPACES="$BROKEN_WS" _at "$TMP/code/broken-workspace")" "alpha/cwd"
_eq "…and it prints a context, not nothing" \
    "$(cd "$TMP/code/broken-workspace" && BATON_WORKSPACES="$BROKEN_WS" "$RESOLVE" 2>/dev/null \
       | jq -r '.task_tracking.dir // "NONE"')" "$TMP/alpha-tracker"

# Fields are parsed out of JSON, never off line positions: a value yq renders as more than one
# line must not shift the fields after it (which would lose `default: true` entirely).
echo "a malformed field value can't shift the fields after it"
mkdir -p "$TMP/code/oddhome-workspace"
printf 'name: oddhome\nhome:\n  - /one\n  - /two\ndefault: true\nmember_repos: []\n' \
  >"$TMP/code/oddhome-workspace/context.yaml"
_eq "a list-valued home: doesn't swallow default: true" \
    "$(BATON_WORKSPACES="$TMP/code/oddhome-workspace" _at "$TMP/elsewhere")" "oddhome/default"

# _under compares path boundaries, so an unnormalized trailing slash matches nothing at all.
echo "trailing slashes are normalized"
_eq "a registry entry with a trailing slash still resolves its own dir" \
    "$(BATON_WORKSPACES="$TMP/code/zed-workspace/:$TMP/code/alpha-workspace" \
       _at "$TMP/code/zed-workspace")" "zed/cwd"
mkdir -p "$TMP/code/slash-workspace" "$TMP/code/slash-home/sub"
printf 'name: slash\nhome: %s/code/slash-home/\nmember_repos:\n  - %s/code/slash-mem/\n' \
  "$TMP" "$TMP" >"$TMP/code/slash-workspace/context.yaml"
mkdir -p "$TMP/code/slash-mem"
_eq "a home: with a trailing slash still matches" \
    "$(BATON_WORKSPACES="$TMP/code/slash-workspace" _at "$TMP/code/slash-home/sub")" "slash/cwd"
_eq "a member_repos entry with a trailing slash still matches" \
    "$(BATON_WORKSPACES="$TMP/code/slash-workspace" _at "$TMP/code/slash-mem")" "slash/cwd"

echo
echo "$PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
