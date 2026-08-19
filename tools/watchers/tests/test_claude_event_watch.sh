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
#
# The delivery-mode section at the bottom additionally spawns a LIVE monitor-
# mode watcher (which by definition does not exit on its own) and drives it
# through a real batch, a second batch, a silence-breaker line, and finally the
# runtime toggle that winds it back down. Every such instance is registered in
# BG_PIDS and reaped by the EXIT trap, so nothing outlives the suite.

set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
WATCHER="$REPO/tools/watchers/claude-event-watch"

if [[ ! -x "$WATCHER" ]]; then
    echo "FAIL: $WATCHER missing or not executable" >&2
    exit 1
fi

TMP="$(mktemp -d)"
# Track background watcher pids so the EXIT trap can reap them — a stray
# watcher left blocking on inotifywait must never outlive the test (that was
# the CI-hang failure mode this suite is hardened against).
BG_PIDS=()
cleanup() {
    local p
    for p in "${BG_PIDS[@]:-}"; do
        [[ -n "$p" ]] || continue
        kill "$p" 2>/dev/null || true
    done
    rm -rf "$TMP"
}
trap cleanup EXIT

# Portable bounded wait: reap $1 within $2 seconds; if it's still alive at the
# deadline, kill it and return non-zero. Avoids an unbounded `wait` that hangs
# CI forever when a backgrounded watcher never self-exits (no GNU `timeout`
# dependency — works on macOS and Linux alike). Polls `kill -0`.
reap_within() {  # <pid> <max_seconds>
    local pid="$1" max="$2" waited=0
    while kill -0 "$pid" 2>/dev/null; do
        if (( waited >= max )); then
            kill "$pid" 2>/dev/null || true
            wait "$pid" 2>/dev/null || true
            return 1
        fi
        sleep 1
        waited=$(( waited + 1 ))
    done
    wait "$pid" 2>/dev/null || true
    return 0
}

# Isolate the singleton lock for the WHOLE suite. Without this, every
# invocation below that does not name its own lockfile competes for the
# watcher's real default path — so a genuine watcher running on the host (or
# the CI image lacking write access to the default lock directory) decides
# whether the suite passes. Individual tests still override this per-invocation
# where they need two instances to contend on a specific file.
export CLAUDE_EVENT_WATCH_LOCK="$TMP/default.lock"

QUEUE="$TMP/queue"
LOG_DIR="$TMP/log"
mkdir -p "$QUEUE" "$LOG_DIR"

# Isolate the event-must-act state dir into the tempdir so auto-ingest (if a
# real event-ack happens to be on PATH) never mutates the user's real
# ~/.config/claude-events during the test run.
export CLAUDE_EVENT_STATE_DIR="$TMP/event-state"
mkdir -p "$CLAUDE_EVENT_STATE_DIR"

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

# --- Auto-ingest into the event-must-act tier system ---------------------
# The watcher should route each delivered event through `event-ack ingest`
# so the event->obligation rung arms without a manual main-loop step. We
# provide a fake `event-ack` on PATH that records its args, and verify (a)
# an actionable event (queue-orphaned) is ingested with the FULL message,
# and (b) CLAUDE_EVENT_WATCH_AUTO_INGEST=0 disables it.
AIQ="$TMP/aiq"; AILOG="$TMP/ailog"; FAKEBIN="$TMP/fakebin"
mkdir -p "$AIQ" "$AILOG" "$FAKEBIN"
INGEST_RECORD="$TMP/ingest-calls.log"
cat >"$FAKEBIN/event-ack" <<FAKE
#!/bin/bash
# Fake event-ack: record ingest invocations for the test to inspect.
printf '%s\n' "\$*" >>"$INGEST_RECORD"
exit 0
FAKE
chmod +x "$FAKEBIN/event-ack"

# (k) auto-ingest ON (default): an actionable orphan event is ingested.
: >"$INGEST_RECORD"
python3 - "$AIQ" <<'PYEOF'
import json, sys, time
q = sys.argv[1]
ev = {"timestamp": time.time(), "source": "claude-watch",
      "tag": "queue-orphaned",
      "message": "1 queue item orphaned: q-2026-07-15-abcd",
      "data": {"condition": "orphaned"}}
with open(f"{q}/100_orphan.json", "w") as f:
    json.dump(ev, f)
PYEOF
PATH="$FAKEBIN:$PATH" CLAUDE_EVENT_QUEUE="$AIQ" CLAUDE_EVENT_LOG_DIR="$AILOG" \
    "$WATCHER" --debounce 0 >/dev/null 2>&1
if [[ ! -s "$INGEST_RECORD" ]]; then
    echo "FAIL: auto-ingest did not invoke event-ack for a delivered event" >&2
    exit 1
fi
if ! grep -q 'ingest --source claude-watch --tag queue-orphaned' "$INGEST_RECORD"; then
    echo "FAIL: auto-ingest passed wrong source/tag" >&2
    cat "$INGEST_RECORD" >&2
    exit 1
fi
# Full (untruncated) message must be forwarded so classify substring rules match.
if ! grep -q 'q-2026-07-15-abcd' "$INGEST_RECORD"; then
    echo "FAIL: auto-ingest did not forward the full message" >&2
    cat "$INGEST_RECORD" >&2
    exit 1
fi
echo "  auto-ingest: actionable event routed through event-ack ingest OK"

