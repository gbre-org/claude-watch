#!/usr/bin/env python3
"""Unit tests for tools/watchers/self-login.

Covers the parts that do NOT need a live Claude Code session: the pane-text
predicates that decide which login screen we are looking at, the authorization
code validator, config-path defaults, and argparse wiring. The parts that DO
need a real terminal are exercised separately by
`tools/watchers/tests/test_self_login_tmux.sh`, which drives a synthetic login
screen inside a genuine tmux pane.

Run:
    python3 tools/watchers/tests/test_self_login.py
"""

from __future__ import annotations

import importlib.machinery
import importlib.util
import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

REPO = Path(__file__).resolve().parents[3]
SCRIPT = REPO / "tools" / "watchers" / "self-login"


def _import_self_login(env_overrides=None):
    """Import the self-login script as a module under controlled env.

    The script defines helpers and imports its self-clear sibling at import
    time; it runs no other top-level work, so this is safe.
    """
    saved = {}
    for k in (
        "CLAUDE_SELF_LOGIN_LOG",
        "CLAUDE_SELF_LOGIN_LOCK",
        "CLAUDE_SELF_LOGIN_STATE",
        "CLAUDE_SELF_LOGIN_NOTIFY_CMD",
        "XDG_STATE_HOME",
        "XDG_RUNTIME_DIR",
    ):
        saved[k] = os.environ.pop(k, None)
    if env_overrides:
        os.environ.update(env_overrides)
    try:
        loader = importlib.machinery.SourceFileLoader("self_login_under_test", str(SCRIPT))
        spec = importlib.util.spec_from_loader(loader.name, loader)
        mod = importlib.util.module_from_spec(spec)
        loader.exec_module(mod)
        return mod
    finally:
        for k, v in saved.items():
            os.environ.pop(k, None)
            if v is not None:
                os.environ[k] = v


# Pane captures modelled on what the shipped Claude Code build actually
# renders at each step of `/login`.
PANE_METHOD_PICKER = """\
 Claude Code can be used with your Claude subscription or billed based on
 API usage through your Console account.

 Select login method:
 ❯ ◯ Claude account with subscription · Pro, Max, Team, or Enterprise
   ◯ Anthropic Console account · API usage billing
   ◯ 3rd-party platform · Amazon Bedrock, Microsoft Foundry, or Vertex AI
"""

PANE_URL_SCREEN = """\
 Browser didn't open? Use the url below to sign in

 https://claude.com/cai/oauth/authorize?code=true&client_id=abc123&state=xyz

 Paste code here if prompted >
"""

PANE_SUCCESS = """\
 Login successful. Press Esc to go back to login options.
"""

PANE_OAUTH_ERROR = """\
 OAuth error: Token exchange failed
 Press Enter to retry.
"""

PANE_LIVE_TUI = """\
● Ran a tool

❯
  ⏵⏵ bypass permissions on · 41,204 tokens · 2 bashes
"""


