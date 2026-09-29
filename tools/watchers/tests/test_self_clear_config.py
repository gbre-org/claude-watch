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


class RewindPickerTest(unittest.TestCase):
    """Pure-function tests for the v2.1.283+ `/clear` Rewind-picker detection
    and confirming-Enter guard. No live tmux needed.

    Since v2.1.283, `/clear` opens an interactive picker (/resume <id>
    (previous), /clear, (current)) instead of clearing directly. self-clear
    must detect it and confirm it with Enter -- but ONLY when the cursor
    sits on the safe `(current)` entry, never `(previous)` (which would fork
    the conversation via /resume instead of clearing it)."""

    def setUp(self):
        self.mod = _import_self_clear()

    def test_picker_visible_with_footer(self):
        pane = "\n".join([
            "  /resume 8f2a1c (previous)",
            "  /clear",
            "❯ (current)",
            "",
            "Enter to continue · Esc to cancel",
        ])
        self.assertTrue(self.mod._rewind_picker_visible(pane))

    def test_picker_not_visible_pre_2_1_283(self):
        # Old direct-clear behavior: no picker footer at all.
        pane = "\n".join([
            "❯ ",
            "  bypass permissions on · 12k tokens",
        ])
        self.assertFalse(self.mod._rewind_picker_visible(pane))

    def test_default_selection_is_current(self):
        pane = "\n".join([
            "  /resume 8f2a1c (previous)",
            "  /clear",
            "❯ (current)",
            "",
            "Enter to continue · Esc to cancel",
        ])
        self.assertTrue(self.mod._rewind_picker_default_is_current(pane))

    def test_cursor_on_previous_is_not_current(self):
        # Guard case: cursor moved off the default -- must NOT confirm.
        pane = "\n".join([
            "❯ /resume 8f2a1c (previous)",
            "  /clear",
            "  (current)",
            "",
            "Enter to continue · Esc to cancel",
        ])
        self.assertFalse(self.mod._rewind_picker_default_is_current(pane))

    def test_confirm_sends_enter_when_picker_on_current(self):
        calls = []
        self.mod.run = lambda cmd, timeout=None: (calls.append(cmd), ("", 0))[1]
        self.mod.log = lambda *a, **k: None
        self.mod.capture_pane_text = lambda pane: ""
        pane_text = "\n".join([
            "  /resume 8f2a1c (previous)",
            "  /clear",
            "❯ (current)",
            "",
            "Enter to continue · Esc to cancel",
        ])
        # First run() call inside confirm_clear_picker is the capture-pane
        # poll; return the picker text for it, then Enter should follow.
        def fake_run(cmd, timeout=None):
            calls.append(cmd)
            if cmd[:2] == ["tmux", "capture-pane"]:
                return (pane_text, 0)
            return ("", 0)
        self.mod.run = fake_run
        self.mod.confirm_clear_picker("sess:0.0", max_wait=1, poll_interval=0.05)
        enter_calls = [c for c in calls if c[:2] == ["tmux", "send-keys"] and c[-1] == "Enter"]
        self.assertEqual(len(enter_calls), 1, calls)

    def test_confirm_does_not_send_enter_when_no_picker(self):
        calls = []
        def fake_run(cmd, timeout=None):
            calls.append(cmd)
            if cmd[:2] == ["tmux", "capture-pane"]:
                return ("❯ \n  bypass permissions on · 12k tokens", 0)
            return ("", 0)
        self.mod.run = fake_run
        self.mod.log = lambda *a, **k: None
        self.mod.confirm_clear_picker("sess:0.0", max_wait=0.2, poll_interval=0.05)
        enter_calls = [c for c in calls if c[:2] == ["tmux", "send-keys"] and c[-1] == "Enter"]
        self.assertEqual(enter_calls, [])

    def test_confirm_refuses_enter_when_cursor_on_previous(self):
        calls = []
        pane_text = "\n".join([
            "❯ /resume 8f2a1c (previous)",
            "  /clear",
            "  (current)",
            "",
            "Enter to continue · Esc to cancel",
        ])
        def fake_run(cmd, timeout=None):
            calls.append(cmd)
            if cmd[:2] == ["tmux", "capture-pane"]:
                return (pane_text, 0)
            return ("", 0)
        self.mod.run = fake_run
        self.mod.log = lambda *a, **k: None
        self.mod.capture_pane_text = lambda pane: ""
        self.mod.confirm_clear_picker("sess:0.0", max_wait=1, poll_interval=0.05)
        enter_calls = [c for c in calls if c[:2] == ["tmux", "send-keys"] and c[-1] == "Enter"]
        self.assertEqual(enter_calls, [], "must never confirm onto (previous) -- would fork via /resume")


