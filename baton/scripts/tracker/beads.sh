#!/usr/bin/env bash
# baton — the `beads` tracker backend. Selected by task_tracking.type: beads (also the default).
#
# Invoked by scripts/tracker.sh, never directly by a skill. The verb set, the normalized shapes
# and the branch registry model are documented in references/tracker.md; this file implements
# them for `bd` and explains only the beads-specific parts.
#
# BEADS_DIR IS PINNED ON EVERY CALL, from $BATON_TRACKER_DIR. An ambient BEADS_DIR silently
# redirects `bd` — `-C` and `cd` do not win against it and nothing warns you — which is the
# footgun baton:beads Step 2 exists to catch. Every invocation here goes through _bd(), which
# sets it explicitly, so no caller has to remember and no ambient value can leak in.
#
# THREE bd BEHAVIOURS THIS FILE ABSORBS, so no caller ever meets them again:
#
#   1. `bd show --json` emits a single-element ARRAY, not an object (bd 1.1.0). A bare `.status`
#      makes jq exit 5, and with stderr discarded a `// "unknown"` default never fires — that
#      produced an empty status on every cleanup run, silently killing two of its buckets. `get`
#      returns a bare object.
#   2. `bd show` on an unknown id fails like any other error. Here it exits 3 (NOT FOUND), which
#      a caller can act on without pattern-matching an error message.
#   3. `bd label list` prints a bulleted human list on stdout. `--json` gives a clean array, so
#      that is what `label-list` uses; callers get JSON and never parse bullets.
#
# WHICH bd, AND WHICH VERSION, ARE PER CONTEXT. Two trackers on one machine can need different bd
# versions: a newer bd migrates a tracker's schema on first use (forking its Dolt history from
# every writer still on the old version), and an older bd refuses a migrated one with "schema
# version mismatch". So the context names its binary and pins its version:
#
#   task_tracking.bd_bin      the binary to run (default `bd`): a name on PATH, or a path
#   task_tracking.bd_version  the exact bd version every writer of this tracker runs
#
# Every invocation goes through _bd(), which runs $BD_BIN — never a bare `bd`. A binary NEWER than
# the pin is refused outright, because running it is precisely what migrates the tracker; an older
# one is only reported (by `health`), since the tracker itself refuses what it cannot read. The
# whole task_tracking block arrives as JSON in $BATON_TRACKER_CONFIG, so tracker.sh never has to
# know these keys exist.
#
# Requires: bd (or task_tracking.bd_bin), jq.

set -uo pipefail

_die()  { echo "tracker[beads]: $*" >&2; exit 1; }
_unsup() { echo "tracker[beads]: verb '$1' is not supported by this backend" >&2; exit 4; }

command -v jq >/dev/null 2>&1 || _die "jq is not installed"

# --- the configured binary and its pin ---------------------------------------------------------
_cfg() { printf '%s' "${BATON_TRACKER_CONFIG:-}" | jq -r --arg k "$1" '.[$k] // empty' 2>/dev/null; }
BD_BIN="$(_cfg bd_bin)"; BD_BIN="${BD_BIN:-bd}"
case "$BD_BIN" in "~/"*) BD_BIN="$HOME/${BD_BIN#\~/}" ;; esac
BD_PIN="$(_cfg bd_version)"; BD_PIN="${BD_PIN#v}"

# The version a bd binary reports: "bd version 1.3.1 (c1c4b642a)" -> 1.3.1. Empty if it won't say.
# Run with NO tracker in reach (BEADS_DIR unset, cwd /): this is how a possibly-too-new binary is
# vetted, so it must not be able to open — and migrate — any database while being asked. (bd 1.3.1
# `version` does not open one; that is not a promise every future bd will keep.)
_bd_version_of() {
  ( cd / && env -u BEADS_DIR "$1" version 2>/dev/null ) | awk '$1 == "bd" && $2 == "version" { sub(/^v/, "", $3); print $3; exit }'
}

# 0 when version $1 sorts strictly after $2. Pre-release suffixes sort before their release
# (1.3.2-rc.1 < 1.3.2), which is what `sort -V` does not do on its own, so they are compared on
# the numeric part first.
_newer() {
  [ "$1" != "$2" ] || return 1
  local a="${1%%-*}" b="${2%%-*}"
  if [ "$a" = "$b" ]; then
    # Same release: the one WITHOUT a suffix is newer; two suffixes compare with sort -V.
    case "$1" in *-*) ;; *) return 0 ;; esac
    case "$2" in *-*) ;; *) return 1 ;; esac
  fi
  [ "$(printf '%s\n%s\n' "$1" "$2" | sort -V | tail -1)" = "$1" ]
}

# Refuse a binary newer than the pin. Checked once per invocation, before any verb touches the
# tracker. Unpinned contexts are not checked: there is nothing to compare against.
_guard_version() {
  [ -n "$BD_PIN" ] || return 0
  local v; v="$(_bd_version_of "$BD_BIN")"
  [ -n "$v" ] || _die "$BD_BIN does not report a version — refusing to run it against a tracker pinned to bd $BD_PIN"
  if _newer "$v" "$BD_PIN"; then
    _die "$BD_BIN is bd $v, newer than this context's pinned bd $BD_PIN — running it would migrate the tracker's schema and fork it from every other writer. Install the pinned release (baton:doctor) or bump the pin for all writers at once."
  fi
}

