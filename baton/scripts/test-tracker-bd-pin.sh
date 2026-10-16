#!/usr/bin/env bash
# baton — tests for the beads backend's per-context binary and version pin
# (task_tracking.bd_bin / bd_version), its `health` verb, and its `install-tool` verb.
#
# What is pinned here, and why it matters:
#
#   THE GUARD.   A bd newer than the tracker's pin migrates the tracker's schema on first use and
#                forks its Dolt history from every other writer (the 2026-09-16 incident). So every
#                verb refuses such a binary BEFORE invoking it for anything but `version`. An older
#                binary is allowed through: the tracker refuses what it cannot read, and a pin
#                ahead of the machine is the expected state during a staged bump.
#   THE BINARY.  `bd_bin` must reach every invocation. A single bare `bd` left behind means a
#                context pinned to `bd-iq` quietly runs whatever `bd` is first on PATH.
#   HEALTH.      What baton:doctor reports. In particular it must never probe the tracker with a
#                too-new binary, since the probe would be the migration.
#   INSTALL.     Checksum-verified against the release, or nothing is installed at all.
#
# Hermetic: stub bd binaries and a file:// "release", so nothing touches a real tracker, the
# network, or this machine's beads install. Requires bash, jq, curl, tar, and shasum or sha256sum.
# Exit 0 = all passed.

set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
TRACKER="$HERE/tracker.sh"
[ -r "$TRACKER" ] || { echo "not found: $TRACKER" >&2; exit 2; }
for t in jq curl tar; do command -v "$t" >/dev/null 2>&1 || { echo "$t is required" >&2; exit 2; }; done

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
PASS=0; FAIL=0

_ok()  { PASS=$((PASS+1)); printf '  ✅ %s\n' "$1"; }
_bad() { FAIL=$((FAIL+1)); printf '  ❌ %s\n     %s\n' "$1" "$2"; }
_eq()  { if [ "$2" = "$3" ]; then _ok "$1"; else _bad "$1" "got '$2', want '$3'"; fi; }
_has() { case "$2" in *"$3"*) _ok "$1" ;; *) _bad "$1" "'$2' does not contain '$3'" ;; esac; }
_hasnt() { case "$2" in *"$3"*) _bad "$1" "'$2' contains '$3'" ;; *) _ok "$1" ;; esac; }

# =============================================================================================
# _mkbd <path> <version> — a stub bd reporting <version>. Every call is logged to $CALLS as
# "<name> <args>", so a test can assert which binary ran and what it was asked. `list` fails with
# bd's real schema-mismatch text when $STUB_SCHEMA_MISMATCH is set.
# =============================================================================================
CALLS="$TMP/calls"; : >"$CALLS"
_mkbd() {
  mkdir -p "$(dirname "$1")"
  cat >"$1" <<STUB
#!/usr/bin/env bash
echo "\$(basename "\$0") \$*" >>"$CALLS"
case "\${1:-}" in
  version) echo "bd version $2 (deadbeef: test@deadbeef)" ;;
  list)
    [ -n "\${STUB_HANG:-}" ] && sleep 30
    if [ -n "\${STUB_SCHEMA_MISMATCH:-}" ]; then
      echo '{"error": "schema version mismatch: database is at v66, binary knows up to v53 (13 migrations ahead)"}' >&2
      exit 1
    fi
    echo '[]' ;;
  show) echo '[{"id":"'"\${2:-}"'","title":"t","status":"open"}]' ;;
  *) echo "stub bd: unhandled \$*" >&2; exit 1 ;;
esac
STUB
  chmod +x "$1"
}

BIN="$TMP/bin"; mkdir -p "$BIN"
_mkbd "$BIN/bd" 1.1.2
_mkbd "$BIN/bd-iq" 1.3.1
TRK_DIR="$TMP/tracker/.beads"; mkdir -p "$TRK_DIR"

# A PATH with the stubs and the basics, and nothing from this machine's own beads install.
SYS="/usr/bin:/bin:/usr/sbin:/sbin"
_ctx() { # $1 = task_tracking extras as a JSON object
  jq -nc --arg d "$TRK_DIR" --argjson x "${1:-{\}}" '{task_tracking: ({type:"beads", dir:$d} + $x)}'
}
_t() { # $1 = extras JSON, rest = tracker args. Runs with only the stubs on PATH.
  local x="$1"; shift
  _ctx "$x" | PATH="$BIN:$SYS" bash "$TRACKER" --context - "$@"
}
_health() { _t "$1" health | jq -r "$2"; }
_check() { _t "$1" health | jq -r --arg n "$2" '.checks[] | select(.name == $n) | .status + " " + .detail'; }

# =============================================================================================
echo "bd_bin reaches every invocation"