class RewindPickerConfirmOnceTest(unittest.TestCase):
    """Pure/single-snapshot tests for `_rewind_picker_confirm_once`, the
    primitive meant to be called on EVERY iteration of the completion-poll
    loop (not just a one-shot pre-check window) -- see the wedge this fixes:
    a fixed pre-check can expire before Claude Code renders the picker, after
    which nothing else ever confirms it and the poll misreads the open
    picker as a completed /clear."""

    def setUp(self):
        self.mod = _import_self_clear()
        self.mod.log = lambda *a, **k: None
        self.mod.capture_pane_text = lambda pane: ""

    def test_picker_on_current_confirms_and_reports_seen(self):
        pane_text = "\n".join([
            "  /resume 8f2a1c (previous)",
            "  /clear",
            "❯ (current)",
            "",
            "Enter to continue · Esc to cancel",
        ])
        calls = []
        self.mod.run = lambda cmd, timeout=None: (calls.append(cmd), ("", 0))[1]
        picker_seen, confirmed = self.mod._rewind_picker_confirm_once(
            "sess:0.0", pane_text=pane_text)
        self.assertTrue(picker_seen)
        self.assertTrue(confirmed)
        enter_calls = [c for c in calls if c[:2] == ["tmux", "send-keys"] and c[-1] == "Enter"]
        self.assertEqual(len(enter_calls), 1, calls)

    def test_picker_on_previous_reports_seen_but_not_confirmed(self):
        pane_text = "\n".join([
            "❯ /resume 8f2a1c (previous)",
            "  /clear",
            "  (current)",
            "",
            "Enter to continue · Esc to cancel",
        ])
        calls = []
        self.mod.run = lambda cmd, timeout=None: (calls.append(cmd), ("", 0))[1]
        picker_seen, confirmed = self.mod._rewind_picker_confirm_once(
            "sess:0.0", pane_text=pane_text)
        self.assertTrue(picker_seen, "picker is open -- caller must not treat this as complete")
        self.assertFalse(confirmed)
        enter_calls = [c for c in calls if c[:2] == ["tmux", "send-keys"] and c[-1] == "Enter"]
        self.assertEqual(enter_calls, [], "must never confirm onto (previous)")

    def test_no_picker_reports_not_seen(self):
        pane_text = "\n".join([
            "❯ ",
            "  bypass permissions on · 12k tokens",
        ])
        calls = []
        self.mod.run = lambda cmd, timeout=None: (calls.append(cmd), ("", 0))[1]
        picker_seen, confirmed = self.mod._rewind_picker_confirm_once(
            "sess:0.0", pane_text=pane_text)
        self.assertFalse(picker_seen)
        self.assertFalse(confirmed)
        self.assertEqual(calls, [])

    def test_captures_pane_itself_when_no_pane_text_given(self):
        pane_text = "\n".join([
            "  /resume 8f2a1c (previous)",
            "  /clear",
            "❯ (current)",
            "",
            "Enter to continue · Esc to cancel",
        ])
        calls = []
        def fake_run(cmd, timeout=None):
            calls.append(cmd)
            if cmd[:2] == ["tmux", "capture-pane"]:
                return (pane_text, 0)
            return ("", 0)
        self.mod.run = fake_run
        picker_seen, confirmed = self.mod._rewind_picker_confirm_once("sess:0.0")
        self.assertTrue(picker_seen)
        self.assertTrue(confirmed)

    def test_capture_failure_reports_not_seen(self):
        self.mod.run = lambda cmd, timeout=None: ("", 1)
        picker_seen, confirmed = self.mod._rewind_picker_confirm_once("sess:0.0")
        self.assertFalse(picker_seen)
        self.assertFalse(confirmed)


class CompletionPollNeverSucceedsWithPickerOpenTest(unittest.TestCase):
    """Integration-shaped test for the load-bearing invariant: the
    completion poll in `child_main` must NEVER declare /clear complete while
    the Rewind picker footer is still visible, even if tokens/idle would
    otherwise read as done on that same snapshot.

    We don't invoke the full `child_main` (it drives a real tmux pane +
    subprocess flow end-to-end), but we exercise the poll's core decision
    rule directly: given a pane snapshot where the picker is visible AND
    tokens/idle look like a fresh session, the picker check must win and
    completion must not be signaled.
    """

    def setUp(self):
        self.mod = _import_self_clear()
        self.mod.log = lambda *a, **k: None
        self.mod.capture_pane_text = lambda pane: ""

    def test_picker_open_suppresses_completion_even_if_tokens_and_idle_look_done(self):
        # This is what the wedge log looked like: picker open, but the
        # picker screen itself reads as tokens=0 (looks fresh) and idle=True
        # (a prompt-cursor glyph appears on the (current) row).
        pane_text = "\n".join([
            "  /resume 8f2a1c (previous)",
            "  /clear",
            "❯ (current)",
            "",
            "Enter to continue · Esc to cancel",
        ])
        self.mod.run = lambda cmd, timeout=None: ("", 0)
        picker_seen, _confirmed = self.mod._rewind_picker_confirm_once(
            "sess:0.0", pane_text=pane_text)
        # The poll loop in child_main is written as:
        #   if picker_seen: continue   # never falls through to the
        #                              # tokens/idle completion check
        # Assert the primitive that decision depends on.
        self.assertTrue(
            picker_seen,
            "picker must be detected as open on this snapshot so the poll "
            "skips the tokens/idle completion check entirely",
        )

    def test_is_idle_alone_would_misread_the_picker_current_row(self):
        # Documents WHY the picker check must come first: is_idle() only
        # looks for the cursor glyph "❯" in the tail, which the picker's
        # "❯ (current)" row also satisfies. This is the exact false-positive
        # that caused the original wedge, and is now guarded against by
        # checking _rewind_picker_confirm_once()'s picker_seen BEFORE
        # trusting is_idle()/get_token_count() in the poll loop.
        pane_text = "\n".join([
            "  /resume 8f2a1c (previous)",
            "  /clear",
            "❯ (current)",
            "",
            "Enter to continue · Esc to cancel",
        ])
        self.mod.run = lambda cmd, timeout=None: (pane_text, 0)
        self.assertTrue(self.mod.is_idle("sess:0.0"))
        self.assertTrue(self.mod._rewind_picker_visible(pane_text))


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
