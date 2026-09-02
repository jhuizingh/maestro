# maestro

Marketplace repo for the Claude Code plugins in this workspace — `baton/` is the main one.
See [README.md](./README.md) for what it is and [baton/README.md](./baton/README.md) for the
workflow it implements.

## Interface first, specifics second

**When adding or changing a feature, define the seam before the implementation.** Settle the
verb set, the data passed across it, and the config extension point first; only then write the
backend that satisfies it.

Building the concrete case first means the abstraction gets reverse-engineered from a single
example — which reliably produces a seam shaped like its only implementation, and a painful
retrofit when the second one arrives. Defining the interface up front is cheap; retrofitting one
is not. **This holds even when only one backend will exist initially** — shipping one
implementation behind a real interface is the goal, not a reason to skip the interface.

Concretely, for this repo: a new capability should land as a documented verb set plus a
provider/dispatch point, with the first backend as one implementation of it — not as direct
calls to a specific tool scattered across skills.

Worked example of getting this wrong, and what it cost: `context.yaml` carried
`task_tracking.type` from the very first version — implying pluggable trackers — while every
skill read only `task_tracking.dir` and hardcoded `bd`. The declared extension point did nothing,
and the count of direct callers kept *growing*, because with no seam to reach for each new skill
added its own calls (`baton:status` did, in 0.7.0). By the time it was fixed in 0.8.0 it was
fourteen skills plus a hook, and the retrofit touched all of them at once.

Two things that retrofit made concrete, both worth remembering. Bugs that belong to a backend
metastasize without a seam: `bd show --json` returns a single-element *array*, and the
`type=="array"` guard every call site needed was missing in one — silently producing an empty
status on every `baton:cleanup-worktrees` run and killing two of its buckets. And a feature can
be blocked outright by the missing seam: the branch registry (`jbh-sh5.2`) is a tracker write, so
building it beads-specifically would have had to be redone for the next backend, which is why the
verb set landed before the first caller.

That seam is now `baton/scripts/tracker.sh` + `baton/scripts/tracker/<type>.sh`, documented in
`baton/references/tracker.md`. **No skill may call `bd`** — the one deliberate exception is
`baton:beads`, which *is* the beads backend's own audit skill.

## Conventions

- **Always use a PR. Never push directly to `main`.** Branch, open a PR, merge it — even for a
  one-line docs change, and even though the early history of this repo was pushed straight to
  `main`. That history is not the convention; this is.
  Work on a branch in a **worktree**, not by checking one out in the primary clone — the primary
  clone stays on `main` so git's "same branch in two worktrees" guard keeps working.
- **Never edit the installed plugin cache** (`~/.claude/plugins/cache/maestro/...`) — that's a
  build artifact. Source of truth is this repo; changes reach a machine via the plugin update
  mechanism described in the README.
- Plugin behaviour must stay generic. Anything user- or context-specific belongs in a user's own
  config repo (`context.yaml`, `guidance.md`), never here — see **Config-driven, not hardcoded**
  in the README's design principles.
