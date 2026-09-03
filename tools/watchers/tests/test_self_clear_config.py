#!/usr/bin/env python3
"""Config-only smoke tests for tools/watchers/self-clear.

The full self-clear flow requires a live tmux pane running Claude Code,
which we can't reproduce in unit tests. These tests cover the *portable*
parts that previously had hardcoded host-specific paths:

  * Default log path falls under XDG_STATE_HOME (or ~/.local/state) when
    no env var is set.
  * Default lock path falls under XDG_RUNTIME_DIR when set, /tmp otherwise.
  * Default resume prompt is the built-in placeholder when no env var is set.
  * `$CLAUDE_SELF_CLEAR_LOG`, `$CLAUDE_SELF_CLEAR_LOCK`, and
    `$CLAUDE_SELF_CLEAR_RESUME_PROMPT` env vars override defaults.
  * `--help` runs cleanly (catches argparse-level wiring bugs).

Run:
    python3 tools/watchers/tests/test_self_clear_config.py
"""

from __future__ import annotations

import importlib.util
import importlib.machinery
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

REPO = Path(__file__).resolve().parents[3]
SCRIPT = REPO / "tools" / "watchers" / "self-clear"


def _import_self_clear(env_overrides=None):
    """Import the self-clear script as a module under controlled env.

    The script touches sys.path / runs no top-level work besides defining
    helpers, so this is safe.
    """
    saved_env = {}
    for k in (
        "CLAUDE_SELF_CLEAR_LOG",
        "CLAUDE_SELF_CLEAR_LOCK",
        "CLAUDE_SELF_CLEAR_RESUME_PROMPT",
        "XDG_STATE_HOME",
        "XDG_RUNTIME_DIR",
    ):
        saved_env[k] = os.environ.pop(k, None)
    if env_overrides:
        for k, v in env_overrides.items():
            if v is None:
                os.environ.pop(k, None)
            else:
                os.environ[k] = v
    try:
        # The script has no .py extension, so we have to give the loader
        # an explicit SourceFileLoader for it to be picked up.
        loader = importlib.machinery.SourceFileLoader(
            "self_clear_under_test", str(SCRIPT)
        )
        spec = importlib.util.spec_from_loader(
            "self_clear_under_test", loader
        )
        mod = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(mod)
        return mod
    finally:
        for k, v in saved_env.items():
            if v is None:
                os.environ.pop(k, None)
            else:
                os.environ[k] = v


class DefaultsTest(unittest.TestCase):
    """Verify the module-level LOG_FILE / LOCKFILE / RESUME_PROMPT constants
    bind correctly under different env-var combinations.

    The defaults are computed at import time, so each test re-imports the
    module under the env it wants — the helper restores env on exit.
    """

    def test_log_default_xdg_state_home(self):
        with tempfile.TemporaryDirectory() as td:
            mod = _import_self_clear({"XDG_STATE_HOME": td})
            self.assertTrue(mod.LOG_FILE.startswith(td), mod.LOG_FILE)
            self.assertTrue(
                mod.LOG_FILE.endswith("/claude-watch/self-clear.log"),
                mod.LOG_FILE,
            )

    def test_log_default_var_log_fallback(self):
        mod = _import_self_clear({"XDG_STATE_HOME": None})
        self.assertEqual(mod.LOG_FILE, "/var/log/claude-watch/self-clear.log")

    def test_log_env_override_wins(self):
        mod = _import_self_clear({"CLAUDE_SELF_CLEAR_LOG": "/somewhere/explicit.log"})
        self.assertEqual(mod.LOG_FILE, "/somewhere/explicit.log")

    def test_lock_default_xdg_runtime_dir(self):
        with tempfile.TemporaryDirectory() as td:
            mod = _import_self_clear({"XDG_RUNTIME_DIR": td})
            self.assertEqual(mod.LOCKFILE, f"{td}/claude-self-clear.lock")

    def test_lock_default_var_run_fallback(self):
        mod = _import_self_clear({"XDG_RUNTIME_DIR": None})
        self.assertEqual(mod.LOCKFILE, "/var/run/claude/claude-self-clear.lock")

    def test_lock_env_override_wins(self):
        mod = _import_self_clear({"CLAUDE_SELF_CLEAR_LOCK": "/run/x.lock"})
        self.assertEqual(mod.LOCKFILE, "/run/x.lock")

    def test_resume_prompt_default(self):
        mod = _import_self_clear()
        prompt = mod.RESUME_PROMPT
        self.assertIn("[SELF-CLEAR-RESUME]", prompt)
        # The portable default must NOT bake in a host-specific path.
        self.assertNotIn("hndrewaall", prompt)
        self.assertNotIn("/.claude/projects/", prompt)

    def test_resume_prompt_env_override(self):
        mod = _import_self_clear({"CLAUDE_SELF_CLEAR_RESUME_PROMPT": "[CUSTOM] go"})
        self.assertEqual(mod.RESUME_PROMPT, "[CUSTOM] go")


