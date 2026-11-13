#!/usr/bin/env python3
"""Unit tests for tools/watchers/self-mcp-reconnect.

Covers the parts that do NOT need a live Claude Code session: the pane-text
parsers/predicates that drive the `/mcp` picker (server-row parsing,
action-row parsing, menu-visibility detection, result-line parsing) and
config-path defaults. These are exactly the pieces the script's own module
docstring calls out as "kept as pure predicates/parsers ... so they are
unit-testable without a live tmux". The menu-navigation choreography itself
(do_reconnect) needs a real Claude Code pane and is exercised manually per
the empirical transcript referenced in the script's docstring and in
container/skills/self-mcp-reconnect.md.

Run:
    python3 tools/watchers/tests/test_self_mcp_reconnect.py
"""

from __future__ import annotations

import importlib.machinery
import importlib.util
import os
import sys
import unittest
from pathlib import Path
from unittest import mock

REPO = Path(__file__).resolve().parents[3]
SCRIPT = REPO / "tools" / "watchers" / "self-mcp-reconnect"


def _import_self_mcp_reconnect(env_overrides=None):
    """Import the self-mcp-reconnect script as a module under controlled env.

    The script defines helpers and imports its self-clear sibling at import
    time (via a SourceFileLoader, same trick used here); it runs no other
    top-level work, so this is safe.
    """
    saved = {}
    for k in (
        "CLAUDE_SELF_MCP_RECONNECT_LOG",
        "CLAUDE_SELF_MCP_RECONNECT_LOCK",
        "CLAUDE_SELF_MCP_RECONNECT_STATE",
        "CLAUDE_SELF_MCP_RECONNECT_SERVER",
        "XDG_STATE_HOME",
        "XDG_RUNTIME_DIR",
    ):
        saved[k] = os.environ.pop(k, None)
    if env_overrides:
        os.environ.update(env_overrides)
    try:
        loader = importlib.machinery.SourceFileLoader(
            "self_mcp_reconnect_under_test", str(SCRIPT)
        )
        spec = importlib.util.spec_from_loader(loader.name, loader)
        mod = importlib.util.module_from_spec(spec)
        loader.exec_module(mod)
        return mod
    finally:
        for k, v in saved.items():
            os.environ.pop(k, None)
            if v is not None:
                os.environ[k] = v


class DefaultsTest(unittest.TestCase):
    def test_default_server_falls_back_to_mcp_adaptor(self):
        mod = _import_self_mcp_reconnect()
        self.assertEqual(mod._default_server(), "mcp-adaptor")

    def test_default_server_honors_env_override(self):
        # _default_server() re-reads the env on every call (it is not bound
        # to a module-level global at import time, unlike LOG_FILE/STATE_FILE
        # below), so the override just needs to be live for the call itself —
        # no re-import required.
        mod = _import_self_mcp_reconnect()
        saved = os.environ.get("CLAUDE_SELF_MCP_RECONNECT_SERVER")
        os.environ["CLAUDE_SELF_MCP_RECONNECT_SERVER"] = "host-bash"
        try:
            self.assertEqual(mod._default_server(), "host-bash")
        finally:
            if saved is None:
                os.environ.pop("CLAUDE_SELF_MCP_RECONNECT_SERVER", None)
            else:
                os.environ["CLAUDE_SELF_MCP_RECONNECT_SERVER"] = saved

    def test_log_file_bound_from_env_at_import_time(self):
        # LOG_FILE (like self-clear's) is bound from _default_log_file() at
        # IMPORT time into a module-level global, so the override must be
        # live during the import call, and the assertion reads the bound
        # global rather than calling the helper again post-import.
        mod = _import_self_mcp_reconnect(
            {"CLAUDE_SELF_MCP_RECONNECT_LOG": "/tmp/example.log"}
        )
        self.assertEqual(mod.LOG_FILE, "/tmp/example.log")


