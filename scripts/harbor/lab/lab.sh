#!/usr/bin/env bash
# One offline validation round: start a recording upstream + kompressor proxy,
# push one codex-shaped Responses turn through, and report what was forwarded.
#
#   ./lab.sh <api_family> [extra-toml-lines-file]
# api_family: "" (documented shape, field omitted) or openai/anthropic/passthrough
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
KB="${KOMPRESSOR_BIN:-$(cd "$HERE/../../.." && pwd)/target/release/kompressor}"
FAMILY="${1-}"
EXTRA="${2-}"
LAB="${LAB:-$HERE/lab}"
UPPORT=9801; KPORT=9800

# never `pkill -f`: the pattern matches the shell running it
for p in $UPPORT $KPORT; do
  pid=$(ss -ltnpH "sport = :$p" 2>/dev/null | grep -oP 'pid=\K[0-9]+' | head -1) || true
  [ -n "${pid:-}" ] && kill "$pid" 2>/dev/null && sleep 0.5 || true
done

rm -rf "$LAB"; mkdir -p "$LAB/up"
{
  cat <<TOML
[server]
listen = "127.0.0.1:$KPORT"
[engine]
mode = "recoverable"
[refs]
backend = "sqlite"
path = "$LAB/refs.db"
[stats]
enabled = true
path = "$LAB/stats.db"
[log]
level = "info"
format = "json"
[proxy]
min_tokens = 512
[[proxy.upstreams]]
name = "openai"
path_prefix = "/v1"
base_url = "http://127.0.0.1:$UPPORT"
forward_auth = true
TOML
  [ -n "$FAMILY" ] && echo "api_family = \"$FAMILY\""
  [ -n "$EXTRA" ] && cat "$EXTRA"
} > "$LAB/kompressor.toml"

python3 "$HERE/recording_upstream.py" --port $UPPORT --dump "$LAB/up" ${RETRIEVE:+--retrieve "$RETRIEVE"} >"$LAB/up.log" 2>&1 &
UPPID=$!
"$KB" serve --proxy --api --config "$LAB/kompressor.toml" >"$LAB/proxy.log" 2>&1 &
KPID=$!
trap 'kill $UPPID $KPID 2>/dev/null || true' EXIT

for _ in $(seq 1 60); do curl -sf -m 1 "http://127.0.0.1:$KPORT/healthz" >/dev/null 2>&1 && break; sleep 0.25; done

echo "== config: api_family=${FAMILY:-<omitted>} =="
curl -s --max-time 60 -D "$LAB/resp.headers" -o "$LAB/resp.sse" \
  -X POST "http://127.0.0.1:$KPORT/v1/responses" \
  -H 'content-type: application/json' \
  -H 'authorization: Bearer sk-test-NOT-A-REAL-KEY-000000000000' \
  -H 'accept: text/event-stream' \
  --data-binary "@$HERE/${PAYLOAD:-payload.json}"

echo "-- response headers --"; grep -i '^HTTP/\|^x-kompressor' "$LAB/resp.headers" || true
echo "-- client saw --"; grep -c '^event:' "$LAB/resp.sse" || true
python3 "$HERE/check_forward.py" "$HERE/${PAYLOAD:-payload.json}" "$LAB/up"
echo "-- stats/summary --"; curl -s "http://127.0.0.1:$KPORT/v1/stats/summary" || true; echo
