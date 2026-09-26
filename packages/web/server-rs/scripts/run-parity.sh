#!/usr/bin/env bash
# Boot both servers with identical seeded state for API parity diffing.
#   JS : port 3100 (node server/index.js)
#   RS : port 3101 (server-rs debug binary)
# Shared: OMPCHAMBER_DATA_DIR (fresh seed), UI password 123456, same cwd.
set -euo pipefail
cd "$(dirname "$0")/../.."   # packages/web

STAGE="${1:-all}"
DATA="$(mktemp -d /tmp/oc-parity.XXXXXX)"
mkdir -p "$DATA"
# Minimal seed so settings-backed endpoints have deterministic content.
cat > "$DATA/settings.json" <<'EOF'
{"version":1}
EOF

cleanup() {
  [ -n "${JS_PID:-}" ] && kill -TERM "$JS_PID" 2>/dev/null || true
  [ -n "${RS_PID:-}" ] && kill -TERM "$RS_PID" 2>/dev/null || true
  sleep 1
  rm -rf "$DATA"
}
trap cleanup EXIT

echo "[parity] data dir: $DATA"

# Pre-warm the omp-host engine (Bun transpile/natives caches) so both
# servers' engines pass their 10s startup health windows deterministically —
# without this, WHICH side misses the window is boot-race luck and the /api
# gate states diverge.
if [ "${PARITY_NO_WARM:-0}" = "1" ]; then echo "[parity] cold-boot mode (no engine pre-warm)"; else
WARM_PORT=$(( 20000 + RANDOM % 20000 ))
( bun "$(dirname "$0")/../../server/lib/omp-host/host.ts" serve --hostname 127.0.0.1 --port $WARM_PORT >"$DATA/warm.log" 2>&1 & echo $! > "$DATA/warm.pid" )
for _ in $(seq 1 60); do
  if grep -q "opencode server listening" "$DATA/warm.log" 2>/dev/null; then break; fi
  sleep 0.5
done
kill -TERM "$(cat "$DATA/warm.pid" 2>/dev/null)" 2>/dev/null || true
sleep 0.5
echo "[parity] engine pre-warmed"
fi

env -u OMPCHAMBER_DIST_DIR -u OMPCHAMBER_RUNTIME -u OMPCHAMBER_HOST \
  OMPCHAMBER_DATA_DIR="$DATA" \
  OMPCHAMBER_UI_PASSWORD=123456 \
  OMPCHAMBER_PORT=3100 \
  OMPCHAMBER_DIST_DIR=/Users/gemini/Documents/playground/ompchamber/packages/web/dist \
  RUST_LOG=warn \
  node server/index.js >"$DATA/js.log" 2>&1 &
JS_PID=$!

env -u OMPCHAMBER_DIST_DIR -u OMPCHAMBER_RUNTIME -u OMPCHAMBER_HOST \
  OMPCHAMBER_DATA_DIR="$DATA" \
  OMPCHAMBER_PORT=3101 \
  OMPCHAMBER_DIST_DIR=/Users/gemini/Documents/playground/ompchamber/packages/web/dist \
  RUST_LOG=warn \
  ./server-rs/target/debug/ompchamber-server --port 3101 --ui-password 123456 >"$DATA/rs.log" 2>&1 &
RS_PID=$!

wait_ready() {
  local port=$1 label=$2 log=$3
  for _ in $(seq 1 60); do
    if curl -sf "http://127.0.0.1:$port/health" >/dev/null 2>&1; then return 0; fi
    kill -0 "$4" 2>/dev/null || { echo "[parity] $label died"; tail -20 "$log"; return 1; }
    sleep 0.5
  done
  echo "[parity] $label not ready"; tail -20 "$log"; return 1
}

wait_ready 3100 js  "$DATA/js.log" "$JS_PID"
wait_ready 3101 rs  "$DATA/rs.log" "$RS_PID"
# Cold-boot engines need 15-25s to answer /global/health; the JS server's
# own 10s readiness gate misses that race and then HOLDS proxied requests.
# Warm both engines directly (auth = the env password both spawned with)
# before staging.
for port_label in "3100 js" "3101 rs"; do
  set -- $port_label
  WEB=$1; LABEL=$2
  for _ in $(seq 1 120); do
    EP=$(curl -sf "http://127.0.0.1:$WEB/health" 2>/dev/null | python3 -c 'import json,sys
try: print(json.load(sys.stdin).get("openCodePort") or 0)
except Exception: print(0)')
    if [ "$EP" != "0" ]; then
      if curl -sf -u "opencode:$OPENCODE_SERVER_PASSWORD" "http://127.0.0.1:$EP/global/health" 2>/dev/null | grep -q '"healthy":true'; then
        echo "[parity] $LABEL engine healthy (port $EP)"; break
      fi
    fi
    sleep 1
  done
done
echo "[parity] both up (js=$JS_PID rs=$RS_PID)"

python3 server-rs/scripts/api-diff.py --base-js "http://127.0.0.1:3100" --base-rs "http://127.0.0.1:3101" --stage "$STAGE" --verbose