class TopMenuTest(unittest.TestCase):
    def setUp(self):
        self.mod = _import_self_mcp_reconnect()

    def test_top_menu_visible_true(self):
        text = (
            "  Manage MCP servers\n"
            " ❯ mcp-adaptor \xb7 ✔ connected \xb7 5 tools\n"
            "   host-bash \xb7 ✔ connected \xb7 2 tools\n"
            "  ↑/↓ to navigate \xb7 Enter to confirm \xb7 Esc to cancel\n"
        )
        self.assertTrue(self.mod.top_menu_visible(text))
        self.assertFalse(self.mod.detail_menu_visible(text))
        self.assertTrue(self.mod.any_mcp_menu_visible(text))

    def test_top_menu_not_visible_on_plain_chat_prompt(self):
        text = "❯ some regular chat text\n  tokens: 1234\n"
        self.assertFalse(self.mod.top_menu_visible(text))
        self.assertFalse(self.mod.any_mcp_menu_visible(text))

    def test_detail_menu_visible_true(self):
        text = (
            "  mcp-adaptor\n"
            " ❯ 1. View tools\n"
            "   2. Clear authentication\n"
            "   3. Reconnect\n"
            "   4. Disable\n"
            "  ↑/↓ to navigate \xb7 Enter to select \xb7 Esc to back\n"
        )
        self.assertTrue(self.mod.detail_menu_visible(text))
        self.assertFalse(self.mod.top_menu_visible(text))
        self.assertTrue(self.mod.any_mcp_menu_visible(text))


class ParseServerRowsTest(unittest.TestCase):
    def setUp(self):
        self.mod = _import_self_mcp_reconnect()

    def test_parses_rows_and_cursor(self):
        text = (
            "  Manage MCP servers\n"
            "   host-bash \xb7 ✔ connected \xb7 2 tools\n"
            " ❯ mcp-adaptor \xb7 ✘ failed\n"
            "   slack \xb7 △ connecting\n"
            "  ↑/↓ to navigate \xb7 Enter to confirm \xb7 Esc to cancel\n"
        )
        rows = self.mod.parse_server_rows(text)
        names = [r[0] for r in rows]
        self.assertEqual(names, ["host-bash", "mcp-adaptor", "slack"])
        # Only the mcp-adaptor row carries the cursor.
        cursor_flags = {name: is_cursor for name, _status, is_cursor in rows}
        self.assertTrue(cursor_flags["mcp-adaptor"])
        self.assertFalse(cursor_flags["host-bash"])
        self.assertFalse(cursor_flags["slack"])

    def test_ignores_non_server_lines(self):
        text = "  Manage MCP servers\n  some \xb7 line without a status glyph\n"
        self.assertEqual(self.mod.parse_server_rows(text), [])


class ParseActionRowsTest(unittest.TestCase):
    def setUp(self):
        self.mod = _import_self_mcp_reconnect()

    def test_parses_numbered_actions_and_finds_reconnect_by_text(self):
        text = (
            "  mcp-adaptor\n"
            "   1. View tools\n"
            "   2. Clear authentication\n"
            " ❯ 3. Reconnect\n"
            "   4. Disable\n"
        )
        actions = self.mod.parse_action_rows(text)
        self.assertEqual(
            [(n, label) for n, label, _c in actions],
            [
                (1, "View tools"),
                (2, "Clear authentication"),
                (3, "Reconnect"),
                (4, "Disable"),
            ],
        )
        reconnect = [(n, l, c) for (n, l, c) in actions if "reconnect" in l.lower()]
        self.assertEqual(len(reconnect), 1)
        self.assertEqual(reconnect[0][0], 3)
        self.assertTrue(reconnect[0][2])  # cursor already on Reconnect in this fixture

    def test_reconnect_slot_position_varies_by_status(self):
        # A FAILED server's detail screen starts "1. Reconnect / 2. Disable" —
        # a different slot than the CONNECTED case above. The lookup must be
        # by text, never a hardcoded index.
        text = " ❯ 1. Reconnect\n   2. Disable\n"
        actions = self.mod.parse_action_rows(text)
        reconnect = [(n, l, c) for (n, l, c) in actions if "reconnect" in l.lower()]
        self.assertEqual(reconnect[0][0], 1)


