#!/usr/bin/env python3
"""Tests for self-login's out-of-band operator channel.

The login modal swallows the session's keystrokes, so the session cannot be
the thing that relays the OAuth URL or accepts the code. These tests pin the
pieces that make the flow independent of it:

  * the URL is pushed / chatted to the operator by self-login itself;
  * a strict `login code <CODE>` chat message from the configured sender is
    the only chat input that can ever be typed into the dialog;
  * no code within the timeout cancels the dialog (pane back to a prompt);
  * failures are loud.

Every test fakes the pane, the chat and the clock. Nothing here touches tmux,
so it is safe on a host with a live Claude Code session.

Run:
    python3 tools/watchers/tests/test_self_login_oob.py
"""

from __future__ import annotations

import os
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from test_self_login import (  # noqa: E402
    PANE_LIVE_TUI,
    PANE_METHOD_PICKER,
    PANE_URL_SCREEN,
    _import_self_login,
)


def _msg(mid, body, sender="op", ts=1000.0):
    return {"id": mid, "sender": sender, "body": body, "ts": ts}


class TestCodeMessageParsing(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.m = _import_self_login()

    def test_exact_form_accepted(self):
        self.assertEqual(self.m.parse_code_message("login code abc123-_.~#st"), "abc123-_.~#st")
        self.assertEqual(self.m.parse_code_message("  Login   Code  XYZ789  \n"), "XYZ789")

    def test_anything_else_rejected(self):
        for body in (
            "",
            "login code",
            "please login code abc123",
            "login code abc123 thanks",
            "login code abc 123",
            "> login code abc123",
            "login code abc123\nmore",
            "login code ab;cd",
            "login code $(id)",
            None,
            123,
        ):
            self.assertEqual(self.m.parse_code_message(body), "", repr(body))


class TestPickCodeMessage(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.m = _import_self_login()

    def pick(self, msgs, sender="op", since=900.0, seen=()):
        return self.m.pick_code_message(msgs, sender, since, set(seen))

    def test_happy_path(self):
        self.assertEqual(self.pick([_msg(7, "login code abc123")]), (7, "abc123"))

    def test_sender_match_is_case_insensitive(self):
        self.assertEqual(self.pick([_msg(7, "login code abc123", sender="OP")]), (7, "abc123"))

    def test_wrong_sender_never_honoured(self):
        self.assertEqual(self.pick([_msg(7, "login code abc123", sender="mallory")]), (None, ""))

    def test_no_configured_sender_honours_nobody(self):
        self.assertEqual(self.pick([_msg(7, "login code abc123")], sender=""), (None, ""))

    def test_stale_message_before_this_login_ignored(self):
        self.assertEqual(self.pick([_msg(7, "login code abc123", ts=800.0)]), (None, ""))

    def test_already_consumed_message_ignored(self):
        self.assertEqual(self.pick([_msg(7, "login code abc123")], seen=[7]), (None, ""))

    def test_oldest_honourable_wins_and_junk_is_skipped(self):
        got = self.pick([
            _msg(9, "login code later", ts=1002.0),
            _msg(8, "hello there", ts=1000.5),
            _msg(7, "login code first", ts=1001.0),
        ])
        self.assertEqual(got, (7, "first"))

    def test_malformed_rows_do_not_crash(self):
        self.assertEqual(
            self.pick(["x", {"id": 1}, {"id": 2, "sender": "op", "ts": "bad"}]), (None, ""))
        self.assertEqual(self.m.pick_code_message("not a list", "op", 0, set()), (None, ""))


class TestSettings(unittest.TestCase):
    def test_env_overrides_file(self):
        with tempfile.TemporaryDirectory() as td:
            cfgdir = Path(td) / "claude-watch"
            cfgdir.mkdir()
            (cfgdir / "self-login.toml").write_text(
                'chat_sender = "fromfile"\ncode_timeout_seconds = 300\n')
            m = _import_self_login()
            env = {"XDG_CONFIG_HOME": td,
                   "CLAUDE_SELF_LOGIN_CHAT_SENDER": "fromenv",
                   "CLAUDE_SELF_LOGIN_PUSH_CMD": "mypush --flag"}
            os.environ.update(env)
            try:
                cfg = m.load_settings()
            finally:
                for k in env:
                    os.environ.pop(k, None)
            self.assertEqual(cfg["chat_sender"], "fromenv")
            self.assertEqual(cfg["push_cmd"], "mypush --flag")
            self.assertEqual(m.code_timeout(cfg), 300)

    def test_timeout_default_and_override(self):
        m = _import_self_login()
        self.assertEqual(m.code_timeout({}), 900)
        self.assertEqual(m.code_timeout({"code_timeout_seconds": "120"}), 120)
        self.assertEqual(m.code_timeout({"code_timeout_seconds": "junk"}), 900)
        self.assertEqual(m.code_timeout({}, override=45), 45)


class TestOperatorDelivery(unittest.TestCase):
    """The URL must reach the operator by a path that does not touch the session."""

    def _stub(self, td, name, body):
        path = Path(td) / name
        path.write_text("#!/bin/sh\n" + body)
        path.chmod(0o755)
        return str(path)

    def test_push_and_chat_receive_the_url_without_claude_event(self):
        with tempfile.TemporaryDirectory() as td:
            pushed = Path(td) / "pushed"
            chatted = Path(td) / "chatted"
            push = self._stub(td, "push", f'echo "$*" >> {pushed}\n')
            chat = self._stub(td, "chat", f'echo "$*" >> {chatted}\n')
            env = {"CLAUDE_SELF_LOGIN_PUSH_CMD": push,
                   "CLAUDE_SELF_LOGIN_CHAT_SEND_CMD": chat,
                   "CLAUDE_SELF_LOGIN_CHAT_POLL_CMD": "true",
                   "CLAUDE_SELF_LOGIN_CHAT_SENDER": "op",
                   "XDG_CONFIG_HOME": td}
            m = _import_self_login({
                "CLAUDE_SELF_LOGIN_STATE": f"{td}/state.json",
                "CLAUDE_SELF_LOGIN_LOG": f"{td}/log",
            })
            os.environ.update(env)
            m.shutil.which = lambda name: None  # no claude-event on PATH at all
            try:
                m.publish_url("%0", "https://example.test/authorize?x=1")
            finally:
                for k in env:
                    os.environ.pop(k, None)
            for f in (pushed, chatted):
                text = f.read_text()
                self.assertIn("https://example.test/authorize?x=1", text)
                self.assertIn("login code <CODE>", text)
            self.assertIn("-p high", pushed.read_text())
            self.assertEqual(m.read_state()["status"], "awaiting-code")

    def test_failing_push_does_not_skip_chat(self):
        with tempfile.TemporaryDirectory() as td:
            chatted = Path(td) / "chatted"
            chat = self._stub(td, "chat", f'echo "$*" >> {chatted}\n')
            m = _import_self_login({"CLAUDE_SELF_LOGIN_LOG": f"{td}/log"})
            cfg = {"push_cmd": self._stub(td, "badpush", "exit 3\n"), "chat_send_cmd": chat}
            m.tell_operator(cfg, "hello")
            self.assertIn("hello", chatted.read_text())

    def test_no_channels_configured_is_silent_not_a_crash(self):
        m = _import_self_login()
        m.tell_operator({}, "hello")


class _FakeClock:
    def __init__(self):
        self.now = 1000.0

    def time(self):
        return self.now

    def sleep(self, secs):
        self.now += max(secs, 0.5)


class TestWatcher(unittest.TestCase):
    """run_watch with the pane, chat and clock faked out. Never touches tmux."""

    def _setup(self, td, frames, polls=None, chat=True):
        m = _import_self_login({
            "CLAUDE_SELF_LOGIN_STATE": f"{td}/state.json",
            "CLAUDE_SELF_LOGIN_LOG": f"{td}/log",
            "CLAUDE_SELF_LOGIN_LOCK": f"{td}/lock",
        })
        clock = _FakeClock()
        m.time = clock
        frames = list(frames)
        m.capture = lambda pane: frames.pop(0) if len(frames) > 1 else frames[0]
        sent = []
        m.tell_operator = lambda cfg, msg, priority="high": sent.append(msg)
        m.emit_event = lambda *a, **k: None
        polls = list(polls or [])
        m.poll_chat = lambda cfg: polls.pop(0) if polls else []
        cfg = {"chat_sender": "op", "chat_poll_cmd": "x"} if chat else {}
        m.load_settings = lambda: dict(cfg)
        return m, clock, sent

    def test_timeout_cancels_dialog_and_tells_operator(self):
        with tempfile.TemporaryDirectory() as td:
            m, clock, sent = self._setup(td, [PANE_URL_SCREEN], chat=False)
            cancelled = []
            m.do_cancel = lambda pane: (cancelled.append(pane) or
                                        {"ok": True, "pane": pane, "cancelled": True})
            rc = m.run_watch("%9", 1000.0, timeout_override=60)
            self.assertEqual(rc, 0)
            self.assertEqual(cancelled, ["%9"])
            self.assertGreaterEqual(clock.now, 1060.0)
            self.assertTrue(any("timed out" in s and "cancelled" in s for s in sent), sent)
            self.assertEqual(m.read_state()["status"], "cancelled")

    def test_timeout_that_cannot_close_the_dialog_is_loud(self):
        with tempfile.TemporaryDirectory() as td:
            m, clock, sent = self._setup(td, [PANE_URL_SCREEN], chat=False)
            m.do_cancel = lambda pane: {"ok": False, "pane": pane, "reason": "stuck",
                                        "exit_code": 4}
            self.assertEqual(m.run_watch("%9", 1000.0, timeout_override=30), 4)
            self.assertTrue(any("would NOT close" in s for s in sent), sent)

    def test_code_from_chat_is_typed_and_reported(self):
        with tempfile.TemporaryDirectory() as td:
            m, clock, sent = self._setup(
                td, [PANE_URL_SCREEN],
                polls=[[], [_msg(5, "login code good-code_1", ts=1001.0)]])
            typed = []
            m.do_code = lambda pane, code, ns: (typed.append(code) or {"ok": True, "pane": pane})
            self.assertEqual(m.run_watch("%9", 1000.0, timeout_override=600), 0)
            self.assertEqual(typed, ["good-code_1"])
            self.assertTrue(any("completed" in s for s in sent), sent)
            self.assertEqual(m.read_state()["status"], "logged-in")

    def test_rejected_code_keeps_waiting_then_cancels(self):
        with tempfile.TemporaryDirectory() as td:
            m, clock, sent = self._setup(
                td, [PANE_URL_SCREEN],
                polls=[[_msg(5, "login code wrong-code", ts=1001.0)]])
            m.do_code = lambda pane, code, ns: {"ok": False, "pane": pane,
                                                "reason": "OAuth error", "exit_code": 4}
            m.do_cancel = lambda pane: {"ok": True, "pane": pane, "cancelled": True}
            self.assertEqual(m.run_watch("%9", 1000.0, timeout_override=60), 0)
            self.assertTrue(any("not accepted" in s for s in sent), sent)
            self.assertTrue(any("timed out" in s for s in sent), sent)

    def test_code_from_wrong_sender_is_never_typed(self):
        with tempfile.TemporaryDirectory() as td:
            bad = [[_msg(5, "login code evil", sender="mallory", ts=1001.0)]] * 100
            m, clock, sent = self._setup(td, [PANE_URL_SCREEN], polls=bad)
            m.do_code = lambda *a: self.fail("an unauthorised code was typed")
            m.do_cancel = lambda pane: {"ok": True, "pane": pane, "cancelled": True}
            self.assertEqual(m.run_watch("%9", 1000.0, timeout_override=60), 0)

    def test_dialog_closed_elsewhere_ends_the_watch_quietly(self):
        with tempfile.TemporaryDirectory() as td:
            m, clock, sent = self._setup(td, [PANE_URL_SCREEN, PANE_LIVE_TUI], chat=False)
            m.do_cancel = lambda pane: self.fail("cancel must not run once the dialog is gone")
            self.assertEqual(m.run_watch("%9", 1000.0, timeout_override=600), 0)
            self.assertEqual(sent, [])


class TestCancelReturnsPaneToPrompt(unittest.TestCase):
    """do_cancel against captured pane text: Escape only while a dialog is up,
    end on the live prompt, and fail loudly when the dialog will not close."""

    def _run(self, frames):
        with tempfile.TemporaryDirectory() as td:
            m = _import_self_login({"CLAUDE_SELF_LOGIN_LOG": f"{td}/log"})
            frames = list(frames)
            m.capture = lambda pane: frames.pop(0) if len(frames) > 1 else frames[0]
            keys = []
            m.sc.run = lambda cmd, **kw: (keys.append(cmd[-1]) if "send-keys" in cmd else None, ("", 0))[1]
            m.time = _FakeClock()
            return m.do_cancel("%0"), keys

    def test_picker_then_url_screen_then_prompt(self):
        result, keys = self._run([PANE_METHOD_PICKER, PANE_METHOD_PICKER,
                                  PANE_URL_SCREEN, PANE_LIVE_TUI])
        self.assertTrue(result["ok"])
        self.assertTrue(result["cancelled"])
        self.assertEqual(set(keys), {"Escape"})
        self.assertGreaterEqual(len(keys), 2)

    def test_no_dialog_means_no_keys_at_all(self):
        result, keys = self._run([PANE_LIVE_TUI])
        self.assertTrue(result["ok"])
        self.assertFalse(result["cancelled"])
        self.assertEqual(keys, [])

    def test_stuck_dialog_is_a_loud_failure_not_a_success(self):
        result, keys = self._run([PANE_URL_SCREEN])
        self.assertFalse(result["ok"])
        self.assertEqual(result["exit_code"], 4)
        self.assertEqual(set(keys), {"Escape"}, keys)
        self.assertLessEqual(len(keys), 4, keys)


if __name__ == "__main__":
    unittest.main(verbosity=2)