_eq "capabilities names the configured binary for doctor to check" \
    "$(_t '{"bd_bin":"bd-iq"}' capabilities | jq -r '.tools | join(",")')" "bd-iq,jq"
_eq "…and plain bd when none is configured" \
    "$(_t '{}' capabilities | jq -r '.tools | join(",")')" "bd,jq"
_has "capabilities advertises health and install-tool" \
    "$(_t '{}' capabilities | jq -r '.verbs | join(",")')" "health,install-tool"

: >"$CALLS"
_t '{"bd_bin":"bd-iq"}' get x-1 >/dev/null
_has   "a verb runs bd_bin"                 "$(cat "$CALLS")" "bd-iq show x-1"
_hasnt "…and never the bare bd beside it"   "$(cat "$CALLS")" "bd show"

: >"$CALLS"
_t "{\"bd_bin\":\"$BIN/bd-iq\"}" get x-2 >/dev/null
_has "bd_bin may be a path"                 "$(cat "$CALLS")" "bd-iq show x-2"

# An explicit --context is read even when --type and --tracker are also given; otherwise a caller
# that passes all three would silently lose bd_bin.
: >"$CALLS"
_ctx '{"bd_bin":"bd-iq"}' | PATH="$BIN:$SYS" bash "$TRACKER" --context - --type beads --tracker "$TRK_DIR" get x-3 >/dev/null
_has "--context with --type and --tracker still applies bd_bin" "$(cat "$CALLS")" "bd-iq show x-3"

# baton:configure's path: no context at all, so the default binary.
: >"$CALLS"
PATH="$BIN:$SYS" bash "$TRACKER" --type beads --tracker "$TRK_DIR" get x-4 >/dev/null
_has "with no context at all, plain bd runs" "$(cat "$CALLS")" "bd show x-4"

OUT="$(_t '{"bd_bin":"bd-nope"}' get x 2>&1)"; RC=$?
_eq  "a missing bd_bin fails loudly"   "$RC" "1"
_has "…naming the configured binary"   "$OUT" "bd-nope is not installed"

# =============================================================================================
echo
echo "the version guard"

: >"$CALLS"
OUT="$(_t '{"bd_bin":"bd-iq","bd_version":"1.1.2"}' get x 2>&1)"; RC=$?
_eq    "a binary NEWER than the pin is refused"            "$RC" "1"
_has   "…saying it would migrate the tracker"              "$OUT" "would migrate"
_hasnt "…before it is asked anything but its version"      "$(cat "$CALLS")" "show"

OUT="$(_t '{"bd_version":"1.3.1"}' get x 2>&1)"; RC=$?
_eq "an OLDER binary is allowed (a pin ahead of the machine is a staged bump)" "$RC" "0"
OUT="$(_t '{"bd_version":"1.1.2"}' get x 2>&1)"; RC=$?
_eq "the exact pinned version runs"                       "$RC" "0"
OUT="$(_t '{"bd_version":"v1.1.2"}' get x 2>&1)"; RC=$?
_eq "a v-prefixed pin means the same version"             "$RC" "0"
OUT="$(_t '{}' get x 2>&1)"; RC=$?
_eq "an unpinned context is not guarded"                  "$RC" "0"
OUT="$(_t '{"bd_version":"1.1.2"}' capabilities 2>&1)"; RC=$?
_eq "capabilities answers whatever the version"           "$RC" "0"

# Pre-release ordering: an rc sorts BEFORE its release.
_mkbd "$BIN/bd-rc" 1.3.2-rc.1
_mkbd "$BIN/bd-132" 1.3.2
OUT="$(_t '{"bd_bin":"bd-rc","bd_version":"1.3.2"}' get x 2>&1)"; RC=$?
_eq "1.3.2-rc.1 against a 1.3.2 pin is older, so allowed" "$RC" "0"
OUT="$(_t '{"bd_bin":"bd-132","bd_version":"1.3.2-rc.1"}' get x 2>&1)"; RC=$?
_eq "1.3.2 against a 1.3.2-rc.1 pin is newer, so refused" "$RC" "1"
_mkbd "$BIN/bd-1100" 1.10.0
OUT="$(_t '{"bd_bin":"bd-1100","bd_version":"1.9.0"}' get x 2>&1)"; RC=$?
_eq "1.10.0 is newer than 1.9.0 (numeric, not lexical)"   "$RC" "1"

# =============================================================================================
echo
echo "health"