class FindLastResultTest(unittest.TestCase):
    def setUp(self):
        self.mod = _import_self_mcp_reconnect()

    def test_success_line(self):
        text = "some chat output\nReconnected to mcp-adaptor.\n"
        result = self.mod.find_last_result(text)
        self.assertEqual(result, ("ok", "mcp-adaptor", None))

    def test_failure_line(self):
        text = "Failed to reconnect to mcp-adaptor: connection refused\n"
        status, name, reason = self.mod.find_last_result(text)
        self.assertEqual(status, "fail")
        self.assertEqual(name, "mcp-adaptor")
        self.assertIn("connection refused", reason)

    def test_returns_last_when_multiple_present(self):
        text = (
            "Reconnected to host-bash.\n"
            "...\n"
            "Failed to reconnect to mcp-adaptor: timeout\n"
        )
        status, name, _reason = self.mod.find_last_result(text)
        self.assertEqual((status, name), ("fail", "mcp-adaptor"))

    def test_no_result_present(self):
        self.assertIsNone(self.mod.find_last_result("just a normal chat reply\n"))


class DoReconnectNonCancellingTest(unittest.TestCase):
    """Regression test for Andrew #6168-6171: driving `/mcp` must NOT
    interrupt the pane's in-flight turn.

    `do_reconnect` used to call `sc.interrupt_and_wait` (an active Escape
    blast) and inject `/mcp` with `escape=True` (the CANCELLING
    `claude-watch inject` path) — both cancel whatever turn is running.
    The fix routes the `/mcp` open through the injector's NON-CANCELLING
    default (no `--escape`) and drops the interrupt call entirely. This
    drives the full `do_reconnect` happy path with `sc.run`/`sc.inject`/
    `sc.interrupt_and_wait` mocked (no live tmux) and asserts both the
    outcome and the injection choreography used to get there.
    """

    def setUp(self):
        self.mod = _import_self_mcp_reconnect()

    def test_do_reconnect_never_interrupts_and_opens_mcp_non_cancelling(self):
        mod = self.mod
        pane = "claude-container:0.0"

        # One capture-pane response per `capture(pane)` call `do_reconnect`
        # makes: (1) pre-existing result check, (2) top-level /mcp menu,
        # (3) mcp-adaptor's detail screen, (4) the post-reconnect result.
        capture_queue = [
            "❯ some prior chat text\n",
            (
                "  Manage MCP servers\n"
                " ❯ host-bash \xb7 ✔ connected \xb7 2 tools\n"
                "   mcp-adaptor \xb7 ✘ failed\n"
                "  ↑/↓ to navigate \xb7 Enter to confirm \xb7 Esc to cancel\n"
            ),
            (
                "  Manage MCP servers\n"
                " ❯ host-bash \xb7 ✔ connected \xb7 2 tools\n"
                "   mcp-adaptor \xb7 ✘ failed\n"
                "  ↑/↓ to navigate \xb7 Enter to confirm \xb7 Esc to cancel\n"
            ),
            (
                "  Manage MCP servers\n"
                "   host-bash \xb7 ✔ connected \xb7 2 tools\n"
                " ❯ mcp-adaptor \xb7 ✘ failed\n"
                "  ↑/↓ to navigate \xb7 Enter to confirm \xb7 Esc to cancel\n"
            ),
            (
                "  Mcp-adaptor MCP Server\n"
                " ❯ 1. Reconnect\n"
                "   2. Disable\n"
                "  ↑/↓ to navigate \xb7 Enter to select \xb7 Esc to back\n"
            ),
            (
                "  Mcp-adaptor MCP Server\n"
                " ❯ 1. Reconnect\n"
                "   2. Disable\n"
                "  ↑/↓ to navigate \xb7 Enter to select \xb7 Esc to back\n"
            ),
            "Reconnected to mcp-adaptor.\n",
        ]

        def fake_run(cmd, timeout=10):
            if cmd[:2] == ["tmux", "capture-pane"]:
                return capture_queue.pop(0), 0
            # tmux send-keys (navigation) and any other shell-out: no-op ok.
            return "", 0

        mod.sc.run = mock.MagicMock(side_effect=fake_run)
        mod.sc.capture_pane_text = mock.MagicMock(return_value="<mocked pane>")
        mod.sc.ensure_main_loop_focus = mock.MagicMock(return_value=True)
        # If do_reconnect ever calls this again, fail loudly: it is an
        # active Escape blast and must not be part of the /mcp flow.
        mod.sc.interrupt_and_wait = mock.MagicMock(
            side_effect=AssertionError(
                "do_reconnect must not call interrupt_and_wait -- that Escape "
                "blast cancels the pane's in-flight turn (Andrew #6168-6171)"
            )
        )
        mod.sc.inject = mock.MagicMock()

        result = mod.do_reconnect(
            pane, "mcp-adaptor", menu_timeout=5.0, result_timeout=5.0
        )

        self.assertEqual(result, {
            "ok": True, "server": "mcp-adaptor", "reason": None, "code": 0,
            "tools_before": "✘ failed",
        })
        mod.sc.interrupt_and_wait.assert_not_called()

        mcp_open_calls = [
            c for c in mod.sc.inject.call_args_list
            if len(c.args) >= 2 and c.args[1] == "/mcp"
        ]
        self.assertEqual(len(mcp_open_calls), 1, "expected exactly one /mcp inject call")
        _, kwargs = mcp_open_calls[0]
        self.assertFalse(
            kwargs.get("escape", False),
            "opening /mcp must use claude-watch inject's non-cancelling "
            "default (escape=False) -- escape=True Escape-blasts the pane "
            "and cancels the in-flight turn",
        )
        self.assertTrue(kwargs.get("slash_command", False))


