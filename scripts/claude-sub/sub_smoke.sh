#!/usr/bin/env bash
# Run one wrapped Claude Code session on the SUBSCRIPTION, by moving the
# settings.json that pins ANTHROPIC_AUTH_TOKEN/BASE_URL out of the way.
#
# The restore is a trap, not a trailing line: if the run times out, fails, or
# this script is killed, the file still comes back. Leaving a user's credential
# config renamed because a benchmark crashed is not an acceptable failure mode.
set -uo pipefail

SETTINGS="$HOME/.claude/settings.json"
PARKED="$HOME/.claude/settings.json.kompressor-smoke-parked"
RUN="$1"
# Resolved from this script's own location, not from a fixed directory. The
# previous value named one checkout of this repository by absolute path, so in
# any other clone it silently smoke-tested a different build -- or none.
# KOMPRESSOR_BIN overrides, as it does in scripts/harbor/suite.sh.
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BIN="${KOMPRESSOR_BIN:-$ROOT/target/release/kompressor}"
if [ ! -x "$BIN" ]; then
  echo "no kompressor binary at $BIN -- run 'make release', or set KOMPRESSOR_BIN" >&2
  exit 2
fi

restore() {
  if [ -f "$PARKED" ]; then
    mv -f "$PARKED" "$SETTINGS" && echo "RESTORED settings.json"
  else
    echo "nothing parked to restore"
  fi
}
trap restore EXIT INT TERM

if [ ! -f "$SETTINGS" ]; then
  echo "no settings.json at $SETTINGS -- nothing to park"; exit 1
fi
mv "$SETTINGS" "$PARKED" || { echo "could not park settings.json"; exit 1; }
echo "PARKED settings.json -> $(basename "$PARKED")"

cd "$RUN/workdir" || exit 1
env -u ANTHROPIC_AUTH_TOKEN -u ANTHROPIC_API_KEY -u ANTHROPIC_BASE_URL \
  timeout 600 "$BIN" wrap --config "$RUN/kompressor.toml" claude -- \
  -p "Read sample.log and src/, then add a function \`slowest(entries, n=5)\` to src/report.py returning the n entries with the highest latency parsed from the message. Add a test in tests/ and run python -m pytest. Show the final report.py." 2>&1 | tail -30
echo "--- agent finished with ${PIPESTATUS[0]:-?} ---"
