# Documentation pass (shared procedure)

Referenced by `baton:finish` (Step 4) and `baton:pr` (Step 3) — run this exact procedure from
both call sites rather than re-deriving it.

## What to check

Look at what this task actually changed (`git diff` against the branch's base, or the set of
files touched this session). Using the repo's *own* conventions — never invent a doc file a repo
doesn't already have — check whether:

- The repo's `CLAUDE.md`/`AGENTS.md` names a doc that must stay in sync with a certain kind of
  change (e.g. "update HARDWARE_INVENTORY.md when hardware changes", "update STORAGE.md when a
  PVC changes"). Check whether this diff matches one of those named triggers.
- A `README` or other top-level doc describes something (an architecture diagram, an inventory,
  a status list, a "how to do X" section) that this diff makes stale, wrong, or incomplete.
- A doc explicitly mentions something this task just removed, renamed, or superseded.

If none of these apply, say so in one line and move on. **Don't manufacture a doc change to
justify running this step** — an internal refactor with no user/operator-facing effect needs no
doc update, and forcing one just adds noise.

## If something applies

1. State specifically what's stale or missing, and where (file + section).
2. Ask the user where the fix should land:
   - **Same PR** — the fix folds into the PR already open for this task. This is usually the
     right default: it keeps the code change and its doc update reviewable together.
   - **Separate PR** — better when the PR for this task is already merged, or when the doc fix is
     large enough (a big rewrite, a new reference doc) that bundling would blow the PR's scope.
3. Apply only after the user confirms scope and destination. Never silently push a docs commit
   the user didn't ask for.

## Call sites

- **`baton:finish`**, before closing the leaf bead — catches anything the task's own PR should
  have included.
- **`baton:pr`**, before creating/updating the PR — catches it earlier, while the PR is still
  open and bundling is cheapest.

Running it twice on the same task is fine and expected: `pr` runs it first (PR still open, so
"same PR" is nearly free), `finish` runs it again as a backstop for anything that changed after
the PR was opened, or for tasks that skipped `baton:pr` entirely.
