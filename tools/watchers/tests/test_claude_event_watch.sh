#!/bin/bash
# Smoke test for tools/watchers/claude-event-watch.
#
# Exercises the fast-path drain plus the adaptive debounce/coalesce loop:
# pre-load events into the queue dir, run the watcher in fast-path mode
# (events already pending), and verify (a) the one-liner stdout shape,
# (b) that the queue file is deleted, (c) that the consumed-log JSONL line
# is appended, (d) that --debounce 0 surfaces immediately, (e) that a
# staggered burst coalesces into ONE batch, (f) that a single lone event
# surfaces after one quiet interval (not the full cap), and (g) that an
# event landing after the drain is NOT lost — it persists for the next run.
#
# We intentionally do NOT test the inotify-blocking path's initial block
# here — that would require a tmux/timeout dance and is best left to the
# live integration. The fast path + debounce loop is the bulk of the
# script's logic and where the batching correctness lives.

set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
WATCHER="$REPO/tools/watchers/claude-event-watch"

if [[ ! -x "$WATCHER" ]]; then
    echo "FAIL: $WATCHER missing or not executable" >&2
    exit 1
fi

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

QUEUE="$TMP/queue"
LOG_DIR="$TMP/log"
mkdir -p "$QUEUE" "$LOG_DIR"

# Pre-load one event into the queue
python3 - "$QUEUE" <<'PYEOF'
import json, sys, time
queue = sys.argv[1]
ev = {
    "timestamp": time.time(),
    "source": "manual",
    "tag": "smoke",
    "message": "hello from test",
    "data": {},
}
with open(f"{queue}/100_smoke.json", "w") as f:
    json.dump(ev, f)
PYEOF

# Run the watcher — fast path should drain immediately (no inotify wait).
# --debounce 0 disables the collect loop so this exits instantly.
out=$(CLAUDE_EVENT_QUEUE="$QUEUE" CLAUDE_EVENT_LOG_DIR="$LOG_DIR" "$WATCHER" --debounce 0 2>&1)

# Verify stdout has the one-liner shape
if ! grep -q '^EVENT\[manual/smoke\] hello from test' <<<"$out"; then
    echo "FAIL: stdout missing one-liner" >&2
    echo "$out"
    exit 1
fi

# Verify the restart banner is present
if ! grep -q 'WATCHER EXITED' <<<"$out"; then
    echo "FAIL: restart banner missing" >&2
    echo "$out"
    exit 1
fi

# Verify the queue file was deleted
if [[ -f "$QUEUE/100_smoke.json" ]]; then
    echo "FAIL: queue file not deleted" >&2
    exit 1
fi

# Verify the consumed-log line was appended
if [[ ! -s "$LOG_DIR/consumed.jsonl" ]]; then
    echo "FAIL: consumed.jsonl not written" >&2
    exit 1
fi
if ! python3 -c "
import json, sys
line = open('$LOG_DIR/consumed.jsonl').read().strip().splitlines()[0]
ev = json.loads(line)
assert ev['tag'] == 'smoke', ev
assert ev['source'] == 'manual', ev
assert ev['message'] == 'hello from test', ev
print('  log entry OK')
"; then
    echo "FAIL: consumed.jsonl content mismatch" >&2
    exit 1
fi

# Test malformed event: should print a placeholder one-liner, NOT crash
echo "not valid json" >"$QUEUE/200_bad.json"
out=$(CLAUDE_EVENT_QUEUE="$QUEUE" CLAUDE_EVENT_LOG_DIR="$LOG_DIR" "$WATCHER" --debounce 0 2>&1)
if ! grep -q 'EVENT\[malformed/unknown\]' <<<"$out"; then
    echo "FAIL: malformed event not handled gracefully" >&2
    echo "$out"
    exit 1
fi

# Test debounce flag validation (non-numeric input should fail with rc=2)
set +e
CLAUDE_EVENT_QUEUE="$QUEUE" "$WATCHER" --debounce abc >/dev/null 2>&1
rc=$?
set -e
if (( rc != 2 )); then
    echo "FAIL: --debounce abc returned rc=$rc, expected 2" >&2
    exit 1
fi

# Test quiet flag validation (0 and non-numeric should fail with rc=2)
for bad in abc 0 -1; do
    set +e
    CLAUDE_EVENT_QUEUE="$QUEUE" "$WATCHER" --quiet "$bad" >/dev/null 2>&1
    rc=$?
    set -e
    if (( rc != 2 )); then
        echo "FAIL: --quiet $bad returned rc=$rc, expected 2" >&2
        exit 1
    fi
done

