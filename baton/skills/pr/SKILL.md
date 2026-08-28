---
description: Open (or report) the PR for the current task — runs any context-specific hooks.worker.pre_pr hook, a documentation pass, then creates the PR via gh pr create. The customizable pre-PR extension point. Never merges here (even for autonomous-safe leaves) — that's baton:finish's job.
argument-hint: "[title] [--draft]"
allowed-tools: Bash(*), Read, Edit
---

## baton:pr

### Step 1 — Resolve context + the leaf

```bash
RESOLVER="${CLAUDE_PLUGIN_ROOT:-$HOME/code/maestro/baton}/scripts/resolve-context.sh"
[ -x "$RESOLVER" ] || RESOLVER="$HOME/code/maestro/baton/scripts/resolve-context.sh"
CTX="$("$RESOLVER")" || { echo "$CTX"; exit 1; }
TRK="${CLAUDE_PLUGIN_ROOT:-$HOME/code/maestro/baton}/scripts/tracker.sh"
[ -x "$TRK" ] || TRK="$HOME/code/maestro/baton/scripts/tracker.sh"
WS="$(echo "$CTX" | jq -r '._workspace')"
GUIDE="$WS/$(echo "$CTX" | jq -r '.guidance // "guidance.md"')"
```

`LEAF` = whatever the identity seam reports for this worktree; else proceed without a bead
reference (not every PR is tied to a tracked leaf):

```bash
IDENT="${CLAUDE_PLUGIN_ROOT:-$HOME/code/maestro/baton}/scripts/task-identity.sh"
[ -x "$IDENT" ] || IDENT="$HOME/code/maestro/baton/scripts/task-identity.sh"
ID="$("$IDENT" --worktree "$PWD" --format env)" || ID=""   # capture first, then eval
eval "$ID"          # LEAF SLUG BR DIR SESSION_NAME SESSION_TITLE IDENTITY_SOURCE
```

That reads the identity carrier `baton:start` wrote into this worktree, falling back to the
legacy `<leaf>-<slug>` directory/branch shape for worktrees created before 0.5.0. **Never read a
bead id out of the branch name yourself** — `naming.branch` makes the branch free-form, so
`DOT-1234/some-description` may carry no id at all. A non-zero exit just means "no tracked leaf
here"; carry on without one.

Read `$GUIDE` and honor it.

`tracker.sh` is the one seam to the task tracker — `task_tracking.type` picks the backend behind
it, and it pins the tracker location per call, so nothing here needs `BEADS_DIR`. Never call `bd`
directly; the verb set is documented in `../../references/tracker.md`.

If `LEAF` resolved, check its labels (`"$TRK" get "$LEAF" | jq -r '.labels[]'`) for
`autonomous-safe`. This
skill's own behavior barely changes either way — see Step 5 — but note it so the PR body/summary
you produce can mention it, and so you know `baton:finish` will handle the merge automatically
once checks are green rather than waiting for the user to ask.

### Step 2 — Already have an open PR?

```bash
git fetch origin --quiet
BR="$(git rev-parse --abbrev-ref HEAD)"    # a git ref, not an identity — gh never parses it
gh pr view "$BR" --json url,state 2>/dev/null
```

If one exists and is `OPEN`, report its URL and stop — don't open a duplicate. (A `MERGED` or
`CLOSED` result means the branch moved on since; treat as "no open PR" and continue.)

Then check there is actually something to open a PR *for*:

```bash
git rev-list --count "origin/HEAD..$BR" 2>/dev/null || git rev-list --count "origin/main..$BR"
```

If that's `0`, `gh pr create` will fail with "No commits between…" — but the interesting case is
the one where that's *correct*: a task whose work legitimately happened outside the repo (a REST
API against a live system, a cluster, an external service) has nothing to put in a PR and never
will. Don't manufacture a commit to have something to open. Say so and send the user to
`baton:finish`, which labels that shape `no-pr-needed` so cleanup stops waiting for a merge that
isn't coming. If instead the work simply isn't committed yet, say that — commit first, then re-run
this skill.

### Step 3 — Run worker.pre_pr hooks

Run each action in `hooks.worker.pre_pr` (from `$CTX`) — this is the customizable extension
point for whatever should happen before a PR opens: typechecks, lint, a changelog entry, a
project-specific check. Empty by default (scaffolded by `baton:configure`). If any action fails,
stop and report — don't open the PR over a failing gate unless the user overrides.

### Step 4 — Documentation pass

Follow the shared procedure in `../../references/doc-check.md` (resolve relative to
`${CLAUDE_PLUGIN_ROOT:-$HOME/code/maestro/baton}`). This is the earliest point to catch a stale
doc — the PR isn't open yet, so "same PR" is nearly free.

### Step 5 — Create the PR

Summarize the branch's commits (`git log main...HEAD --oneline`) and, if this is a baton leaf,
the bead's title/description, into a PR body. Use `$ARGUMENTS` as the title if given, else derive
one from the top commit or the bead title. Pass `--draft` through if present in `$ARGUMENTS`.

```bash
gh pr create --title "<title>" --body "<body>" ${DRAFT:+--draft}
```

Report the resulting URL. Do not merge it here, even for an `autonomous-safe` leaf — merging is
`baton:finish`'s job (Step 7), since that's where check-run status is already tracked. For a
non-autonomous leaf, merging stays a separate, explicit step the user asks for.

### Step 6 — Move the branch's registry entry

If `LEAF` resolved, record on the task that this branch now has a PR:

```bash
PR_NUM="$(gh pr view "$BR" --json number -q .number 2>/dev/null)"
[ -n "$LEAF" ] && "$TRK" update-branch "$LEAF" "$BR" status=pr-open ${PR_NUM:+pr="$PR_NUM"}
```

`baton:start` recorded the branch against the task; this moves its status from `open` to
`pr-open` and attaches the PR number. The registry is an append-only log folded on read, so this
is a partial update — it touches only the two fields it names and leaves the worktree path and
creation time alone.

**Not fatal if it fails.** Say so and carry on: the PR exists either way, `baton:finish` writes
the terminal status, and cleanup falls back to the task's labels when a branch has no entry. A
tracker hiccup must never be why a PR doesn't get reported.

Skip this entirely when Step 2 found an already-open PR and stopped — nothing changed, so
there's nothing to record.