# EVERY bd call needs a tracker directory, and an EMPTY one is not "use the default" — bd falls
# back to discovering a .beads/ from the current directory. Inside a member repo that carries its
# own tracker (a configuration baton:session-start and baton.zsh both support), that silently
# redirects reads AND writes to the wrong database with no error, which is the exact failure this
# seam claims to have removed. A failed context resolve must therefore stop here, loudly, rather
# than proceed against whatever happens to be under $PWD.
_bd() {
  [ -n "${BATON_TRACKER_DIR:-}" ] \
    || _die "no tracker directory resolved (task_tracking.dir) — refusing to run bd, which would fall back to a .beads/ under \$PWD"
  BEADS_DIR="$BATON_TRACKER_DIR" "$BD_BIN" "$@"
}

VERB="${1:-}"; shift || true
[ -n "$VERB" ] || _die "no verb given"

# `capabilities` MUST ANSWER WITHOUT bd; every other verb requires it up front.
#
# Two things forced this shape, both found by CI (which has no bd installed — exactly the machine
# where it matters). First, baton:doctor calls `capabilities` to learn WHICH tools this backend
# needs, so requiring the tool in order to report the tool is a circle: on a machine without bd,
# doctor would get nothing back and could not say what was missing.
#
# Second, checking lazily inside _bd() is not enough. `get` runs `_bd show … 2>/dev/null || exit
# 3`, so a "bd is not installed" death was swallowed and reported as NOT FOUND — a missing tool
# masquerading as a definite answer about a task that is actually fine. That is precisely the
# conflation the rest of this file exists to prevent, so the check belongs here, before any verb
# can bury it.
#
# THE TRACKER DIRECTORY IS CHECKED HERE FOR THE SAME REASON. `get` runs
# `OUT="$(_bd show … 2>/dev/null)" || exit 3`, so a death inside _bd() is swallowed by the
# command substitution and re-reported as NOT FOUND — a missing *tracker* masquerading as a
# definite answer about a task. Checking up front is the only place it cannot be buried.
# `init` and `bootstrap` are exempt: they take the directory as a positional and check it
# themselves, which is how baton:configure creates a tracker before any context exists.
#
# `health` and `install-tool` are exempt too: reporting a missing or wrong bd, and installing the
# right one, are their whole job, so they cannot require it first.
case "$VERB" in
  capabilities|health|install-tool) ;;
  init|bootstrap)
    command -v "$BD_BIN" >/dev/null 2>&1 || _die "$BD_BIN is not installed (baton:doctor can install it)"
    _guard_version
    ;;
  *)
    command -v "$BD_BIN" >/dev/null 2>&1 || _die "$BD_BIN is not installed (baton:doctor can install it)"
    [ -n "${BATON_TRACKER_DIR:-}" ] \
      || _die "no tracker directory resolved (task_tracking.dir is empty or the context did not resolve)"
    _guard_version
    ;;
esac

# --- normalization ----------------------------------------------------------------------------
# bd's statuses happen to line up with baton's closed vocabulary today. Mapping them explicitly
# anyway is the point of a seam: an unrecognized status becomes `unknown`, never a silent `open`.
# cleanup-verdict.sh treats "the lookup failed" and "the task is open" completely differently, so
# collapsing them is how a lookup failure turns into a confident wrong verdict.
#
# bd 1.1.0's full status set is: open, in_progress, blocked, deferred, closed, pinned, hooked
# (`bd list --status <bogus>` prints it). The seam's closed vocabulary has no deferred/pinned/
# hooked, so they map to `open` — they are all non-terminal states, and `open` is both the honest
# reading ("still outstanding") and the fail-safe one (an open task blocks cleanup from removing
# a worktree). They must NOT fall through to `unknown`: downstream, `unknown` means the LOOKUP
# FAILED — cleanup-verdict.sh prints "[bead lookup FAILED — state unknown]" — so a deferred bead
# would be reported as a broken tracker on every cleanup and status run.
_NORM_STATUS_DEF='
  def norm_status:
    if . == "open" then "open"
    elif . == "in_progress" then "in_progress"
    elif . == "blocked" then "blocked"
    elif . == "closed" then "closed"
    elif . == "deferred" or . == "pinned" or . == "hooked" then "open"
    else "unknown" end;'

