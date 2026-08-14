#!/usr/bin/env python3
"""Tests for queue-notify's debounce + batch layer (2026-08 addition).

``queue-notify`` coalesces a BURST of queue lifecycle pushes into a SINGLE
summarised Pushover, instead of one push per transition. session-task's
per-event invocation contract is unchanged (see test_queue_pingme.py); the
debounce/batch happens INSIDE queue-notify, between "invoked N times" and
"POST once".

These tests exercise the real ``queue-notify`` via subprocess, using:
  * ``QUEUE_NOTIFY_SINK`` — dry-run seam: each delivery is appended as a
    JSON line to a file instead of hitting Pushover. So "how many pushes
    did a burst produce" == "how many lines in the sink".
  * ``QUEUE_NOTIFY_SPOOL`` — a per-test tmp spool file.
  * ``QUEUE_NOTIFY_DEBOUNCE`` / ``QUEUE_NOTIFY_MAX_WINDOW`` — tiny windows
    so the debounce flush happens within test time.

Run:
    uv run --python 3.11 --with pytest pytest tests/test_queue_notify_batch.py -v

Or directly:
    python3 tests/test_queue_notify_batch.py
"""

import importlib.machinery
import importlib.util
import json
import os
import subprocess
import sys
import tempfile
import time
from pathlib import Path

QUEUE_NOTIFY = Path(__file__).resolve().parent.parent / "queue-notify"


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------


def _load_module():
    """Import queue-notify (no .py extension) as a module for unit tests."""
    loader = importlib.machinery.SourceFileLoader("queue_notify",
                                                  str(QUEUE_NOTIFY))
    spec = importlib.util.spec_from_loader("queue_notify", loader)
    mod = importlib.util.module_from_spec(spec)
    loader.exec_module(mod)
    return mod


def _env(tmp, **overrides):
    env = dict(os.environ)
    env["HOME"] = str(tmp)
    env["QUEUE_NOTIFY_SPOOL"] = str(Path(tmp) / "spool.jsonl")
    env["QUEUE_NOTIFY_SINK"] = str(Path(tmp) / "sink.jsonl")
    # Fast windows by default; individual tests override.
    env.setdefault("QUEUE_NOTIFY_DEBOUNCE", "3")
    env.setdefault("QUEUE_NOTIFY_MAX_WINDOW", "10")
    for k, v in overrides.items():
        env[k] = v
    return env


def _fire(env, message, title, priority="normal"):
    """Invoke queue-notify once (the shape session-task uses)."""
    cmd = [sys.executable, str(QUEUE_NOTIFY), "-p", priority, message, title]
    return subprocess.run(cmd, capture_output=True, text=True, env=env,
                          timeout=20)


def _read_sink(env):
    p = Path(env["QUEUE_NOTIFY_SINK"])
    if not p.exists():
        return []
    out = []
    for line in p.read_text().splitlines():
        line = line.strip()
        if line:
            out.append(json.loads(line))
    return out


def _wait_for_sink(env, count, timeout=18.0):
    """Poll the sink until it has >= count lines or timeout. Returns lines."""
    deadline = time.time() + timeout
    while time.time() < deadline:
        lines = _read_sink(env)
        if len(lines) >= count:
            return lines
        time.sleep(0.2)
    return _read_sink(env)


# ---------------------------------------------------------------------------
# 1. A burst of N transitions produces ONE batched push
# ---------------------------------------------------------------------------


def test_burst_coalesces_into_single_push():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env(tmp, QUEUE_NOTIFY_DEBOUNCE="3", QUEUE_NOTIFY_MAX_WINDOW="20")
        # 5 transitions, back-to-back: 2 done, 2 started, 1 abandoned.
        _fire(env, "task A\nelapsed: 3m", "queue done: q-1")
        _fire(env, "task B\nelapsed: 4m", "queue done: q-2")
        _fire(env, "task C\nscope: repo:x", "queue started: q-3")
        _fire(env, "task D\nscope: repo:y", "queue started: q-4")
        _fire(env, "task E\nreason: crashed", "queue abandoned: q-5")

        lines = _wait_for_sink(env, 1, timeout=20)
        assert len(lines) == 1, (
            f"expected exactly ONE batched push, got {len(lines)}: {lines}"
        )
        msg = lines[0]["message"]
        assert "5 queue events" in msg, msg
        assert "2 done" in msg, msg
        assert "2 started" in msg, msg
        assert "1 abandoned" in msg, msg
        # Detail lines carry ids + summaries.
        assert "q-1" in msg and "q-5" in msg, msg
        assert "task A" in msg, msg
        assert lines[0]["title"].endswith("5 queue events"), lines[0]["title"]


# ---------------------------------------------------------------------------
# 2. A lone event is delivered verbatim (no batch wrapper)
# ---------------------------------------------------------------------------


