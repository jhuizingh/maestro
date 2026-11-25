#!/usr/bin/env bash
# baton — list the git invocations a shell command line would run, and where.
#
# Reads a command on stdin and prints one line per git invocation, in order:
#
#   <subcommand> TAB <directory> TAB <value of --var>
#
# The directory follows `cd`, `pushd`, subshells and `git -C`; it is EMPTY when it can't be known
# without running the command. Text that is not executed — quoted arguments, heredoc bodies,
# comments — never produces a line.
#
# Usage:
#   git-invocations.sh --cwd <dir> [--var <NAME>]
#
#   --cwd   the directory the command starts in
#   --var   report the value this variable was given IN THE COMMAND (as a prefix, an assignment or
#           an export) when the invocation runs; the caller's own environment does not count
#
# Not followed: `bash -c`, `eval`, `xargs`, backticks, scripts and aliases; `--git-dir`, `GIT_DIR`
# and `--work-tree` do not move the reported directory. Callers that guard something should treat
# this as "no invocation seen". Runs under bash 3.2 and any POSIX awk.

set -u

CWD=""; VAR=""
while [ $# -gt 0 ]; do
  case "$1" in
    --cwd) CWD="${2:-}"; shift; [ $# -eq 0 ] || shift ;;
    --var) VAR="${2:-}"; shift; [ $# -eq 0 ] || shift ;;
    -h|--help) sed -n '2,24p' "$0"; exit 0 ;;
    *) echo "git-invocations: unknown argument '$1'" >&2; exit 1 ;;
  esac
done

GI_CWD="$CWD" GI_VAR="$VAR" LC_ALL=C awk '
# UNK inside a word marks a part whose value is not known statically.
BEGIN { UNK = "\001"; S = "" }
{ S = S $0 "\n" }
END {
  N = split(S, C, "")
  P = 1; DIR = ENVIRON["GI_CWD"]; OLD = UNK; VARNAME = ENVIRON["GI_VAR"]; NHERE = 0
  while (P <= N) parse_list("")
}

function known(s) { return index(s, UNK) == 0 }

function lookup(name) {
  if (name in VARS) return VARS[name]
  if (name == "PWD") return DIR
  if (name in ENVIRON && name !~ /^(GI_CWD|GI_VAR|LC_ALL)$/) return ENVIRON[name]
  return UNK
}

function resolve(path) {
  if (!known(path)) return UNK
  if (path == "") return DIR
  if (path ~ /^\//) return path
  if (!known(DIR)) return UNK
  return DIR "/" path
}

# Parse commands up to `end` ("" = end of input, or ")"), consuming it. Runs in its own subshell
# when `end` is ")": the caller saves and restores DIR.
function parse_list(end,    W, n, c, word, has, quoted, skip, cases, op) {
  n = 0; word = ""; has = 0; quoted = 0; skip = 0; cases = 0; op = ""
  while (P <= N) {
    c = C[P]
    if (c == "\\") {
      if (C[P+1] == "\n") { P += 2; continue }
      word = word C[P+1]; has = 1; quoted = 1; P += 2; continue
    }
    if (c == "\047") { P++; has = 1; quoted = 1
      while (P <= N && C[P] != "\047") { word = word C[P]; P++ }
      P++; continue
    }
    if (c == "\"") { word = word dquoted(); has = 1; quoted = 1; continue }
    if (c == "$") { word = word dollar(0); has = 1; continue }
    if (c == "`") { backtick(); word = word UNK; has = 1; continue }
    if (c == "#" && !has) { while (P <= N && C[P] != "\n") P++; continue }

    if (c == "~" && !has && (C[P+1] ~ /^[\/ \t\n;&|)]$/)) {
      word = lookup("HOME"); has = 1; P++; continue
    }

    if (c == "<" || c == ">" || (c == "&" && C[P+1] == ">")) {
      if (has && word ~ /^[0-9]+$/ && !quoted) { word = ""; has = 0 }
      if (has) { if (skip) skip = 0; else W[++n] = word; word = ""; has = 0; quoted = 0 }
      if ((c == "<" || c == ">") && C[P+1] == "(") { P += 2; subshell(); word = UNK; has = 1; continue }
      if (c == "<" && C[P+1] == "<" && C[P+2] == "<") { P += 3; skip = 1; continue }
      if (c == "<" && C[P+1] == "<") { P += 2; heredoc(); continue }
      while (P <= N && C[P] ~ /^[<>&|]$/) P++
      skip = 1; continue
    }

    if (c == " " || c == "\t" || c == "\r" || c ~ /^[;&|()\n]$/) {
      if (has) {
        if (skip) skip = 0; else W[++n] = word
        if (n == 1 && !quoted) {
          if (word == "case") cases++
          else if (word == "esac" && cases > 0) cases--
        }
        word = ""; has = 0; quoted = 0
      }
      if (c == " " || c == "\t" || c == "\r") { P++; continue }
      skip = 0
      if (c == "(") {
        # name=( … ) and name() are not commands of their own.
        if (n > 0 && W[n] ~ /=$/ && C[P-1] == "=") { P++; subshell_quiet(); W[n] = W[n] UNK; continue }
        if (n > 0) { run(W, n, op); n = 0; op = "" }
        if (C[P+1] == "(") { arith(); continue }
        P++; subshell(); continue
      }
      run(W, n, op); n = 0
      P++
      if (c == "|" && C[P] == "|") { op = "||"; P++ }
      else if (c == "&" && C[P] == "&") { op = "&&"; P++ }
      else if (c == "|" && C[P] == "&") { op = "|"; P++ }
      else op = c
      if (c == ")") {
        if (cases > 0) continue
        if (end == ")") return
        continue
      }
      if (c == "\n") skip_heredocs()
      continue
    }
    word = word c; has = 1; P++
  }
  if (has && !skip) W[++n] = word
  run(W, n, op)
}