_eq "pinned and matching: ok"        "$(_health '{"bd_bin":"bd-iq","bd_version":"1.3.1"}' .ok)" "true"
_has "…with the tracker probed"      "$(_check '{"bd_bin":"bd-iq","bd_version":"1.3.1"}' tracker)" "ok bd 1.3.1 reads"
_eq "…and reporting tool, version and pin" \
    "$(_health '{"bd_bin":"bd-iq","bd_version":"1.3.1"}' '[.tool,.version,.pinned] | join(" ")')" "bd-iq 1.3.1 1.3.1"

_eq  "older than the pin: not ok"    "$(_health '{"bd_version":"1.3.1"}' .ok)" "false"
_has "…a version failure"            "$(_check '{"bd_version":"1.3.1"}' version)" "fail bd 1.1.2 is older than the pinned 1.3.1"

: >"$CALLS"
_has "newer than the pin: a version failure" \
     "$(_check '{"bd_bin":"bd-iq","bd_version":"1.1.2"}' version)" "fail bd 1.3.1 is NEWER"
_has   "…and the tracker is NOT probed" "$(_check '{"bd_bin":"bd-iq","bd_version":"1.1.2"}' tracker)" "skip"
_hasnt "…not even once"                 "$(cat "$CALLS")" "list"

_has "unpinned: a warning, not a failure" "$(_check '{}' version)" "warn bd 1.1.2, but task_tracking.bd_version is not set"
_eq  "…so still ok"                       "$(_health '{}' .ok)" "true"

_has "a missing binary fails the tool check" "$(_check '{"bd_bin":"bd-nope","bd_version":"1.3.1"}' tool)" "fail bd-nope is not installed (this context needs bd 1.3.1)"
_eq  "…and health still answers (exit 0)" \
     "$(_t '{"bd_bin":"bd-nope"}' health >/dev/null 2>&1; echo $?)" "0"

OUT="$(STUB_SCHEMA_MISMATCH=1 _check '{"bd_version":"1.1.2"}' tracker)"
_has "a tracker the binary cannot read fails, with bd's reason" "$OUT" "fail bd 1.1.2 cannot read this tracker: schema version mismatch: database is at v66"

# A tracker held by another bd process: the probe must give up, not hang session start.
START=$SECONDS
OUT="$(STUB_HANG=1 BATON_HEALTH_PROBE_TIMEOUT=1 _check '{"bd_version":"1.1.2"}' tracker)"
_has "a probe that blocks times out as a warning"  "$OUT" "warn probe timed out after 1s"
_eq  "…promptly"                                   "$([ $((SECONDS - START)) -lt 10 ] && echo yes)" "yes"

OUT="$(_ctx '{}' | jq -c --arg d "$TMP/nowhere" '.task_tracking.dir = $d' \
        | PATH="$BIN:$SYS" bash "$TRACKER" --context - health | jq -r '.checks[] | select(.name=="tracker") | .status')"
_eq "a tracker directory that doesn't exist fails the probe" "$OUT" "fail"

# Duplicates: the same directory twice on PATH, or a symlink, is ONE binary.
DUP="$TMP/dup"; mkdir -p "$DUP"; _mkbd "$DUP/bd" 1.1.2
OUT="$(_ctx '{}' | PATH="$BIN:$DUP:$SYS" bash "$TRACKER" --context - health | jq -r '.checks[] | select(.name=="duplicates") | .detail')"
_has "two different bd binaries on PATH warn" "$OUT" "2 different bd binaries"
OUT="$(_ctx '{}' | PATH="$BIN:$BIN:$SYS" bash "$TRACKER" --context - health | jq -r '[.checks[] | select(.name=="duplicates")] | length')"
_eq  "the same directory listed twice is not a duplicate" "$OUT" "0"
LNK="$TMP/lnk"; mkdir -p "$LNK"; ln -s "$BIN/bd" "$LNK/bd"
OUT="$(_ctx '{}' | PATH="$BIN:$LNK:$SYS" bash "$TRACKER" --context - health | jq -r '[.checks[] | select(.name=="duplicates")] | length')"
_eq  "a symlink to the same binary is not a duplicate"    "$OUT" "0"
OUT="$(_ctx '{"bd_bin":"bd-iq"}' | PATH="$BIN:$SYS" bash "$TRACKER" --context - health | jq -r '[.checks[] | select(.name=="duplicates")] | length')"
_eq  "a bd beside a configured bd-iq is not a duplicate of it" "$OUT" "0"

# Homebrew's beads formula.
BREW="$TMP/brew"; mkdir -p "$BREW"
cat >"$BREW/brew" <<'EOF'
#!/usr/bin/env bash
case "$*" in
  "list --formula beads") exit 0 ;;
  "list --versions beads") echo "beads 1.3.1" ;;
  *) exit 1 ;;
