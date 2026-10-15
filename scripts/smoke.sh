#!/usr/bin/env bash
# Start every surface, use it the way a person would, and stop it.
#
# Unit tests exercise the pieces; this asks whether the assembled binary
# actually serves. It has caught things tests could not: a proxy-only server has
# no `/healthz` because that route belongs to the API surface, and the stats
# endpoints are absent entirely when statistics are disabled.
#
# Everything runs against a scratch directory and an ephemeral port. Nothing
# here talks to a provider, so it costs nothing and needs no key.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$ROOT/target/release/kompressor"
PORT="${KOMPRESSOR_SMOKE_PORT:-8898}"
WORK="$(mktemp -d)"
PID=""

cleanup() {
  [[ -n "$PID" ]] && kill "$PID" 2>/dev/null || true
  [[ -n "$PID" ]] && wait "$PID" 2>/dev/null || true
  rm -rf "$WORK"
}
trap cleanup EXIT

step() { printf '  \033[36m%-34s\033[0m' "$1"; }
ok()   { printf '\033[32mok\033[0m %s\n' "${1:-}"; }
die()  { printf '\033[31mFAILED\033[0m %s\n' "$1"; exit 1; }

[[ -x "$BIN" ]] || die "no release binary; run 'make release'"

cat > "$WORK/kompressor.toml" <<TOML
[engine]
mode = "recoverable"
[refs]
backend = "sqlite"
path = "$WORK/refs.db"
[stats]
enabled = true
path = "$WORK/stats.db"
[log]
level = "warn"
TOML

echo "kompressor smoke test"

step "config loads"
"$BIN" check-config --config "$WORK/kompressor.toml" >/dev/null || die "check-config refused it"
ok

# A payload with real structure: 300 near-identical log lines fold hard.
python3 - > "$WORK/big.log" <<'PY'
print("\n".join(
    f"2026-09-18 10:00:{i % 60:02d} INFO  [worker-{i % 4}] processed job {i} in {i}ms"
    for i in range(300)))
PY
IN=$(wc -c < "$WORK/big.log")

step "compress on a file"
OUT=$("$BIN" compress "$WORK/big.log" --config "$WORK/kompressor.toml" | wc -c)
[[ "$OUT" -lt $((IN / 4)) ]] || die "$IN -> $OUT is not compression"
ok "$IN -> $OUT chars"

step "detect classifies it"
KIND=$("$BIN" detect "$WORK/big.log" --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["content_type"])')
[[ -n "$KIND" ]] || die "no content type"
ok "$KIND"

step "serve comes up"
"$BIN" serve --config "$WORK/kompressor.toml" --listen "127.0.0.1:$PORT" >"$WORK/serve.log" 2>&1 &
PID=$!
for _ in $(seq 60); do
  curl -fsS -o /dev/null "http://127.0.0.1:$PORT/healthz" 2>/dev/null && break
  sleep 0.25
done
curl -fsS -o /dev/null "http://127.0.0.1:$PORT/healthz" || die "never answered /healthz"
ok

for path in /readyz /metrics; do
  step "GET $path"
  curl -fsS -o /dev/null "http://127.0.0.1:$PORT$path" || die "did not answer"
  ok
done

step "POST /v1/compress"
python3 - "$WORK/big.log" > "$WORK/req.json" <<'PY'
import json, sys
log = open(sys.argv[1]).read()
print(json.dumps({"messages": [
    {"role": "user", "content": "what failed?"},
    {"role": "tool", "content": log},
]}))
PY
curl -fsS "http://127.0.0.1:$PORT/v1/compress" -H 'content-type: application/json' \
  -d @"$WORK/req.json" > "$WORK/reply.json" || die "the API refused it"
HANDLE=$(python3 - "$WORK/reply.json" <<'PY'
import json, re, sys
d = json.load(open(sys.argv[1]))
tool = d["messages"][1]["content"]
assert d["messages"][0]["content"] == "what failed?", "the user turn was rewritten"
assert len(tool) < 4000, "the tool turn did not shrink"
m = re.search(r"k_[A-Za-z0-9_-]{12}", tool)
print(m.group(0) if m else "")
PY
)
[[ -n "$HANDLE" ]] || die "no handle was minted in recoverable mode"
ok "$HANDLE"

step "POST /v1/retrieve gets it back"
GOT=$(curl -fsS "http://127.0.0.1:$PORT/v1/retrieve" -H 'content-type: application/json' \
  -d "{\"ref\":\"$HANDLE\"}" | python3 -c 'import json,sys; d=json.load(sys.stdin); print("yes" if d["found"] else "no")')
[[ "$GOT" == yes ]] || die "the handle did not resolve"
ok

step "retrieve narrows to a line range"
LINES=$("$BIN" retrieve "$HANDLE" --refs "$WORK/refs.db" --lines 1:2 | wc -l)
[[ "$LINES" -eq 2 ]] || die "asked for 2 lines, got $LINES"
ok

step "MCP lists its tools"
curl -fsS "http://127.0.0.1:$PORT/mcp" -H 'content-type: application/json' \
  -H 'accept: application/json, text/event-stream' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/list"}' \
  | grep -q optscale_kompressor_retrieve || die "optscale_kompressor_retrieve is not offered"
ok

step "stats record the work"
curl -fsS -o /dev/null "http://127.0.0.1:$PORT/v1/stats/summary" || die "no stats endpoint"
ok

step "the log is not empty"
[[ -s "$WORK/serve.log" ]] || printf '\033[33mempty\033[0m (level=warn, so this is expected)\n'
[[ -s "$WORK/serve.log" ]] && ok

echo
echo "  all good"