def test_single_event_delivered_verbatim():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env(tmp, QUEUE_NOTIFY_DEBOUNCE="1", QUEUE_NOTIFY_MAX_WINDOW="5")
        _fire(env, "solo summary\nscope: repo:z", "queue done: q-solo")

        lines = _wait_for_sink(env, 1, timeout=12)
        assert len(lines) == 1, lines
        assert lines[0]["title"] == "queue done: q-solo", lines[0]
        assert lines[0]["message"] == "solo summary\nscope: repo:z", lines[0]
        assert "queue events" not in lines[0]["message"], lines[0]
        assert lines[0]["priority"] == 0, lines[0]


# ---------------------------------------------------------------------------
# 3. QUEUE_NOTIFY_BATCH=0 -> immediate synchronous send (legacy behaviour)
# ---------------------------------------------------------------------------


def test_batch_disabled_sends_immediately():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env(tmp, QUEUE_NOTIFY_BATCH="0")
        r = _fire(env, "immediate body", "queue done: q-imm")
        assert r.returncode == 0, r.stderr
        # No wait: the send is synchronous when batching is off.
        lines = _read_sink(env)
        assert len(lines) == 1, lines
        assert lines[0]["title"] == "queue done: q-imm"
        assert lines[0]["message"] == "immediate body"
        # Spool must not have been used.
        assert not Path(env["QUEUE_NOTIFY_SPOOL"]).exists() or \
            Path(env["QUEUE_NOTIFY_SPOOL"]).read_text().strip() == ""


# ---------------------------------------------------------------------------
# 4. Emergency priority bypasses the debounce window (immediate)
# ---------------------------------------------------------------------------


def test_emergency_bypasses_batch():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env(tmp, QUEUE_NOTIFY_DEBOUNCE="30", QUEUE_NOTIFY_MAX_WINDOW="60")
        r = _fire(env, "the world is on fire", "queue WEDGED: q-emg",
                  priority="emergency")
        assert r.returncode == 0, r.stderr
        # Immediate despite a long debounce window.
        lines = _read_sink(env)
        assert len(lines) == 1, lines
        assert lines[0]["priority"] == 2, lines[0]


# ---------------------------------------------------------------------------
# 5. Escalation: a batch takes the MAX priority among its events
# ---------------------------------------------------------------------------


def test_batch_priority_is_max_of_members():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env(tmp, QUEUE_NOTIFY_DEBOUNCE="3", QUEUE_NOTIFY_MAX_WINDOW="20")
        _fire(env, "low one", "queue unblocked: q-a", priority="low")
        _fire(env, "normal one", "queue done: q-b", priority="normal")
        _fire(env, "high one", "queue BLOCKED: q-c", priority="high")

        lines = _wait_for_sink(env, 1, timeout=20)
        assert len(lines) == 1, lines
        # high == 1
        assert lines[0]["priority"] == 1, lines[0]
        assert "3 queue events" in lines[0]["message"], lines[0]


# ---------------------------------------------------------------------------
# 6. Unit: _compose_batch / _event_kind
# ---------------------------------------------------------------------------


def test_compose_batch_unit():
    mod = _load_module()
    recs = [
        {"ts": 1, "priority": "normal", "title": "queue done: q-1",
         "message": "did a thing\nelapsed: 2m"},
        {"ts": 2, "priority": "normal", "title": "queue done: q-2",
         "message": "did another"},
        {"ts": 3, "priority": "high", "title": "queue WEDGED: q-3",
         "message": "stuck"},
        {"ts": 4, "priority": "low", "title": "queue force-started: q-4",
         "message": "forced"},
    ]
    title, message, priority, sound = mod._compose_batch(recs)
    assert "4 queue events" in message
    assert "2 done" in message
    assert "1 wedged" in message
    assert "1 force-started" in message
    assert priority == mod._PRIORITY_MAP["high"]  # max of members
    assert title.endswith("4 queue events")
    # kind extraction is case-insensitive & strips the "queue " prefix
    assert mod._event_kind("queue WEDGED: q-9") == "wedged"
    assert mod._event_kind("queue force-started: q-9") == "force-started"
    assert mod._event_id("queue done: q-abc") == "q-abc"


# ---------------------------------------------------------------------------
# Entry point for direct invocation
# ---------------------------------------------------------------------------


def _all_tests():
    return [
        test_burst_coalesces_into_single_push,
        test_single_event_delivered_verbatim,
        test_batch_disabled_sends_immediately,
        test_emergency_bypasses_batch,
        test_batch_priority_is_max_of_members,
        test_compose_batch_unit,
    ]


if __name__ == "__main__":
    fail = 0
    for t in _all_tests():
        try:
            t()
            print(f"PASS: {t.__name__}")
        except Exception as e:  # noqa: BLE001
            fail += 1
            print(f"FAIL: {t.__name__}: {e}")
    sys.exit(0 if fail == 0 else 1)
