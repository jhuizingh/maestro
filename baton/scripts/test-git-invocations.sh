#!/usr/bin/env bash
# baton — tests for git-invocations.sh: which git commands a command line runs, and where.
#
# Pure text in, text out; touches no repository. Exit 0 = all passed.

set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
GI="$HERE/git-invocations.sh"
[ -x "$GI" ] || { echo "not found: $GI" >&2; exit 2; }
PASS=0; FAIL=0

# $1 = what, $2 = expected "sub dir override;…" (an unknown dir is ?), command on stdin
_is() {
  local got
  got="$(HOME=/home/me "$GI" --cwd /a --var OK | awk -F'\t' '{printf "%s%s %s%s", (NR>1?";":""), $1, ($2==""?"?":$2), ($3==""?"":" " $3)}')"
  if [ "$got" = "$2" ]; then PASS=$((PASS+1)); printf '  ✅ %s\n' "$1"
  else FAIL=$((FAIL+1)); printf '  ❌ %s\n     want: %s\n     got:  %s\n' "$1" "$2" "$got"; fi
}

echo "directories"
_is "plain"                     "commit /a"              <<<'git commit -m x'
_is "cd then commit and push"   "add /b;commit /b;push /b" <<<'cd /b && git add . && git commit -m x && git push'
_is "relative cd"               "commit /a/../b"         <<<'cd ../b; git commit'
_is "cd over lines"             "commit /b"              <<<$'cd /b\ngit commit'
_is "quoted cd"                 "commit /b c"            <<<'cd "/b c" && git commit'
_is "escaped space"             "commit /b c"            <<<'cd /b\ c && git commit'
_is "cd with a redirect"        "commit /b"              <<<'cd /b 2>/dev/null && git commit'
_is "cd -- and -P"              "commit /b"              <<<'cd -P -- /b && git commit'
_is "cd -"                      "commit /a"              <<<'cd /b && cd - && git commit'
_is "bare cd"                   "commit /home/me"        <<<'cd && git commit'
_is "tilde"                     "commit /home/me/b"      <<<'cd ~/b && git commit'
_is "pushd and popd"            "commit /b;commit ?"     <<<'pushd /b; git commit; popd; git commit'
_is "cd A || exit"              "commit /b"              <<<'cd /b || exit 1; git commit'
_is "cd A || cd B keeps A"      "commit /b"              <<<'cd /b || cd /c; git commit'
_is "subshell does not leak"    "commit /b;push /a"      <<<'(cd /b && git commit); git push'
_is "braces do"                 "commit /b;push /b"      <<<'{ cd /b; git commit; }; git push'
_is "git -C"                    "commit /b"              <<<'git -C /b commit'
_is "git -C relative, twice"    "commit /a/b/c"          <<<'git -C b -C c commit'
_is "git -C with -c"            "commit /b"              <<<'git -C /b -c user.name=x commit'
_is "variable set here"         "commit /b/s"            <<<'WT=/b; git -C "$WT/s" commit'
_is "braced variable"           "commit /b"              <<<'export WT=/b && cd "${WT}" && git commit'
_is "unknown variable"          "commit ?"               <<<'cd "$NO_SUCH_VAR_X" && git commit'
_is "command substitution dir"  "rev-parse /a;commit ?"  <<<'cd "$(git rev-parse --show-toplevel)" && git commit'
_is "relative after unknown"    "commit ?"               <<<'cd "$NO_SUCH_VAR_X"; cd b; git commit'
_is "absolute after unknown"    "commit /b"              <<<'cd "$NO_SUCH_VAR_X"; cd /b; git commit'
_is "loop variable is unknown"  "push ?"                 <<<'w=/b; for w in 1 2; do git -C $w push; done'

