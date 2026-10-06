#!/usr/bin/env bash
# baton — tests that no skill or script re-emits captured JSON through `echo`.
#
# Skills capture JSON in a variable (`CTX="$("$RESOLVER")"`, `BEAD="$("$TRK" get ...)"`) and read
# fields back out of it. The Bash tool runs the user's shell — zsh on macOS — and zsh's `echo`
# (like dash's, and bash's with xpg_echo) interprets backslash escapes. So `echo "$CTX" | jq`
# turns the two characters `\n` inside a JSON string into a real newline, which JSON forbids, and
# jq aborts. Any context with a multi-line hook or guidance string has one. The captures come back
# empty and nothing says so: GUIDE resolved to /guidance.md and every worker hook was skipped
# (jbh-grjr). The safe forms are `jq ... <<<"$VAR"` (what skills use) and
# `printf '%s' "$VAR" | jq ...` (what scripts and hooks use).
#
# Two properties are pinned:
#   1. no file under baton/ pipes an `echo` of a variable into jq;
#   2. every single-line `jq ... <<<"$CTX"` read in a SKILL.md actually parses a context whose
#      strings contain escaped newlines — under bash and, where it is installed, zsh, which is
#      also where property 1's premise (echo mangles the JSON) is checked, so the lint can't
#      outlive the hazard it exists for unnoticed.
#
# Hermetic: reads the repo and a fixture; touches no tracker or config. Exit 0 = all passed.

set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(dirname "$HERE")"

PASS=0; FAIL=0
_ok()  { PASS=$((PASS+1)); printf '  ✅ %s\n' "$1"; }
_bad() { FAIL=$((FAIL+1)); printf '  ❌ %s\n     %s\n' "$1" "$2"; }

# ============================================================================================
echo "no echo of a variable is piped into jq"

# An `echo` whose arguments (up to the first pipe) mention a variable, with jq later in the
# pipeline. Covers skills, references, hooks, scripts and the shell integration alike. Markdown
# prose that names the pattern in an inline `code span` is exempt; the snippets skills run live in
# fenced blocks, whose lines carry no backticks.
HITS="$(grep -rnE '\becho\b[^|;&]*\$[^|;&]*\|[^;&]*\bjq\b' "$ROOT" \
  --include='*.md' --include='*.sh' --include='*.zsh' \
  | grep -v "^$HERE/test-json-reemit.sh:" \
  | grep -vE '^[^:]*\.md:[0-9]+:.*`[^`]*\becho\b' || true)"
if [ -z "$HITS" ]; then
  _ok "none found"
else
  while IFS= read -r h; do
    h="${h#$ROOT/}"
    _bad "${h%%:*}:$(cut -d: -f2 <<<"$h")" "$(cut -d: -f3- <<<"$h" | sed -E 's/^[[:space:]]+//') — use jq ... <<<\"\$VAR\" or printf '%s' \"\$VAR\" | jq"
  done <<<"$HITS"
fi

# ============================================================================================
echo "every skill's \$CTX read parses a context with escaped newlines"

T="$(mktemp -d)"; trap 'rm -rf "$T"' EXIT

# A context shaped like a real one, whose hook and guidance strings carry `\n` escapes.
cat > "$T/ctx.json" <<'EOF'
{"name":"fixture","_workspace":"/ws","_match":"default","guidance":"guide.md",
 "code_root":"~/code","worktree_base":"{code_root}/{repo}-worktrees","member_repos":["~/code/a"],
 "task_tracking":{"type":"beads","dir":"~/tracker"},
 "github":{"owner":"someone","new_repo_prefix":"x-"},
 "work_mode":{"default":"worktree-new-session","home":"inline"},
 "handoff":{"launcher":"tmux","dangerous":false},
 "required_tools":["yq"],
 "hooks":{"worker":{"on_resume":["line one\nline two"],"pre_finish":["a\n\tb"]}},
 "note":"multi\nline\nguidance"}
EOF

# Each single-line read, prefixed with its file and line so a failure says where it lives. The
# jq call must open on that line — the last line of a multi-line program also ends in <<<"$CTX",
# but is not runnable on its own.
LINES="$(grep -rnF '<<<"$CTX"' "$ROOT"/skills/*/SKILL.md \
  | awk '{ code = $0; sub(/^[^:]*:[0-9]+:/, "", code); pre = code; sub(/\047.*/, "", pre)
           if (pre ~ /(^|[^A-Za-z0-9_])jq( |$)/) print }' || true)"
N="$(grep -c . <<<"$LINES")"
[ "$N" -ge 20 ] && _ok "$N reads found" || _bad "reads found" "only $N — the extraction is not finding them"

SHELLS="bash"; command -v zsh >/dev/null 2>&1 && SHELLS="bash zsh"
for SH in $SHELLS; do
  echo " ($SH)"

  # The premise: if this shell's echo leaves `\n` alone, the lint above guards nothing here.
  if [ "$SH" = zsh ]; then
    zsh -c 'CTX="$(cat "$1")"; echo "$CTX" | jq -e . >/dev/null 2>&1' _ "$T/ctx.json" \
      && _bad "zsh echo mangles escaped newlines" "it didn't — the hazard this test guards is gone or the fixture lost its \\n" \
      || _ok "zsh echo mangles escaped newlines (the hazard is real)"
  fi

  BAD=0
  while IFS= read -r entry; do
    loc="${entry%%:*}:$(cut -d: -f2 <<<"$entry")"; code="$(cut -d: -f3- <<<"$entry")"
    # Run the line itself; a jq parse error is the failure. Assignments and prints both work.
    err="$("$SH" -c 'CTX="$(cat "$1")"; HOME=/home/x; eval "$2" >/dev/null' _ "$T/ctx.json" "$code" 2>&1)"
    if [ -n "$err" ]; then BAD=$((BAD+1)); _bad "${loc#$ROOT/}" "$err"; fi
  done <<<"$LINES"
  [ "$BAD" -eq 0 ] && _ok "all $N reads parse cleanly"
done

# ============================================================================================
echo
echo "passed: $PASS  failed: $FAIL"
[ "$FAIL" -eq 0 ]
