#!/usr/bin/env bash
# baton — tests for the entry hop: the cascade every skill uses to find the plugin's scripts.
#
# The cascade (references/locating-baton.md) is the one piece of baton that cannot live in a
# script, because it is how a skill FINDS the scripts. So it is copied into every Bash block that
# needs it, and copies drift. The bug these tests exist for: the old two-rung hop
# (`${CLAUDE_PLUGIN_ROOT:-$HOME/code/maestro/baton}`) never looked in the installed plugin cache,
# so on a published install with no dev clone every skill ended up at a path that didn't exist.
#
# Four properties are pinned:
#   1. the old two-rung pattern appears nowhere;
#   2. every copy of the cascade is byte-identical to the one in references/locating-baton.md;
#   3. every skill code block that uses $BATON computes it in that same block (shell state does
#      not survive from one block to the next, so an inherited $BATON is an empty one);
#   4. the cascade picks the right root in each situation (harness env, dev clone, install record
#      over a newer cached version, newest cached with no record, CLAUDE_CONFIG_DIR honoured) and
#      fails loudly when nothing matches. It runs under bash and, where it is installed, zsh,
#      because the Bash tool runs the user's shell.
#
# Hermetic: the behaviour block builds a fake HOME and config dir under a temp dir and never
# touches the real ~/.claude. Exit 0 = all passed.

set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(dirname "$HERE")"
REF="$ROOT/references/locating-baton.md"
[ -r "$REF" ] || { echo "not found: $REF" >&2; exit 2; }

PASS=0; FAIL=0
_ok()  { PASS=$((PASS+1)); printf '  ✅ %s\n' "$1"; }
_bad() { FAIL=$((FAIL+1)); printf '  ❌ %s\n     %s\n' "$1" "$2"; }
_eq()  { if [ "$2" = "$3" ]; then _ok "$1"; else _bad "$1" "got '$2', want '$3'"; fi }

# The canonical cascade: the first six lines of the first fenced block in the reference.
CASCADE="$(awk '/^```/{n++; next} n==1' "$REF" | head -6)"
FIRST="$(head -1 <<<"$CASCADE")"
[ "$(wc -l <<<"$CASCADE")" -eq 6 ] && [[ "$FIRST" == "# Locate baton:"* ]] \
  || { echo "could not extract the cascade from $REF" >&2; exit 2; }

# ============================================================================================
echo "no old two-rung entry hop remains"

OLD="$(grep -rnE 'CLAUDE_PLUGIN_ROOT:-\$HOME/code/maestro/baton|\|\| *[A-Z_]+="\$HOME/code/maestro/baton/' \
  "$ROOT" --include='*.md' --include='*.sh' --include='*.zsh' --include='*.json' \
  | grep -v "^$HERE/test-entry-hop.sh:" || true)"
_eq "no \${CLAUDE_PLUGIN_ROOT:-\$HOME/code/maestro/baton} and no dev-clone-only fallback" "$OLD" ""

# ============================================================================================
echo "every copy of the cascade is identical to the reference"

COPIES=0
while IFS= read -r f; do
  # For each line that opens a cascade, compare it and the five after it (indent stripped).
  while IFS=: read -r ln _; do
    COPIES=$((COPIES+1))
    GOT="$(sed -n "${ln},$((ln+5))p" "$f" | sed -E 's/^[[:space:]]+//')"
    [ "$GOT" = "$CASCADE" ] || _bad "${f#$ROOT/}:$ln" "cascade copy differs from references/locating-baton.md"
  done < <(grep -nF -- "$FIRST" "$f")
done < <(grep -rlF -- "$FIRST" "$ROOT")
[ "$COPIES" -gt 20 ] && _ok "$COPIES copies checked" || _bad "copies found" "only $COPIES — the extraction is not finding them"

# ============================================================================================
echo "every skill block that uses \$BATON computes it in the same block"