# (l) auto-ingest OFF via env: event delivered but NOT ingested.
: >"$INGEST_RECORD"
python3 - "$AIQ" <<'PYEOF'
import json, sys, time
q = sys.argv[1]
ev = {"timestamp": time.time(), "source": "manual", "tag": "smoke2",
      "message": "no ingest please", "data": {}}
with open(f"{q}/110_noingest.json", "w") as f:
    json.dump(ev, f)
PYEOF
out=$(PATH="$FAKEBIN:$PATH" CLAUDE_EVENT_WATCH_AUTO_INGEST=0 \
    CLAUDE_EVENT_QUEUE="$AIQ" CLAUDE_EVENT_LOG_DIR="$AILOG" \
    "$WATCHER" --debounce 0 2>&1)
if ! grep -q '^EVENT\[manual/smoke2\]' <<<"$out"; then
    echo "FAIL: event not delivered with auto-ingest disabled" >&2
    echo "$out" >&2
    exit 1
fi
if [[ -s "$INGEST_RECORD" ]]; then
    echo "FAIL: CLAUDE_EVENT_WATCH_AUTO_INGEST=0 should NOT ingest" >&2
    cat "$INGEST_RECORD" >&2
    exit 1
fi
echo "  auto-ingest: disabled via CLAUDE_EVENT_WATCH_AUTO_INGEST=0 OK"

# (m) auto-ingest is best-effort: a FAILING event-ack must not break delivery.
FAILBIN="$TMP/failbin"; mkdir -p "$FAILBIN"
cat >"$FAILBIN/event-ack" <<'FAKE'
#!/bin/bash
echo "boom" >&2
exit 1
FAKE
chmod +x "$FAILBIN/event-ack"
python3 - "$AIQ" <<'PYEOF'
import json, sys, time
q = sys.argv[1]
ev = {"timestamp": time.time(), "source": "manual", "tag": "smoke3",
      "message": "delivery survives ingest failure", "data": {}}
with open(f"{q}/120_failingest.json", "w") as f:
    json.dump(ev, f)
PYEOF
out=$(PATH="$FAILBIN:$PATH" CLAUDE_EVENT_QUEUE="$AIQ" CLAUDE_EVENT_LOG_DIR="$AILOG" \
    "$WATCHER" --debounce 0 2>&1)
if ! grep -q '^EVENT\[manual/smoke3\]' <<<"$out"; then
    echo "FAIL: a failing event-ack broke event delivery (must be best-effort)" >&2
    echo "$out" >&2
    exit 1
fi
if [[ -f "$AIQ/120_failingest.json" ]]; then
    echo "FAIL: queue file not deleted after best-effort ingest failure" >&2
    exit 1
fi
echo "  auto-ingest: best-effort — delivery survives a failing event-ack OK"

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

# Test --min-interval validation: non-numeric / negative fail rc=2. NOTE 0 is
# VALID here (0 = throttle disabled), unlike --quiet, so it is NOT in this list.
for bad in abc -1; do
    set +e
    CLAUDE_EVENT_QUEUE="$QUEUE" "$WATCHER" --min-interval "$bad" >/dev/null 2>&1
    rc=$?
    set -e
    if (( rc != 2 )); then
        echo "FAIL: --min-interval $bad returned rc=$rc, expected 2" >&2
        exit 1
    fi
done

# Test --help works (and mentions both knobs)
help_out=$("$WATCHER" --help)
grep -q -- '--debounce' <<<"$help_out" || { echo "FAIL: --help missing --debounce" >&2; exit 1; }
grep -q -- '--quiet' <<<"$help_out" || { echo "FAIL: --help missing --quiet" >&2; exit 1; }
grep -q -- '--min-interval' <<<"$help_out" || { echo "FAIL: --help missing --min-interval" >&2; exit 1; }

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

# --- leading-edge throttle (--min-interval) tests -------------------------
# The throttle caps how often the watcher WAKES: at most one delivery per N
# seconds regardless of arrival spacing. Events during the hold are batched,
# never dropped. Default 0 = disabled (existing behavior). State is persisted
# to a file (env-overridable) so the cap survives the fire-and-exit restart.
TQ2="$TMP/tq2"; TLOG2="$TMP/tlog2"; TSTATE="$TMP/throttle.state"
mkdir -p "$TQ2" "$TLOG2"

# (n) First delivery is IMMEDIATE on empty state (leading edge fires at once),
# and the last-delivery epoch is persisted to disk.
write_event "$TQ2" "100_t1.json" "throttle first"
start=$(date +%s)
t1_out=$(CLAUDE_EVENT_QUEUE="$TQ2" CLAUDE_EVENT_LOG_DIR="$TLOG2" \
    CLAUDE_EVENT_WATCH_THROTTLE_STATE="$TSTATE" \
    "$WATCHER" --debounce 0 --min-interval 6 2>&1)
elapsed=$(( $(date +%s) - start ))
if ! grep -q 'throttle first' <<<"$t1_out"; then
    echo "FAIL: first throttled event not delivered immediately" >&2; echo "$t1_out" >&2; exit 1
fi
if (( elapsed > 3 )); then
    echo "FAIL: first throttled delivery waited ${elapsed}s (should be immediate on empty state)" >&2; exit 1
fi
if [[ ! -s "$TSTATE" ]] || ! grep -qE '^[0-9]+$' "$TSTATE"; then
    echo "FAIL: throttle did not persist an epoch last-delivery timestamp" >&2
    cat "$TSTATE" 2>/dev/null >&2; exit 1
