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

# `pwd -P` inside the resolver reports the physical path, so the fixtures must be physical too
# (on macOS $TMPDIR lives under the /var -> /private/var symlink).
TMP="$(cd "$(mktemp -d)" && pwd -P)"
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

echo
echo "$PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
