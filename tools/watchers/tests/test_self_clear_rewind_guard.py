#!/usr/bin/env python3
"""self-clear must never leave/inject into a Rewind dialog, and must not
chain Escapes (double-Esc opens Rewind on an idle prompt).

Run: python3 tools/watchers/tests/test_self_clear_rewind_guard.py
"""
import importlib.machinery
import importlib.util
import unittest
from pathlib import Path

REPO = Path(__file__).resolve().parents[3]
SCRIPT = REPO / "tools" / "watchers" / "self-clear"
REWIND = ("Rewind\nRestore the conversation to the point before...\n"
          "❯ (current)\nEnter to continue · Esc to cancel\n")
IDLE = "some output\n❯ \n"
BUSY = "✶ Thinking...\n"


def load():
    loader = importlib.machinery.SourceFileLoader("sc_rw", str(SCRIPT))
    spec = importlib.util.spec_from_loader("sc_rw", loader)
    mod = importlib.util.module_from_spec(spec)
    loader.exec_module(mod)
    mod.log = lambda *_: None
    return mod


def wire(m, frames, clock=None):
    """Mock the capture/send-keys layer. Returns the list of sent keys."""
    sent = []
    t = [1000.0]
    m.time.sleep = lambda s=0: t.__setitem__(0, t[0] + s)
    m.time.time = lambda: t[0]
    m._last_escape_at[0] = 0.0

    def fake_run(cmd, *a, **k):
        if cmd[:2] == ["tmux", "capture-pane"]:
            return (frames.pop(0) if len(frames) > 1 else frames[0]), 0
        if cmd[:2] == ["tmux", "send-keys"]:
            sent.append((t[0], cmd[-1]))
        return "", 0

    m.run = fake_run
    m._return_focus_to_main = lambda pane: None
    return sent


class RewindGuard(unittest.TestCase):
    def test_clean_pane_sends_nothing(self):
        m = load()
        sent = wire(m, [IDLE])
        self.assertTrue(m.ensure_no_rewind("%0"))
        self.assertEqual(sent, [])

    def test_rewind_dismissed_with_single_escape_then_clean(self):
        m = load()
        sent = wire(m, [REWIND, IDLE])
        self.assertTrue(m.ensure_no_rewind("%0"))
        self.assertEqual([k for _, k in sent], ["Escape"])

    def test_stuck_rewind_bounded_and_escapes_spaced(self):
        m = load()
        sent = wire(m, [REWIND])
        self.assertFalse(m.ensure_no_rewind("%0", max_tries=3))
        self.assertEqual(len(sent), 3)
        times = [t for t, _ in sent]
        for a, b in zip(times, times[1:]):
            self.assertGreaterEqual(b - a, m.ESC_MIN_GAP)

    def test_interrupt_does_not_escape_idle_prompt(self):
        m = load()
        sent = wire(m, [IDLE])
        self.assertTrue(m.interrupt_and_wait("%0", timeout=5))
        self.assertEqual(sent, [])

    def test_interrupt_escapes_spaced_and_dismisses_rewind(self):
        m = load()
        sent = wire(m, [BUSY, REWIND, IDLE])
        self.assertTrue(m.interrupt_and_wait("%0", timeout=30))
        keys = [k for _, k in sent]
        self.assertTrue(keys and set(keys) == {"Escape"})
        times = [t for t, _ in sent]
        for a, b in zip(times, times[1:]):
            self.assertGreaterEqual(b - a, m.ESC_MIN_GAP)


if __name__ == "__main__":
    unittest.main()
