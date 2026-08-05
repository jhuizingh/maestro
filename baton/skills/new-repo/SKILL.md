---
description: Create a new repository for the active context — proposes a name using the context's new_repo_prefix, requires your signoff, creates it under the context's GitHub owner, and registers it as a member repo so context auto-detection picks it up.
argument-hint: "<short-name>"
allowed-tools: Bash(*), Read, Edit
---

## baton:new-repo

### Step 1 — Resolve context

```bash
RESOLVER="${CLAUDE_PLUGIN_ROOT:-$HOME/code/maestro/baton}/scripts/resolve-context.sh"
[ -x "$RESOLVER" ] || RESOLVER="$HOME/code/maestro/baton/scripts/resolve-context.sh"
CTX="$("$RESOLVER")" || { echo "$CTX"; exit 1; }
OWNER="$(echo "$CTX" | jq -r '.github.owner')"
PREFIX="$(echo "$CTX" | jq -r '.github.new_repo_prefix // ""')"
CODE_ROOT="$(echo "$CTX" | jq -r '.code_root // "~/code"' | sed "s|^~|$HOME|")"
WS="$(echo "$CTX" | jq -r '._workspace')"
```

### Step 2 — Propose the name + get signoff

Proposed repo name: `<PREFIX><short-name>` (from `$ARGUMENTS`). Show it and ask for explicit
confirmation (the context sets `github.signoff_required: true` by default — **always** confirm the
final name before creating anything). Let the user amend it.

### Step 3 — Create

After signoff:
```bash
NAME="<confirmed name>"
gh repo create "$OWNER/$NAME" --private --clone "$CODE_ROOT/$NAME"   # ask public vs private
cd "$CODE_ROOT/$NAME" && git commit --allow-empty -m "Initial commit" && git push -u origin HEAD
```
Ask public vs private (default private). If the user prefers a different visibility or an initial
README/license, honor that.

### Step 4 — Register as a member repo

Add the new repo to the context's `member_repos` in its `context.yaml` (so cwd auto-detection
finds it), unless a glob already covers it:

```bash
yq -i '.member_repos += ["'"$CODE_ROOT/$NAME"'"] | .member_repos |= unique' "$WS/context.yaml"
```

Report the repo URL and local path, and offer `baton:task-add` / `baton:start` to begin work in it.