class HelpTest(unittest.TestCase):
    def test_help_runs(self):
        # --help exits 0 even though main never gets a chance to fork
        proc = subprocess.run(
            [sys.executable, str(SCRIPT), "--help"],
            capture_output=True,
            text=True,
            timeout=5,
        )
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertIn("--no-resume", proc.stdout)
        self.assertIn("--log-file", proc.stdout)
        self.assertIn("--lock-file", proc.stdout)
        self.assertIn("--resume-prompt", proc.stdout)


class FleetViewDetectionTest(unittest.TestCase):
    """Pure-function tests for the FleetView-awareness helpers added with the
    FleetView-aware navigation fix. No live tmux needed."""

    def setUp(self):
        self.mod = _import_self_clear()

    def test_agent_view_selected_agent_row(self):
        pane = "\n".join([
            "  ● main",
            "❯ ◯ general-purpose  running a search",
            "  ↑/↓ to select · Enter to view",
        ])
        self.assertTrue(self.mod._fleetview_agent_view_visible(pane))
        self.assertFalse(self.mod._main_loop_prompt_visible(pane))

    def test_agent_view_footer_hint(self):
        pane = "some output\n  ← for agents\n"
        self.assertTrue(self.mod._fleetview_agent_view_visible(pane))

    def test_main_loop_prompt_detected(self):
        pane = "\n".join([
            "⏺ done",
            "❯ ",
            "  bypass permissions on · 123k tokens",
        ])
        self.assertTrue(self.mod._main_loop_prompt_visible(pane))
        self.assertFalse(self.mod._fleetview_agent_view_visible(pane))

    def test_numbered_option_is_not_main_prompt(self):
        pane = "Do you want to proceed?\n❯ 1. Yes\n  2. No\n"
        self.assertFalse(self.mod._main_loop_prompt_visible(pane))

    def test_read_focus_main_keys_from_config(self):
        with tempfile.TemporaryDirectory() as td:
            cfg = Path(td) / "config.toml"
            cfg.write_text('[tmux]\nfocus_main_keys = ["Right", "Left"]\n')
            saved = os.environ.get("CLAUDE_WATCH_CONFIG")
            os.environ["CLAUDE_WATCH_CONFIG"] = str(cfg)
            try:
                self.assertEqual(self.mod._read_focus_main_keys(), ["Right", "Left"])
            finally:
                if saved is None:
                    os.environ.pop("CLAUDE_WATCH_CONFIG", None)
                else:
                    os.environ["CLAUDE_WATCH_CONFIG"] = saved

    def test_read_focus_main_keys_default_empty(self):
        with tempfile.TemporaryDirectory() as td:
            cfg = Path(td) / "config.toml"
            cfg.write_text('[tmux]\nfocus_main_keys = []\n')
            saved = os.environ.get("CLAUDE_WATCH_CONFIG")
            os.environ["CLAUDE_WATCH_CONFIG"] = str(cfg)
            try:
                self.assertEqual(self.mod._read_focus_main_keys(), [])
            finally:
                if saved is None:
                    os.environ.pop("CLAUDE_WATCH_CONFIG", None)
                else:
                    os.environ["CLAUDE_WATCH_CONFIG"] = saved


class InjectCommandTest(unittest.TestCase):
    """The argv `inject()` hands to `claude-watch inject`.

    `--escape` is the flag that decides whether an Escape blast is fired into
    the pane. Since 2026-08-18 the subcommand does NOT escape unless asked, and
    this wrapper mirrors that default rather than re-arming it — self-login
    shares this helper to drive a pane that may be showing a modal, where an
    Escape cancels the login. Both directions are pinned here because a silent
    flip either way is invisible until it costs somebody a cleared context or a
    cancelled login.
    """

    def setUp(self):
        self.mod = _import_self_clear()
        self.calls = []
        self.mod.run = lambda cmd, timeout=None: (self.calls.append(cmd), ("", 0))[1]
        self.mod.log = lambda *a, **k: None
        self.mod.capture_pane_text = lambda pane: ""

    def _argv(self, **kwargs):
        self.calls.clear()
        self.mod.inject("sess:0.0", "payload", **kwargs)
        self.assertEqual(len(self.calls), 1, self.calls)
        return self.calls[0]

    def test_default_does_not_escape(self):
        argv = self._argv()
        self.assertNotIn("--escape", argv)
        self.assertNotIn("--cancel", argv)

    def test_escape_true_passes_the_flag(self):
        self.assertIn("--escape", self._argv(escape=True))

    def test_slash_command_is_independent_of_escape(self):
        argv = self._argv(slash_command=True)
        self.assertIn("--slash-command", argv)
        self.assertNotIn("--escape", argv)
        argv = self._argv(slash_command=True, escape=True)
        self.assertIn("--slash-command", argv)
        self.assertIn("--escape", argv)

    def test_payload_and_pane_are_passed_through(self):
        argv = self._argv()
        self.assertEqual(argv[:2], ["claude-watch", "inject"])
        self.assertIn("--pane", argv)
        self.assertEqual(argv[argv.index("--pane") + 1], "sess:0.0")
        self.assertEqual(argv[argv.index("--submit") + 1], "payload")


if __name__ == "__main__":
    unittest.main(verbosity=2)
