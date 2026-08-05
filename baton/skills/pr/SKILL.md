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
export BEADS_DIR="$(echo "$CTX" | jq -r '.task_tracking.dir' | sed "s|^~|$HOME|")"
WS="$(echo "$CTX" | jq -r '._workspace')"
GUIDE="$WS/$(echo "$CTX" | jq -r '.guidance // "guidance.md"')"
```

`LEAF` = derived from the current branch (`<leaf-id>-<slug>`) if this is a baton worktree; else
proceed without a bead reference (not every PR is tied to a tracked leaf). Read `$GUIDE` and
honor it.

If `LEAF` resolved, check its labels (`bd show "$LEAF" --json`) for `autonomous-safe`. This
skill's own behavior barely changes either way — see Step 5 — but note it so the PR body/summary
you produce can mention it, and so you know `baton:finish` will handle the merge automatically
once checks are green rather than waiting for the user to ask.

### Step 2 — Already have an open PR?

```bash
git fetch origin --quiet
BR="$(git rev-parse --abbrev-ref HEAD)"
gh pr view "$BR" --json url,state 2>/dev/null
```

If one exists and is `OPEN`, report its URL and stop — don't open a duplicate. (A `MERGED` or
`CLOSED` result means the branch moved on since; treat as "no open PR" and continue.)

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
