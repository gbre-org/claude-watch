#!/usr/bin/env python3
"""Tests for the queue-minisite collapsible header (botchat #1762).

The topbar gained a disclosure caret (``#header-toggle``) that folds the
header controls (source filter, liveness dot, info popup) down to a minimal
bar, leaving only the title (left) and the count pills (right). The collapsed
state is purely client-side, persisted in localStorage under
``qsite_header_collapsed`` and restored flash-free by a <head> pre-paint guard
(mirroring the pr-watch minisite's ``header-collapsed`` pattern).

These tests pin the server-side first paint: the caret button, the pre-paint
guard script, and the localStorage key are all present in the rendered HTML.
(The interactive toggle + persistence live in info.js; they are exercised
client-side.)

Run::

    python3 queue-minisite/test_header_collapse.py
"""

from __future__ import annotations

import json
import os
import shutil
import sys
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent


def _write_queue(path: Path, items: list[dict]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with open(path, "w") as f:
        json.dump({"schema_version": 3, "items": items, "locked_scopes": {}}, f)


def _item(item_id: str, status: str, created_by: str = "main-loop") -> dict:
    return {
        "id": item_id,
        "summary": f"summary {item_id}",
        "description": "",
        "scope": [],
        "status": status,
        "priority": 5,
        "created_by": created_by,
        "created_at": "2026-06-01T00:00:00+00:00",
        "registered_at": "2026-06-01T00:00:00+00:00",
        "completed_at": "2026-06-01T00:05:00+00:00",
        "abandoned_at": "2026-06-01T00:05:00+00:00",
    }


class HeaderCollapseTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.tmp = tempfile.mkdtemp(prefix="qmin-header-collapse-")
        cls.queue_actual = Path(cls.tmp) / ".config/session/queue.json"
        os.environ["QUEUE_JSON"] = str(cls.queue_actual)
        os.environ["AGENT_STATE_JSON"] = str(Path(cls.tmp) / "no-agents.json")
        os.environ["AGENTS_JSONL_ROOT"] = str(Path(cls.tmp) / "no-jsonl")
        os.environ["QUEUE_LOG_ARCHIVE_DIR"] = str(Path(cls.tmp) / "no-archive")
        os.environ["WORKLOAD_LOG_DIR"] = str(Path(cls.tmp) / "no-workloads")

        sys.path.insert(0, str(HERE))
        for mod in list(sys.modules):
            if mod in ("app", "claude_agents"):
                del sys.modules[mod]
        import app as appmod  # noqa: E402

        cls.appmod = appmod
        cls.client = appmod.app.test_client()

    @classmethod
    def tearDownClass(cls):
        shutil.rmtree(cls.tmp, ignore_errors=True)

    def setUp(self):
        self.appmod._cache.fetched_at = 0.0
        _write_queue(self.queue_actual, [_item("q-1", "running")])
        self.appmod._cache.fetched_at = 0.0

    def _html(self) -> str:
        return self.client.get("/").data.decode("utf-8", errors="replace")

    def test_header_toggle_caret_present(self):
        """The disclosure caret button renders in the topbar title."""
        html = self._html()
        self.assertIn('id="header-toggle"', html)
        self.assertIn('class="header-toggle"', html)
        # Starts expanded (aria-expanded=true); JS syncs on interaction.
        self.assertIn('aria-expanded="true"', html)

    def test_prepaint_guard_and_localstorage_key(self):
        """The <head> pre-paint guard reads the persisted collapsed state."""
        html = self._html()
        self.assertIn("qsite_header_collapsed", html)
        self.assertIn("header-collapsed", html)

    def test_clock_lives_in_info_popup_not_inline(self):
        """Regression guard for #492: the last-fetch clock is inside the info
        dropdown ("last fetch" row), NOT an inline topbar span."""
        html = self._html()
        # The clock is a labeled row inside the info dropdown.
        self.assertIn("last fetch", html)
        self.assertIn('id="info-dropdown"', html)
        # It must appear AFTER the dropdown opens (i.e. nested inside it),
        # never as a standalone inline .ts sibling of the count pills.
        dropdown_at = html.index('id="info-dropdown"')
        last_fetch_at = html.index("last fetch")
        self.assertGreater(last_fetch_at, dropdown_at)


if __name__ == "__main__":
    unittest.main(verbosity=2)
