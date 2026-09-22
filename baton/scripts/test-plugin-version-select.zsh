#!/usr/bin/env zsh
# baton — tests for shell/baton.zsh's selection of a cached plugin copy.
#
# The chpwd hook sets BEADS_DIR from whichever resolve-context.sh _baton_resolver hands back, so
# the version it picks out of the plugin cache is the version of baton's context resolution that
# a bare `bd` obeys. The bug this file exists for (jbh-9ilr): the cache holds every version ever
# installed, the selector globbed it and took the FIRST match, and glob order is lexicographic —
# not version order. With 0.8.0/0.9.0/0.10.0/0.10.1 cached, that picked 0.10.0; reverse-sorting
# the glob with `(On)` — the fix originally proposed — picks 0.9.0, because as text
# "0.10.0" < "0.8.0". A fix of that shape looks right and is wrong, which is why the ordering is
# asserted here against a cache whose versions have crossed 0.10 rather than eyeballed.
#
# Hermetic: builds a fake $HOME of empty stub scripts, sources the real shell/baton.zsh against
# it, and calls the real functions. Needs zsh only. Exit 0 = all passed.

emulate -L zsh
setopt no_unset

HERE="${0:A:h}"
ZSHRC="$HERE/../shell/baton.zsh"
[[ -r "$ZSHRC" ]] || { print -u2 "not found: $ZSHRC"; exit 2; }

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
PASS=0; FAIL=0

_ok()  { PASS=$((PASS+1)); printf '  ✅ %s\n' "$1"; }
_bad() { FAIL=$((FAIL+1)); printf '  ❌ %s\n     %s\n' "$1" "$2"; }
_eq()  { if [[ "$2" == "$3" ]]; then _ok "$1"; else _bad "$1" "got '$2', want '$3'"; fi }

# A stub resolve-context.sh under a fake HOME. Printing {} keeps the source-time _baton_chpwd
# harmless: the hook reads .task_tracking.dir out of it, finds nothing, and exports nothing.
_cached() {   # <home> <marketplace> <version>
  local d="$1/.claude/plugins/cache/$2/baton/$3/scripts"
  mkdir -p "$d"; print -r -- $'#!/bin/sh\nprintf \'{}\\n\'' > "$d/resolve-context.sh"
  chmod +x "$d/resolve-context.sh"
}
_marketplace() {  # <home> <marketplace>
  local d="$1/.claude/plugins/marketplaces/$2/baton/scripts"
  mkdir -p "$d"; print -r -- $'#!/bin/sh\nprintf \'{}\\n\'' > "$d/resolve-context.sh"
  chmod +x "$d/resolve-context.sh"
}

# Source the plugin's shell integration against a fake HOME and print what _baton_resolver picks.
# A subshell per case so each gets a clean function table and no chpwd hook survives into the next.
_pick() {   # <home>
  ( HOME="$1"; BATON_REGISTRY="$1/no-registry.yaml"; unset BATON_RESOLVER
    source "$ZSHRC" >/dev/null 2>&1
    _baton_resolver )
}
# The version component of a picked cache path, or the whole path when it isn't a cache path.
_ver() { local p="$1"; [[ "$p" == */.claude/plugins/cache/* ]] && print -r -- "${${p%/scripts/resolve-context.sh}:t}" || print -r -- "$p" }

print "== version ordering across the 0.10 boundary =="
H="$TMP/h1"; for v in 0.8.0 0.9.0 0.10.0 0.10.1; do _cached "$H" maestro "$v"; done
_eq "picks the newest, not the lexicographically first (0.10.0) or last (0.9.0)" "$(_ver "$(_pick "$H")")" "0.10.1"

H="$TMP/h2"; for v in 0.9.0 0.10.1 1.0.0; do _cached "$H" maestro "$v"; done
_eq "a major bump outranks everything below it" "$(_ver "$(_pick "$H")")" "1.0.0"

H="$TMP/h3"; _cached "$H" maestro 0.7.0
_eq "a single cached version is that version" "$(_ver "$(_pick "$H")")" "0.7.0"

H="$TMP/h4"; for v in 0.10.1 0.10.10 0.10.2; do _cached "$H" maestro "$v"; done
_eq "patch components compare numerically too" "$(_ver "$(_pick "$H")")" "0.10.10"

print "== the version wins over the marketplace name =="
# Sorting whole paths — including `(nOn)` — sorts on the marketplace name first, so a stale copy
# under a later-sorting marketplace would beat a newer one. The version is the only key.
H="$TMP/h5"; _cached "$H" zzz-other 0.8.0; _cached "$H" maestro 0.10.1
_eq "newest version across marketplaces" "$(_ver "$(_pick "$H")")" "0.10.1"

print "== fallbacks below the cache =="
H="$TMP/h6"; _marketplace "$H" maestro
_eq "no cache -> the marketplace checkout" "$(_pick "$H")" "$H/.claude/plugins/marketplaces/maestro/baton/scripts/resolve-context.sh"

H="$TMP/h7"; mkdir -p "$H"
_eq "nothing anywhere -> empty, not an error" "$(_pick "$H")" ""

H="$TMP/h8"; _cached "$H" maestro 0.10.1
mkdir -p "$H"; print -r -- $'#!/bin/sh\nprintf \'{}\\n\'' > "$H/pinned.sh"; chmod +x "$H/pinned.sh"
R="$( HOME="$H"; BATON_REGISTRY="$H/no-registry.yaml"; BATON_RESOLVER="$H/pinned.sh"
      source "$ZSHRC" >/dev/null 2>&1
      _baton_resolver )"
_eq "an explicit BATON_RESOLVER still wins over the cache" "$R" "$H/pinned.sh"

print "== a non-executable copy is not a candidate =="
H="$TMP/h9"; _cached "$H" maestro 0.9.0; _cached "$H" maestro 0.10.1
chmod -x "$H/.claude/plugins/cache/maestro/baton/0.10.1/scripts/resolve-context.sh"
_eq "skips the newest when it isn't runnable" "$(_ver "$(_pick "$H")")" "0.9.0"

print ""
print "passed: $PASS   failed: $FAIL"
(( FAIL == 0 ))