class RealPickerFixtureTest(unittest.TestCase):
    """Captured from a scratch tmux pane under Claude Code v2.1.287."""

    FX = Path(__file__).resolve().parent / "fixtures"

    def setUp(self):
        self.mod = _import_self_mcp_reconnect()
        self.top = (self.FX / "mcp_picker_v2.1.287_top.txt").read_text()
        self.detail = (self.FX / "mcp_picker_v2.1.287_detail.txt").read_text()
        self.result = (self.FX / "mcp_picker_v2.1.287_result.txt").read_text()

    def test_top_rows_parse(self):
        rows = self.mod.parse_server_rows(self.top)
        self.assertEqual([r[0] for r in rows],
                         ["chrome-devtools", "host-bash", "mcp-adaptor", "slack"])
        self.assertTrue(rows[0][2])
        self.assertFalse(any(r[2] for r in rows[1:]))
        self.assertIn("tools fetch failed", rows[2][1])
        self.assertIn("needs authentication", rows[3][1])
        self.assertTrue(self.mod.top_menu_visible(self.top))

    def test_detail_actions(self):
        self.assertTrue(self.mod.detail_menu_visible(self.detail))
        acts = self.mod.parse_action_rows(self.detail)
        self.assertEqual([(n, l) for n, l, _ in acts], [(1, "Reconnect"), (2, "Disable")])

    def test_partial_result(self):
        r = self.mod.find_last_result(self.result)
        self.assertEqual(r[0], "partial")
        self.assertEqual(r[1], "mcp-adaptor")
        self.assertIn("no active session", r[2])

    def test_legacy_layout_still_parses(self):
        rows = self.mod.parse_server_rows(
            "  Manage MCP servers\n ❯ a-srv \xb7 ✔ connected \xb7 2 tools\n"
            "   b-srv \xb7 ✘ failed\n  Enter to confirm\n")
        self.assertEqual([r[0] for r in rows], ["a-srv", "b-srv"])


