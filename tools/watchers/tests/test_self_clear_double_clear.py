#!/usr/bin/env python3
"""Regression: self-clear must not re-send /clear after a clear that worked.

Incident 2026-11-10: attempt 1's first completion poll read 51837 tokens
(from 965298) -- the clear had worked -- but a fresh session's baseline is
already above FRESH_SESSION_MAX_TOKENS (30000) and kept growing from hook
injections (52036, 78508, ... 80295). The poll never matched the absolute
floor, so the window expired, attempt 1 was logged FAILED, and /clear was
re-sent into the fresh session (and again on attempt 2).

These tests replay that exact token sequence through child_main() with the
tmux / claude-watch layer mocked, and check that a genuine no-drop still
retries.

Run: python3 tools/watchers/tests/test_self_clear_double_clear.py
"""
import importlib.machinery
import importlib.util
import types
import unittest
from pathlib import Path

REPO = Path(__file__).resolve().parents[3]
SCRIPT = REPO / "tools" / "watchers" / "self-clear"
IDLE = "some output\n❯ \n"

# Exact readings from the 2026-11-10 01:33 self-clear log.
INCIDENT_BEFORE = 965298
INCIDENT_POLLS = [51837, 52036, 78508, 78883, 80152, 80295]
INCIDENT_AFTER_WINDOW = 92367


def load():
    loader = importlib.machinery.SourceFileLoader("sc_dbl", str(SCRIPT))
    spec = importlib.util.spec_from_loader("sc_dbl", loader)
    mod = importlib.util.module_from_spec(spec)
    loader.exec_module(mod)
    return mod


def wire(m, token_readings):
    """Mock everything child_main touches. Returns (injected, logs)."""
    clock = [1000.0]
    m.time = types.SimpleNamespace(
        time=lambda: clock[0],
        sleep=lambda s=0: clock.__setitem__(0, clock[0] + s),
    )
    readings = list(token_readings)

    def get_token_count():
        # Each poll takes ~2.5s in the real log (claude-watch status latency).
        clock[0] += 2.0
        return readings.pop(0) if len(readings) > 1 else readings[0]

    injected, logs = [], []
    m.get_token_count = get_token_count
    m.log = logs.append
    m.run = lambda cmd, *a, **k: (IDLE, 0)
    m.capture_pane_text = lambda pane: "idle"
    m.ensure_main_loop_focus = lambda pane: True
    m.cancel_stray_rewind_picker = lambda pane: False
    m.interrupt_and_wait = lambda pane, timeout=60: True
    m.ensure_no_rewind = lambda pane, stage="": True
    m.wait_for_idle = lambda pane, timeout=15: True
    m.dismiss_feedback_prompt = lambda pane: True
    m._rewind_picker_confirm_once = lambda pane, pane_text=None: (False, False)
    m._stamp_handoff_marker = lambda: None
    m.inject = lambda pane, text, **k: injected.append(text)
    return injected, logs


def args():
    return types.SimpleNamespace(timeout=60, delay=15, no_resume=False)


class ClearTookEffectPredicate(unittest.TestCase):
    def setUp(self):
        self.m = load()

    def test_incident_first_poll_is_a_decisive_drop(self):
        self.assertTrue(self.m._clear_took_effect(INCIDENT_BEFORE, 51837, True))

    def test_drop_counts_even_when_fresh_session_is_busy(self):
        self.assertTrue(self.m._clear_took_effect(INCIDENT_BEFORE, 80295, False))

    def test_no_drop_is_not_success(self):
        self.assertFalse(self.m._clear_took_effect(INCIDENT_BEFORE, 965298, True))
        self.assertFalse(self.m._clear_took_effect(INCIDENT_BEFORE, 970000, True))
        self.assertFalse(self.m._clear_took_effect(INCIDENT_BEFORE, 600000, True))

    def test_zero_reading_needs_idle(self):
        # 0 is also what get_token_count() returns on a status failure.
        self.assertTrue(self.m._clear_took_effect(INCIDENT_BEFORE, 0, True))
        self.assertFalse(self.m._clear_took_effect(INCIDENT_BEFORE, 0, False))

    def test_legacy_floor_still_confirms(self):
        self.assertTrue(self.m._clear_took_effect(40000, 25000, True))


class DoubleClearReplay(unittest.TestCase):
    def test_incident_sequence_confirms_attempt_one_and_never_resends(self):
        m = load()
        readings = [INCIDENT_BEFORE] + INCIDENT_POLLS + [INCIDENT_AFTER_WINDOW]
        injected, logs = wire(m, readings)
        m.child_main("%0", args())

        self.assertEqual(injected.count("/clear"), 1,
                         f"/clear re-sent after a working clear: {injected}")
        self.assertIn("  /clear confirmed (tokens 965298 -> 51837)", logs)
        self.assertFalse(any("FAILED" in l for l in logs), logs)
        self.assertFalse(any("attempt 2/" in l for l in logs), logs)
        # Polling stopped on the first decisive reading.
        self.assertEqual(sum(1 for l in logs if l.startswith("  poll: tokens=")), 1)
        # Confirmed clear -> resume handed off even though the fresh session
        # is already above FRESH_SESSION_MAX_TOKENS.
        self.assertEqual(injected[-1], m.RESUME_PROMPT)
        self.assertFalse(any("SKIP-RESUME" in l for l in logs), logs)

    def test_true_no_drop_still_retries(self):
        m = load()
        # Tokens never drop (e.g. /clear swallowed): every reading ~unchanged.
        injected, logs = wire(m, [INCIDENT_BEFORE, 965500, 966000, 966400])
        m.child_main("%0", args())

        self.assertEqual(injected.count("/clear"), 3, injected)
        self.assertEqual(sum(1 for l in logs if "FAILED" in l), 3, logs)
        self.assertTrue(any("SKIP-RESUME" in l for l in logs), logs)
        self.assertNotIn(m.RESUME_PROMPT, injected)


if __name__ == "__main__":
    unittest.main()
