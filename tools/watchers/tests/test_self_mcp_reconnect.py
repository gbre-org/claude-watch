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
            "❯ \n",     # final close-check: picker gone, prompt is back
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
        mod.verify_live = lambda *a, **k: ("connected", "row", "")

        result = mod.do_reconnect(
            pane, "mcp-adaptor", menu_timeout=5.0, result_timeout=5.0
        )

        self.assertEqual(result, {
            "ok": True, "server": "mcp-adaptor", "reason": None, "code": 0,
            "tools_before": "✘ failed", "live_state": "connected", "pane_restored": True,
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
        self.reconnected = False
        self.live_after = "✔"      # glyph the list shows after Reconnect

    def reopen(self):
        self.screen = "top"

    def _top_lines(self):
        out = ["Manage MCP servers", f"{len(self.names)} servers"]
        for i, n in enumerate(self.names):
            glyph = self.live_after if self.reconnected else "✔"
            out.append(f"{' ❯' if i == self.cursor else '  '} {glyph} {n}   3 tools")
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
                    self.reconnected = True
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
        mod.sc.inject = mock.MagicMock(
            side_effect=lambda _p, text, **_k: pane_obj.reopen() if text == "/mcp" else None)
        mod.time.sleep = lambda *_a, **_k: None
        mod.VERIFY_SECONDS = 0.3
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

    def test_headerless_detail_screen_still_reconnects(self):
        """Newer Claude Code prints no '<Name> MCP Server' header (2026-11-18
        code-4 failures); the verified cursor row + result check carry identity."""
        pane = FakeMcpPane(self.NAMES)
        pane._detail_lines = lambda: [
            "Learn more:", "https://code.claude.com/docs/en/mcp", " ❯ 1. View tools",
            "   2. Reconnect", "   3. Disable", "Enter to select \xb7 Esc to back"]
        result = self._drive(pane, "personal-mbp")
        self.assertTrue(result["ok"], result)

    def _no_reconnect_pane(self, status_line):
        pane = FakeMcpPane(self.NAMES)
        lines = ["personal-mbp MCP Server"] + ([status_line] if status_line else []) + [
            " \u276f 1. View tools", "   2. Disable",
            "\u2191/\u2193 to navigate \xb7 Enter to select \xb7 Esc to back"]
        pane._detail_lines = lambda: lines
        return pane

    def test_no_reconnect_but_connected_on_detail_is_noop_success(self):
        pane = self._no_reconnect_pane("Status: \u2714 connected")
        result = self._drive(pane, "personal-mbp")
        self.assertTrue(result["ok"], result)
        self.assertEqual(result["code"], 0)
        self.assertEqual(result["status"], "already connected")

    def test_no_reconnect_connected_on_list_row_is_noop_success(self):
        pane = self._no_reconnect_pane(None)
        self.mod.verify_via_cli = lambda s: f"{s}: x - \u2714 Connected"
        result = self._drive(pane, "personal-mbp")
        self.assertTrue(result["ok"], result)
        self.assertEqual(result["status"], "already connected")

    def test_no_reconnect_and_not_connected_still_code_4(self):
        pane = self._no_reconnect_pane("Status: \u2718 failed")
        self.mod.verify_via_cli = lambda s: f"{s}: x - \u2718 Failed to connect"
        result = self._drive(pane, "personal-mbp")
        self.assertFalse(result["ok"])
        self.assertEqual(result["code"], 4)
        self.assertIn("no Reconnect action", result["reason"])

    def test_status_helpers(self):
        m = self.mod
        self.assertTrue(m.detail_status_connected("Status: \u2714 connected\n"))
        self.assertFalse(m.detail_status_connected("Status: disconnected\n"))
        self.assertFalse(m.detail_status_connected("nothing"))
        self.assertTrue(m.cli_status_connected("a: b - \u2714 Connected"))
        self.assertFalse(m.cli_status_connected("a: b - \u2718 Failed to connect"))

    def test_detail_header_tolerates_border_and_suffix(self):
        self.assertEqual(
            self.mod.detail_header_name("\u2502 Personal-mbp MCP Server \u2502\n"),
            "Personal-mbp")

    def test_notify_done_emits_event_and_injects(self):
        mod = self.mod
        mod.sc.inject = mock.MagicMock()
        mod.emit_event = mock.MagicMock()
        mod.notify_done({"ok": False, "server": "x", "code": 4, "reason": "boom"}, "p:0.0")
        msg = mod.emit_event.call_args[0][0]
        self.assertIn("FAILED", msg)
        self.assertIn("code=4", msg)
        self.assertEqual(mod.emit_event.call_args[1]["tag"], "self-mcp-reconnect")
        self.assertIn("code=4", mod.sc.inject.call_args[0][1])
        self.assertFalse(mod.sc.inject.call_args[1]["escape"])

    def test_notify_done_no_inject(self):
        mod = self.mod
        mod.sc.inject = mock.MagicMock()
        mod.emit_event = mock.MagicMock()
        mod.notify_done({"ok": True, "server": "x", "code": 0}, "p:0.0", inject=False)
        mod.emit_event.assert_called_once()
        mod.sc.inject.assert_not_called()

    def test_detail_header_name_parses_capitalised(self):
        self.assertEqual(self.mod.detail_header_name("  Mcp-adaptor MCP Server\n x"),
                         "Mcp-adaptor")

    def test_scrollback_duplicate_renders_are_deduped(self):
        pane = FakeMcpPane(self.NAMES)
        one = pane.capture(full=True)
        rows = self.mod.parse_server_rows(one + one)
        self.assertEqual([r[0] for r in rows], self.NAMES)


class PanelPane:
    """Stateful fake of the newer `/mcp` panel ("MCP servers" title, "Esc to
    close" footer, "Local MCPs" group, cursor starting on the FIRST server) built
    from captured-pane fixtures. Records every key sent and models Escape
    (detail -> list -> closed). `crash_after` raises from send-keys after that
    many keys; `stuck` makes Escape a no-op."""

    FX = Path(__file__).resolve().parent / "fixtures"

    def __init__(self, stuck=False, crash_after=None, fail_reconnect=False):
        self.top_tpl = (self.FX / "mcp_picker_panel_cursor_off_target.txt").read_text()
        self.detail = (self.FX / "mcp_picker_panel_detail_failed.txt").read_text()
        self.names = ["chrome-devtools", "personal-mbp"]
        self.cursor = 0
        self.screen = "top"
        self.keys = []
        self.stuck = stuck
        self.crash_after = crash_after
        self.fail_reconnect = fail_reconnect
        self.result = ""
        self.reconnected = False

    def reopen(self):
        self.screen = "top"

    def _top(self):
        text = self.top_tpl
        if self.reconnected:
            text = text.replace("✘ personal-mbp      failed", "✔ personal-mbp      3 tools")
        # move the cursor glyph to the active row
        text = text.replace(" \u276f \u2714 chrome-devtools", "   \u2714 chrome-devtools")
        text = text.replace("   \u2718 personal-mbp", " \u2718 personal-mbp")
        if self.cursor == 0:
            text = text.replace("   \u2714 chrome-devtools", " \u276f \u2714 chrome-devtools")
        else:
            text = text.replace(" \u2718 personal-mbp", " \u276f \u2718 personal-mbp")
        return text

    def run(self, cmd, timeout=10):
        if cmd[:2] == ["tmux", "capture-pane"]:
            if self.screen == "top":
                return self._top(), 0
            if self.screen == "detail":
                return self.detail, 0
            return self.result or "\u276f \n", 0
        if cmd[:2] == ["tmux", "send-keys"]:
            key = cmd[-1]
            self.keys.append(key)
            if self.crash_after is not None and len(self.keys) > self.crash_after:
                raise RuntimeError("boom mid-interaction")
            if key == "Escape":
                if self.stuck:
                    return "", 0
                self.screen = "top" if self.screen == "detail" else "closed"
            elif self.screen == "top":
                if key == "Down":
                    self.cursor = min(self.cursor + 1, 1)
                elif key == "Up":
                    self.cursor = max(self.cursor - 1, 0)
                elif key == "Enter":
                    self.screen = "detail"
            elif self.screen == "detail" and key == "Enter":
                self.screen = "closed"
                self.reconnected = not self.fail_reconnect
                self.result = ("Failed to reconnect to personal-mbp: ECONNREFUSED\n"
                               if self.fail_reconnect else "Reconnected to personal-mbp.\n")
        return "", 0


class PanelClosesOnEveryExitTest(unittest.TestCase):
    """Regression: the `/mcp` panel (title "MCP servers", footer "Esc to close",
    cursor on a different server) was not recognised, so the tool returned
    menu-did-not-open and left the panel open, blocking the session."""

    def setUp(self):
        self.mod = _import_self_mcp_reconnect()
        self.events = []
        self.mod.emit_event = lambda *a, **k: self.events.append((a, k))

    def _drive(self, pane, server="personal-mbp"):
        mod = self.mod
        mod.sc.run = mock.MagicMock(side_effect=pane.run)
        mod.sc.capture_pane_text = mock.MagicMock(return_value="<mocked>")
        mod.sc.ensure_main_loop_focus = mock.MagicMock(return_value=True)
        mod.sc.inject = mock.MagicMock(
            side_effect=lambda _p, text, **_k: pane.reopen() if text == "/mcp" else None)
        mod.time.sleep = lambda *_a, **_k: None
        mod.VERIFY_SECONDS = 0.3
        return mod.do_reconnect("p:0.0", server, menu_timeout=0.3, result_timeout=0.3)

    def test_panel_fixture_is_recognised_and_parsed(self):
        top = PanelPane()._top()
        self.assertTrue(self.mod.top_menu_visible(top))
        self.assertFalse(self.mod.detail_menu_visible(top))
        rows = self.mod.parse_server_rows(top)
        self.assertEqual([(r[0], r[2]) for r in rows],
                         [("chrome-devtools", True), ("personal-mbp", False)])
        self.assertEqual(self.mod.cursor_name(top), "chrome-devtools")

    def test_navigates_from_other_server_to_target_by_name(self):
        pane = PanelPane()
        result = self._drive(pane)
        self.assertTrue(result["ok"], result)
        self.assertEqual(pane.cursor, 1)
        self.assertIn("Down", pane.keys)
        self.assertEqual(pane.screen, "closed")
        self.assertEqual(self.events, [])

    def test_success_does_not_escape_at_the_normal_prompt(self):
        pane = PanelPane()
        self._drive(pane)
        # The only Escape allowed is the one closing the verify re-open of the
        # list (the picker is on screen then); never one at the normal prompt.
        self.assertLessEqual(pane.keys.count("Escape"), 1)
        self.assertEqual(pane.screen, "closed")

    def test_failure_path_closes_the_panel(self):
        # Server absent from the list -> code 4 failure with the panel open.
        pane = PanelPane()
        result = self._drive(pane, server="nonexistent")
        self.assertFalse(result["ok"])
        self.assertEqual(result["code"], 4)
        self.assertEqual(pane.screen, "closed")
        self.assertIn("Escape", pane.keys)
        self.assertNotIn("menu_stuck", result)

    def test_failed_reconnect_result_leaves_prompt(self):
        pane = PanelPane(fail_reconnect=True)
        result = self._drive(pane)
        self.assertEqual(result["code"], 5)
        self.assertEqual(pane.screen, "closed")

    def test_exception_mid_interaction_closes_the_panel(self):
        # Crash on the 2nd key (after Down); the Escape that follows must still
        # be sent, so let only the navigation key crash.
        pane = PanelPane(crash_after=0)
        orig = pane.run

        def run(cmd, timeout=10):
            if cmd[:2] == ["tmux", "send-keys"] and cmd[-1] != "Escape" and not pane.keys:
                pane.keys.append(cmd[-1])
                raise RuntimeError("boom mid-interaction")
            pane.crash_after = None
            return orig(cmd, timeout)
        pane.run = run
        result = self._drive(pane)
        self.assertFalse(result["ok"])
        self.assertEqual(result["code"], 1)
        self.assertIn("boom", result["reason"])
        self.assertEqual(pane.screen, "closed")

    def test_unclosable_panel_emits_loud_event(self):
        pane = PanelPane(stuck=True)
        result = self._drive(pane, server="nonexistent")
        self.assertFalse(result["ok"])
        self.assertTrue(result.get("menu_stuck"))
        self.assertEqual(len(self.events), 1)
        self.assertEqual(self.events[0][1]["tag"], "self-mcp-reconnect-menu-stuck")

    def test_close_any_menu_never_escapes_at_plain_prompt(self):
        pane = PanelPane()
        pane.screen = "closed"
        self.mod.sc.run = mock.MagicMock(side_effect=pane.run)
        self.mod.time.sleep = lambda *_a, **_k: None
        self.assertTrue(self.mod.close_any_menu("p:0.0"))
        self.assertEqual(pane.keys, [])

    def test_top_menu_footer_variants(self):
        for footer in ("Enter to confirm \xb7 Esc to cancel", "Esc to close",
                       "Enter to select \xb7 Esc to close"):
            text = f" MCP servers\n \u276f \u2714 a-srv   1 tools\n {footer}\n"
            self.assertTrue(self.mod.top_menu_visible(text), footer)
            self.assertFalse(self.mod.detail_menu_visible(text), footer)



FX_DIR = Path(__file__).resolve().parent / "fixtures"


def _fx(name):
    return (FX_DIR / name).read_text()


class LiveFailedHttpServerTest(unittest.TestCase):
    """Real screens captured 2026-11-18 (identifiers scrubbed) for an HTTP MCP
    server whose tunnel went away: the LIVE session shows it failed
    (ECONNREFUSED) even though a fresh `claude mcp list` probe says Connected.

    Facts: the list row is just "✘ <name>" (no status text); the detail screen
    has a "Status: ✘ failed" field block and UNNUMBERED actions
    ("❯ Reconnect / Disable in this project / Back"); the title is
    "<name>" + "MCP server" on separate lines.
    """

    NAME = "remote-http"

    def setUp(self):
        self.mod = _import_self_mcp_reconnect()

    def test_list_row_is_failed_without_status_text(self):
        text = _fx("mcp_picker_2026-11_list_http_failed.txt")
        self.assertTrue(self.mod.top_menu_visible(text))
        state, row = self.mod.live_row_state(text, self.NAME)
        self.assertEqual(state, "failed")
        self.assertIn("\u2718", row)
        self.assertEqual(self.mod.live_row_state(text, "chrome-devtools")[0], "connected")
        self.assertEqual(self.mod.live_row_state(text, "claude.ai Gmail")[0], None)
        self.assertEqual(self.mod.cursor_name(text), self.NAME)

    def test_unnumbered_actions_are_parsed(self):
        text = _fx("mcp_picker_2026-11_detail_http_failed.txt")
        self.assertTrue(self.mod.detail_menu_visible(text))
        acts = self.mod.parse_action_rows(text)
        self.assertEqual([(n, l, c) for n, l, c in acts],
                         [(1, "Reconnect", True), (2, "Disable in this project", False),
                          (3, "Back", False)])
        self.assertEqual(self.mod.detail_live_state(text), "failed")

    def test_connected_detail_offers_no_reconnect(self):
        text = _fx("mcp_picker_2026-11_detail_http_connected.txt")
        labels = [l for _n, l, _c in self.mod.parse_action_rows(text)]
        self.assertNotIn("Reconnect", labels)
        self.assertEqual(self.mod.detail_live_state(text), "connected")

    def test_numbered_actions_still_win(self):
        acts = self.mod.parse_action_rows(
            " Status: x\n \u276f 1. View tools\n   2. Reconnect\n  Enter to select\n")
        self.assertEqual([l for _n, l, _c in acts], ["View tools", "Reconnect"])

    def test_classify_state(self):
        c = self.mod.classify_state
        self.assertEqual(c("\u2714", "29 tools"), "connected")
        self.assertEqual(c("\u2718", ""), "failed")
        self.assertEqual(c("\u26a0", "needs authentication"), "needs-auth")
        self.assertEqual(c("\u26a0", "tools fetch failed \xb7 connected"), "degraded")
        self.assertEqual(c("", ""), "unknown")


class FailureDialogTest(unittest.TestCase):
    """True failure outcomes ("Couldn't reconnect", Status failed + Issue) are
    terminal results (code 7) and the dialog is always dismissed."""

    FIX = "mcp_picker_2026-11_detail_reconnect_failed_inline_title.txt"

    def setUp(self):
        self.mod = _import_self_mcp_reconnect()

    def test_parses_couldnt_reconnect_with_wrapped_issue(self):
        f = self.mod.parse_failure_dialog(_fx(self.FIX))
        self.assertEqual(f["kind"], "failed")
        self.assertTrue(f["explicit"])
        self.assertEqual(f["server"], "personal-mbp")
        self.assertEqual(f["issue"], "connection timed out after 30000ms")

    def test_standing_status_failed_with_issue_is_not_explicit(self):
        f = self.mod.parse_failure_dialog(_fx("mcp_picker_2026-11_detail_http_failed.txt"))
        self.assertFalse(f["explicit"])
        self.assertTrue(f["issue"].startswith("ECONNREFUSED"))

    def test_connected_detail_is_not_a_failure(self):
        self.assertIsNone(self.mod.parse_failure_dialog(
            _fx("mcp_picker_2026-11_detail_http_connected.txt")))

    def test_needs_auth_is_a_failure(self):
        f = self.mod.parse_failure_dialog(
            "  srv\n  MCP server\n\n  Status:  \u26a0 needs authentication\n\n"
            "  \u276f Authenticate\n    Back\n\n  Enter to select \xb7 Esc to go back\n")
        self.assertEqual(f["kind"], "needs-auth")

    def test_modal_detected_even_with_stale_title_above(self):
        # The old title-based predicates said "no menu" for this screen, so the
        # defensive close never pressed Esc and the dialog stayed open.
        text = _fx(self.FIX)
        self.assertTrue(self.mod.modal_footer_at_bottom(text))
        self.assertTrue(self.mod.any_mcp_menu_visible(text))

    def test_plain_prompt_is_not_a_modal(self):
        self.assertFalse(self.mod.any_mcp_menu_visible(
            "some chat about MCP servers\n\n\u276f \n  ? for shortcuts\n"))

    def test_summary_line_fields(self):
        line = self.mod.summary_line({"ok": False, "server": "personal-mbp", "code": 7,
                                      "issue": "connection timed out after 30000ms",
                                      "reason": "reconnect-failed: connection timed out after 30000ms",
                                      "pane_restored": True})
        self.assertIn("FAILED server=personal-mbp code=7", line)
        self.assertIn("issue=connection timed out after 30000ms", line)
        self.assertIn("pane_restored=yes", line)
        bad = self.mod.summary_line({"ok": False, "server": "x", "code": 7, "pane_restored": False})
        self.assertIn("pane_restored=NO", bad)


class FailureDriveTest(unittest.TestCase):
    """End to end through do_reconnect: Reconnect pressed, dialog stays open
    showing the failure; Esc must be repeated until the prompt is back."""

    NAME = "personal-mbp"

    class Pane:
        def __init__(self, escapes_needed=2, stuck=False):
            self.screen = "top"
            self.keys = []
            self.escapes_needed = escapes_needed
            self.esc_seen = 0
            self.stuck = stuck

        def reopen(self):
            self.screen = "top"

        def run(self, cmd, timeout=10):
            if cmd[:2] == ["tmux", "capture-pane"]:
                if self.screen == "top":
                    return _fx("mcp_picker_2026-11_list_http_failed.txt").replace(
                        "remote-http", "personal-mbp"), 0
                if self.screen == "detail":
                    return _fx("mcp_picker_2026-11_detail_http_failed.txt").replace(
                        "remote-http", "personal-mbp"), 0
                if self.screen == "failed":
                    return _fx(FailureDialogTest.FIX), 0
                return "\u276f \n  ? for shortcuts\n", 0
            if cmd[:2] == ["tmux", "send-keys"]:
                key = cmd[-1]
                self.keys.append(key)
                if key == "Escape" and not self.stuck:
                    self.esc_seen += 1
                    if self.esc_seen >= self.escapes_needed:
                        self.screen = "closed"
                elif key == "Enter":
                    self.screen = {"top": "detail", "detail": "failed"}.get(self.screen, self.screen)
            return "", 0

    def setUp(self):
        self.mod = _import_self_mcp_reconnect()
        self.events = []
        self.mod.emit_event = lambda *a, **k: self.events.append((a, k))

    def _drive(self, pane):
        mod = self.mod
        mod.sc.run = mock.MagicMock(side_effect=pane.run)
        mod.sc.capture_pane_text = mock.MagicMock(return_value="<mocked>")
        mod.sc.ensure_main_loop_focus = mock.MagicMock(return_value=True)
        mod.sc.inject = mock.MagicMock(
            side_effect=lambda _p, text, **_k: pane.reopen() if text == "/mcp" else None)
        mod.time.sleep = lambda *_a, **_k: None
        mod.FAILURE_SETTLE_SECONDS = 0.0
        return mod.do_reconnect("p:0.0", self.NAME, menu_timeout=0.3, result_timeout=0.3)

    def test_true_failure_is_code_7_and_pane_restored_with_multiple_escapes(self):
        pane = self.Pane(escapes_needed=3)
        result = self._drive(pane)
        self.assertEqual(result["code"], 7, result)
        self.assertEqual(result["reason"], "reconnect-failed: connection timed out after 30000ms")
        self.assertEqual(result["issue"], "connection timed out after 30000ms")
        self.assertTrue(result["pane_restored"])
        self.assertEqual(pane.screen, "closed")
        self.assertGreaterEqual(pane.keys.count("Escape"), 3)
        self.assertNotIn("menu_stuck", result)

    def test_unrestorable_pane_is_reported_explicitly(self):
        pane = self.Pane(stuck=True)
        result = self._drive(pane)
        self.assertEqual(result["code"], 7)
        self.assertFalse(result["pane_restored"])
        self.assertTrue(result["menu_stuck"])
        self.assertIn("PANE NOT RESTORED", result["reason"])
        self.assertLessEqual(pane.keys.count("Escape"), 8 + 8 + 8)  # bounded
        self.assertTrue(any(a[1]["tag"] == "self-mcp-reconnect-menu-stuck"
                            for a in self.events))


class LivePane:
    """Stateful fake of the 2026-11 picker driven by the real-capture
    fixtures. `healthy_after` = number of /mcp re-opens (after the Reconnect)
    before the list row turns connected; None = never heals."""

    NAME = "remote-http"

    def __init__(self, start_connected=False, healthy_after=0, offer_reconnect=True):
        self.connected = start_connected
        self.healthy_after = healthy_after
        self.offer_reconnect = offer_reconnect
        self.screen = "top"
        self.keys = []
        self.reopens_after_reconnect = 0
        self.reconnect_fired = False
        self.result = ""

    def _row_ok(self):
        if self.connected:
            return True
        return (self.reconnect_fired and self.healthy_after is not None
                and self.reopens_after_reconnect > self.healthy_after)

    def reopen(self):
        self.screen = "top"
        if self.reconnect_fired:
            self.reopens_after_reconnect += 1

    def _top(self):
        text = _fx("mcp_picker_2026-11_list_http_failed.txt")
        if self._row_ok():
            text = text.replace("\u2718 remote-http", "\u2714 remote-http          4 tools")
        return text

    def _detail(self):
        if self._row_ok() and self.connected:
            return _fx("mcp_picker_2026-11_detail_http_connected.txt")
        text = _fx("mcp_picker_2026-11_detail_http_failed.txt")
        if not self.offer_reconnect:
            text = text.replace("  \u276f Reconnect\n", "  \u276f Disable in this project\n")
            text = text.replace("    Disable in this project\n    Back",
                                "    Back").replace(
                "  \u276f Disable in this project\n    Back",
                "  \u276f Disable in this project\n    Back")
        return text

    def run(self, cmd, timeout=10):
        if cmd[:2] == ["tmux", "capture-pane"]:
            if self.screen == "top":
                return self._top(), 0
            if self.screen == "detail":
                return self._detail(), 0
            return self.result or "\u276f \n", 0
        if cmd[:2] == ["tmux", "send-keys"]:
            key = cmd[-1]
            self.keys.append(key)
            if key == "Escape":
                self.screen = "top" if self.screen == "detail" else "closed"
            elif self.screen == "top" and key == "Enter":
                self.screen = "detail"
            elif self.screen == "detail" and key == "Enter":
                self.screen = "closed"
                self.reconnect_fired = True
                self.result = f"Reconnected to {self.NAME}.\n"
        return "", 0


class LiveStateDecidesTest(unittest.TestCase):
    NAME = "remote-http"

    def setUp(self):
        self.mod = _import_self_mcp_reconnect()
        self.events = []
        self.mod.emit_event = lambda *a, **k: self.events.append((a, k))
        self.cli_calls = []

        def cli(server):
            self.cli_calls.append(server)
            return f"{server}: http://x (HTTP) - \u2714 Connected"
        self.mod.verify_via_cli = cli

    def _drive(self, pane):
        mod = self.mod
        mod.sc.run = mock.MagicMock(side_effect=pane.run)
        mod.sc.capture_pane_text = mock.MagicMock(return_value="<mocked>")
        mod.sc.ensure_main_loop_focus = mock.MagicMock(return_value=True)
        mod.sc.inject = mock.MagicMock(
            side_effect=lambda _p, text, **_k: pane.reopen() if text == "/mcp" else None)
        mod.time.sleep = lambda *_a, **_k: None
        mod.VERIFY_SECONDS = 0.4
        return mod.do_reconnect("p:0.0", self.NAME, menu_timeout=0.3, result_timeout=0.3)

    def test_failed_row_presses_the_reconnect_action_and_verifies_live(self):
        pane = LivePane(start_connected=False, healthy_after=0)
        result = self._drive(pane)
        self.assertTrue(result["ok"], result)
        self.assertEqual(result["live_state"], "connected")
        self.assertTrue(pane.reconnect_fired)
        self.assertEqual(self.cli_calls, [], "claude mcp list must never decide success")
        self.assertEqual(pane.screen, "closed")

    def test_reconnect_line_ok_but_row_still_failed_is_failure_with_screen(self):
        pane = LivePane(start_connected=False, healthy_after=None)
        result = self._drive(pane)
        self.assertFalse(result["ok"])
        self.assertEqual(result["code"], 5)
        self.assertEqual(result["live_state"], "failed")
        self.assertIn("remote-http", result["screen"])
        self.assertIn("\u2718", result["screen"])
        self.assertEqual(pane.screen, "closed")

    def test_row_recovers_within_the_retry_window(self):
        pane = LivePane(start_connected=False, healthy_after=2)
        self.mod.VERIFY_SECONDS = 5.0
        result = self._drive(pane)
        self.assertTrue(result["ok"], result)
        self.assertGreaterEqual(pane.reopens_after_reconnect, 3)

    def test_live_connected_without_reconnect_is_noop_success(self):
        pane = LivePane(start_connected=True)
        # the list fixture is failed; model a healthy session's list too
        orig = pane._top
        pane._top = lambda: orig().replace("\u2718 remote-http", "\u2714 remote-http          4 tools")
        result = self._drive(pane)
        self.assertTrue(result["ok"], result)
        self.assertEqual(result["status"], "already connected")
        self.assertEqual(result["live_state"], "connected")
        self.assertFalse(pane.reconnect_fired)
        self.assertEqual(self.cli_calls, [])

    def test_cli_connected_cannot_rescue_a_failed_live_row_without_reconnect(self):
        # The exact production misfire: live row failed, no Reconnect parsed,
        # `claude mcp list` says Connected -> must NOT be ok.
        pane = LivePane(start_connected=False, healthy_after=None, offer_reconnect=False)
        result = self._drive(pane)
        self.assertFalse(result["ok"])
        self.assertEqual(result["code"], 4)
        self.assertEqual(result["live_state"], "failed")
        self.assertIn("Status:", result["screen"])
        self.assertEqual(self.cli_calls, [])

    def test_do_run_reports_cli_vs_live_discrepancy(self):
        mod = self.mod
        mod.sc.find_claude_pane = lambda: "p:0.0"
        mod._write_state = lambda *_a, **_k: None
        mod.do_reconnect = lambda *a, **k: {
            "ok": False, "server": self.NAME, "code": 5, "live_state": "failed",
            "reason": "x"}
        args = mock.MagicMock(pane=None, server=self.NAME, menu_timeout=1, result_timeout=1,
                              verify=True, notify=False)
        result = mod.do_run(args)
        self.assertIn("discrepancy", result)
        self.assertIn("live state wins", result["discrepancy"])
        self.assertFalse(result["ok"])

    def test_do_run_no_discrepancy_when_they_agree(self):
        mod = self.mod
        mod.sc.find_claude_pane = lambda: "p:0.0"
        mod._write_state = lambda *_a, **_k: None
        mod.do_reconnect = lambda *a, **k: {
            "ok": True, "server": self.NAME, "code": 0, "live_state": "connected"}
        args = mock.MagicMock(pane=None, server=self.NAME, menu_timeout=1, result_timeout=1,
                              verify=True, notify=False)
        self.assertNotIn("discrepancy", mod.do_run(args))


if __name__ == "__main__":
    unittest.main()
