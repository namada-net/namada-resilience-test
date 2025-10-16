#!/usr/bin/env bash
set -euo pipefail

COMPOSE_FILE="config/docker-compose.yml"
WORKLOAD="workload"
WATCHDOG="watchdog"
TEST_TIME_SEC="${TEST_TIME_SEC:-60}"
TIMEOUT="$(( TEST_TIME_SEC + 600 ))"
TS=$(date +%s)
LOG="logs/test-${TS}.log"
mkdir -p logs

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
tmpd="$(mktemp -d)"

cleanup() {
  docker compose -f "$COMPOSE_FILE" logs --no-color -t > "$LOG" 2>/dev/null || true
  docker compose -f "$COMPOSE_FILE" stop >/dev/null 2>&1 || true
  rm -rf "$tmpd" || true
}
trap cleanup EXIT

"${SCRIPT_DIR}/clean.sh"

# 1. Start the test
docker compose -f "$COMPOSE_FILE" up -d --force-recreate

# 2. container IDs
WLCID="$(docker compose -f "$COMPOSE_FILE" ps -q "$WORKLOAD")"
WDCID="$(docker compose -f "$COMPOSE_FILE" ps -q "$WATCHDOG")"
[ -n "$WLCID" ] && [ -n "$WDCID" ] || { echo "[ERR] service not found"; exit 2; }

# 3. wait for workload or watchdog
( docker wait "$WDCID" > "$tmpd/wd.code" ) & wd_pid=$!
( docker wait "$WLCID" > "$tmpd/wl.code" ) & wl_pid=$!
( sleep "$TIMEOUT"; echo timeout > "$tmpd/timeout" ) & to_pid=$!

# 4. which one finished first
winner=""
while :; do
  if [ -f "$tmpd/timeout" ]; then winner="timeout"; break; fi
  if ! kill -0 "$wd_pid" 2>/dev/null; then winner="watchdog"; break; fi
  if ! kill -0 "$wl_pid" 2>/dev/null; then winner="workload"; break; fi
  sleep 1
done

# 5. stop the timer
kill "$to_pid" 2>/dev/null || true

# 6. Check the result
case "$winner" in
  timeout)
    echo "[ERR] timeout after ${TIMEOUT}s"
    exit 124
    ;;

  watchdog)
    wd_code="$(tr -d '\r\n' < "$tmpd/wd.code" || echo 1)"
    echo "[FAIL] watchdog exited early with code $wd_code"
    exit "${wd_code:-1}"
    ;;

  workload)
    wl_code="$(tr -d '\r\n' < "$tmpd/wl.code" || echo 1)"
    echo "[INFO] workload finished with code $wl_code; stopping stack..."
    # check if watchdog was down
    if [ -s "$tmpd/wd.code" ]; then
      wd_code="$(tr -d '\r\n' < "$tmpd/wd.code" || echo 1)"
      if [ "${wd_code:-0}" != "0" ]; then
        echo "[FAIL] watchdog had failed earlier with code $wd_code"
        exit "$wd_code"
      fi
    fi

    # Output the summary and exit
    summary="$(docker logs "$WLCID" 2>/dev/null | sed -n '/Summary:/,$p' || true)"
    [ -n "$summary" ] && echo "$summary"

    exit "${wl_code:-1}"
    ;;
esac