# Test --help works (and mentions both knobs)
help_out=$("$WATCHER" --help)
grep -q -- '--debounce' <<<"$help_out" || { echo "FAIL: --help missing --debounce" >&2; exit 1; }
grep -q -- '--quiet' <<<"$help_out" || { echo "FAIL: --help missing --quiet" >&2; exit 1; }

# --- Adaptive debounce / coalesce tests ----------------------------------

# Helper: write a single event file.
write_event() {  # <queue> <fname> <message>
    python3 - "$1" "$2" "$3" <<'PYEOF'
import json, sys, time
q, fname, msg = sys.argv[1], sys.argv[2], sys.argv[3]
ev = {"timestamp": time.time(), "source": "manual", "tag": "batch",
      "message": msg, "data": {}}
with open(f"{q}/{fname}", "w") as f:
    json.dump(ev, f)
PYEOF
}

# (e) Staggered burst coalesces into ONE batch: preload one event, drip two
# more (each within the quiet window), expect all three in a single surface.
BQ="$TMP/bq"; BLOG="$TMP/blog"; mkdir -p "$BQ" "$BLOG"
write_event "$BQ" "100_a.json" "burst A"
(
    sleep 1; write_event "$BQ" "110_b.json" "burst B"
    sleep 1; write_event "$BQ" "120_c.json" "burst C"
) &
DRIP=$!
batch_out=$(CLAUDE_EVENT_QUEUE="$BQ" CLAUDE_EVENT_LOG_DIR="$BLOG" "$WATCHER" --debounce 20 --quiet 2 2>&1)
wait "$DRIP" 2>/dev/null || true
n=$(grep -c '^EVENT' <<<"$batch_out")
if (( n != 3 )); then
    echo "FAIL: staggered burst surfaced $n events, expected 3 (no coalesce)" >&2
    echo "$batch_out" >&2
    exit 1
fi
if [[ -n "$(ls "$BQ" 2>/dev/null)" ]]; then
    echo "FAIL: queue not drained after batch surface" >&2
    exit 1
fi
echo "  staggered burst coalesced 3 events into one surface OK"

# (f) Single lone event surfaces after ~one quiet interval, NOT the full cap.
SQ="$TMP/sq"; SLOG="$TMP/slog"; mkdir -p "$SQ" "$SLOG"
write_event "$SQ" "100_only.json" "lonely"
start=$(date +%s)
single_out=$(CLAUDE_EVENT_QUEUE="$SQ" CLAUDE_EVENT_LOG_DIR="$SLOG" "$WATCHER" --debounce 30 --quiet 1 2>&1)
elapsed=$(( $(date +%s) - start ))
if ! grep -q '^EVENT\[manual/batch\] lonely' <<<"$single_out"; then
    echo "FAIL: lone event not surfaced" >&2; echo "$single_out" >&2; exit 1
fi
if (( elapsed > 10 )); then
    echo "FAIL: lone event took ${elapsed}s (waited the full cap, not the quiet interval)" >&2
    exit 1
fi
echo "  lone event surfaced in ${elapsed}s (quiet interval, not cap) OK"

# (g) No-loss: an event landing AFTER the drain is not lost — it persists on
# disk and surfaces on the next run.
NQ="$TMP/nq"; NLOG="$TMP/nlog"; mkdir -p "$NQ" "$NLOG"
write_event "$NQ" "100_first.json" "first"
( sleep 5; write_event "$NQ" "200_late.json" "late" ) &
DRIP2=$!
run1=$(CLAUDE_EVENT_QUEUE="$NQ" CLAUDE_EVENT_LOG_DIR="$NLOG" "$WATCHER" --debounce 10 --quiet 1 2>&1)
if ! grep -q 'first' <<<"$run1" || grep -q 'late' <<<"$run1"; then
    echo "FAIL: run1 should surface only 'first'" >&2; echo "$run1" >&2; exit 1
fi
wait "$DRIP2" 2>/dev/null || true
# 'late' must now be on disk.
if [[ -z "$(ls "$NQ" 2>/dev/null)" ]]; then
    echo "FAIL: late event was lost (queue empty after run1, before run2)" >&2
    exit 1
fi
run2=$(CLAUDE_EVENT_QUEUE="$NQ" CLAUDE_EVENT_LOG_DIR="$NLOG" "$WATCHER" --debounce 2 --quiet 1 2>&1)
if ! grep -q 'late' <<<"$run2"; then
    echo "FAIL: late event not surfaced on run2" >&2; echo "$run2" >&2; exit 1
fi
echo "  no-loss: late event persisted and surfaced on next run OK"

echo "PASS: all claude-event-watch checks (fast-path + adaptive debounce)"
