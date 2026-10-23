#!/usr/bin/env bash
# End-to-end check against a running signaling server: start a host, connect
# a client with the right password (latency + throughput), and confirm a
# wrong password is refused.
#
#   scripts/e2e.sh <path-to-reemoth-binary> <signal-url>
set -euo pipefail

BIN=$1
export REEMOTH_SIGNAL_URL=$2
WORK=$(mktemp -d)
trap 'kill $HOST_PID 2>/dev/null || true; rm -rf "$WORK"' EXIT

REEMOTH_CONFIG_DIR="$WORK/host" REEMOTH_PASSWORD="e2e-password-123" \
  "$BIN" host > "$WORK/host.log" 2>&1 &
HOST_PID=$!

for _ in $(seq 1 30); do
  grep -q "reemoth connect" "$WORK/host.log" && break
  kill -0 $HOST_PID 2>/dev/null || { cat "$WORK/host.log"; exit 1; }
  sleep 1
done
ID=$(grep -o "reemoth connect [0-9]*" "$WORK/host.log" | awk '{print $3}')
[ -n "$ID" ] || { echo "host never came online"; cat "$WORK/host.log"; exit 1; }
echo "host online as $ID"

echo "== wrong password must be refused"
if REEMOTH_CONFIG_DIR="$WORK/client" timeout 60 "$BIN" connect "$ID" --password "not-the-password" \
     --no-chat > "$WORK/bad.log" 2>&1; then
  echo "FAIL: wrong password was accepted"; cat "$WORK/bad.log"; exit 1
fi
grep -q "wrong password" "$WORK/bad.log" || { echo "FAIL: unexpected error"; cat "$WORK/bad.log"; exit 1; }
echo "ok: $(tail -1 "$WORK/bad.log")"

echo "== correct password connects"
REEMOTH_CONFIG_DIR="$WORK/client" timeout 120 "$BIN" connect "$ID" --password "e2e-password-123" \
  --no-chat --pings 20 --bulk-mib 8 | tee "$WORK/good.log"
grep -q "Connected in" "$WORK/good.log"
grep -q "Mbit/s" "$WORK/good.log"

echo "== host log"
cat "$WORK/host.log"
echo "E2E PASSED"
