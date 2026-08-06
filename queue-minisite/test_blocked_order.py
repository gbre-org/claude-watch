#!/usr/bin/env python3
"""Tests for BLOCKED-section ordering (Andrew, 2026-08-05).

Andrew: "in q site sort blocked items by dated added *most recent first*".

The blocked section had grown past 45 parked items, ordered oldest-first, so
anything shelved tonight sat at the very bottom under weeks-old blockers. It
now renders **newest-added first**.

The load-bearing subtlety these tests pin: the sort key is ``created_at``
(when the item entered the queue), NOT ``blocked_at`` (when it was parked).
Both fields exist on every queue item and they genuinely differ — an item can
sit pending for minutes or days before being blocked. The obvious-looking
implementation (reusing ``age_seconds``) would silently sort by date-BLOCKED,
because ``_shape`` anchors a blocked item's age on ``blocked_at``. That bug is
invisible whenever the two orders happen to coincide, so
``test_sorts_by_created_at_not_blocked_at`` deliberately constructs a fixture
where they disagree.

Scope guard: Andrew asked about BLOCKED only.
``test_other_sections_unchanged`` pins running / pending / done / abandoned
ordering so this change can't quietly reshuffle sections he reads constantly.

Run::

    python3 queue-minisite/test_blocked_order.py
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


def _item(item_id: str, status: str, **extra) -> dict:
    d = {
        "id": item_id,
        "summary": f"summary {item_id}",
        "description": "",
        "scope": [],
        "status": status,
        "priority": 5,
        "created_by": "main-loop",
        "created_at": "2026-06-01T00:00:00+00:00",
    }
    d.update(extra)
    return d


class BlockedOrderTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.tmp = tempfile.mkdtemp(prefix="qmin-blocked-order-")
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

    def _write(self, items: list[dict]) -> None:
        self.queue_actual.parent.mkdir(parents=True, exist_ok=True)
        with open(self.queue_actual, "w") as f:
            json.dump(
                {"schema_version": 3, "items": items, "locked_scopes": {}}, f
            )
        self.appmod._cache.fetched_at = 0.0

    def _section(self, key: str) -> list[str]:
        payload = self.client.get("/api/queue").get_json()
        return [it["id"] for it in payload.get(key, [])]

    def test_newest_added_first(self):
        """Blocked items render most-recently-added first."""
        self._write(
            [
                _item(
                    "q-old",
                    "blocked",
                    created_at="2026-06-25T09:05:32+00:00",
                    blocked_at="2026-06-25T09:05:33+00:00",
                ),
                _item(
                    "q-new",
                    "blocked",
                    created_at="2026-08-05T20:38:00+00:00",
                    blocked_at="2026-08-05T20:39:00+00:00",
                ),
                _item(
                    "q-mid",
                    "blocked",
                    created_at="2026-07-24T03:33:59+00:00",
                    blocked_at="2026-07-24T03:34:04+00:00",
                ),
            ]
        )
        self.assertEqual(self._section("blocked"), ["q-new", "q-mid", "q-old"])

    def test_sorts_by_created_at_not_blocked_at(self):
        """`created_at` wins over `blocked_at` when the two disagree.

        ``q-early-add`` was added FIRST but parked LAST; ``q-late-add`` was
        added SECOND but parked FIRST. Sorting by date-added puts the later
        ADDITION on top; sorting by date-blocked (or by `age_seconds`, which
        for a blocked item anchors on `blocked_at`) would invert this.
        """
        self._write(
            [
                _item(
                    "q-early-add",
                    "blocked",
                    created_at="2026-08-01T00:00:00+00:00",
                    blocked_at="2026-08-05T00:00:00+00:00",
                ),
                _item(
                    "q-late-add",
                    "blocked",
                    created_at="2026-08-02T00:00:00+00:00",
                    blocked_at="2026-08-03T00:00:00+00:00",
                ),
            ]
        )
        self.assertEqual(
            self._section("blocked"),
            ["q-late-add", "q-early-add"],
            "blocked section must sort by date ADDED, not date blocked",
        )

    def test_missing_created_at_sorts_last(self):
        """A legacy item with no `created_at` sinks instead of crashing."""
        self._write(
            [
                _item("q-nodate", "blocked", created_at=""),
                _item(
                    "q-dated",
                    "blocked",
                    created_at="2026-07-01T00:00:00+00:00",
                    blocked_at="2026-07-01T00:00:00+00:00",
                ),
            ]
        )
        self.assertEqual(self._section("blocked"), ["q-dated", "q-nodate"])

    def test_other_sections_unchanged(self):
        """Scope guard: only BLOCKED ordering changed.

        running   — oldest-running first
        pending   — ready_now first, then priority asc
        done      — most-recently-completed first
        abandoned — most-recently-abandoned first
        """
        self._write(
            [
                _item(
                    "q-run-new",
                    "running",
                    registered_at="2026-08-05T20:00:00+00:00",
                ),
                _item(
                    "q-run-old",
                    "running",
                    registered_at="2026-08-01T00:00:00+00:00",
                ),
                _item("q-pen-lo", "pending", priority=9),
                _item("q-pen-hi", "pending", priority=1),
                _item(
                    "q-done-old",
                    "done",
                    completed_at="2026-08-01T00:00:00+00:00",
                ),
                _item(
                    "q-done-new",
                    "done",
                    completed_at="2026-08-05T00:00:00+00:00",
                ),
                _item(
                    "q-aba-old",
                    "abandoned",
                    abandoned_at="2026-08-01T00:00:00+00:00",
                ),
                _item(
                    "q-aba-new",
                    "abandoned",
                    abandoned_at="2026-08-05T00:00:00+00:00",
                ),
            ]
        )
        # running: longest-running first
        self.assertEqual(self._section("running"), ["q-run-old", "q-run-new"])
        # pending: lower priority number first
        self.assertEqual(self._section("pending"), ["q-pen-hi", "q-pen-lo"])
        # done / abandoned: newest terminal event first
        self.assertEqual(
            self._section("done_recent"), ["q-done-new", "q-done-old"]
        )
        self.assertEqual(
            self._section("abandoned_recent"), ["q-aba-new", "q-aba-old"]
        )


if __name__ == "__main__":
    unittest.main(verbosity=2)