fi
echo "  throttle: first delivery immediate + last-delivery epoch persisted OK"

# (o) A run WITHIN the window HOLDS until it closes; events landing DURING the
# hold are batched (delivered, never dropped). Pre-seed last-delivery to "now"
# so the full window must elapse — deterministic regardless of prior timing.
date +%s >"$TSTATE"
write_event "$TQ2" "110_t2.json" "hold A"
( sleep 1; write_event "$TQ2" "120_t3.json" "hold B" ) &
DRIP3=$!
start=$(date +%s)
t2_out=$(CLAUDE_EVENT_QUEUE="$TQ2" CLAUDE_EVENT_LOG_DIR="$TLOG2" \
    CLAUDE_EVENT_WATCH_THROTTLE_STATE="$TSTATE" \
    "$WATCHER" --debounce 0 --min-interval 5 2>&1)
elapsed=$(( $(date +%s) - start ))
wait "$DRIP3" 2>/dev/null || true
if (( elapsed < 3 )); then
    echo "FAIL: throttled run did not HOLD for ~the window (elapsed ${elapsed}s, --min-interval 5)" >&2
    echo "$t2_out" >&2; exit 1
fi
if ! grep -q 'hold A' <<<"$t2_out" || ! grep -q 'hold B' <<<"$t2_out"; then
    echo "FAIL: events arriving during the throttle hold were not both delivered (dropped/lost?)" >&2
    echo "$t2_out" >&2; exit 1
fi
if [[ -n "$(ls "$TQ2" 2>/dev/null)" ]]; then
    echo "FAIL: queue not drained after throttled batch surface" >&2; exit 1
fi
echo "  throttle: held until window, batched during-hold events, dropped nothing OK"

# (p) Explicit --min-interval 0 (disabled) is INERT: even with a "recent" state
# file present, delivery is immediate and record_delivery is a no-op (state
# untouched). This is the default behavior when the flag is omitted.
DQ="$TMP/dq"; DLOG="$TMP/dlog"; DSTATE="$TMP/dq.state"
mkdir -p "$DQ" "$DLOG"
echo "SENTINEL_UNTOUCHED" >"$DSTATE"
write_event "$DQ" "100_d.json" "default off"
start=$(date +%s)
d_out=$(CLAUDE_EVENT_QUEUE="$DQ" CLAUDE_EVENT_LOG_DIR="$DLOG" \
    CLAUDE_EVENT_WATCH_THROTTLE_STATE="$DSTATE" \
    "$WATCHER" --debounce 0 --min-interval 0 2>&1)
elapsed=$(( $(date +%s) - start ))
if ! grep -q 'default off' <<<"$d_out"; then
    echo "FAIL: --min-interval 0 (disabled) did not deliver" >&2; echo "$d_out" >&2; exit 1
fi
if (( elapsed > 2 )); then
    echo "FAIL: --min-interval 0 delayed delivery ${elapsed}s — must be inert" >&2; exit 1
fi
if [[ "$(cat "$DSTATE")" != "SENTINEL_UNTOUCHED" ]]; then
    echo "FAIL: throttle-off run overwrote the state file (record_delivery must be a no-op when disabled)" >&2
    cat "$DSTATE" >&2; exit 1
fi
echo "  throttle: explicit-0/default is inert (immediate delivery, state untouched) OK"

# --- TOCTOU gap-event / bounded-block loop test --------------------------
# Regression for the check-then-block race: the watcher's catch-up scan finds
# an empty queue, then arms inotifywait. An event landing in the gap before
# inotifywait's watch is armed would, with an UNBOUNDED block, be invisible to
# inotifywait (it only fires on events AFTER arming) and sit unconsumed until
# some LATER unrelated event woke it — long enough for the cron health-check
# to false-flag "WATCHER DOWN". The fix bounds the block with `inotifywait -t`
# and loops back to the catch-up scan, so a gap-event is drained within one
# timeout window.
#
# We can't deterministically hit the microsecond arm-gap, but we can assert the
# equivalent guarantee the bounded loop provides: start the watcher on an EMPTY
# queue (so it arms inotifywait and blocks), then drop an event. Whether the
# event is caught by inotifywait's wakeup OR by the next iteration's catch-up
# scan after a timeout, the bounded loop MUST drain it and exit within a couple
# of timeout windows. A pre-fix unbounded block that missed the create event
# would hang here forever (reaped as a FAIL by reap_within).
GQ="$TMP/gq"; GLOG="$TMP/glog"; GLOCK="$TMP/gap.lock"
mkdir -p "$GQ" "$GLOG"
# Short inotify timeout so the catch-up rescan fires quickly even if the create
# event itself is missed (the gap-event path we're guarding).
EVENT_WATCH_INOTIFY_TIMEOUT=2 CLAUDE_EVENT_QUEUE="$GQ" \
    CLAUDE_EVENT_LOG_DIR="$GLOG" CLAUDE_EVENT_WATCH_LOCK="$GLOCK" \
    "$WATCHER" --debounce 0 >"$TMP/gap.out" 2>&1 &
GAP=$!
BG_PIDS+=("$GAP")
# Let it reach the inotifywait block, then drop an event.
sleep 1
write_event "$GQ" "100_gap.json" "gap event"
# The bounded loop must drain + exit. Allow a generous margin (a few timeout
# windows) before declaring a hang.
if ! reap_within "$GAP" 15; then
    echo "FAIL: watcher did not drain a post-arm event within 15s (TOCTOU loop hung)" >&2
    cat "$TMP/gap.out" >&2
    exit 1
