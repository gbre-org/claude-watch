#!/usr/bin/env python3
"""Tests for the queue-minisite source-filter dropdown data.

The topbar source dropdown filters queue items by their ``created_by``
producer (``main-loop``, ``workload``, …). Before the fix there was no
dropdown at all — and crucially no GLOBAL distinct-source query to feed
one, so the operator saw "no sources". The fix adds a ``sources`` list to
the render payload, computed as the distinct ``created_by`` values across
EVERY item in the queue (not just one visible section). These tests pin
that contract on ``/api/queue`` (the JSON the SPA refresh tick reads) and
the home page render.

Run::

    python3 queue-minisite/test_source_filter.py
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


def _item(item_id: str, status: str, created_by: str) -> dict:
    """Minimal queue item with the fields _shape / _render_payload read."""
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


class SourceFilterTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.tmp = tempfile.mkdtemp(prefix="qmin-source-filter-")
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

    def test_sources_are_distinct_created_by_sorted(self):
        """sources = sorted distinct created_by across ALL items / sections."""
        _write_queue(
            self.queue_actual,
            [
                _item("q-1", "running", "main-loop"),
                _item("q-2", "pending", "workload"),
                _item("q-3", "done", "main-loop"),  # dup created_by
                _item("q-4", "abandoned", "cron-producer"),
            ],
        )
        self.appmod._cache.fetched_at = 0.0
        r = self.client.get("/api/queue")
        self.assertEqual(r.status_code, 200)
        body = r.get_json()
        self.assertIn("sources", body)
        # Distinct + sorted; pulled from every section (running/pending/
        # done/abandoned), not just one.
        self.assertEqual(
            body["sources"], ["cron-producer", "main-loop", "workload"]
        )

    def test_blank_created_by_excluded(self):
        """Empty / whitespace created_by is dropped (implicit 'all')."""
        _write_queue(
            self.queue_actual,
            [
                _item("q-1", "running", "main-loop"),
                _item("q-2", "pending", ""),
                _item("q-3", "pending", "   "),
            ],
        )
        self.appmod._cache.fetched_at = 0.0
        body = self.client.get("/api/queue").get_json()
        self.assertEqual(body["sources"], ["main-loop"])

    def test_empty_queue_yields_empty_sources(self):
        _write_queue(self.queue_actual, [])
        self.appmod._cache.fetched_at = 0.0
        body = self.client.get("/api/queue").get_json()
        self.assertEqual(body["sources"], [])

    def test_home_page_renders_source_options(self):
        """The server-side first paint includes the dropdown options."""
        _write_queue(
            self.queue_actual,
            [
                _item("q-1", "running", "main-loop"),
                _item("q-2", "pending", "workload"),
            ],
        )
        self.appmod._cache.fetched_at = 0.0
        html = self.client.get("/").data.decode("utf-8", errors="replace")
        self.assertIn('id="source-filter"', html)
        self.assertIn('<option value="main-loop">main-loop</option>', html)
        self.assertIn('<option value="workload">workload</option>', html)
        self.assertIn('<option value="">all sources</option>', html)


if __name__ == "__main__":
    unittest.main(verbosity=2)
