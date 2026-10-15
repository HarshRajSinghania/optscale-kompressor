#!/usr/bin/env bash
#
# What plan codex is on, and how much of its rate limits it has used.
#
# There is no codex command for this. `codex login status` says which auth mode
# is in use and `codex doctor` says whether auth is configured, but neither
# reports the plan or the limits — checked against 0.157.0 and 0.158.0-alpha.2.
#
# The server sends both with every turn, and codex records them in its session
# file, so this reads the last thing the server actually said. That makes it a
# cached reading rather than a query: the `as of` line is when it was received,
# and refreshing it means making a request.
#
# Why it exists here: "is this a rate limit?" is the first question asked when a
# sweep stage fails, and answering it wrongly costs an afternoon. During a 401
# storm on 2026-09-26 this field read `throttled no` with usage at 3%, which is
# what ruled quota out and moved the search to the credential.
#
#   ./codex-limits.sh            the current reading
#   ./codex-limits.sh --refresh  make one cheap request first, then read
#
set -euo pipefail

CODEX="${CODEX:-$HOME/.local/bin/codex}"

if [ "${1:-}" = "--refresh" ]; then
  # Cheapest turn that still makes the server answer. Its output is discarded;
  # the point is the rate_limits payload it causes to be written.
  echo "refreshing (one small request)..."
  "$CODEX" exec --skip-git-repo-check "reply with just: OK" >/dev/null 2>&1 || true
elif [ "${1:-}" != "" ]; then
  sed -n '3,20p' "$0"
  exit 0
fi

python3 - <<'PY'
import datetime
import glob
import json
import os

files = sorted(
    glob.glob(os.path.expanduser("~/.codex/sessions/**/*.jsonl"), recursive=True),
    key=os.path.getmtime,
    reverse=True,
)

# Newest reading wins. Two things make that less simple than it sounds.
#
# **A record can carry `"rate_limits": null`.** The key is present and the
# lookup succeeds, so no exception is raised and the value is `None` — which
# then fails on the first `.get`. Measured on this machine: 9 such records
# against 111 real ones, and whether a run crashed depended on which kind the
# newest file happened to hold first. The type is checked rather than the
# lookup guarded, because "present but null" is the case that got through.
#
# **`plan_type` may be null in every record.** It was on the account this was
# written against: 111 of 111. The search for one is therefore bounded to the
# file that supplied the reading — hunting further means reading every session
# on disk, every run, for a field the server is simply not sending.
latest = plan = source = None
for path in files:
    for line in open(path, errors="replace"):
        if '"rate_limits"' not in line:
            continue
        try:
            limits = json.loads(line)["payload"]["rate_limits"]
        except (ValueError, KeyError, TypeError):
            continue
        if not isinstance(limits, dict):
            continue
        if latest is None:
            latest, source = limits, path
        if plan is None:
            plan = limits.get("plan_type")
    # Stop at the end of the file that had the reading, with or without a plan.
    if latest:
        break

if not latest:
    print("no rate-limit record yet.")
    print("codex writes one only after the server answers a turn, so make one:")
    print("  ./codex-limits.sh --refresh")
    raise SystemExit(1)


def window(spec, label):
    """One limit window, as a percentage and a wall-clock reset."""
    if not spec:
        return
    minutes = spec.get("window_minutes")
    used = spec.get("used_percent")
    if used is None:
        used = "?"
    resets = spec.get("resets_at")
    when = (
        datetime.datetime.fromtimestamp(resets).strftime("%Y-%m-%d %H:%M")
        if resets
        else "?"
    )
    if not minutes:
        span = "?"
    elif minutes < 1440:
        span = f"{minutes / 60:g}h"
    else:
        span = f"{minutes / 1440:g}d"
    print(f"  {label:<10} {used:>5}% used of the {span} window   resets {when}")


credits = latest.get("credits") or {}
seen = datetime.datetime.fromtimestamp(os.path.getmtime(source))

# Said plainly. The server leaves this null on at least some accounts, so a
# bare "not reported" reads as a bug in this script rather than an absence in
# the data it is quoting.
print(f"  plan       {plan or 'not reported by codex (plan_type is null)'}")
window(latest.get("primary"), "primary")
window(latest.get("secondary"), "secondary")
print(
    f"  credits    has={credits.get('has_credits')} "
    f"unlimited={credits.get('unlimited')} balance={credits.get('balance')}"
)
# The field that actually answers "am I rate limited". Usage percentages can sit
# high without anything being refused; this is null until something is.
print(f"  throttled  {latest.get('rate_limit_reached_type') or 'no'}")
if latest.get("spend_control_reached"):
    print(f"  spend      {latest['spend_control_reached']}")
print(f"  as of      {seen:%Y-%m-%d %H:%M}  ({os.path.basename(source)[:30]})")
PY
