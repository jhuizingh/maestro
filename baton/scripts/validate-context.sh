#!/usr/bin/env bash
# baton — validate a context.yaml against references/context.schema.json.
#
# Catches what silently degrades otherwise: every skill reads config as `jq -r '.x // default'`,
# so a typo'd key (`sesion_name`, `handoff.lancher`) doesn't error — it falls back to the default
# and you're left wondering why your setting had no effect. `additionalProperties: false` in the
# schema turns that whole class of bug into a message.
#
# Usage:
#   validate-context.sh [<path/to/context.yaml>]     # default: the active context's
#
# Exit 0 = valid, 1 = problems found (listed on stdout), 2 = usage/environment error.
#
# TWO BACKENDS, ONE SCHEMA:
#
#   check-jsonschema   preferred, authoritative, full JSON Schema. `brew install check-jsonschema`
#                      (also `pipx install check-jsonschema`). OPTIONAL — baton never requires it.
#   jq                 fallback. Implements the subset context.schema.json actually uses: type,
#                      enum, required, additionalProperties:false, properties, items, minLength,
#                      pattern.
#
# The fallback exists because baton:doctor runs on every session start; making a Python-backed
# tool mandatory would mean a red ❌ on every startup for a check that only matters when you edit
# context.yaml. So the check is dependency-free by default and gets authoritative when the real
# validator happens to be installed.
#
# The subset is not a guess. .github/workflows/validate.yml fails the build if the schema starts
# using a keyword the jq backend doesn't implement, and cross-checks the two backends against
# each other on a valid and a deliberately-broken fixture — so the fallback cannot silently drift
# from the schema it claims to enforce.
#
# Force one with --backend check-jsonschema|jq (default: auto).

set -euo pipefail

_die() { echo "validate-context: $*" >&2; exit 2; }

command -v jq >/dev/null 2>&1 || _die "jq is required"
command -v yq >/dev/null 2>&1 || _die "yq is required"

HERE="$(cd "$(dirname "$0")" && pwd)"
SCHEMA="$HERE/../references/context.schema.json"
[ -f "$SCHEMA" ] || _die "schema not found at $SCHEMA"

BACKEND=auto
TARGET=""
while [ $# -gt 0 ]; do
  case "$1" in
    --backend) BACKEND="${2:-auto}"; shift 2 ;;
    -h|--help) sed -n '2,30p' "$0"; exit 0 ;;
    -*) _die "unknown option '$1'" ;;
    *) TARGET="$1"; shift ;;
  esac
done

if [ -z "$TARGET" ]; then
  RESOLVER="$HERE/resolve-context.sh"
  [ -x "$RESOLVER" ] || _die "no path given and resolve-context.sh not found"
  WS="$("$RESOLVER" 2>/dev/null | jq -r '._workspace // empty' || true)"
  [ -n "$WS" ] || _die "no path given and no context resolved — pass a context.yaml explicitly"
  TARGET="$WS/context.yaml"
fi
[ -f "$TARGET" ] || _die "no such file: $TARGET"

# --- the checker ---------------------------------------------------------------------------
# Walks schema and instance together, emitting one message per problem. Recursion follows
# `properties` and `items` only — the schema describes exactly where it expects to go, which is
# what makes unknown-key detection meaningful rather than recursive guesswork.
JQ_CHECK='
def type_ok($t; $v):
  if   $t == "object"  then ($v|type) == "object"
  elif $t == "array"   then ($v|type) == "array"
  elif $t == "string"  then ($v|type) == "string"
  elif $t == "boolean" then ($v|type) == "boolean"
  elif $t == "number"  then ($v|type) == "number"
  else true end;
def at($path; $k): if $path == "" then $k else "\($path).\($k)" end;
def shown($path): if $path == "" then "(root)" else $path end;

