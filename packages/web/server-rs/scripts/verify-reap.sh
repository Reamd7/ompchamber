#!/usr/bin/env bash
# Verify the Rust server reaps the managed engine child on SIGTERM.
set -u
cd "$(dirname "$0")/.."
PORT="${1:-3992}"
DATA="$(mktemp -d /tmp/ocsmoke-reap.XXXXXX)"

env -u OMPCHAMBER_UI_PASSWORD -u OPENCODE_UI_PASSWORD \
  OMPCHAMBER_DATA_DIR="$DATA" RUST_LOG=info \
  ./target/debug/ompchamber-server --port "$PORT" >"$DATA/log.txt" 2>&1 &
SERVER_PID=$!
echo "server_pid=$SERVER_PID"

for _ in $(seq 1 40); do
  curl -sf "http://127.0.0.1:$PORT/health" >/dev/null 2>&1 && break
  sleep 0.5
done

ENGINE_PORT=$(curl -s "http://127.0.0.1:$PORT/health" | python3 -c 'import json,sys; print(json.load(sys.stdin)["engine"]["lastLaunch"]["port"])')
ENGINE_PID=$(pgrep -f "omp-host/host.ts serve --hostname 127.0.0.1 --port $ENGINE_PORT" | head -1)
echo "engine_port=$ENGINE_PORT engine_pid=$ENGINE_PID"
if [ -z "$ENGINE_PID" ]; then
  echo "FAIL: engine child not found"
  kill -TERM "$SERVER_PID" 2>/dev/null
  cat "$DATA/log.txt" 2>/dev/null | tail -12; rm -rf "$DATA"
  exit 1
fi

kill -TERM "$SERVER_PID"
for _ in $(seq 1 10); do
  kill -0 "$ENGINE_PID" 2>/dev/null || break
  sleep 1
done

if kill -0 "$ENGINE_PID" 2>/dev/null; then
  echo "ENGINE_LEAKED"
  kill -9 "$ENGINE_PID" 2>/dev/null
  RESULT=1
else
  echo "ENGINE_REAPED"
  RESULT=0
fi
wait "$SERVER_PID" 2>/dev/null
echo "server_exit=$?"
cat "$DATA/log.txt" 2>/dev/null | tail -12; rm -rf "$DATA"
exit $RESULT