for f in "$ROOT"/skills/*/SKILL.md; do
  BAD="$(awk -v first="$FIRST" '
    /^[[:space:]]*```/ { infence = !infence; have = 0; next }
    infence { s = $0; sub(/^[[:space:]]+/, "", s)
              if (s == first) have = 1
              else if ($0 ~ /\$BATON([^A-Za-z0-9_]|$)/ && !have) print NR }' "$f")"
  [ -z "$BAD" ] && _ok "${f#$ROOT/}" || _bad "${f#$ROOT/}" "\$BATON used before the cascade on line(s): $(echo $BAD)"
done

# ============================================================================================
echo "the cascade resolves the right root"

T="$(mktemp -d)"; trap 'rm -rf "$T"' EXIT
printf '%s\n' "$CASCADE" 'printf %s "$BATON"' > "$T/cascade.sh"

_plugin() { mkdir -p "$1/scripts"; }            # a directory that looks like a baton root
_record() {                                      # <config-dir> <installPath>
  mkdir -p "$1/plugins"
  jq -n --arg p "$2" '{version: 2, plugins: {"baton@maestro": [
    {scope: "project", installPath: "/nowhere"}, {scope: "user", installPath: $p}]}}' \
    > "$1/plugins/installed_plugins.json"
}
# Run the cascade in a clean env: only HOME, PATH, and whatever the caller adds.
_run() { local sh="$1"; shift; env -i PATH="$PATH" "$@" "$sh" "$T/cascade.sh" 2>"$T/err"; }

SHELLS="bash"; command -v zsh >/dev/null 2>&1 && SHELLS="bash zsh"
for SH in $SHELLS; do
  echo " ($SH)"
  H="$T/$SH"

  # Published install: no dev clone, no CLAUDE_PLUGIN_ROOT, several cached versions, and an
  # install record naming one that is NOT the newest. The record must win.
  C="$H/home/.claude"
  for v in 0.9.0 0.10.0 0.10.3; do _plugin "$C/plugins/cache/maestro/baton/$v"; done
  _record "$C" "$C/plugins/cache/maestro/baton/0.10.0"
  _eq "install record beats the newest cached version" \
      "$(_run "$SH" HOME="$H/home")" "$C/plugins/cache/maestro/baton/0.10.0"

  _eq "CLAUDE_PLUGIN_ROOT, when it is a real root, wins" \
      "$(_run "$SH" HOME="$H/home" CLAUDE_PLUGIN_ROOT="$C/plugins/cache/maestro/baton/0.9.0")" \
      "$C/plugins/cache/maestro/baton/0.9.0"

  _eq "a CLAUDE_PLUGIN_ROOT that doesn't exist falls through" \
      "$(_run "$SH" HOME="$H/home" CLAUDE_PLUGIN_ROOT="$T/missing")" "$C/plugins/cache/maestro/baton/0.10.0"

  # A record whose installPath has been pruned from disk must not be trusted.
  _record "$C" "$C/plugins/cache/maestro/baton/0.8.0"
  _eq "a stale install record falls through to the newest cached (version order, not text)" \
      "$(_run "$SH" HOME="$H/home")" "$C/plugins/cache/maestro/baton/0.10.3"

  rm "$C/plugins/installed_plugins.json"
  _eq "no install record → newest cached" \
      "$(_run "$SH" HOME="$H/home")" "$C/plugins/cache/maestro/baton/0.10.3"

  # The dev clone sits above the installed plugin, so contributors' setups keep working.
  _plugin "$H/home/code/maestro/baton"
  _eq "dev clone beats the installed plugin" \
      "$(_run "$SH" HOME="$H/home")" "$H/home/code/maestro/baton"
  rm -rf "$H/home/code"

  # CLAUDE_CONFIG_DIR moves the whole plugin dir; ~/.claude must then be ignored.
  CC="$H/alt-config"
  _plugin "$CC/plugins/cache/maestro/baton/0.10.1"
  _eq "CLAUDE_CONFIG_DIR is honoured over ~/.claude" \
      "$(_run "$SH" HOME="$H/home" CLAUDE_CONFIG_DIR="$CC")" "$CC/plugins/cache/maestro/baton/0.10.1"

  # Nothing anywhere: exit non-zero with the message, and print no path.
  OUT="$(_run "$SH" HOME="$H/empty")"; RC=$?
  _eq "nothing found → exits non-zero" "$RC" "1"
  _eq "nothing found → prints no path" "$OUT" ""
  grep -q 'baton: cannot locate plugin scripts' "$T/err" \
    && _ok "nothing found → says why on stderr" || _bad "nothing found → says why on stderr" "$(cat "$T/err")"
done

# ============================================================================================
echo
echo "passed: $PASS  failed: $FAIL"
[ "$FAIL" -eq 0 ]
