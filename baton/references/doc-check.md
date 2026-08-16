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
- Any doc prose this diff *adds or edits* describes the end state rather than the merge state —
  see the rule below. Unlike the checks above, this one is about the wording of the diff's own doc
  changes, so it applies whenever the diff touches a doc at all.

If none of these apply, say so in one line and move on. **Don't manufacture a doc change to
justify running this step** — an internal refactor with no user/operator-facing effect needs no
doc update, and forcing one just adds noise. The end-state rule below doesn't loosen that: it
constrains *how* doc prose is written when the diff already writes some, and is never a reason to
go touch a doc that didn't otherwise need touching.

### Docs describe end state, never merge state

**Never write a line into a doc whose truth depends on the change not having landed yet.** A line
like that is accurate while the PR is open and false the instant it merges — the merge itself
falsifies it, with no further edit and nothing to trip over. An ordinary doc pass only asks whether
a doc is stale *right now*, and at PR-open time the doc and the code agree perfectly, so there's
nothing to flag; the wrongness is scheduled for the future. That's why it needs its own rule.

Treat a merge-state line as a **defect to fix before the PR opens** — not something to annotate,
and not something to schedule a follow-up commit or a docs-only PR for. Both of those just accept
a known-wrong window on `main`.

Recognisable forms — mechanical to check. Scan the diff's added doc lines for:

- **Review-pipeline status**: "in PR", "not yet merged", "pending", "in progress", "awaiting
  review", "will be fixed by #N".
- **Status markers encoding open-ness**: `⏳`, `🚧`, an unchecked `- [ ]`, a `TODO:` prefix — used
  to mean "this change hasn't landed yet" rather than "this work doesn't exist yet".
- **Self-referential cleanup notes**: "remove this once #123 lands" written *inside* PR #123, or
  "once `<bead-id>` is done" inside that bead's own PR.
- **Date-stamped not-yet claims**: "not yet deployed as of 2026-08-16", "currently still on the old
  version", "will be true after rollout".

The rewrite, not just the ban. Say what the change *does* and what was *verified*, in the present
tense, and let the tracker carry review state — that's what it's for. If a line exists only to
record that something is being worked on, it belongs in the tracker and should be dropped from the
doc rather than rephrased.

| Instead of | Write |
| --- | --- |
| `- Foo leaks memory (jbh-6ys, P1 — fix in PR, not yet merged)` | Nothing — this PR fixes it, so don't add the open item at all (or delete it if it's already there). |
| `Ingest is single-threaded (parallel version pending in #202).` | `Ingest runs four workers in parallel (verified: 4× throughput on the 1M-row sample).` |
| `⏳ Migrate to v2 — in progress` | `Migrated to v2.` — or leave the item alone entirely if this PR doesn't finish it. |

Scope note: this only bites status/tracking prose — `TODO.md`, changelogs, migration-status and
roadmap docs. Docs that describe end state by construction (API reference, architecture,
inventories) are immune already, which is exactly the point: writing merge-agnostically makes every
doc behave the way those do.

## If something applies

1. State specifically what's stale or missing, and where (file + section).
2. Ask the user where the fix should land:
   - **Same PR** — the fix folds into the PR already open for this task. This is usually the
     right default: it keeps the code change and its doc update reviewable together.
   - **Separate PR** — better when the PR for this task is already merged, or when the doc fix is
     large enough (a big rewrite, a new reference doc) that bundling would blow the PR's scope.
3. Apply only after the user confirms scope and destination. Never silently push a docs commit
   the user didn't ask for.

A merge-state line is the one exception to step 2: it's text this branch is itself adding, so
there's no destination to choose — reword it in place, mention it in one line, and move on.

## Call sites

- **`baton:finish`**, before closing the leaf bead — catches anything the task's own PR should
  have included.
- **`baton:pr`**, before creating/updating the PR — catches it earlier, while the PR is still
  open and bundling is cheapest.

Running it twice on the same task is fine and expected: `pr` runs it first (PR still open, so
"same PR" is nearly free), `finish` runs it again as a backstop for anything that changed after
the PR was opened, or for tasks that skipped `baton:pr` entirely.
