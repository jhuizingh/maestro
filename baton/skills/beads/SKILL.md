---
description: Audit and enforce safe beads/Dolt tracker usage — ambient BEADS_DIR drift, config.yaml lying about sync.remote, init vs bootstrap for connecting to an existing tracker, and scratch-clone hygiene. Run directly to audit a tracker; other baton skills that touch bd/dolt/config.yaml point here.
argument-hint: "[tracker-dir]"
allowed-tools: Bash(*), Read
---

## baton:beads

The canonical reference for beads/Dolt tracker safety across baton, plus a live audit. Every
gotcha here was discovered the hard way (misconfigured tracker, silent write to the wrong
database, a near-force-push). Other baton skills that create, connect to, or sync a tracker
should already do the safe thing described below — if something looks wrong, run this skill.

### Step 1 — Resolve the tracker to audit

If `$ARGUMENTS` is a path, use it as `TRACKER`. Otherwise resolve the active context:

```bash
RESOLVER="${CLAUDE_PLUGIN_ROOT:-$HOME/code/maestro/baton}/scripts/resolve-context.sh"
[ -x "$RESOLVER" ] || RESOLVER="$HOME/code/maestro/baton/scripts/resolve-context.sh"
CTX="$("$RESOLVER" 2>/dev/null || true)"
TRACKER="$(echo "$CTX" | jq -r '.task_tracking.dir' | sed "s|^~|$HOME|")"
```

If neither yields a tracker dir, ask which one to audit.

### Step 2 — Ambient `BEADS_DIR` can silently redirect `bd`

A `BEADS_DIR` environment variable set anywhere in the shell overrides `bd`'s normal
directory-based `.beads` discovery — **`-C` and `cd` do not win against it, and there's no
error.** This has silently redirected writes into an unrelated database for entire sessions.
Baton's own shell integration compounds this: it sets `BEADS_DIR` per-context on `cd` into a
member repo, but does **not** re-resolve it on a further plain `cd` — a bare `bd` command run
from a different tracker's directory can silently write to the wrong one.

Check for drift:

```bash
echo "Ambient BEADS_DIR: ${BEADS_DIR:-<unset>}"
BEADS_DIR="$TRACKER" bd context --json | jq -r '.repository // .backend // .'
```

If ambient `BEADS_DIR` is set and does not match `$TRACKER`, flag it loudly — every write in
this shell is at risk until it's unset or the command explicitly overrides it.

**The rule every baton skill already follows:** always pin `BEADS_DIR=<tracker>` explicitly on
each `bd` invocation (or `export` it right after resolving context, as `baton:start`,
`baton:task-add`, `baton:finish`, `baton:split`, `baton:resume`, and `baton:whereami` all do) —
never rely on ambient state or directory-based auto-discovery alone. Spot-check with a known-ID
lookup (`bd show <a-real-id> --json`) rather than trusting a plain `bd list` count if anything
looks unfamiliar.

### Step 3 — `config.yaml` comments can lie about `sync.remote` — verify live

`bd` rewrites the `sync.remote` key in `.beads/config.yaml` silently on `bd dolt remote
add`/auto-detect. A comment in that file (e.g. "sync.remote intentionally unset") can sit
directly above an active line pointing somewhere else entirely — the prose and the value
disagree, and nothing warns you.

**Never trust the comment. Verify with the live command:**

```bash
BEADS_DIR="$TRACKER" bd dolt remote list
```

Confirm the output is exactly the remote you expect (e.g.
`origin  git+ssh://git@github.com/<owner>/<repo>.git`) before running any `bd dolt
push`/`pull` against this tracker. If it's missing, wrong, or points somewhere unexpected —
stop and fix the remote (`bd dolt remote add`/`remove`) before syncing.

**Never copy another repo's `.beads/config.yaml` wholesale** when pointing at an existing
tracker or setting one up to match another context's conventions. That's exactly how a
stale/wrong `sync.remote` ends up live under a comment claiming otherwise.

### Step 4 — `bd init` vs `bd bootstrap`: know which one you're running

These are **not interchangeable**:

- **`bd init`** creates a brand-new, independent Dolt history. Correct only when a tracker for
  this project **does not exist anywhere yet** — e.g. `baton:configure`'s first-time tracker
  creation (`mkdir -p "$TT" && ( cd "$TT" && git init -q && bd init )`).
- **`bd bootstrap`** is non-destructive and auto-detects the right action: clones from
  `sync.remote` if configured, clones from git origin's Dolt data if present, restores from a
  `.beads/backup/*.jsonl`, imports from git-tracked JSONL, or creates fresh only if truly
  nothing exists. **Always use this** when connecting a new clone or git worktree to a tracker
  that may already exist elsewhere (this machine, another machine, or a teammate's).

Running `bd init` against a project that already has a tracker elsewhere creates a second,
unrelated Dolt history with no common ancestor to the real one. It will look like it worked —
right up until the two histories need to merge, which fails, and the only way through is a
destructive `--force`/`--discard-remote` push that can clobber the real tracker's data.

**Rule of thumb:** if `.beads/` doesn't exist yet in a fresh clone or worktree and you are not
certain whether a tracker for this project already exists elsewhere, run `bd bootstrap` —
never `bd init`. It's safe even when nothing exists yet.

### Step 5 — Don't leave full scratch clones lying around

A full `git clone` of a tracker repo just to poke around drags along its entire embedded Dolt
data directory — these accumulate fast and don't clean themselves up (three had to be deleted
from under `~/code` after one investigation). Prefer, in order:

1. `BEADS_DIR="$TRACKER" bd export` — dump issues to JSONL for inspection, no clone needed.
2. `git -C <tracker-repo> ls-remote` — check remote refs without cloning anything.
3. Work directly in the tracker repo's existing checkout rather than a fresh clone.

If a scratch clone was genuinely necessary for a one-off repair, delete it as soon as you're
done — don't leave it as a "just in case" copy.

### Step 6 — Run as a live audit

When invoked directly, run Steps 2–3 and 5 against `$TRACKER` (Step 4 is a decision rule, not
something to check live — note it as a reminder) and print a checklist:

```
baton:beads — <tracker path>
  ✅ BEADS_DIR: no ambient drift (or: ⚠️ ambient BEADS_DIR=<x> does not match <tracker>)
  ✅ dolt remote: git+ssh://git@github.com/<owner>/<repo>.git
  ℹ️  init vs bootstrap: use bootstrap for any new clone/worktree of this tracker
  ✅ no stray scratch clones found under ~/code (or: ⚠️ found <N> at <paths>)
```

Non-destructive — this skill only reports; it never runs `push`/`pull`/`init`/`bootstrap`
itself. If something looks wrong, tell the user what you found and let them decide the fix.

Every other baton skill that touches beads is expected to already be doing the safe thing
described in Steps 2–4 (pinning `BEADS_DIR` explicitly, using `bootstrap` not `init` for
existing trackers). If one isn't, that's a bug in that skill — flag it.
