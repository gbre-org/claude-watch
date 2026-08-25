#!/usr/bin/env python3
"""Tests for queue-notify's TRANSITION PUSH POLICY (2026-08 addition).

Not every queue lifecycle transition earns a phone push. `started` and
`done` are bookkeeping — one per register, one per completion, nothing for
the operator to act on — and between them they burned roughly a third of
the Pushover account's monthly message allowance in Aug 2026. So
`queue-notify` filters them out before the debounce/batch layer.

What these tests pin down:

  * `started` / `done` produce NO push, and exit 0 (a policy decision is
    not a delivery failure — the caller must not treat it as one).
  * The attention-required transitions still push: `abandoned`, `blocked`,
    `wedged`, `quarantined`, `force-started`, `unblocked`, `unwedged`.
  * `QUEUE_NOTIFY_PUSH_TRANSITIONS` re-enables suppressed transitions
    without a code change (`all`, `default,done`, an explicit list).
  * Suppression FAILS OPEN on unknown titles: a hand-written title, or a
    transition kind added to session-task later, still pushes.
  * A suppressed transition never reaches the SPOOL, so it can't inflate a
    later batch's event count.
  * The policy applies on the immediate path too (`QUEUE_NOTIFY_BATCH=0`).

The claude-event side is deliberately NOT touched by any of this — see
test_queue_claude_event.py, which asserts session-task still emits
`queue-started` / `queue-done` events.

Run:
    uv run --python 3.11 --with pytest pytest tests/test_queue_notify_policy.py -v

Or directly:
    python3 tests/test_queue_notify_policy.py
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
    """Env for a subprocess run: tmp HOME, tmp spool, sink dry-run.

    NOTE: unlike test_queue_notify_batch.py's helper this does NOT set
    QUEUE_NOTIFY_PUSH_TRANSITIONS — the whole point here is the DEFAULT
    policy. It is explicitly stripped so a value in the ambient
    environment can't mask a regression.
    """
    env = dict(os.environ)
    env.pop("QUEUE_NOTIFY_PUSH_TRANSITIONS", None)
    env["HOME"] = str(tmp)
    env["QUEUE_NOTIFY_SPOOL"] = str(Path(tmp) / "spool.jsonl")
    env["QUEUE_NOTIFY_SINK"] = str(Path(tmp) / "sink.jsonl")
    # Short windows so a push that IS expected lands within test time.
    env.setdefault("QUEUE_NOTIFY_DEBOUNCE", "1")
    env.setdefault("QUEUE_NOTIFY_MAX_WINDOW", "5")
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
    return [json.loads(ln) for ln in p.read_text().splitlines() if ln.strip()]


def _spool_lines(env):
    p = Path(env["QUEUE_NOTIFY_SPOOL"])
    if not p.exists():
        return []
    return [ln for ln in p.read_text().splitlines() if ln.strip()]


def _wait_for_sink(env, count, timeout=12.0):
    deadline = time.time() + timeout
    while time.time() < deadline:
        lines = _read_sink(env)
        if len(lines) >= count:
            return lines
        time.sleep(0.2)
    return _read_sink(env)


def _settle(env, seconds=6.0):
    """Give any flusher time to fire, then report what landed in the sink.

    Used for NEGATIVE assertions: we need the window to actually elapse
    before concluding "no push was sent".
    """
    time.sleep(seconds)
    return _read_sink(env)


# ---------------------------------------------------------------------------
# 1. started / done produce no push under the default policy
# ---------------------------------------------------------------------------


def test_started_produces_no_push():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env(tmp)
        r = _fire(env, "some task\nscope: repo:x", "queue started: q-1")
        assert r.returncode == 0, r.stderr
        assert _settle(env) == [], "started must not push under default policy"
        # And it never even hit the spool.
        assert _spool_lines(env) == [], _spool_lines(env)


def test_done_produces_no_push():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env(tmp)
        r = _fire(env, "some task\nelapsed: 4m", "queue done: q-2")
        assert r.returncode == 0, r.stderr
        assert _settle(env) == [], "done must not push under default policy"
        assert _spool_lines(env) == [], _spool_lines(env)


def test_started_and_done_suppressed_on_immediate_path_too():
    """QUEUE_NOTIFY_BATCH=0 takes the synchronous send path; policy still applies."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env(tmp, QUEUE_NOTIFY_BATCH="0")
        assert _fire(env, "body", "queue started: q-a").returncode == 0
        assert _fire(env, "body", "queue done: q-b").returncode == 0
        assert _read_sink(env) == []
        # A push-worthy transition on the same path still lands, immediately.
        assert _fire(env, "body", "queue BLOCKED: q-c").returncode == 0
        lines = _read_sink(env)
        assert len(lines) == 1, lines
        assert lines[0]["title"] == "queue BLOCKED: q-c", lines[0]


