#!/usr/bin/env bash
# Smoke test for the Rust OpenChamber server.
#
# Boots the server on a scratch port with a scratch data dir, waits for the
# managed omp-host engine readiness, then exercises:
#   /health                       -> 200 with engine ready
#   proxied wire route (GET /config or /agent) -> 200 from the engine
#   SSE /event                    -> at least one data frame
#   /api/magic-prompts            -> 200 default state
#   /api/session-folders          -> 200 default state
#   /api/permission-auto-accept   -> 200 policy snapshot
#
# Usage: bash scripts/smoke.sh [port]
set -euo pipefail

PORT="${1:-3987}"
DATA_DIR="$(mktemp -d /tmp/ompchamber-rs-smoke.XXXXXX)"
BIN="target/debug/ompchamber-server"
cd "$(dirname "$0")/.."

if [[ ! -x "$BIN" ]]; then
  echo "building $BIN…" >&2
  cargo build --quiet
fi

cleanup() {
  if [[ -n "${SERVER_PID:-}" ]]; then
    kill -TERM "$SERVER_PID" 2>/dev/null || true
    wait "$SERVER_PID" 2>/dev/null || true
  fi
  rm -rf "$DATA_DIR"
}
trap cleanup EXIT

echo "[smoke] starting server on port $PORT (data: $DATA_DIR)"
OMPCHAMBER_DATA_DIR="$DATA_DIR" RUST_LOG=info "$BIN" --port "$PORT" >"$DATA_DIR/server.log" 2>&1 &
SERVER_PID=$!

fail() {
  echo "[smoke] FAIL: $1" >&2
  echo "--- server log tail ---" >&2
  tail -40 "$DATA_DIR/server.log" >&2 || true
  exit 1
}

# Wait for server up.
for _ in $(seq 1 120); do
  if curl -sf "http://127.0.0.1:$PORT/health" >/dev/null 2>&1; then break; fi
  if ! kill -0 "$SERVER_PID" 2>/dev/null; then fail "server process died during boot"; fi
  sleep 0.5
done
kill -0 "$SERVER_PID" 2>/dev/null || fail "server exited"

# 1. /health reports engine ready.
HEALTH="$(curl -sf "http://127.0.0.1:$PORT/health")"
echo "$HEALTH" | grep -q '"ready":true' || fail "/health engine not ready: $HEALTH"
echo "[smoke] /health ok"

# 2. Proxied wire route reaches the engine (proxy module owns /config,/agent…).
PROXY_STATUS="$(curl -s -o "$DATA_DIR/proxy.json" -w '%{http_code}' "http://127.0.0.1:$PORT/config")"
if [[ "$PROXY_STATUS" != "200" ]]; then
  PROXY_STATUS="$(curl -s -o "$DATA_DIR/proxy.json" -w '%{http_code}' "http://127.0.0.1:$PORT/agent")"
fi
[[ "$PROXY_STATUS" == "200" ]] || fail "proxied wire route status $PROXY_STATUS (proxy port may still be partial — check PORT-MANIFEST)"
echo "[smoke] proxied wire route ok"

# 3. SSE /event yields at least one frame within 20s.
SSE_OK=0
for _ in $(seq 1 40); do
  if timeout 2 curl -sN "http://127.0.0.1:$PORT/event" 2>/dev/null | head -c 4096 | grep -q 'data:'; then
    SSE_OK=1; break
  fi
done
[[ "$SSE_OK" == "1" ]] || fail "no SSE data frame on /event"
echo "[smoke] SSE /event ok"

# 4-6. Ported route modules answer.
for route in /api/magic-prompts /api/session-folders /api/permission-auto-accept; do
  STATUS="$(curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:$PORT$route")"
  [[ "$STATUS" == "200" ]] || fail "$route status $STATUS"
  echo "[smoke] $route ok"
done

# 7. Graceful shutdown leaves no engine child behind.
ENGINE_PIDS="$(pgrep -f "omp-host/host.ts serve" || true)"
kill -TERM "$SERVER_PID"
wait "$SERVER_PID" 2>/dev/null || true
SERVER_PID=""
sleep 2
for pid in $ENGINE_PIDS; do
  if kill -0 "$pid" 2>/dev/null; then
    fail "engine child $pid survived server shutdown"
  fi
done
echo "[smoke] engine child reaped on shutdown"
echo "[smoke] ALL OK"
