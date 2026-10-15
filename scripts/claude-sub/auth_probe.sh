#!/usr/bin/env bash
# Two-step probe: does Claude Code reach the subscription at all, and does it
# survive a custom base URL? Costs one trivial prompt per step.
#
# Restore is a trap, so settings.json comes back even on interrupt or failure.
set -uo pipefail

SETTINGS="$HOME/.claude/settings.json"
PARKED="$HOME/.claude/settings.json.auth-probe-parked"

restore() {
  [ -f "$PARKED" ] && mv -f "$PARKED" "$SETTINGS" && echo "RESTORED settings.json"
}
trap restore EXIT INT TERM

[ -f "$SETTINGS" ] || { echo "no settings.json to park"; exit 1; }
mv "$SETTINGS" "$PARKED" || { echo "could not park"; exit 1; }
echo "PARKED settings.json"
echo

echo "=== STEP 1: subscription, NO proxy, no base URL ==="
env -u ANTHROPIC_AUTH_TOKEN -u ANTHROPIC_API_KEY -u ANTHROPIC_BASE_URL \
  timeout 120 claude -p "reply with exactly: OK" 2>&1 | head -3
echo "    step 1 exit: ${PIPESTATUS[0]}"
echo

echo "=== STEP 2: same, but pointed at a plain pass-through on localhost ==="
echo "    (skipped unless step 1 succeeded -- no point testing the proxy"
echo "     if the subscription itself is not reachable)"