# ---------------------------------------------------------------------------
# 2. The informative transitions still push
# ---------------------------------------------------------------------------


def test_abandoned_still_pushes():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env(tmp)
        r = _fire(env, "task E\nreason: crashed", "queue abandoned: q-5")
        assert r.returncode == 0, r.stderr
        lines = _wait_for_sink(env, 1)
        assert len(lines) == 1, lines
        assert lines[0]["title"] == "queue abandoned: q-5", lines[0]


def test_blocked_still_pushes():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env(tmp)
        r = _fire(env, "waiting on a human", "queue BLOCKED: q-6")
        assert r.returncode == 0, r.stderr
        lines = _wait_for_sink(env, 1)
        assert len(lines) == 1, lines
        assert lines[0]["title"] == "queue BLOCKED: q-6", lines[0]


def test_every_attention_transition_is_push_worthy_by_default():
    """The full default set, asserted as behaviour rather than as a constant."""
    for title in (
        "queue abandoned: q-1",
        "queue BLOCKED: q-2",
        "queue WEDGED: q-3",
        "queue QUARANTINED: q-4",
        "queue force-started: q-5",
        "queue unblocked: q-6",
        "queue unwedged: q-7",
    ):
        with tempfile.TemporaryDirectory() as tmp:
            env = _env(tmp, QUEUE_NOTIFY_BATCH="0")
            r = _fire(env, "body", title)
            assert r.returncode == 0, r.stderr
            lines = _read_sink(env)
            assert len(lines) == 1, f"{title} should push: {lines}"
            assert lines[0]["title"] == title, lines[0]


# ---------------------------------------------------------------------------
# 3. Env override re-enables suppressed transitions
# ---------------------------------------------------------------------------


def test_env_override_all_reenables_started_and_done():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env(tmp, QUEUE_NOTIFY_BATCH="0",
                   QUEUE_NOTIFY_PUSH_TRANSITIONS="all")
        assert _fire(env, "body", "queue started: q-1").returncode == 0
        assert _fire(env, "body", "queue done: q-2").returncode == 0
        lines = _read_sink(env)
        assert len(lines) == 2, lines
        assert [ln["title"] for ln in lines] == [
            "queue started: q-1", "queue done: q-2"]


def test_env_override_default_plus_done():
    """`default,done` == the built-in set plus `done`; `started` stays off."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env(tmp, QUEUE_NOTIFY_BATCH="0",
                   QUEUE_NOTIFY_PUSH_TRANSITIONS="default,done")
        assert _fire(env, "body", "queue done: q-1").returncode == 0
        assert _fire(env, "body", "queue started: q-2").returncode == 0
        assert _fire(env, "body", "queue BLOCKED: q-3").returncode == 0
        titles = [ln["title"] for ln in _read_sink(env)]
        assert titles == ["queue done: q-1", "queue BLOCKED: q-3"], titles


def test_env_override_explicit_list_is_exclusive():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env(tmp, QUEUE_NOTIFY_BATCH="0",
                   QUEUE_NOTIFY_PUSH_TRANSITIONS="wedged, started")
        assert _fire(env, "body", "queue WEDGED: q-1").returncode == 0
        assert _fire(env, "body", "queue started: q-2").returncode == 0
        # Not in the list -> suppressed, even though it is a default member.
        assert _fire(env, "body", "queue BLOCKED: q-3").returncode == 0
        titles = [ln["title"] for ln in _read_sink(env)]
        assert titles == ["queue WEDGED: q-1", "queue started: q-2"], titles


def test_env_override_none_suppresses_every_known_transition():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env(tmp, QUEUE_NOTIFY_BATCH="0",
                   QUEUE_NOTIFY_PUSH_TRANSITIONS="none")
        for title in ("queue done: q-1", "queue WEDGED: q-2",
                      "queue abandoned: q-3"):
            assert _fire(env, "body", title).returncode == 0
        assert _read_sink(env) == []
        # Unknown titles still fail open even under `none`.
        assert _fire(env, "body", "gomorrah").returncode == 0
        assert len(_read_sink(env)) == 1


# ---------------------------------------------------------------------------
# 4. Fail-open on unknown / ad-hoc titles
# ---------------------------------------------------------------------------


def test_unknown_transition_still_pushes():
    """A transition kind this file doesn't know about must NOT be silenced."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env(tmp, QUEUE_NOTIFY_BATCH="0")
        assert _fire(env, "body", "queue teleported: q-9").returncode == 0
        assert _fire(env, "an ad-hoc message", "gomorrah").returncode == 0
        titles = [ln["title"] for ln in _read_sink(env)]
        assert titles == ["queue teleported: q-9", "gomorrah"], titles


