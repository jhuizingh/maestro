#!/bin/sh
# Run `fermata hook <event>` with the hook JSON on stdin. Exits 0 whatever happens, so a missing
# or broken binary never blocks Claude. `exec` keeps Claude as the binary's parent process.
for bin in "${FERMATA_BIN:-}" "$(command -v fermata 2>/dev/null)" "$HOME/.cargo/bin/fermata"; do
  [ -n "$bin" ] && [ -x "$bin" ] && exec "$bin" hook "$1"
done
cat >/dev/null
exit 0