def check($schema; $inst; $path):
  (if ($schema.type != null) and (type_ok($schema.type; $inst) | not)
   then ["\(shown($path)): expected \($schema.type), got \($inst|type)"] else [] end)
  +
  (if ($schema.enum != null) and (($schema.enum | index($inst)) == null)
   then ["\(shown($path)): \($inst|tojson) is not one of \($schema.enum|tojson)"] else [] end)
  +
  (if ($schema.minLength != null) and (($inst|type) == "string")
      and (($inst|length) < $schema.minLength)
   then ["\(shown($path)): must not be empty"] else [] end)
  +
  (if ($schema.pattern != null) and (($inst|type) == "string")
      and (($inst|test($schema.pattern)) | not)
   then ["\(shown($path)): \($inst|tojson) does not match /\($schema.pattern)/"] else [] end)
  +
  (if ($schema.type == "object") and (($inst|type) == "object")
   then
     [ (($schema.required // [])[]) as $k | select(($inst|has($k)) | not)
       | "\(shown($path)): missing required key \($k|tojson)" ]
     +
     (if $schema.additionalProperties == false
      then [ ($inst|keys_unsorted[]) as $k
             | select((($schema.properties // {}) | has($k)) | not)
             | "\(at($path; $k)): unknown key — not in the schema (typo?)" ]
      else [] end)
     +
     [ ($inst|keys_unsorted[]) as $k
       | select(($schema.properties // {}) | has($k))
       | check($schema.properties[$k]; $inst[$k]; at($path; $k))[] ]
   else [] end)
  +
  (if ($schema.type == "array") and (($inst|type) == "array") and ($schema.items != null)
   then [ range(0; $inst|length) as $i
          | check($schema.items; $inst[$i]; "\($path)[\($i)]")[] ]
   else [] end);

check($schema; $inst; "") | .[]
'

INSTANCE_JSON="$(yq -o=json '.' "$TARGET" 2>/dev/null)" || {
  echo "❌ $TARGET — not valid YAML"; exit 1;
}

# --- pick a backend --------------------------------------------------------------------------
case "$BACKEND" in
  auto) command -v check-jsonschema >/dev/null 2>&1 && BACKEND=check-jsonschema || BACKEND=jq ;;
  check-jsonschema) command -v check-jsonschema >/dev/null 2>&1 \
      || _die "--backend check-jsonschema, but it isn't installed (brew install check-jsonschema)" ;;
  jq) ;;
  *) _die "unknown --backend '$BACKEND' (want auto, check-jsonschema, or jq)" ;;
esac

if [ "$BACKEND" = check-jsonschema ]; then
  TMP="$(mktemp -t baton-ctx.XXXXXX.json)"
  trap 'rm -f "$TMP"' EXIT
  printf '%s' "$INSTANCE_JSON" >"$TMP"
  if OUT="$(check-jsonschema --schemafile "$SCHEMA" "$TMP" 2>&1)"; then
    echo "✅ $(basename "$TARGET") — valid against context.schema.json (check-jsonschema)"
    exit 0
  fi
  echo "❌ $TARGET — invalid (check-jsonschema):"
  printf '%s\n' "$OUT" | sed "s|$TMP|$TARGET|g; s/^/  /"
  exit 1
fi

FINDINGS="$(jq -r -n \
  --slurpfile schema "$SCHEMA" \
  --argjson inst "$INSTANCE_JSON" \
  '$schema[0] as $schema | '"$JQ_CHECK")" || _die "checker failed on $TARGET"

if [ -z "$FINDINGS" ]; then
  echo "✅ $(basename "$TARGET") — valid against context.schema.json (jq subset checker)"
  exit 0
fi

echo "❌ $TARGET — $(printf '%s\n' "$FINDINGS" | grep -c .) problem(s):"
printf '%s\n' "$FINDINGS" | sed 's/^/  • /'
echo
echo "  (checked with the jq fallback; \`brew install check-jsonschema\` for full JSON Schema)"
exit 1
