# Locating baton — the entry hop

Every baton script finds its siblings with `$(dirname "$0")`, so once a skill has reached *one*
script, nothing else needs locating. The hard part is that first hop: a skill's Bash block is a
fresh shell with no idea where the plugin lives. This is the one way every skill makes that hop.

## The cascade

```bash
# Locate baton: harness env → dev clone → install record → newest cached (references/locating-baton.md)
BATON="${CLAUDE_PLUGIN_ROOT:-}"; [ -n "$BATON" ] && [ -d "$BATON/scripts" ] || BATON="$HOME/code/maestro/baton"
_BP="${CLAUDE_CONFIG_DIR:-$HOME/.claude}/plugins"
[ -d "$BATON/scripts" ] || BATON="$(jq -r '.plugins["baton@maestro"] // [] | ((.[] | select(.scope=="user")) // .[0]) | .installPath // empty' "$_BP/installed_plugins.json" 2>/dev/null)"
[ -d "$BATON/scripts" ] || BATON="$(ls -d "$_BP"/cache/maestro/baton/* 2>/dev/null | sort -V | tail -1)"
[ -d "$BATON/scripts" ] || { echo "baton: cannot locate plugin scripts (set CLAUDE_PLUGIN_ROOT or install the plugin)" >&2; exit 1; }
RESOLVER="$BATON/scripts/resolve-context.sh"    # …and every other script path, from $BATON
```

Copy it **verbatim**: the first six lines must be identical everywhere they appear, and
`scripts/test-entry-hop.sh` fails on any copy that drifts. Every Bash block that needs a baton
script carries its own copy, because shell state does not survive from one block to the next.

The rungs, in order:

1. **`$CLAUDE_PLUGIN_ROOT`.** The harness sets it for a skill, but it does not reliably reach the
   Bash tool's shell, so it can't be the only rung.
2. **The dev clone** (`~/code/maestro/baton`). This keeps contributors' setups working, and when
   one exists it takes precedence over the installed copy.
3. **The install record.** `installed_plugins.json` names the `installPath` of the copy that
   `claude plugins update` last installed. Its `user` scope is preferred, the same choice
   `plugin-freshness.sh` makes.
4. **The newest cached version.** This is a fallback for when there is no install record (or no
   `jq`). It is a last resort for a reason: the cache keeps every version ever downloaded side by
   side, so "newest directory" is not necessarily "installed". `plugin-freshness.sh` documents a
   false pass that this exact shortcut produced. `sort -V` at least orders `0.10.x` above `0.9.x`,
   which a plain glob does not.
5. **Fail loudly.** A non-zero exit with a message that names the fix. Never carry on with a path
   that doesn't exist: every later failure would then blame the wrong thing.

The cache root is always `${CLAUDE_CONFIG_DIR:-$HOME/.claude}`, never a hardcoded `~/.claude`.

## Where this does *not* apply

- **`scripts/*.sh`** resolve their siblings from `$(dirname "$0")`. They are already inside the
  plugin, so they need no cascade.
- **`hooks/*.sh`** are inside the plugin too, so they take the root from their own location
  (`$(cd -P "$(dirname "$0")/.." && pwd)`). That is exact: it is the running copy. They also
  fail *open* (`exit 0`) rather than loudly, since a hook that errors disrupts unrelated work.
- **`shell/baton.zsh`** runs outside any session. It has its own version-aware cache lookup
  (`_baton_newest_cached`).
