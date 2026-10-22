#!/usr/bin/env bash
# baton — tests that no skill snippet relies on bash's word-splitting of an unquoted expansion.
#
# Skill snippets run in the user's shell — zsh on macOS — and zsh does not word-split unquoted
# parameter expansions. So `${PR_URL:+--pr-url "$PR_URL"}` arrives as ONE argument,
# `--pr-url https://…`, the script rejects it as an unknown argument, and the `|| RS=""` after
# the capture hides that: baton:finish recorded a review's state and unpushed as `unknown` on
# every close (jbh-kxol). The shell-safe form keeps flag and value in separate expansions, each of
# which is one word or nothing in either shell: `${V:+--flag} ${V:+"$V"}`.
#
# Two properties are pinned:
#   1. no Markdown file under baton/ has a `${V:+…}` expansion whose alternative holds a space;
#   2. the safe form passes the flag and its value as two arguments under bash and, where it is
#      installed, zsh — which is also where the hazard itself is checked, so the lint can't
#      outlive it unnoticed.
#
# Hermetic: reads the repo only. Exit 0 = all passed.

set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(dirname "$HERE")"

PASS=0; FAIL=0
_ok()  { PASS=$((PASS+1)); printf '  ✅ %s\n' "$1"; }
_bad() { FAIL=$((FAIL+1)); printf '  ❌ %s\n     %s\n' "$1" "$2"; }
_eq()  { if [ "$2" = "$3" ]; then _ok "$1"; else _bad "$1" "got '$2', want '$3'"; fi; }

# ============================================================================================
echo "no multi-word \${V:+…} expansion in a skill snippet"

# Scripts are exempt: they run under their own `#!/usr/bin/env bash`.
HITS="$(grep -rnE '\$\{[A-Za-z_][A-Za-z0-9_]*:\+[^}]* [^}]*\}' "$ROOT" --include='*.md' || true)"
if [ -z "$HITS" ]; then
  _ok "none found"
else
  while IFS= read -r h; do
    h="${h#$ROOT/}"
    _bad "${h%%:*}:$(cut -d: -f2 <<<"$h")" "$(cut -d: -f3- <<<"$h" | sed -E 's/^[[:space:]]+//') — use \${V:+--flag} \${V:+\"\$V\"}"
  done <<<"$HITS"
fi

# ============================================================================================
echo "the safe form splits in every shell"

SAFE='V="https://x/pull/1"; printf "[%s]" a ${V:+--pr-url} ${V:+"$V"} b; E=; printf "[%s]" ${E:+--x} ${E:+"$E"} c'
WANT='[a][--pr-url][https://x/pull/1][b][c]'
_eq "bash" "$(bash -c "$SAFE")" "$WANT"
if command -v zsh >/dev/null 2>&1; then
  _eq "zsh"  "$(zsh -c "$SAFE")" "$WANT"
  _eq "zsh really does not split the unsafe form (the premise)" \
      "$(zsh -c 'V=1; printf "[%s]" ${V:+--pr "$V"}')" "[--pr 1]"
else
  echo "  (zsh not installed — skipped)"
fi

echo
echo "passed: $PASS  failed: $FAIL"
[ "$FAIL" -eq 0 ]