esac
EOF
chmod +x "$BREW/brew"
OUT="$(_ctx '{}' | PATH="$BIN:$BREW:$SYS" bash "$TRACKER" --context - health | jq -r '.checks[] | select(.name=="homebrew") | .status + " " + .detail')"
_has "an installed Homebrew beads warns" "$OUT" "warn Homebrew beads is installed (beads 1.3.1)"

# =============================================================================================
echo
echo "install-tool"

# A file:// release laid out like gastownhall/beads': a tarball holding `bd`, and checksums.txt.
OS="$(uname -s | tr '[:upper:]' '[:lower:]')"
case "$(uname -m)" in x86_64|amd64) ARCH=amd64 ;; arm64|aarch64) ARCH=arm64 ;; *) ARCH="$(uname -m)" ;; esac
_sha() { if command -v shasum >/dev/null 2>&1; then shasum -a 256 "$1"; else sha256sum "$1"; fi | awk '{print $1}'; }
_release() { # $1 = dir, $2 = version, [$3 = "bad" to corrupt the checksum]
  local stage="$TMP/stage-$2"; mkdir -p "$stage" "$1"
  _mkbd "$stage/bd" "$2"
  local asset="beads_$2_${OS}_${ARCH}.tar.gz"
  tar -czf "$1/$asset" -C "$stage" bd
  local sum; sum="$(_sha "$1/$asset")"
  [ "${3:-}" = bad ] && sum="0000${sum#????}"
  printf '%s  %s\n' "$sum" "$asset" >"$1/checksums.txt"
}
REL="$TMP/release"; _release "$REL" 1.3.1
_inst() { # $1 = extras JSON, rest = args
  local x="$1"; shift
  _ctx "$x" | BATON_BEADS_RELEASE_BASE="file://$REL" PATH="$BIN:$SYS" bash "$TRACKER" --context - install-tool "$@"
}

DEST="$TMP/dest"
OUT="$(_inst '{"bd_bin":"bd-iq","bd_version":"1.3.1"}' --dest "$DEST" 2>&1)"; RC=$?
_eq  "installs the pinned release"                 "$RC" "0"
_eq  "…under the configured binary's name"        "$("$DEST/bd-iq" version 2>/dev/null | awk '{print $3}')" "1.3.1"
_has "…and says so"                                "$OUT" "installed $DEST/bd-iq (bd 1.3.1)"

DEST2="$TMP/dest2"
_inst "{\"bd_bin\":\"$DEST2/bd\",\"bd_version\":\"1.3.1\"}" >/dev/null 2>&1
_eq "a bd_bin path is installed exactly there"    "$([ -x "$DEST2/bd" ] && echo yes)" "yes"

OUT="$(_inst '{}' --dest "$TMP/dest3" 2>&1)"; RC=$?
_eq  "no pin and no --version: refused"            "$RC" "1"
_has "…saying where a version comes from"          "$OUT" "set task_tracking.bd_version"

BAD="$TMP/badrelease"; _release "$BAD" 1.3.1 bad
OUT="$(_ctx '{"bd_version":"1.3.1"}' | BATON_BEADS_RELEASE_BASE="file://$BAD" PATH="$BIN:$SYS" \
        bash "$TRACKER" --context - install-tool --dest "$TMP/dest4" 2>&1)"; RC=$?
_eq  "a checksum mismatch installs nothing"         "$RC" "1"
_has "…and says why"                                "$OUT" "checksum mismatch"
_eq  "…leaving no binary behind"                    "$([ -e "$TMP/dest4/bd" ] && echo present || echo absent)" "absent"

# A release whose binary reports a different version than its name claims.
LIE="$TMP/lierelease"; _release "$LIE" 1.3.1
_mkbd "$TMP/stage-lie/bd" 9.9.9; tar -czf "$LIE/beads_1.3.1_${OS}_${ARCH}.tar.gz" -C "$TMP/stage-lie" bd
printf '%s  %s\n' "$(_sha "$LIE/beads_1.3.1_${OS}_${ARCH}.tar.gz")" "beads_1.3.1_${OS}_${ARCH}.tar.gz" >"$LIE/checksums.txt"
OUT="$(_ctx '{"bd_version":"1.3.1"}' | BATON_BEADS_RELEASE_BASE="file://$LIE" PATH="$BIN:$SYS" \
        bash "$TRACKER" --context - install-tool --dest "$TMP/dest5" 2>&1)"; RC=$?
_eq  "a binary that reports the wrong version fails the install" "$RC" "1"
_has "…naming both versions"                                     "$OUT" "reports bd 9.9.9, expected 1.3.1"
_eq  "…before anything is installed"                             "$([ -e "$TMP/dest5/bd" ] && echo present || echo absent)" "absent"

# =============================================================================================
echo
printf 'bd pin: %d passed, %d failed\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