class TestPanePredicates(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.m = _import_self_login()

    def test_method_picker_detected(self):
        self.assertTrue(self.m.login_menu_visible(PANE_METHOD_PICKER))
        self.assertFalse(self.m.login_menu_visible(PANE_URL_SCREEN))
        self.assertFalse(self.m.login_menu_visible(PANE_LIVE_TUI))

    def test_code_prompt_detected(self):
        self.assertTrue(self.m.code_prompt_visible(PANE_URL_SCREEN))
        self.assertFalse(self.m.code_prompt_visible(PANE_LIVE_TUI))

    def test_login_dialog_covers_both_screens(self):
        # Both the picker and the URL screen count as "the login UI is up",
        # because both mean an inject would land in a modal, not the prompt.
        self.assertTrue(self.m.login_dialog_visible(PANE_METHOD_PICKER))
        self.assertTrue(self.m.login_dialog_visible(PANE_URL_SCREEN))
        self.assertFalse(self.m.login_dialog_visible(PANE_LIVE_TUI))

    def test_success_and_failure_are_distinct(self):
        self.assertTrue(self.m.login_succeeded(PANE_SUCCESS))
        self.assertFalse(self.m.login_failed(PANE_SUCCESS))
        self.assertTrue(self.m.login_failed(PANE_OAUTH_ERROR))
        self.assertFalse(self.m.login_succeeded(PANE_OAUTH_ERROR))

    def test_tui_detected_only_on_the_live_session(self):
        self.assertTrue(self.m.tui_visible(PANE_LIVE_TUI))
        self.assertFalse(self.m.tui_visible(PANE_METHOD_PICKER))
        self.assertFalse(self.m.tui_visible(PANE_SUCCESS))

    def test_login_method_order_matches_the_pickers_order(self):
        # The Down-press count is derived from this list's index, so its order
        # is load-bearing: getting it wrong selects the wrong account type.
        self.assertEqual(
            self.m.LOGIN_METHOD_ORDER, ["claudeai", "console", "platform"]
        )


class TestCodeValidation(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.m = _import_self_login()

    def test_plausible_codes_accepted(self):
        for code in (
            "abcDEF123-_.~",
            "ac_01H8XABCDEF#state-value",
            "a" * 200,
        ):
            self.assertEqual(self.m.validate_code(code), "", code)

    def test_empty_rejected(self):
        self.assertNotEqual(self.m.validate_code(""), "")

    def test_whitespace_rejected(self):
        # A space or newline in a code would be typed into the modal as extra
        # keystrokes — Enter mid-code submits a truncated value.
        self.assertIn("whitespace", self.m.validate_code("abc def"))
        self.assertIn("whitespace", self.m.validate_code("abc\ndef"))
        self.assertIn("whitespace", self.m.validate_code("abc\tdef"))

    def test_absurd_length_rejected(self):
        self.assertIn("long", self.m.validate_code("a" * 513))

    def test_shell_metacharacters_rejected(self):
        for bad in ("abc;rm -rf /", "abc`id`", "abc$(id)", "abc|def", 'abc"def'):
            self.assertNotEqual(self.m.validate_code(bad), "", bad)


class TestConfigDefaults(unittest.TestCase):
    def test_state_and_log_default_under_xdg_state_home(self):
        with tempfile.TemporaryDirectory() as td:
            m = _import_self_login({"XDG_STATE_HOME": td})
            self.assertEqual(m.LOG_FILE, f"{td}/claude-watch/self-login.log")
            self.assertEqual(m.STATE_FILE, f"{td}/claude-watch/self-login.json")

    def test_lock_default_under_xdg_runtime_dir(self):
        with tempfile.TemporaryDirectory() as td:
            m = _import_self_login({"XDG_RUNTIME_DIR": td})
            self.assertEqual(m.LOCKFILE, f"{td}/claude-self-login.lock")

    def test_env_overrides_win(self):
        m = _import_self_login({
            "CLAUDE_SELF_LOGIN_LOG": "/tmp/sl.log",
            "CLAUDE_SELF_LOGIN_STATE": "/tmp/sl.json",
            "CLAUDE_SELF_LOGIN_LOCK": "/tmp/sl.lock",
        })
        self.assertEqual(m.LOG_FILE, "/tmp/sl.log")
        self.assertEqual(m.STATE_FILE, "/tmp/sl.json")
        self.assertEqual(m.LOCKFILE, "/tmp/sl.lock")

    def test_no_hardcoded_host_paths(self):
        # This repo is public: a default must never bake in one operator's
        # home directory.
        src = SCRIPT.resolve().read_text()
        self.assertNotIn("/home/", src)


class TestStateFile(unittest.TestCase):
    def test_write_then_read_round_trips(self):
        with tempfile.TemporaryDirectory() as td:
            m = _import_self_login({"CLAUDE_SELF_LOGIN_STATE": f"{td}/state.json"})
            m.write_state({"status": "awaiting-code", "url": "https://example.test/x"})
            got = m.read_state()
            self.assertEqual(got["status"], "awaiting-code")
            self.assertEqual(got["url"], "https://example.test/x")
            self.assertIn("updated_at", got)

    def test_read_state_of_missing_file_is_empty(self):
        with tempfile.TemporaryDirectory() as td:
            m = _import_self_login({"CLAUDE_SELF_LOGIN_STATE": f"{td}/nope.json"})
            self.assertEqual(m.read_state(), {})


class TestCli(unittest.TestCase):
    def _run(self, *args):
        return subprocess.run(
            [sys.executable, str(SCRIPT), *args],
            capture_output=True, text=True, timeout=60,
        )

    def test_help_runs(self):
        r = self._run("--help")
        self.assertEqual(r.returncode, 0, r.stderr)
        for sub in ("start", "code", "url", "status"):
            self.assertIn(sub, r.stdout)

    def test_subcommand_help_runs(self):
        for sub in ("start", "code", "url", "status"):
            r = self._run(sub, "--help")
            self.assertEqual(r.returncode, 0, f"{sub}: {r.stderr}")

    def test_status_on_missing_state_prints_empty_json(self):
        with tempfile.TemporaryDirectory() as td:
            env = dict(os.environ, CLAUDE_SELF_LOGIN_STATE=f"{td}/nope.json")
            r = subprocess.run(
                [sys.executable, str(SCRIPT), "status"],
                capture_output=True, text=True, timeout=60, env=env,
            )
            self.assertEqual(r.returncode, 0, r.stderr)
            self.assertEqual(json.loads(r.stdout), {})

    def test_bad_pane_fails_loudly_rather_than_reporting_success(self):
        # A pane that cannot be captured must never look like "no URL, fine".
        with tempfile.TemporaryDirectory() as td:
            env = dict(
                os.environ,
                CLAUDE_SELF_LOGIN_STATE=f"{td}/state.json",
                CLAUDE_SELF_LOGIN_LOG=f"{td}/self-login.log",
            )
            r = subprocess.run(
                [sys.executable, str(SCRIPT), "--pane",
                 "cw-self-login-nonexistent:0.0", "url"],
                capture_output=True, text=True, timeout=90, env=env,
            )
            self.assertEqual(r.returncode, 4, r.stdout + r.stderr)
            self.assertIn("ERROR", r.stderr)


if __name__ == "__main__":
    unittest.main(verbosity=2)