fi
if ! grep -q '^EVENT\[manual/batch\] gap event' "$TMP/gap.out"; then
    echo "FAIL: gap event not surfaced by bounded-block loop" >&2
    cat "$TMP/gap.out" >&2
    exit 1
fi
if ! grep -q 'WATCHER EXITED' "$TMP/gap.out"; then
    echo "FAIL: gap-event run missing restart banner (fire-and-exit contract)" >&2
    cat "$TMP/gap.out" >&2
    exit 1
fi
if [[ -n "$(ls "$GQ" 2>/dev/null)" ]]; then
    echo "FAIL: queue not drained after gap-event surface" >&2
    exit 1
fi
echo "  TOCTOU: post-arm gap event drained + exited within bounded window OK"

# --- flock singleton guard tests -----------------------------------------
# These verify the watcher self-defends against a duplicate launch racing the
# same queue. We isolate every instance onto a per-test lockfile via
# $CLAUDE_EVENT_WATCH_LOCK so a real watcher running on the host (or a
# previous test) can't perturb the result.

if ! command -v flock >/dev/null 2>&1; then
    echo "  SKIP: flock not available — singleton guard tests skipped" >&2
else
    # (h) Lock path is env-overridable, and a SECOND instance is refused
    # (exit 3) while the lock is held by a first. We hold the lock from the
    # test itself (deterministic — no inotify timing) by opening an fd on the
    # lockfile and flock'ing it, then invoke the watcher pointed at the SAME
    # lockfile and assert it refuses.
    LOCKFILE="$TMP/cew.lock"
    LQ="$TMP/lq"; LLOG="$TMP/llog"; mkdir -p "$LQ" "$LLOG"

    exec 8>"$LOCKFILE"
    if ! flock -n 8; then
        echo "FAIL: test harness could not acquire its own lock" >&2; exit 1
    fi
    # Lock now held by the test shell (fd 8). Second instance must refuse.
    set +e
    dup_out=$(CLAUDE_EVENT_QUEUE="$LQ" CLAUDE_EVENT_LOG_DIR="$LLOG" \
        CLAUDE_EVENT_WATCH_LOCK="$LOCKFILE" "$WATCHER" --debounce 0 2>&1)
    dup_rc=$?
    set -e
    if (( dup_rc != 3 )); then
        echo "FAIL: duplicate instance returned rc=$dup_rc, expected 3" >&2
        echo "$dup_out" >&2
        exit 1
    fi
    if ! grep -q 'already running' <<<"$dup_out"; then
        echo "FAIL: duplicate instance missing 'already running' message" >&2
        echo "$dup_out" >&2
        exit 1
    fi
    # Release the held lock; the SAME invocation must now succeed (proves the
    # refusal was the lock, not some other failure, and that the lockfile path
    # was honored via the env override). Preload an event so the now-unblocked
    # watcher takes the fast-path (drain → banner → exit) instead of arming
    # inotifywait and blocking forever on an empty queue (that empty-queue +
    # --debounce 0 block was an unbounded hang in this command substitution).
    flock -u 8
    exec 8>&-
    write_event "$LQ" "100_free.json" "free run"
    free_out=$(CLAUDE_EVENT_QUEUE="$LQ" CLAUDE_EVENT_LOG_DIR="$LLOG" \
        CLAUDE_EVENT_WATCH_LOCK="$LOCKFILE" "$WATCHER" --debounce 0 2>&1)
    if grep -q 'already running' <<<"$free_out"; then
        echo "FAIL: instance refused even though lock was released" >&2
        echo "$free_out" >&2
        exit 1
    fi
    if ! grep -q 'WATCHER EXITED' <<<"$free_out"; then
        echo "FAIL: instance with free lock did not run to completion" >&2
        echo "$free_out" >&2
        exit 1
    fi
    echo "  singleton: 2nd instance refused (rc=3) while lock held, runs once free OK"

    # (i) Real concurrent case: a FIRST watcher blocking on an empty queue
    # holds the lock; a SECOND launched against the same lockfile is refused.
    #
    # The ONLY thing this case asserts is the singleton guard under genuine
    # concurrency: while instance #1 holds the flock (blocked on inotifywait
    # over an empty queue), instance #2 must fail-fast with rc=3. The
    # teardown of instance #1 deliberately does NOT rely on a racy
    # inotify wakeup (drop-event → drain → self-exit): on the CI runner a
    # missed/filtered inotify CREATE — or `--include` regex support varying
    # across inotify-tools builds — could leave instance #1 blocked forever,
    # turning the subsequent `wait` into the unbounded hang that pinned the
    # "Run watcher tests" step for 15+ min. We tear instance #1 down
    # explicitly and reap it with a bounded poll instead.
    CQ="$TMP/cq"; CLOG="$TMP/clog"; CLOCK="$TMP/concurrent.lock"
    mkdir -p "$CQ" "$CLOG"
    # First instance: empty queue → it blocks on inotifywait holding the lock.
    CLAUDE_EVENT_QUEUE="$CQ" CLAUDE_EVENT_LOG_DIR="$CLOG" \
        CLAUDE_EVENT_WATCH_LOCK="$CLOCK" "$WATCHER" --debounce 0 >"$TMP/first.out" 2>&1 &
    FIRST=$!
    BG_PIDS+=("$FIRST")
    # Give the first instance a beat to acquire the lock + reach inotifywait.
    sleep 2
    set +e
    conc_out=$(CLAUDE_EVENT_QUEUE="$CQ" CLAUDE_EVENT_LOG_DIR="$CLOG" \
        CLAUDE_EVENT_WATCH_LOCK="$CLOCK" "$WATCHER" --debounce 0 2>&1)
    conc_rc=$?
    set -e
    if (( conc_rc != 3 )); then
        echo "FAIL: concurrent 2nd watcher returned rc=$conc_rc, expected 3" >&2
        echo "$conc_out" >&2
        kill "$FIRST" 2>/dev/null || true
        exit 1
    fi
    # Tear down instance #1 deterministically. We FIRST try the graceful path
    # (drop a release event so a healthy watcher drains + exits, exercising the
    # lock auto-release on a clean exit), but bound the reap so a missed
    # inotify wakeup can't hang the suite. If the graceful path doesn't reap it
    # in time, reap_within kills + waits it — the singleton assertion above has
    # already passed, so a kill teardown is fine.
    write_event "$CQ" "100_release.json" "release"
    if ! reap_within "$FIRST" 10; then
        echo "  NOTE: 1st watcher did not self-exit on release within 10s; killed it (inotify wakeup race on this runner) — singleton assertion already verified" >&2
    fi
    echo "  singleton: real concurrent 2nd watcher refused while 1st blocks OK"

    # (i2) REGRESSION: the singleton lock fd must NOT leak into the
    # inotifywait child.
    #
    # An flock is held by the OPEN FILE DESCRIPTION, so a child that inherits
    # the lock fd keeps the lock alive after the parent dies. inotifywait is
    # spawned with a timeout, so an orphaned one held the singleton lock for
    # up to that timeout — and every `watcher-ctl run` issued in that window
    # was refused with "already running (pid ...)" by a lock whose only holder
    # was an orphan. That is the stop-then-immediately-start refusal loop.
    #
    # Two assertions, both pinning the same fact:
    #   1. structural (Linux only) — inotifywait's fd table has no fd pointing
    #      at the lockfile;
    #   2. behavioural — SIGKILL the parent (no cleanup path runs), leave the
    #      orphan alive, and a fresh instance must still acquire the lock.
    IQ="$TMP/iq"; ILOG="$TMP/ilog"; ILOCK="$TMP/inherit.lock"
    mkdir -p "$IQ" "$ILOG"
    CLAUDE_EVENT_QUEUE="$IQ" CLAUDE_EVENT_LOG_DIR="$ILOG" \
        CLAUDE_EVENT_WATCH_LOCK="$ILOCK" "$WATCHER" --debounce 0 >"$TMP/inherit.out" 2>&1 &
    INHERIT=$!
    BG_PIDS+=("$INHERIT")
    # Let it acquire the lock and arm inotifywait.
    sleep 2

    # Locate the inotifywait child of this watcher. The watcher may have
    # re-exec'd under stdbuf, so search descendants rather than direct
    # children only.
    ino_pid=""
    if [[ -d /proc ]]; then
        for _try in 1 2 3 4 5; do
            for cand in $(pgrep -P "$INHERIT" 2>/dev/null || true) \
                        $(pgrep -f 'inotifywait .*'"$IQ" 2>/dev/null || true); do
                [[ -r "/proc/$cand/comm" ]] || continue
                if [[ "$(cat "/proc/$cand/comm" 2>/dev/null)" == inotifywait* ]]; then
                    ino_pid="$cand"; break
                fi
            done
            [[ -n "$ino_pid" ]] && break
            sleep 1
        done
    fi

    if [[ -n "$ino_pid" && -d "/proc/$ino_pid/fd" ]]; then
        leaked=""
        for fd in "/proc/$ino_pid/fd"/*; do
            [[ -e "$fd" ]] || continue
            tgt="$(readlink "$fd" 2>/dev/null || true)"
            if [[ "$tgt" == "$ILOCK" ]]; then
                leaked="$fd -> $tgt"
                break
            fi
        done
        if [[ -n "$leaked" ]]; then
            echo "FAIL: inotifywait inherited the singleton lock fd ($leaked)" >&2
            kill "$INHERIT" 2>/dev/null || true
            exit 1
        fi
        echo "  singleton: lock fd not inherited by inotifywait OK"
    else
        echo "  SKIP: could not locate the inotifywait child — fd-table check skipped" >&2
    fi

    # Behavioural half: SIGKILL the parent so no shell cleanup can run, then
    # confirm the (still-alive) orphan does not keep the lock held.
    kill -9 "$INHERIT" 2>/dev/null || true
    reap_within "$INHERIT" 5 >/dev/null 2>&1 || true
    if [[ -n "$ino_pid" ]] && ! kill -0 "$ino_pid" 2>/dev/null; then
        echo "  NOTE: inotifywait orphan already gone; orphan-lock assertion is weaker on this runner" >&2
    fi
    write_event "$IQ" "100_after_kill.json" "after kill"
    set +e
    after_out=$(CLAUDE_EVENT_QUEUE="$IQ" CLAUDE_EVENT_LOG_DIR="$ILOG" \
        CLAUDE_EVENT_WATCH_LOCK="$ILOCK" "$WATCHER" --debounce 0 2>&1)
    after_rc=$?
    set -e
    kill "$ino_pid" 2>/dev/null || true
    if (( after_rc == 3 )) || grep -q 'already running' <<<"$after_out"; then
        echo "FAIL: restart refused after the parent was killed — the lock is still held by an orphaned child" >&2
        echo "$after_out" >&2
        exit 1
    fi
    echo "  singleton: fresh instance acquires the lock right after a SIGKILLed parent OK"
fi

# (i3) The lockfile default must not depend on the caller's environment.
# It used to prefer $XDG_RUNTIME_DIR, so an interactive caller and a
# service/cron caller resolved DIFFERENT lockfiles and the singleton guard
# allowed two live watchers. Assert the script no longer consults it.
# Comments may still explain the old behaviour, so only CODE lines count.
if grep -v '^[[:space:]]*#' "$WATCHER" | grep -q 'XDG_RUNTIME_DIR'; then
    echo "FAIL: watcher lock path still references \$XDG_RUNTIME_DIR (env-dependent lock path)" >&2
    exit 1
fi
echo "  singleton: lock path is env-independent OK"

# (j) tty-warning path: when stdout is a tty the watcher must WARN (not fail).
# We can't easily give the subprocess a real tty here without a pty helper, so
# we assert the inverse contract instead — in our normal piped invocations the
# warning never appears — and additionally confirm the warning STRING exists in
# the script so a refactor can't silently drop it. (The non-tty no-warning
# behavior is already exercised by every other test above capturing stderr.)
if grep -q 'stdout is a tty' <<<"$run2"; then
    echo "FAIL: tty warning leaked into a piped (non-tty) invocation" >&2
    exit 1
fi
if ! grep -q 'stdout is a tty' "$WATCHER"; then
    echo "FAIL: watcher missing the tty-misuse warning" >&2
    exit 1
fi
# Best-effort real-tty check: if `script` (util that allocates a pty) is
# available, run the watcher under it and confirm the warning fires. Skipped
# silently where `script`'s flags differ (macOS vs Linux) or it's absent.
if command -v script >/dev/null 2>&1; then
    TQ="$TMP/tq"; TLOG="$TMP/tlog"; TLOCK="$TMP/tty.lock"
    mkdir -p "$TQ" "$TLOG"
    # Preload an event so the watcher takes the fast-path (warn about the tty,
    # drain, exit) under the pty. WITHOUT a pending event, --debounce 0 on an
    # empty queue arms inotifywait and blocks FOREVER inside the pty — and
    # `script` waits for its child, so the command substitution would never
    # return (this was a CI hang: GNU `script -qec` on the ubuntu runner held
    # the watcher-test step for minutes; the BSD-script path on macOS happened
    # to terminate, masking it locally).
    write_event "$TQ" "100_tty.json" "tty run"
    tty_out=""
    if script -qec "true" /dev/null >/dev/null 2>&1; then
        # GNU script (Linux): script -qec "<cmd>" <logfile>
        tty_out=$(CLAUDE_EVENT_QUEUE="$TQ" CLAUDE_EVENT_LOG_DIR="$TLOG" \
            CLAUDE_EVENT_WATCH_LOCK="$TLOCK" \
            script -qec "$WATCHER --debounce 0" /dev/null 2>&1 || true)
    elif script -q /dev/null true >/dev/null 2>&1; then
        # BSD script (macOS): script -q <logfile> <cmd...>
        tty_out=$(CLAUDE_EVENT_QUEUE="$TQ" CLAUDE_EVENT_LOG_DIR="$TLOG" \
            CLAUDE_EVENT_WATCH_LOCK="$TLOCK" \
            script -q /dev/null "$WATCHER" --debounce 0 2>&1 || true)
    fi
    if [[ -n "$tty_out" ]]; then
        if grep -q 'stdout is a tty' <<<"$tty_out"; then
            echo "  tty-misuse: warning fired under a real pty OK"
        else
            echo "  NOTE: pty run produced no tty warning (script flag mismatch?) — string-presence check still enforced" >&2
        fi
    fi
fi
echo "  tty-misuse: warning string present + absent from piped runs OK"

# --- delivery mode (--mode / mode file) tests -----------------------------
# Two delivery shapes coexist: the DEFAULT block-print-exit ("exit") and the
# long-lived "monitor". The mode file is the RUNTIME toggle — re-read on every
# loop iteration, so flipping it needs no rebuild, no revert and no restart.
# These tests pin down (i) the default is unchanged, (ii) precedence, (iii)
# fail-safe on bad input, (iv) monitor mode really does keep running and keep
# delivering, (v) flipping the file back to exit makes a LIVE monitor exit on
# its own with the usual clean-exit banner, and (vi) the silence-breaker.

MQ="$TMP/mq"; MLOG="$TMP/mlog"; mkdir -p "$MQ" "$MLOG"
MODEFILE="$MLOG/mode"

# (o) Default is `exit` — no flag, no env, no mode file.
mode_out=$(CLAUDE_EVENT_QUEUE="$MQ" CLAUDE_EVENT_LOG_DIR="$MLOG" "$WATCHER" --print-mode 2>/dev/null)
if [[ "$mode_out" != "exit" ]]; then
    echo "FAIL: default mode is '$mode_out', expected 'exit' (the default MUST be unchanged)" >&2
    exit 1
fi
echo "  mode: default is 'exit' (block-print-exit) OK"

# (p) Precedence: mode file < env < flag.
echo monitor >"$MODEFILE"
m_file=$(CLAUDE_EVENT_QUEUE="$MQ" CLAUDE_EVENT_LOG_DIR="$MLOG" "$WATCHER" --print-mode 2>/dev/null)
m_env=$(CLAUDE_EVENT_QUEUE="$MQ" CLAUDE_EVENT_LOG_DIR="$MLOG" \
    CLAUDE_EVENT_WATCH_MODE=exit "$WATCHER" --print-mode 2>/dev/null)
m_flag=$(CLAUDE_EVENT_QUEUE="$MQ" CLAUDE_EVENT_LOG_DIR="$MLOG" \
    CLAUDE_EVENT_WATCH_MODE=exit "$WATCHER" --mode monitor --print-mode 2>/dev/null)
if [[ "$m_file" != "monitor" || "$m_env" != "exit" || "$m_flag" != "monitor" ]]; then
    echo "FAIL: mode precedence wrong (file=$m_file env=$m_env flag=$m_flag; expected monitor/exit/monitor)" >&2
    exit 1
fi
echo "  mode: precedence flag > env > file OK"

# (q) Fail-safe: garbage in the mode file degrades to `exit` (with a warning)
# rather than taking event delivery down over a typo. An explicit BAD --mode
# flag, by contrast, is a direct instruction and is a hard error.
printf 'moniter\n' >"$MODEFILE"
bad_out=$(CLAUDE_EVENT_QUEUE="$MQ" CLAUDE_EVENT_LOG_DIR="$MLOG" "$WATCHER" --print-mode 2>"$TMP/bad.err")
if [[ "$bad_out" != "exit" ]]; then
    echo "FAIL: invalid mode file resolved to '$bad_out', expected fail-safe 'exit'" >&2
    exit 1
fi
grep -q 'unrecognised mode' "$TMP/bad.err" || {
    echo "FAIL: invalid mode file produced no warning" >&2; cat "$TMP/bad.err" >&2; exit 1; }
if CLAUDE_EVENT_QUEUE="$MQ" CLAUDE_EVENT_LOG_DIR="$MLOG" "$WATCHER" --mode bogus --print-mode >/dev/null 2>&1; then
    echo "FAIL: --mode bogus should exit non-zero" >&2
    exit 1
fi
echo "  mode: invalid file fails safe to 'exit'; invalid --mode flag is an error OK"

# (r) --mode-status reports the resolved mode, its source and the toggle.
echo monitor >"$MODEFILE"
st_out=$(CLAUDE_EVENT_QUEUE="$MQ" CLAUDE_EVENT_LOG_DIR="$MLOG" "$WATCHER" --mode-status 2>/dev/null)
grep -q '^mode: *monitor' <<<"$st_out" || { echo "FAIL: --mode-status missing mode line" >&2; echo "$st_out" >&2; exit 1; }
grep -q "$MODEFILE" <<<"$st_out" || { echo "FAIL: --mode-status does not name the mode file" >&2; echo "$st_out" >&2; exit 1; }
echo "  mode: --mode-status shows resolved mode + source + toggle OK"

# (s) Supervised-monitor guard: a monitor launched UNDER the block-print-exit
# supervisor would drain events to a stdout nobody reads until it exits (which
# it never does), so it degrades to `exit` with a warning. Identity requires
# BOTH a supervisor comm and the `run <watcher>` argv — a plain shell whose
# command line merely contains the phrase must NOT trip it.
if [[ -d /proc/$$ ]]; then
    ln -sf /bin/bash "$TMP/watcher-ctl"
    # `; :` defeats bash's exec-optimisation so the fake supervisor survives as
    # the watcher's parent instead of being replaced by it.
    sup_out=$(CLAUDE_EVENT_QUEUE="$MQ" CLAUDE_EVENT_LOG_DIR="$MLOG" \
        "$TMP/watcher-ctl" -c '"$0" --print-mode; :' "$WATCHER" run claude-event-watch 2>/dev/null)
    sup_ovr=$(CLAUDE_EVENT_QUEUE="$MQ" CLAUDE_EVENT_LOG_DIR="$MLOG" \
        CLAUDE_EVENT_WATCH_ALLOW_SUPERVISED_MONITOR=1 \
        "$TMP/watcher-ctl" -c '"$0" --print-mode; :' "$WATCHER" run claude-event-watch 2>/dev/null)
    plain_out=$(CLAUDE_EVENT_QUEUE="$MQ" CLAUDE_EVENT_LOG_DIR="$MLOG" \
        bash -c '"$0" --print-mode; :' "$WATCHER" run claude-event-watch 2>/dev/null)
    if [[ "$sup_out" != "exit" ]]; then
        echo "FAIL: monitor under a block-print-exit supervisor resolved '$sup_out', expected 'exit'" >&2
        exit 1
    fi
    if [[ "$sup_ovr" != "monitor" ]]; then
        echo "FAIL: CLAUDE_EVENT_WATCH_ALLOW_SUPERVISED_MONITOR=1 did not override the guard (got '$sup_ovr')" >&2
        exit 1
    fi
    if [[ "$plain_out" != "monitor" ]]; then
        echo "FAIL: a plain shell ancestor tripped the supervisor guard (got '$plain_out') — false positive" >&2
        exit 1
    fi
    echo "  mode: supervised-monitor guard fires on a real supervisor only OK"
fi

# (t)+(u)+(v) The live monitor: it delivers a batch WITHOUT exiting, delivers a
# SECOND batch from the same process, breaks its own silence, and then — this
# is the acceptance test for "toggle without a restart" — exits cleanly on its
# own when the mode file is flipped back to `exit`, printing the same restart
# banner the block-print-exit path prints.
LIVE_Q="$TMP/liveq"; LIVE_LOG="$TMP/livelog"; mkdir -p "$LIVE_Q" "$LIVE_LOG"
LIVE_MODEFILE="$LIVE_LOG/mode"
LIVE_OUT="$TMP/live.out"
echo monitor >"$LIVE_MODEFILE"
write_event "$LIVE_Q" "100_m1.json" "monitor first"
CLAUDE_EVENT_QUEUE="$LIVE_Q" CLAUDE_EVENT_LOG_DIR="$LIVE_LOG" \
    CLAUDE_EVENT_WATCH_LOCK="$TMP/live.lock" \
    EVENT_WATCH_INOTIFY_TIMEOUT=2 \
    "$WATCHER" --debounce 2 --quiet 1 --liveness-interval 3 >"$LIVE_OUT" 2>&1 &
LIVE_PID=$!
BG_PIDS+=("$LIVE_PID")

# Wait (bounded) for the first batch to appear.
waited=0
while ! grep -q 'monitor first' "$LIVE_OUT" 2>/dev/null; do
    if (( waited >= 20 )); then
        echo "FAIL: monitor mode never surfaced the first batch" >&2
        cat "$LIVE_OUT" >&2; exit 1
    fi
    sleep 1; waited=$(( waited + 1 ))
done
if ! kill -0 "$LIVE_PID" 2>/dev/null; then
    echo "FAIL: monitor mode EXITED after its first batch (it must stay alive)" >&2
    cat "$LIVE_OUT" >&2; exit 1
fi
if grep -q 'WATCHER EXITED' "$LIVE_OUT"; then
    echo "FAIL: monitor mode printed the restart banner after a batch" >&2
    cat "$LIVE_OUT" >&2; exit 1
fi
grep -q 'MONITOR MODE ACTIVE' "$LIVE_OUT" || {
    echo "FAIL: monitor mode printed no startup line (no positive control)" >&2
    cat "$LIVE_OUT" >&2; exit 1; }
# No-consume contract holds in monitor mode too: the queue file is drained and
# the consumed-log line is appended, exactly as in exit mode.
if [[ -n "$(ls "$LIVE_Q" 2>/dev/null)" ]]; then
    echo "FAIL: monitor mode did not drain the queue" >&2; exit 1
fi
grep -q 'monitor first' "$LIVE_LOG/consumed.jsonl" || {
    echo "FAIL: monitor mode did not append to the consumed log" >&2; exit 1; }
echo "  mode: monitor delivered a batch and stayed alive (no banner) OK"

# Second batch from the SAME process — this is the whole point of the mode.
write_event "$LIVE_Q" "200_m2.json" "monitor second"
waited=0
while ! grep -q 'monitor second' "$LIVE_OUT" 2>/dev/null; do
    if (( waited >= 20 )); then
        echo "FAIL: monitor mode did not deliver a SECOND batch without a restart" >&2
        cat "$LIVE_OUT" >&2; exit 1
    fi
    sleep 1; waited=$(( waited + 1 ))
done
echo "  mode: monitor delivered a second batch from the same process OK"

# Silence-breaker: with --liveness-interval 3 a quiet monitor must say so.
waited=0
while ! grep -q 'EVENT-WATCH ALIVE' "$LIVE_OUT" 2>/dev/null; do
    if (( waited >= 20 )); then
        echo "FAIL: monitor mode emitted no EVENT-WATCH ALIVE line during a lull" >&2
        cat "$LIVE_OUT" >&2; exit 1
    fi
    sleep 1; waited=$(( waited + 1 ))
done
echo "  mode: silence-breaker emitted EVENT-WATCH ALIVE during a lull OK"

# THE ACCEPTANCE TEST — flip the file back and the LIVE process winds itself
# down: no kill, no restart command, no rebuild, no revert.
echo exit >"$LIVE_MODEFILE"
if ! reap_within "$LIVE_PID" 25; then
    echo "FAIL: flipping the mode file to 'exit' did not stop the live monitor" >&2
    cat "$LIVE_OUT" >&2; exit 1
fi
grep -q 'MODE CHANGED monitor -> exit' "$LIVE_OUT" || {
    echo "FAIL: monitor did not announce the mode change" >&2; cat "$LIVE_OUT" >&2; exit 1; }
grep -q 'WATCHER EXITED' "$LIVE_OUT" || {
    echo "FAIL: monitor exited without the restart banner (the loop would not restart it)" >&2
    cat "$LIVE_OUT" >&2; exit 1; }
echo "  mode: mode-file flip made a LIVE monitor exit cleanly, no restart needed OK"

# --help advertises the new surface.
grep -q -- '--mode' <<<"$help_out" || { echo "FAIL: --help missing --mode" >&2; exit 1; }
grep -q -- '--liveness-interval' <<<"$help_out" || { echo "FAIL: --help missing --liveness-interval" >&2; exit 1; }
grep -q -- '--print-mode' <<<"$help_out" || { echo "FAIL: --help missing --print-mode" >&2; exit 1; }
echo "  mode: --help documents --mode / --liveness-interval / --print-mode OK"

echo "PASS: all claude-event-watch checks (fast-path + adaptive debounce + throttle + singleton + tty guard + delivery mode)"