_NORMALIZE="$_NORM_STATUS_DEF"'
  {
    id:                  (.id // ""),
    title:               (.title // ""),
    description:         (.description // ""),
    acceptance_criteria: (.acceptance_criteria // ""),
    notes:               (.notes // ""),
    status:              ((.status // "unknown") | norm_status),
    priority:            (.priority // null),
    type:                (.issue_type // null),
    assignee:            (.assignee // null),
    labels:              (.labels // []),
    parent:              (.parent // null),
    created_at:          (.created_at // null),
    updated_at:          (.updated_at // null),
    closed_at:           (.closed_at // null),
    close_reason:        (.close_reason // null)
  }'

# bd emits a single-element array from `show`, and plain arrays elsewhere. Both are handled here
# rather than at fourteen call sites.
_one()  { jq "if type==\"array\" then (.[0] // {}) else . end | $_NORMALIZE"; }
_many() { jq "if type==\"array\" then . else [.] end | map($_NORMALIZE)"; }

# EDGE TYPES ARE TRANSLATED, NOT PASSED THROUGH. bd's vocabulary is
# blocks|tracks|related|parent-child|discovered-from; the seam's is
# blocks|parent-child|relates-to|discovered-from. The two overlap but are not the same, so both
# directions map explicitly here. Anything bd grows that the seam has no name for reads as
# `relates-to` — the seam's "there is an edge, but not a structural one" value, which no caller
# acts on. That is a deliberate catch-all for edge TYPE, not the `unknown` rule for status: an
# unrecognized status must never be guessed, because callers branch on it.
_EDGES="$_NORM_STATUS_DEF"'
  def norm_edge:
    if . == "related" then "relates-to"
    elif . == "blocks" or . == "parent-child" or . == "discovered-from" then .
    else "relates-to" end;
  map({ id:        (.id // ""),
        title:     (.title // ""),
        status:    ((.status // "unknown") | norm_status),
        type:      ((.dependency_type // "related") | norm_edge),
        direction: $dir })'

# --- registry ---------------------------------------------------------------------------------
# The registry lives in bd comments. Not the `notes` field: notes are one long prose blob that
# humans write in and baton:start reads, and interleaving machine records there would corrupt
# something a person maintains by hand. Comments are a separate, per-entry, timestamped stream —
# and the same substrate jira and github-issues would use, which is why the fold is shared.
_reg_comments_json() { # $1 = id -> [{text, created_at}, ...]
  _bd comments "$1" --json 2>/dev/null | jq -c 'if type=="array" then . else [] end
    | map({text: (.text // ""), created_at: (.created_at // "")})'
}

_reg_comment_add() { # $1 = id, $2 = text
  _bd comment "$1" "$2" >/dev/null 2>&1 || _die "could not add a registry comment to $1"
}

# shellcheck source=./lib-registry.sh
. "${BATON_TRACKER_LIB:-$(cd "$(dirname "$0")" && pwd)}/lib-registry.sh"

# --- argument helpers -------------------------------------------------------------------------
_need_id() { [ -n "${1:-}" ] || _die "verb '$VERB' needs a task id"; }

case "$VERB" in

  # ============================================================ reading ========================
  get)
    _need_id "${1:-}"
    OUT="$(_bd show "$1" --json 2>/dev/null)" || { echo "tracker[beads]: no such task '$1'" >&2; exit 3; }
    # An empty result is "not found" too: bd exits 0 for some misses, and an empty object would
    # normalize to a task with an empty id and status `unknown` — a confident answer about
    # nothing. Fail closed instead.
    printf '%s' "$OUT" | jq -e 'if type=="array" then (length > 0 and (.[0].id // "") != "")
                                else ((.id // "") != "") end' >/dev/null 2>&1 \
      || { echo "tracker[beads]: no such task '$1'" >&2; exit 3; }
    printf '%s' "$OUT" | _one
    ;;

  list)
    STATUS=""; LABEL=""; LIMIT=""
    while [ $# -gt 0 ]; do
      case "$1" in
        --status) STATUS="${2:-}"; shift 2 || _die "option '$1' needs a value" ;;
        --label)  LABEL="${2:-}";  shift 2 || _die "option '$1' needs a value" ;;
        --limit)  LIMIT="${2:-}";  shift 2 || _die "option '$1' needs a value" ;;
        *) _die "list: unknown option '$1'" ;;
      esac
    done
    ARGS=(list --json)
    [ -n "$STATUS" ] && ARGS+=(--status "$STATUS")
    [ -n "$LABEL" ]  && ARGS+=(--label "$LABEL")
    [ -n "$LIMIT" ]  && ARGS+=(--limit "$LIMIT")
    # `--label` needs `--all` in bd to reach beyond the default status filter; whereami counts
    # ready-for-worktree-delete across closed beads, which is most of them.
    [ -n "$LABEL" ] && [ -z "$STATUS" ] && ARGS+=(--all)
    _bd "${ARGS[@]}" 2>/dev/null | _many
    ;;

  ready)    _bd ready --json 2>/dev/null | _many ;;

  children)
    _need_id "${1:-}"
    _bd children "$1" --json 2>/dev/null | _many
    ;;

  deps)
    _need_id "${1:-}"
    ID="$1"; shift
    DIR=down
    while [ $# -gt 0 ]; do
      case "$1" in --direction) DIR="${2:-down}"; shift 2 || _die "option '$1' needs a value" ;; *) _die "deps: unknown option '$1'" ;; esac
    done
    case "$DIR" in down|up) ;; *) _die "deps: --direction must be down or up (got '$DIR')" ;; esac
    if [ "$DIR" = up ]; then
      _bd dep list "$ID" --direction=up --json 2>/dev/null
    else
      _bd dep list "$ID" --json 2>/dev/null
    fi | jq -c --arg dir "$DIR" "if type==\"array\" then . else [] end | $_EDGES"
    ;;

  label-list)
    _need_id "${1:-}"
    _bd label list "$1" --json 2>/dev/null | jq -c 'if type=="array" then . else [] end'
    ;;

  # ============================================================ writing ========================
  create)
    TITLE="${1:-}"; shift || true
    [ -n "$TITLE" ] || _die "create needs a title"
    DESC=""; PARENT=""; LABELS=""; PRIORITY=""
    while [ $# -gt 0 ]; do
      case "$1" in
        --description) DESC="${2:-}";     shift 2 || _die "option '$1' needs a value" ;;
        --parent)      PARENT="${2:-}";   shift 2 || _die "option '$1' needs a value" ;;
        --labels)      LABELS="${2:-}";   shift 2 || _die "option '$1' needs a value" ;;
        --priority)    PRIORITY="${2:-}"; shift 2 || _die "option '$1' needs a value" ;;
        *) _die "create: unknown option '$1'" ;;
      esac
    done
    ARGS=(create "$TITLE" --json)
    [ -n "$DESC" ]     && ARGS+=(-d "$DESC")
    [ -n "$PARENT" ]   && ARGS+=(--parent "$PARENT")
    [ -n "$LABELS" ]   && ARGS+=(--labels "$LABELS")
    [ -n "$PRIORITY" ] && ARGS+=(-p "$PRIORITY")
    OUT="$(_bd "${ARGS[@]}" 2>/dev/null)" || _die "create failed"
    # Print ONLY the id. A caller capturing `$(tracker.sh create …)` must get something it can
    # use as an id without parsing.
    printf '%s' "$OUT" | jq -r 'if type=="array" then (.[0].id // "") else (.id // "") end' \
      | grep . || _die "create returned no id"
    ;;

  update)
    _need_id "${1:-}"
    ID="$1"; shift
    ARGS=(update "$ID")
    DESC=""; DESC_FILE=""; DESC_SET=no
    while [ $# -gt 0 ]; do
      case "$1" in
        --status)   ARGS+=(--status "${2:-}");   shift 2 || _die "option '$1' needs a value" ;;
        --priority) ARGS+=(--priority "${2:-}"); shift 2 || _die "option '$1' needs a value" ;;
        --title)
          [ -n "${2:-}" ] || _die "update: --title must not be empty"
          ARGS+=(--title "$2"); shift 2 || _die "option '$1' needs a value" ;;
        --description)
          [ "$DESC_SET" = no ] || _die "update: give --description or --description-file, not both"
          DESC="${2:-}"; DESC_SET=yes; shift 2 || _die "option '$1' needs a value" ;;
        --description-file)
          [ "$DESC_SET" = no ] || _die "update: give --description or --description-file, not both"
          DESC_FILE="${2:-}"; DESC_SET=yes; shift 2 || _die "option '$1' needs a value" ;;
        *) _die "update: unknown option '$1'" ;;
      esac
    done
    if [ -n "$DESC_FILE" ]; then
      # `-` is stdin, so a long body can be piped in without shell quoting. Read it here rather
      # than handing bd --body-file: the seam then has ONE rule for what an empty body means,
      # instead of each backend's own (bd needs --allow-empty-description; gh does not care).
      if [ "$DESC_FILE" = - ]; then DESC="$(cat)"
      else
        [ -r "$DESC_FILE" ] || _die "update: cannot read --description-file '$DESC_FILE'"
        DESC="$(cat "$DESC_FILE")"
      fi
    fi
    if [ "$DESC_SET" = yes ]; then
      # REPLACE, never blank. A correction that resolves to nothing is far more likely an unset
      # variable or an empty heredoc than an intent to erase the body — and erasing is not what
      # this flag is for. `note` is the append path; this is the "the text was wrong" path.
      [ -n "$DESC" ] || _die "update: the new description is empty — refusing to blank $ID's body"
      ARGS+=(-d "$DESC")
    fi
    [ ${#ARGS[@]} -gt 2 ] || _die "update needs something to change"
    _bd "${ARGS[@]}" >/dev/null 2>&1 || _die "update of $ID failed"
    ;;

  claim)
    _need_id "${1:-}"
    _bd update "$1" --claim >/dev/null 2>&1 || _die "could not claim $1"
    ;;

  close)
    _need_id "${1:-}"
    ID="$1"; shift
    REASON=""
    while [ $# -gt 0 ]; do
      case "$1" in --reason) REASON="${2:-}"; shift 2 || _die "option '$1' needs a value" ;; *) _die "close: unknown option '$1'" ;; esac
    done
    [ -n "$REASON" ] || _die "close needs --reason (it is the durable record of what was done)"
    _bd close "$ID" --reason "$REASON" >/dev/null 2>&1 || _die "could not close $ID"
    ;;

  reopen)
    _need_id "${1:-}"
    _bd reopen "$1" >/dev/null 2>&1 || _die "could not reopen $1"
    ;;

  label-add)
    _need_id "${1:-}"
    [ -n "${2:-}" ] || _die "label-add needs a label"
    _bd label add "$1" "$2" >/dev/null 2>&1 || _die "could not add label '$2' to $1"
    ;;

  label-remove)
    _need_id "${1:-}"
    [ -n "${2:-}" ] || _die "label-remove needs a label"
    _bd label remove "$1" "$2" >/dev/null 2>&1 || _die "could not remove label '$2' from $1"
    ;;

  link)
    [ -n "${1:-}" ] && [ -n "${2:-}" ] || _die "link needs <from> <to>"
    FROM="$1"; TO="$2"; shift 2
    TYPE=blocks
    while [ $# -gt 0 ]; do
      case "$1" in --type) TYPE="${2:-}"; shift 2 || _die "option '$1' needs a value" ;; *) _die "link: unknown option '$1'" ;; esac
    done
    case "$TYPE" in blocks|parent-child|relates-to|discovered-from) ;;
      *) _die "link: --type must be blocks, parent-child, relates-to or discovered-from" ;;
    esac
    # Translate to bd's own spelling. bd calls this edge `related`, so passing the seam's
    # `relates-to` straight through was rejected by bd on every call — a documented, validated
    # verb that could never once have succeeded.
    case "$TYPE" in relates-to) BD_TYPE=related ;; *) BD_TYPE="$TYPE" ;; esac
    _bd link "$FROM" "$TO" --type "$BD_TYPE" >/dev/null 2>&1 || _die "could not link $FROM -> $TO"
    ;;

  note)
    _need_id "${1:-}"
    [ -n "${2:-}" ] || _die "note needs text"
    _bd note "$1" "$2" >/dev/null 2>&1 || _die "could not add a note to $1"
    ;;

  # ============================================================ registry =======================
  record-branch)
    _need_id "${1:-}"
    ID="$1"; shift
    REPO=""; BRANCH=""; WT=""; STATUS=open; CREATED=""
    while [ $# -gt 0 ]; do
      case "$1" in
        --repo)     REPO="${2:-}";    shift 2 || _die "option '$1' needs a value" ;;
        --branch)   BRANCH="${2:-}";  shift 2 || _die "option '$1' needs a value" ;;
        --worktree) WT="${2:-}";      shift 2 || _die "option '$1' needs a value" ;;
        --status)   STATUS="${2:-}";  shift 2 || _die "option '$1' needs a value" ;;
        --created)  CREATED="${2:-}"; shift 2 || _die "option '$1' needs a value" ;;
        *) _die "record-branch: unknown option '$1'" ;;
      esac
    done
    [ -n "$BRANCH" ] || _die "record-branch needs --branch"
    [ -n "$CREATED" ] || CREATED="$(_reg_now)"
    # --new: this starts a fresh epoch for repo+branch, so a re-used branch name cannot inherit
    # a previous worktree's ready/keep_task_open/no_pr_needed. See lib-registry.sh.
    _reg_append --new "$ID" "repo=$REPO" "branch=$BRANCH" "worktree=$WT" \
                            "created=$CREATED" "status=$STATUS"
    ;;

  list-branches)
    _need_id "${1:-}"
    _reg_list "$1"
    ;;

  update-branch)
    _need_id "${1:-}"
    [ -n "${2:-}" ] || _die "update-branch needs a branch"
    ID="$1"; BRANCH="$2"; shift 2
    [ $# -gt 0 ] || _die "update-branch needs at least one field=value"
    _reg_update "$ID" "$BRANCH" "$@"
    ;;

  # ============================================================ metadata ======================
  capabilities)
    # `tools` names the CONFIGURED binary, so doctor checks for `bd-iq` on a context that runs
    # bd-iq rather than for a `bd` that context never uses.
    jq -n --arg t "beads" --arg bin "$BD_BIN" '{
      type: $t,
      registry: true,
      registry_substrate: "comments",
      tools: [$bin, "jq"],
      update_fields: ["status","priority","title","description"],
      verbs: ["get","list","ready","children","deps","label-list",
              "create","update","claim","close","reopen",
              "label-add","label-remove","link","note",
              "record-branch","list-branches","update-branch",
              "capabilities","health","install-tool",
              "sync","sync-status","remote","init","bootstrap"]
    }'
    ;;

  health)
    # Is this backend's tool the right one, and can it read the tracker? Always exits 0 with a
    # verdict object; a check that cannot run says so as its own result rather than failing the
    # verb. See references/tracker.md (`health`) for the shape baton:doctor reads.
    CHECKS="[]"
    _check() { # $1 name, $2 ok|warn|fail|skip, $3 detail
      CHECKS="$(jq -c --arg n "$1" --arg s "$2" --arg d "$3" '. + [{name:$n, status:$s, detail:$d}]' <<<"$CHECKS")"
    }
    # Every distinct binary called $1 on PATH, resolved through symlinks. A directory listed twice
    # in PATH, or a symlink to a binary already counted, is one binary, not two.
    _real() { realpath "$1" 2>/dev/null || readlink -f "$1" 2>/dev/null || printf '%s\n' "$1"; }
    _all_on_path() {
      local d IFS=:
      for d in $PATH; do [ -n "$d" ] && [ -x "$d/$1" ] && [ ! -d "$d/$1" ] && _real "$d/$1"; done | awk '!seen[$0]++'
    }

    BIN_PATH="$(command -v "$BD_BIN" 2>/dev/null)"
    V=""; PROBE=yes
    if [ -z "$BIN_PATH" ]; then
      _check tool fail "$BD_BIN is not installed${BD_PIN:+ (this context needs bd $BD_PIN)}"
      PROBE=no
    else
      _check tool ok "$BD_BIN → $BIN_PATH"
      V="$(_bd_version_of "$BD_BIN")"
      if [ -z "$V" ]; then
        _check version fail "$BD_BIN does not report a bd version"
        PROBE=no
      elif [ -z "$BD_PIN" ]; then
        _check version warn "bd $V, but task_tracking.bd_version is not set — whatever bd is on PATH gets used, and a newer one migrates the tracker"
      elif [ "$V" = "$BD_PIN" ]; then
        _check version ok "bd $V (pinned)"
      elif _newer "$V" "$BD_PIN"; then
        _check version fail "bd $V is NEWER than the pinned $BD_PIN — every verb refuses to run it, since it would migrate the tracker"
        # Probing with it is exactly what migrates the tracker, so don't.
        PROBE=no
      else
        _check version fail "bd $V is older than the pinned $BD_PIN"
      fi
    fi

    if [ "$PROBE" = no ]; then
      _check tracker skip "not probed — no usable $BD_BIN at the pinned version"
    elif [ -z "${BATON_TRACKER_DIR:-}" ]; then
      _check tracker skip "no tracker directory resolved (task_tracking.dir)"
    elif [ ! -d "$BATON_TRACKER_DIR" ]; then
      _check tracker fail "$BATON_TRACKER_DIR does not exist (bootstrap it)"
    else
      # BOUNDED. An embedded-Dolt tracker allows one bd process at a time, so the probe blocks for
      # as long as another session's bd holds it — and doctor runs at session start, where a hang
      # is worse than an unknown. No coreutils `timeout` on macOS, so: background it and poll.
      PROBE_FILE="$(mktemp)"
      ( _bd list --limit 1 --json >"$PROBE_FILE" 2>&1 ) & PROBE_PID=$!
      WAITED=0; LIMIT="${BATON_HEALTH_PROBE_TIMEOUT:-20}"
      while kill -0 "$PROBE_PID" 2>/dev/null && [ "$WAITED" -lt "$((LIMIT * 4))" ]; do
        sleep 0.25; WAITED=$((WAITED + 1))
      done
      if kill -0 "$PROBE_PID" 2>/dev/null; then
        pkill -P "$PROBE_PID" 2>/dev/null; kill "$PROBE_PID" 2>/dev/null; wait "$PROBE_PID" 2>/dev/null
        PROBE_RC=124
      else
        wait "$PROBE_PID"; PROBE_RC=$?
      fi
      PROBE_OUT="$(cat "$PROBE_FILE")"; rm -f "$PROBE_FILE"
      if [ "$PROBE_RC" -eq 124 ]; then
        _check tracker warn "probe timed out after ${LIMIT}s — another bd process probably holds the tracker; re-run doctor"
      elif grep -qi "schema version mismatch" <<<"$PROBE_OUT"; then
        _check tracker fail "bd $V cannot read this tracker: $(grep -io -m1 'schema version mismatch[^"]*' <<<"$PROBE_OUT")"
      elif [ "$PROBE_RC" -ne 0 ]; then
        _check tracker fail "bd $V could not read this tracker: $(head -1 <<<"$PROBE_OUT")"
      else
        _check tracker ok "bd $V reads $BATON_TRACKER_DIR"
      fi
    fi

    # Homebrew's beads formula is always the latest bd, so `brew upgrade` alone can move a pinned
    # machine past its pin — the 2026-10-16 incident was a `brew install beads`.
    if command -v brew >/dev/null 2>&1 && brew list --formula beads >/dev/null 2>&1; then
      _check homebrew warn "Homebrew beads is installed ($(brew list --versions beads 2>/dev/null)) — brew always tracks the latest bd; remove it (brew uninstall beads) and use pinned release binaries"
    fi
    # A second binary of the same name means which one runs depends on PATH order.
    DUPES="$(_all_on_path "${BD_BIN##*/}")"
    N="$(grep -c . <<<"$DUPES")"
    if [ "$N" -gt 1 ]; then
      _check duplicates warn "$N different ${BD_BIN##*/} binaries on PATH: $(tr '\n' ' ' <<<"$DUPES")— keep one"
    fi

    jq -n --arg t beads --arg bin "$BD_BIN" --arg v "$V" --arg pin "$BD_PIN" --argjson c "$CHECKS" \
      '{type: $t, tool: $bin, version: (if $v == "" then null else $v end),
        pinned: (if $pin == "" then null else $pin end),
        ok: ([$c[] | select(.status == "fail")] | length == 0), checks: $c}'
    ;;

  install-tool)
    # Install the pinned bd as task_tracking.bd_bin, from the gastownhall/beads GitHub release,
    # verified against that release's checksums.txt. Never Homebrew: `brew install beads` installs
    # whatever is latest, which is the opposite of a pin.
    VERSION="$BD_PIN"; DEST=""
    while [ $# -gt 0 ]; do
      case "$1" in
        --version) VERSION="${2:-}"; shift 2 || _die "option '$1' needs a value" ;;
        --dest)    DEST="${2:-}";    shift 2 || _die "option '$1' needs a value" ;;
        *) _die "install-tool: unknown option '$1'" ;;
      esac
    done
    VERSION="${VERSION#v}"
    [ -n "$VERSION" ] || _die "install-tool: no version — set task_tracking.bd_version, or pass --version"
    case "$VERSION" in *[!0-9A-Za-z.-]*) _die "install-tool: '$VERSION' is not a version" ;; esac
    command -v curl >/dev/null 2>&1 || _die "install-tool: curl is not installed"
    # Where it goes: a bd_bin that is a path is installed exactly there; a bare name goes into
    # ~/.local/bin under that name.
    NAME="${BD_BIN##*/}"
    if [ -z "$DEST" ]; then
      case "$BD_BIN" in */*) DEST="${BD_BIN%/*}" ;; *) DEST="$HOME/.local/bin" ;; esac
    fi
    OS="$(uname -s | tr '[:upper:]' '[:lower:]')"
    case "$(uname -m)" in
      x86_64|amd64)  ARCH=amd64 ;;
      arm64|aarch64) ARCH=arm64 ;;
      *) _die "install-tool: unsupported architecture $(uname -m)" ;;
    esac
    ASSET="beads_${VERSION}_${OS}_${ARCH}.tar.gz"
    BASE="${BATON_BEADS_RELEASE_BASE:-https://github.com/gastownhall/beads/releases/download/v$VERSION}"
    WORK="$(mktemp -d)"; trap 'rm -rf "$WORK"' EXIT
    curl -fsSL -o "$WORK/$ASSET" "$BASE/$ASSET"       || _die "install-tool: could not download $BASE/$ASSET"
    curl -fsSL -o "$WORK/checksums.txt" "$BASE/checksums.txt" || _die "install-tool: could not download $BASE/checksums.txt"
    WANT="$(awk -v a="$ASSET" '$2 == a || $2 == "*"a { print $1; exit }' "$WORK/checksums.txt")"
    if command -v shasum >/dev/null 2>&1; then GOT="$(shasum -a 256 "$WORK/$ASSET" | awk '{print $1}')"
    else GOT="$(sha256sum "$WORK/$ASSET" | awk '{print $1}')"; fi
    [ -n "$WANT" ] && [ "$WANT" = "$GOT" ] \
      || _die "install-tool: checksum mismatch for $ASSET (release says ${WANT:-nothing}, download is $GOT) — not installing"
    mkdir -p "$WORK/x" "$DEST" || _die "install-tool: could not create $DEST"
    tar -xzf "$WORK/$ASSET" -C "$WORK/x" bd 2>/dev/null || tar -xzf "$WORK/$ASSET" -C "$WORK/x" ./bd 2>/dev/null \
      || _die "install-tool: $ASSET has no bd binary in it"
    xattr -d com.apple.quarantine "$WORK/x/bd" 2>/dev/null || true
    # Ask the binary itself BEFORE it replaces anything: a release asset that is not the version
    # it is named for must not overwrite a working bd.
    V="$(_bd_version_of "$WORK/x/bd")"
    [ "$V" = "$VERSION" ] || _die "install-tool: $ASSET's binary reports bd ${V:-nothing}, expected $VERSION — not installing"
    install -m 0755 "$WORK/x/bd" "$DEST/$NAME" || _die "install-tool: could not install $DEST/$NAME"
    echo "installed $DEST/$NAME (bd $V)"
    case ":$PATH:" in *":$DEST:"*) ;; *) echo "note: $DEST is not on PATH — add it, or set task_tracking.bd_bin to $DEST/$NAME" ;; esac
    ;;

  sync)
    DIRECTION=pull
    while [ $# -gt 0 ]; do
      case "$1" in
        --pull) DIRECTION=pull; shift ;;
        --push) DIRECTION=push; shift ;;
        *) _die "sync: unknown option '$1'" ;;
      esac
    done
    # Issue state lives in Dolt's own refs, NOT in the tracker repo's git-tracked files — so a
    # clean `git pull` can succeed while `bd show` still returns stale status. This is the sync
    # that actually moves issue data; the caller does the git one separately.
    _bd dolt "$DIRECTION" 2>&1 || _die "bd dolt $DIRECTION failed"
    ;;

  sync-status)
    # How far the local Dolt history and the remote's have diverged, counted against the
    # remote-tracking ref — i.e. as of the last pull or push. No network: call it after a pull.
    #
    # One query, two transports. Server mode answers `bd sql`; embedded mode does not ("not yet
    # supported in embedded mode"), so the same SQL goes to the dolt CLI in the database's own
    # directory. dolt_log() takes literal arguments only, so the remote and branch are read first.
    SHOW="$(_bd dolt show 2>/dev/null)" || _die "sync-status: bd dolt show failed"
    MODE="$(printf '%s\n' "$SHOW" | awk -F': *' '/^ *Mode:/ {print $2; exit}')"
    DB="$(printf '%s\n' "$SHOW" | awk -F': *' '/^ *Database:/ {print $2; exit}')"
    DATA="$(printf '%s\n' "$SHOW" | awk -F': *' '/^ *Data:/ {print $2; exit}')"
    case "$MODE" in
      embedded*)
        command -v dolt >/dev/null 2>&1 \
          || _die "sync-status: the dolt CLI is not installed (needed to read an embedded tracker's history)"
        [ -n "$DB" ] && [ -d "$DATA/$DB/.dolt" ] \
          || _die "sync-status: no Dolt database at '$DATA/$DB'"
        _sql() { ( cd "$DATA/$DB" && dolt sql -r csv -q "$1" ); }
        ;;
      *) _sql() { _bd sql --csv "$1"; } ;;
    esac
    # CSV with a header row; every value below is a single token, so the second line is the value.
    _val() { _sql "$1" 2>/dev/null | sed -n 2p | tr -d '\r'; }
    REMOTE="$(_val "SELECT name FROM dolt_remotes ORDER BY name = 'origin' DESC, name LIMIT 1")"
    [ -n "$REMOTE" ] || _die "sync-status: no remote configured"
    URL="$(_val "SELECT url FROM dolt_remotes WHERE name = '$REMOTE'")"
    BRANCH="$(_val "SELECT active_branch()")"
    [ -n "$BRANCH" ] || _die "sync-status: could not read the active branch"
    REF="remotes/$REMOTE/$BRANCH"
    # A missing ref means this tracker has never pulled or pushed that branch. That is not "in
    # sync", and it is not a count either — so it is an error, and the caller reports unknown.
    AHEAD="$(_val "SELECT COUNT(*) FROM dolt_log('$BRANCH', '--not', '$REF')")"
    BEHIND="$(_val "SELECT COUNT(*) FROM dolt_log('$REF', '--not', '$BRANCH')")"
    for N in "$AHEAD" "$BEHIND"; do
      case "$N" in
        ''|*[!0-9]*) _die "sync-status: could not compare $BRANCH with $REF (has this tracker ever pulled or pushed?)" ;;
      esac
    done
    jq -n --arg remote "$REMOTE" --arg url "$URL" --arg branch "$BRANCH" --arg ref "$REF" \
          --argjson ahead "$AHEAD" --argjson behind "$BEHIND" \
      '{remote: $remote, url: $url, branch: $branch, ref: $ref, ahead: $ahead, behind: $behind}'
    ;;

  remote)
    # Live, never a config comment: bd rewrites sync.remote in .beads/config.yaml silently, so a
    # comment there can sit directly above a line pointing somewhere else entirely. Anything about
    # to push must read the configured value, not the prose. (baton:beads Step 3.)
    OUT="$(_bd dolt remote list 2>/dev/null)" || OUT=""
    # MATCH THE URL, NOT THE COLUMN. With no remote, bd prints the prose "No remotes configured."
    # — whose second whitespace field is "remotes", so a bare `$2` reported a configured remote
    # called "remotes". This verb is what baton:session-start consults BEFORE pulling, so a false
    # "configured" is precisely the wrong direction to fail in. Require something that actually
    # looks like a remote spec: a scheme (`git+ssh://`, `https://`, `file://`) or an scp-style
    # `user@host:path`.
    REMOTE="$(printf '%s\n' "$OUT" \
      | awk '{ for (i=1; i<=NF; i++) if ($i ~ /:\/\// || $i ~ /^[^@[:space:]]+@[^:[:space:]]+:/) { print $i; exit } }')"
    jq -n --arg remote "${REMOTE:-}" --arg raw "$OUT" \
      '{configured: ($remote != ""), remote: $remote, raw: $raw}'
    ;;

  init)
    DIR="${1:-$BATON_TRACKER_DIR}"
    [ -n "$DIR" ] || _die "init needs a directory"
    # `bd init` creates a brand-new, independent Dolt history. Against a project whose tracker
    # already exists elsewhere that makes a SECOND history with no common ancestor — it looks
    # like it worked until the two need to merge, and the only way through is a destructive
    # force-push. Use `bootstrap` unless you are certain nothing exists anywhere.
    mkdir -p "$DIR" || _die "could not create $DIR"
    ( cd "$DIR/.." 2>/dev/null || cd "$DIR"; env -u BEADS_DIR "$BD_BIN" init ) \
      || _die "bd init failed in $DIR"
    ;;

  bootstrap)
    DIR="${1:-$BATON_TRACKER_DIR}"
    [ -n "$DIR" ] || _die "bootstrap needs a directory"
    mkdir -p "$DIR" || _die "could not create $DIR"
    ( cd "$DIR/.." 2>/dev/null || cd "$DIR"; env -u BEADS_DIR "$BD_BIN" bootstrap ) \
      || _die "bd bootstrap failed in $DIR"
    ;;

  *) _unsup "$VERB" ;;
esac
