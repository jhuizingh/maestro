#!/usr/bin/env bash
# baton — the branch registry, for any backend whose substrate is an append-only comment stream.
#
# Sourced by a tracker backend; not executable on its own. beads, jira and github-issues all store
# registry entries as comments, so the envelope and the fold live here once rather than three
# times. See references/tracker.md for the interface this implements.
#
# THE MODEL: AN APPEND-ONLY LOG, FOLDED ON READ.
#
# record-branch and update-branch both APPEND an entry. Nothing is ever edited or deleted.
# list-branches reads the whole log and folds it: entries group by repo+branch, and within a
# group later entries overlay earlier ones KEY BY KEY, so a partial update ({"status":"merged"})
# changes only what it names and leaves the worktree path and creation time alone.
#
# That is forced by the substrate, not chosen for elegance: comment streams are append-only in
# practice everywhere (editing a comment is either unavailable or a permission a workflow tool
# should not need). Designing the fold into the interface means no backend has to fake mutation.
#
# It also buys the thing this whole feature was filed for. The log IS the provenance record: a
# bead that had three branches over its life leaves three groups, each with its own history, and
# an interrupted session can only ever leave a stale entry — never a half-written one.
#
# THE ENVELOPE. One JSON object per comment, on a single line:
#
#     {"baton":"branch","v":1,"repo":"…","branch":"…","worktree":"…","created":"…","status":"open"}
#
# `"baton":"branch"` is the discriminator. A comment that is not JSON, or is JSON without that
# key, is somebody's prose and is SKIPPED SILENTLY — a registry that fell over on a human comment
# would be useless on any tracker people also talk in.
#
# WHAT A BACKEND MUST PROVIDE, and all it must provide:
#   _reg_comments_json <id>   -> a JSON array of {text, created_at} on stdout, oldest first
#   _reg_comment_add   <id> <text>
#
# Requires: jq.

# Fields a caller may set on an entry. Anything else is rejected rather than silently stored:
# a typo'd field would fold in, never be read, and look like it took.
REG_FIELDS="repo branch worktree created status pr ready keep_task_open no_pr_needed note"

# Status values a branch entry may carry. `no-change` is the shape `no-pr-needed` names — work
# that deliberately landed outside the repo — and is why "merged" is not the only terminal value.
REG_STATUSES="open pr-open merged no-change abandoned"

_reg_now() { date -u +%Y-%m-%dT%H:%M:%SZ; }

_reg_die() { echo "tracker[$BATON_TRACKER_TYPE]: $*" >&2; exit 1; }

# --- validation -------------------------------------------------------------------------------
_reg_check_field() { # $1 = field name
  local f
  for f in $REG_FIELDS; do [ "$f" = "$1" ] && return 0; done
  _reg_die "unknown branch field '$1' (want: $REG_FIELDS)"
}

_reg_check_status() { # $1 = status value ("" is fine — it means "not being set")
  [ -n "$1" ] || return 0
  local s
  for s in $REG_STATUSES; do [ "$s" = "$1" ] && return 0; done
  _reg_die "unknown branch status '$1' (want: $REG_STATUSES)"
}

# --- writing ----------------------------------------------------------------------------------
# $1 = task id, then field=value pairs. Emits one appended entry.
_reg_append() {
  local id="$1"; shift
  local args=() kv k v
  for kv in "$@"; do
    k="${kv%%=*}"; v="${kv#*=}"
    [ "$k" != "$kv" ] || _reg_die "expected field=value, got '$kv'"
    _reg_check_field "$k"
    [ "$k" != status ] || _reg_check_status "$v"
    args+=(--arg "$k" "$v")
  done
  local payload
  payload="$(jq -cn "${args[@]}" --arg _recorded "$(_reg_now)" \
    '{baton:"branch", v:1} + ($ARGS.named | del(._recorded)) + {recorded:$_recorded}')" \
    || _reg_die "could not build the registry entry"
  _reg_comment_add "$id" "$payload"
}

# --- reading ----------------------------------------------------------------------------------
# THE FOLD. Reads the comment array on stdin, emits the folded branch entries on stdout.
#
# Kept as one jq program, and kept pure, so scripts/test-tracker.sh can exercise it with no
# tracker, no network and no backend at all — the registry decides what baton:cleanup-worktrees
# treats as finished, so its rules are pinned by tests rather than by a careful reading.
_reg_fold() {
  jq -c '
    # Parse each comment, keeping only well-formed registry entries. `try` rather than a regex:
    # a human comment that merely starts with "{" must be skipped, not fail the whole read.
    [ .[]
      | . as $c
      | (try (.text | fromjson) catch null) as $e
      | select($e != null and ($e | type) == "object" and $e.baton == "branch")
      | $e + {_at: ($c.created_at // $e.recorded // "")}
    ]
    # Oldest first. The comment stream is already ordered, but a backend that returns it any
    # other way must not silently invert which entry wins.
    | sort_by(._at)
    | map(select((.branch // "") != ""))
    | . as $all

    # WHICH REPOS A BRANCH NAME IS KNOWN IN. update-branch names only the branch — requiring a
    # repo just to flip a status would be friction on every call, for a collision that has never
    # occurred — so a repo-less entry has to find its group. Only entries stating a repo define
    # one.
    | ( reduce $all[] as $e ({};
          if (($e.repo // "") != "")
          then .[$e.branch] = (((.[$e.branch] // []) + [$e.repo]) | unique)
          else . end) ) as $repos

    # Expand a repo-less entry to one copy per repo that branch is known in. With none it stays a
    # single entry under the empty repo (the plain one-repo case, before anything named a repo);
    # with two it applies to both, as documented. Never a third group of its own — that is what
    # made an update-branch look like it took and then vanish from the folded view.
    | [ $all[]
        | . as $e
        | if (($e.repo // "") != "") then $e
          else ( ($repos[$e.branch] // []) as $rs
                 | if ($rs | length) == 0 then $e
                   else ($rs[] | . as $r | $e + {repo: $r}) end )
          end ]

    | group_by((.repo // "") + " " + .branch)
    | map(
        . as $g
        # Overlay key by key, oldest to newest: a partial update touches only what it names.
        | ( reduce $g[] as $e ({}; . + ($e | del(.baton, .v, ._at, .recorded))) )
        + { created: ($g | map(.created // empty) | first // ($g[0]._at // "")),
            updated: ($g[-1]._at // $g[-1].recorded // ""),
            revisions: ($g | length) }
      )
    | sort_by(.created)
  '
}

# $1 = task id
_reg_list() {
  _reg_comments_json "$1" | _reg_fold
}

# $1 = task id, $2 = branch, then field=value pairs. Appends a partial entry naming the branch.
#
# Matching is on the branch name alone unless a repo=… is given. A task with the same branch name
# in two repos is possible and vanishingly rare; making every caller pass a repo to update a
# status would be friction on the common path for a case that has never occurred.
_reg_update() {
  local id="$1" branch="$2"; shift 2
  [ -n "$branch" ] || _reg_die "update-branch needs a branch"
  _reg_append "$id" "branch=$branch" "$@"
}
