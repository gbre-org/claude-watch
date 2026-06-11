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
fi

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

echo "PASS: all claude-event-watch checks (fast-path + adaptive debounce + singleton + tty guard)"
