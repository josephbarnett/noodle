#!/usr/bin/env bash
# Self-test for demos/otlp_sink.py. Runs four checks against a real
# instance of the sink and reports per-check pass/fail.
#
# Usage:  bash demos/test_otlp_sink.sh
#
# Each check exits the script on failure with the offending output, so
# a green run is exactly four "✓" lines and a final "ALL CHECKS PASSED".

set -euo pipefail

PORT=44333
SANDBOX=/tmp/noodle-otlp-sink-selftest
SCRIPT="$(cd "$(dirname "$0")" && pwd)/otlp_sink.py"

rm -rf "$SANDBOX"
mkdir -p "$SANDBOX/bodies"
LOG="$SANDBOX/sink.log"

cleanup() {
    if [[ -n "${PID:-}" ]] && kill -0 "$PID" 2>/dev/null; then
        kill -9 "$PID" 2>/dev/null || true
    fi
}
trap cleanup EXIT

# ── Check 1: clean start + body capture ────────────────────────────
python3 "$SCRIPT" --port $PORT --dest "$SANDBOX/bodies" 2>"$LOG" &
PID=$!
sleep 1
if ! kill -0 $PID 2>/dev/null; then
    echo "C1 FAIL: sink didn't start"; cat "$LOG"; exit 1
fi

PAYLOAD='{"resourceLogs":[{"resource":{"attributes":[]},"scopeLogs":[{"logRecords":[{"timeUnixNano":"0","body":{"stringValue":"r1"}},{"timeUnixNano":"1","body":{"stringValue":"r2"}}]}]}]}'
HTTP=$(curl -sS -o /dev/null -w "%{http_code}" -X POST \
            -H 'Content-Type: application/json' \
            --data "$PAYLOAD" \
            "http://127.0.0.1:$PORT/v1/logs")
[[ "$HTTP" == "200" ]] || { echo "C1 FAIL: HTTP=$HTTP"; cat "$LOG"; exit 1; }
[[ -f "$SANDBOX/bodies/001.json" ]] || { echo "C1 FAIL: no 001.json"; ls "$SANDBOX/bodies/"; exit 1; }
grep -q "log_records=2 -> $SANDBOX/bodies/001.json" "$LOG" || {
    echo "C1 FAIL: log line missing / wrong"; cat "$LOG"; exit 1;
}
echo "✓ C1: clean start + valid POST captured + log line correct"

# ── Check 2: malformed JSON still captured, marked non-json ────────
HTTP=$(curl -sS -o /dev/null -w "%{http_code}" -X POST \
            -H 'Content-Type: application/json' \
            --data 'not valid json' \
            "http://127.0.0.1:$PORT/v1/logs")
[[ "$HTTP" == "200" ]] || { echo "C2 FAIL: HTTP=$HTTP"; cat "$LOG"; exit 1; }
[[ -f "$SANDBOX/bodies/002.json" ]] || { echo "C2 FAIL: no 002.json"; exit 1; }
grep -q "non-json" "$LOG" || { echo "C2 FAIL: non-json branch didn't log"; cat "$LOG"; exit 1; }
echo "✓ C2: malformed body still 200 + captured + non-json log"

# ── Check 3: SIGINT shuts the sink down cleanly within 5s ─────────
START=$(date +%s)
kill -INT $PID
for _ in $(seq 1 10); do
    kill -0 $PID 2>/dev/null || break
    sleep 0.5
done
ELAPSED=$(( $(date +%s) - START ))
if kill -0 $PID 2>/dev/null; then
    echo "C3 FAIL: still alive ${ELAPSED}s after SIGINT"; cat "$LOG"; exit 1
fi
wait $PID 2>/dev/null || true
grep -q "caught signal 2, shutting down" "$LOG" || {
    echo "C3 FAIL: shutdown message missing"; cat "$LOG"; exit 1;
}
echo "✓ C3: SIGINT → clean exit in ${ELAPSED}s + shutdown logged"

# ── Check 4: second instance fails cleanly with rc=2 + useful err ─
python3 "$SCRIPT" --port $PORT --dest "$SANDBOX/bodies" 2>"$LOG" &
PID=$!
sleep 1

# Now start a SECOND instance on the same port — must fail
LOG2="$SANDBOX/sink2.log"
set +e
python3 "$SCRIPT" --port $PORT --dest "$SANDBOX/bodies" 2>"$LOG2"
RC2=$?
set -e
[[ $RC2 -eq 2 ]] || { echo "C4 FAIL: expected rc=2, got rc=$RC2"; cat "$LOG2"; exit 1; }
grep -q "already in use" "$LOG2" || { echo "C4 FAIL: error message not friendly"; cat "$LOG2"; exit 1; }
echo "✓ C4: second instance exits rc=2 with helpful 'already in use' message"

# tidy first instance for the cleanup trap
kill -INT $PID 2>/dev/null || true
wait $PID 2>/dev/null || true

echo
echo "ALL CHECKS PASSED"