# ---------------------------------------------------------------------------
# 5. Suppressed events never inflate a batch
# ---------------------------------------------------------------------------


def test_suppressed_events_do_not_join_a_batch():
    """A burst of 5 with 4 suppressed collapses to the ONE that matters."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env(tmp, QUEUE_NOTIFY_DEBOUNCE="2", QUEUE_NOTIFY_MAX_WINDOW="15")
        _fire(env, "task A", "queue done: q-1")
        _fire(env, "task B", "queue started: q-2")
        _fire(env, "task C", "queue done: q-3")
        _fire(env, "task D", "queue started: q-4")
        _fire(env, "task E\nreason: crashed", "queue abandoned: q-5")

        lines = _wait_for_sink(env, 1, timeout=20)
        assert len(lines) == 1, lines
        # A lone survivor is delivered VERBATIM, not as a "5 queue events"
        # batch summary — the suppressed four never reached the spool.
        assert lines[0]["title"] == "queue abandoned: q-5", lines[0]
        assert "queue events" not in lines[0]["message"], lines[0]


# ---------------------------------------------------------------------------
# 6. Unit: the policy resolver itself
# ---------------------------------------------------------------------------


def test_policy_unit():
    mod = _load_module()
    prior = os.environ.pop("QUEUE_NOTIFY_PUSH_TRANSITIONS", None)
    try:
        # Default set: every known transition except started/done.
        assert mod.DEFAULT_PUSH_TRANSITIONS == frozenset({
            "abandoned", "blocked", "force-started", "quarantined",
            "unblocked", "unwedged", "wedged",
        }), sorted(mod.DEFAULT_PUSH_TRANSITIONS)
        assert "started" not in mod.DEFAULT_PUSH_TRANSITIONS
        assert "done" not in mod.DEFAULT_PUSH_TRANSITIONS
        assert mod.DEFAULT_PUSH_TRANSITIONS < mod.KNOWN_TRANSITIONS

        assert mod._push_transitions() == mod.DEFAULT_PUSH_TRANSITIONS
        assert mod._should_push("queue done: q-1") is False
        assert mod._should_push("queue started: q-1") is False
        assert mod._should_push("queue WEDGED: q-1") is True
        # Unknown -> fail open.
        assert mod._should_push("queue teleported: q-1") is True
        assert mod._should_push("") is True

        # Empty / whitespace env falls back to the default (not "none").
        os.environ["QUEUE_NOTIFY_PUSH_TRANSITIONS"] = "   "
        assert mod._push_transitions() == mod.DEFAULT_PUSH_TRANSITIONS

        os.environ["QUEUE_NOTIFY_PUSH_TRANSITIONS"] = "all"
        assert mod._push_transitions() == mod.KNOWN_TRANSITIONS

        os.environ["QUEUE_NOTIFY_PUSH_TRANSITIONS"] = "none"
        assert mod._push_transitions() == frozenset()

        # Tokens are case-insensitive, tolerate a `queue ` prefix, and
        # accept comma and/or whitespace separation.
        os.environ["QUEUE_NOTIFY_PUSH_TRANSITIONS"] = "queue DONE,  WEDGED"
        assert mod._push_transitions() == frozenset({"done", "wedged"})

        os.environ["QUEUE_NOTIFY_PUSH_TRANSITIONS"] = "default done"
        assert mod._push_transitions() == (
            mod.DEFAULT_PUSH_TRANSITIONS | {"done"})

        assert mod._normalize_transition("  Queue Force-Started ") == \
            "force-started"
    finally:
        os.environ.pop("QUEUE_NOTIFY_PUSH_TRANSITIONS", None)
        if prior is not None:
            os.environ["QUEUE_NOTIFY_PUSH_TRANSITIONS"] = prior


# ---------------------------------------------------------------------------
# Entry point for direct invocation
# ---------------------------------------------------------------------------


def _all_tests():
    return [
        test_started_produces_no_push,
        test_done_produces_no_push,
        test_started_and_done_suppressed_on_immediate_path_too,
        test_abandoned_still_pushes,
        test_blocked_still_pushes,
        test_every_attention_transition_is_push_worthy_by_default,
        test_env_override_all_reenables_started_and_done,
        test_env_override_default_plus_done,
        test_env_override_explicit_list_is_exclusive,
        test_env_override_none_suppresses_every_known_transition,
        test_unknown_transition_still_pushes,
        test_suppressed_events_do_not_join_a_batch,
        test_policy_unit,
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