class FakeMcpPane:
    """Stateful fake of the inline `/mcp` picker in a SHORT tmux pane.

    The list is taller than the pane, so the visible capture shows only the
    tail of the render (title + top rows scrolled off) while a scrollback
    capture (`-S`) shows everything. Models the 2026-11-12 / 2026-11-13
    misfire: chrome-devtools (cursor start, row 0) is off screen.
    """

    HEIGHT = 5

    def __init__(self, names, start=0, header_override=None):
        self.names = list(names)
        self.cursor = start
        self.screen = "top"
        self.keys = []
        self.header_override = header_override
        self.result = ""

    def _top_lines(self):
        out = ["Manage MCP servers", f"{len(self.names)} servers"]
        for i, n in enumerate(self.names):
            out.append(f"{' ❯' if i == self.cursor else '  '} ✔ {n}   3 tools")
        out += ["https://code.claude.com/docs/en/mcp for help",
                "↑/↓ to navigate \xb7 Enter to confirm \xb7 Esc to cancel"]
        return out

    def _detail_lines(self):
        name = self.header_override or self.names[self.cursor]
        return [f"{name} MCP Server", "Status: ✔ connected",
                " ❯ 1. View tools", "   2. Reconnect", "   3. Disable",
                "↑/↓ to navigate \xb7 Enter to select \xb7 Esc to back"]

    def capture(self, full):
        if self.screen == "top":
            lines = self._top_lines()
        elif self.screen == "detail":
            lines = self._detail_lines()
        else:
            return self.result
        return "\n".join(lines if full else lines[-self.HEIGHT:]) + "\n"

    def run(self, cmd, timeout=10):
        if cmd[:2] == ["tmux", "capture-pane"]:
            return self.capture("-S" in cmd), 0
        if cmd[:2] == ["tmux", "send-keys"]:
            key = cmd[-1]
            self.keys.append(key)
            if self.screen == "top":
                if key == "Down":
                    self.cursor = min(self.cursor + 1, len(self.names) - 1)
                elif key == "Up":
                    self.cursor = max(self.cursor - 1, 0)
                elif key == "Enter":
                    self.screen = "detail"
            elif self.screen == "detail" and key == "Enter":
                # cursor in detail menu is on item 1 + number of Downs
                downs = 0
                for k in reversed(self.keys[:-1]):
                    if k == "Down":
                        downs += 1
                    else:
                        break
                if downs == 1:
                    self.screen = "done"
                    self.result = f"Reconnected to {self.names[self.cursor]}.\n"
                else:
                    self.screen = "done"
                    self.result = "View tools opened\n"
            elif key == "Escape":
                self.screen = "done"
        return "", 0


class WrongRowRegressionTest(unittest.TestCase):
    """The tool reconnected chrome-devtools instead of personal-mbp because
    the parsed list was truncated by pane height (first rows scrolled off)."""

    NAMES = ["chrome-devtools", "personal-mbp", "easel", "claude.ai-Gmail"]

    def setUp(self):
        self.mod = _import_self_mcp_reconnect()

    def _drive(self, pane_obj, server):
        mod = self.mod
        mod.sc.run = mock.MagicMock(side_effect=pane_obj.run)
        mod.sc.capture_pane_text = mock.MagicMock(return_value="<mocked>")
        mod.sc.ensure_main_loop_focus = mock.MagicMock(return_value=True)
        mod.sc.inject = mock.MagicMock()
        mod.time.sleep = lambda *_a, **_k: None
        return mod.do_reconnect("p:0.0", server, menu_timeout=0.5, result_timeout=0.5)

    def test_visible_capture_alone_would_have_truncated_the_list(self):
        pane = FakeMcpPane(self.NAMES)
        visible = pane.capture(full=False)
        self.assertNotIn("chrome-devtools",
                         [r[0] for r in self.mod.parse_server_rows(visible)])
        full = pane.capture(full=True)
        self.assertEqual([r[0] for r in self.mod.parse_server_rows(full)], self.NAMES)

    def test_target_not_first_row_reconnects_the_right_server(self):
        pane = FakeMcpPane(self.NAMES)
        result = self._drive(pane, "personal-mbp")
        self.assertTrue(result["ok"], result)
        self.assertEqual(pane.cursor, 1)

    def test_cursor_start_off_target_below_moves_up(self):
        pane = FakeMcpPane(self.NAMES, start=3)
        result = self._drive(pane, "personal-mbp")
        self.assertTrue(result["ok"], result)
        self.assertIn("Up", pane.keys)

    def test_wrong_detail_header_aborts_before_reconnect(self):
        pane = FakeMcpPane(self.NAMES, header_override="chrome-devtools")
        result = self._drive(pane, "personal-mbp")
        self.assertFalse(result["ok"])
        self.assertEqual(result["code"], 4)
        self.assertIn("refusing to Reconnect", result["reason"])
        # never confirmed a Reconnect: only the top-list Down + Enter happened
        self.assertEqual(pane.keys.count("Enter"), 1)

    def test_detail_header_name_parses_capitalised(self):
        self.assertEqual(self.mod.detail_header_name("  Mcp-adaptor MCP Server\n x"),
                         "Mcp-adaptor")

    def test_scrollback_duplicate_renders_are_deduped(self):
        pane = FakeMcpPane(self.NAMES)
        one = pane.capture(full=True)
        rows = self.mod.parse_server_rows(one + one)
        self.assertEqual([r[0] for r in rows], self.NAMES)


if __name__ == "__main__":
    unittest.main()