echo "what counts as running git"
_is "pipeline and redirect"     "push /a"                <<<'git push 2>&1 | tail -3'
_is "&> redirect"               "push /a;commit /a"      <<<'git push &>/tmp/x; git commit'
_is "background"                "push /a;commit /a"      <<<'git push & git commit'
_is "no spaces round operators" "commit /b;push /b"      <<<'cd /b&&git commit&&git push'
_is "control flow"              "commit /b;push /b"      <<<'if cd /b; then git commit; else git push; fi'
_is "case arms"                 "push /a;commit /a"      <<<'case $x in a) git push;; (*) git commit;; esac'
_is "wrappers"                  "push /a;push /a;push /a" <<<'timeout -k 5 60 git push; env A=b git push; command git push'
_is "absolute git"              "push /a"                <<<'/usr/bin/git push'
_is "inside \$( )"              "commit /b;push /a"      <<<'x="$(cd /b; git commit)"; git push'
_is "inside <( )"               "log /a"                 <<<'diff <(git log) x'
_is "line continuation"         "commit /b"              <<<$'git -C /b \\\n  commit'
_is "arithmetic is not a heredoc" "commit /a"            <<<$'x=$((1<<2)); (( x << 2 ))\ngit commit'
_is "array literal"             "push /b"                <<<'a=(git commit); cd /b; git push'
_is "variable subcommand"       ""                       <<<'git $SUB'
_is "only git"                  ""                       <<<'git'
_is "nothing"                   ""                       </dev/null

echo "text is not a command"
_is "double quotes"   "" <<<'echo "cd /b && git commit"'
_is "single quotes"   "" <<<"bd create -d 'x; git push'"
_is "comment"         "status /a" <<<'git status # ; git push'
_is "# in a word"     "push /a"   <<<'echo a#b; git push'
_is "argument"        ""          <<<'echo git commit'
_is "here-string"     "push /a"   <<<'cat <<<"git commit"; git push'
_is "heredoc"         "push /a"   <<'CMD'
cat > f <<EOF
git commit
it's "unbalanced ( cd /b && git commit
EOF
git push
CMD
_is "quoted, indented heredoc" "push /a" <<'CMD'
cat <<-'END X' | tee f
	git commit
	END X
git push
CMD
_is "two heredocs" "push /a" <<'CMD'
paste <(cat) <<A - <<B
git commit
A
git merge
B
git push
CMD
_is "commit message heredoc" "commit /b;push /b" <<'CMD'
cd /b && git commit -m "$(cat <<'EOF'
Don't (ever "quote" this; git merge x) `git rebase`

case) esac
EOF
)" && git push
CMD
_is "single-quoted ANSI" "push /a" <<<"echo \$'it\\'s; git commit'; git push"
_is "backticks"          "push /a" <<<'echo `git commit`; git push'
_is "default expansion"  "push /a" <<<'echo "${x:-a; git commit "q"}"; git push'
_is "unterminated quote" ""        <<<"echo 'git commit"
_is "unterminated heredoc" ""      <<<$'cat <<EOF\ngit commit'

echo "--var"
_is "prefix"           "commit /b t-2"        <<<'cd /b && OK=t-2 git commit'
_is "prefix is per command" "commit /a t-2;push /a" <<<'OK=t-2 git commit; git push'
_is "export"           "commit /a t-2;push /a t-2"  <<<'export OK=t-2; git commit; git push'
_is "env"              "commit /a t-2"        <<<'env OK=t-2 git commit'
_is "quoted value"     "commit /a t-2"        <<<'OK="t-2" git commit'
_is "unset again"      "commit /a"            <<<'OK=t-2; unset OK; git commit'
_is "as an argument"   "commit /a"            <<<'git commit -m OK=t-2'
_is "in prose"         "commit /a"            <<<'echo "OK=t-2"; git commit'
OK=t-2 _is "the caller's environment" "commit /a" <<<'git commit'

BIG="$(awk 'BEGIN { for (i = 0; i < 4000; i++) print "it'"'"'s line git commit " i }')"
_is "a large heredoc" "push /a" <<<"cat <<'EOF'
$BIG
EOF
git push"

echo
echo "$PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