function subshell(    d, o) { d = DIR; o = OLD; parse_list(")"); DIR = d; OLD = o }

# An array literal: consume it, emit nothing.
function subshell_quiet(    q) { q = QUIET; QUIET = 1; subshell(); QUIET = q }

# At "((" (or just past "$" of "$(("): consume the balanced parentheses.
function arith(    depth) {
  depth = 0
  while (P <= N) {
    if (C[P] == "(") depth++
    else if (C[P] == ")") { depth--; if (depth == 0) { P++; return } }
    P++
  }
}

function backtick() {
  P++
  while (P <= N && C[P] != "`") { if (C[P] == "\\") P++; P++ }
  P++
}

function dquoted(    s, c) {
  s = ""; P++
  while (P <= N) {
    c = C[P]
    if (c == "\"") { P++; return s }
    if (c == "\\") {
      if (C[P+1] == "\n") { P += 2; continue }
      if (C[P+1] ~ /^[$`"\\]$/) { s = s C[P+1]; P += 2; continue }
      s = s c; P++; continue
    }
    if (c == "$") { s = s dollar(1); continue }
    if (c == "`") { backtick(); s = s UNK; continue }
    s = s c; P++
  }
  return s
}

# At "$". Returns the value of the expansion, or UNK.
function dollar(inq,    name, depth, c) {
  c = C[P+1]
  if (c == "(") {
    if (C[P+2] == "(") { P++; arith(); return UNK }
    P += 2; subshell(); return UNK
  }
  if (c == "{") {
    P += 2; name = ""; depth = 1
    while (P <= N) {
      c = C[P]
      if (c == "\\") { name = name UNK; P += 2; continue }
      if (c == "\047" && !inq) { P++; while (P <= N && C[P] != "\047") P++; P++; name = name UNK; continue }
      if (c == "\"") { dquoted(); name = name UNK; continue }
      if (c == "$" && name != "") { dollar(inq); name = name UNK; continue }
      if (c == "{") depth++
      if (c == "}") { depth--; if (depth == 0) { P++; break } }
      name = name c; P++
    }
    return (name ~ /^[A-Za-z_][A-Za-z0-9_]*$/) ? lookup(name) : UNK
  }
  if (c == "\047" && !inq) {
    P += 2
    while (P <= N && C[P] != "\047") { if (C[P] == "\\") P++; P++ }
    P++; return UNK
  }
  if (c == "\"" && !inq) { P++; return dquoted() }
  if (c ~ /^[A-Za-z_]$/) {
    P++; name = ""
    while (P <= N && C[P] ~ /^[A-Za-z0-9_]$/) { name = name C[P]; P++ }
    return lookup(name)
  }
  if (c ~ /^[0-9@*#?$!-]$/) { P += 2; return UNK }
  P++; return "$"
}

# Just past "<<": read the delimiter and queue the body for the next newline.
function heredoc(    dash, d, q) {
  dash = 0
  if (C[P] == "-") { dash = 1; P++ }
  while (C[P] == " " || C[P] == "\t") P++
  d = ""
  while (P <= N && C[P] !~ /^[ \t\r\n;&|()<>]$/) {
    q = C[P]
    if (q == "\047" || q == "\"") {
      P++
      while (P <= N && C[P] != q) { d = d C[P]; P++ }
      P++; continue
    }
    if (q == "\\") { P++; d = d C[P]; P++; continue }
    d = d q; P++
  }
  NHERE++; HD[NHERE] = d; HDASH[NHERE] = dash
}

function skip_heredocs(    i, line) {
  for (i = 1; i <= NHERE; i++) {
    while (P <= N) {
      line = ""
      while (P <= N && C[P] != "\n") { line = line C[P]; P++ }
      P++
      if (HDASH[i]) sub(/^\t+/, "", line)
      if (line == HD[i]) break
    }
  }
  NHERE = 0
}

function assign(w,    i) { i = index(w, "="); VARS[substr(w, 1, i - 1)] = substr(w, i + 1) }
function is_assign(w) { return w ~ /^[A-Za-z_][A-Za-z0-9_]*=/ }

function run(W, n, op,    i, w, PRE, np, j, dir, ov) {
  if (n == 0 || QUIET) return
  i = 1; np = 0
  while (i <= n) {
    w = W[i]
    if (is_assign(w)) { PRE[++np] = w; i++; continue }
    if (w ~ /^(if|then|elif|else|while|until|do|!|\{|time|command|builtin|exec|nohup)$/) {
      i++; while (i <= n && W[i] ~ /^-/ && w ~ /^(time|command|exec)$/) i++
      continue
    }
    if (w ~ /^(env|sudo|\/usr\/bin\/env)$/) {
      i++
      while (i <= n && W[i] ~ /^-/) { if (W[i] ~ /^-(u|g|C|S)$/) i++; i++ }
      continue
    }
    if (w ~ /^(timeout|gtimeout|nice)$/) {
      i++
      while (i <= n && W[i] ~ /^-/) { if (W[i] ~ /^-(k|s|n)$/) i++; i++ }
      if (w != "nice") i++
      continue
    }
    break
  }
  if (i > n) { for (j = 1; j <= np; j++) assign(PRE[j]); return }
  w = W[i]

  if (w ~ /^(export|declare|local|typeset|readonly)$/) {
    for (j = i + 1; j <= n; j++) if (is_assign(W[j])) assign(W[j])
    return
  }
  if (w == "unset") { for (j = i + 1; j <= n; j++) VARS[W[j]] = UNK; return }
  if (w == "read" || w == "for") { for (j = i + 1; j <= n && W[j] != "in"; j++) VARS[W[j]] = UNK; return }

  if (w == "cd" || w == "pushd") {
    if (op == "||") return
    j = i + 1
    while (j <= n && W[j] ~ /^-[A-Za-z-]+$/) j++
    if (j > n) dir = (w == "cd") ? lookup("HOME") : UNK
    else if (W[j] == "-") dir = OLD
    else dir = resolve(W[j])
    OLD = DIR; DIR = dir
    return
  }
  if (w == "popd") { OLD = DIR; DIR = UNK; return }

  sub(/^.*\//, "", w)
  if (w != "git") return
  dir = DIR
  for (i++; i <= n; i++) {
    w = W[i]
    if (w == "-C") {
      i++
      if (i > n) return
      if (W[i] ~ /^\// || dir == "") dir = W[i]
      else if (W[i] != "") dir = (known(dir) && known(W[i])) ? dir "/" W[i] : UNK
      continue
    }
    if (w ~ /^(-c|--git-dir|--work-tree|--namespace|--super-prefix|--config-env|--exec-path|--attr-source)$/) {
      if (w != "--exec-path") i++
      continue
    }
    if (w ~ /^-/) continue
    break
  }
  if (i > n || !known(W[i])) return
  ov = (VARNAME != "" && VARNAME in VARS) ? VARS[VARNAME] : ""
  for (j = 1; j <= np; j++) if (VARNAME != "" && index(PRE[j], VARNAME "=") == 1) ov = substr(PRE[j], length(VARNAME) + 2)
  if (!known(dir)) dir = ""
  if (!known(ov)) ov = ""
  gsub(/[\t\n]/, " ", ov)
  if (dir ~ /[\t\n]/) dir = ""
  if (W[i] !~ /[\t\n]/) printf "%s\t%s\t%s\n", W[i], dir, ov
}
'
