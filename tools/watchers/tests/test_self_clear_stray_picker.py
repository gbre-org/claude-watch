#!/usr/bin/env python3
"""Regression: self-clear must Escape a Rewind picker that is already open.

Incident 2026-10-19/20: a picker parked on the pane (status bar hidden). Its
`(current)` row reads as an idle prompt, so self-clear would type /clear into it.

Run: python3 tools/watchers/tests/test_self_clear_stray_picker.py
"""
import importlib.machinery
import importlib.util
import unittest
from pathlib import Path

REPO = Path(__file__).resolve().parents[3]
SCRIPT = REPO / "tools" / "watchers" / "self-clear"
FIXTURE = (REPO / "tests" / "fixtures" / "rewind_picker_stuck_after_clear.txt").read_text()


def load():
    loader = importlib.machinery.SourceFileLoader("sc_stray", str(SCRIPT))
    spec = importlib.util.spec_from_loader("sc_stray", loader)
    mod = importlib.util.module_from_spec(spec)
    loader.exec_module(mod)
    return mod


class StrayPicker(unittest.TestCase):
    def test_fixture_is_detected_as_picker(self):
        self.assertTrue(load()._rewind_picker_visible(FIXTURE))

    def test_escape_sent_until_picker_gone_never_enter(self):
        m = load()
        m.time.sleep = lambda *_: None
        m.log = lambda *_: None
        frames = [FIXTURE, FIXTURE, "  some idle pane\n\u276f \n"]
        sent = []

        def fake_run(cmd, *a, **k):
            if cmd[:2] == ["tmux", "capture-pane"]:
                return (frames.pop(0) if frames else "idle\n\u276f \n"), 0
            if cmd[:2] == ["tmux", "send-keys"]:
                sent.append(cmd[-1])
                return "", 0
            return "", 0

        m.run = fake_run
        self.assertTrue(m.cancel_stray_rewind_picker("%0"))
        self.assertEqual(sent, ["Escape", "Escape"])
        self.assertNotIn("Enter", sent)

    def test_no_picker_sends_nothing(self):
        m = load()
        m.time.sleep = lambda *_: None
        m.log = lambda *_: None
        sent = []

        def fake_run(cmd, *a, **k):
            if cmd[:2] == ["tmux", "send-keys"]:
                sent.append(cmd[-1])
            return "idle\n\u276f \n", 0

        m.run = fake_run
        self.assertFalse(m.cancel_stray_rewind_picker("%0"))
        self.assertEqual(sent, [])


if __name__ == "__main__":
    unittest.main()
