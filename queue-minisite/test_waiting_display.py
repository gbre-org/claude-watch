#!/usr/bin/env python3
"""A `waiting` queue item renders as "waiting on <thing>" -- never as running,
never as an orphan -- in the queue site's parked section.

A finished agent's unacked completion renders as "agent finished - awaiting
ack" with the agent's final message tail. Run::

    python3 queue-minisite/test_waiting_display.py
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
        "id": item_id, "summary": f"summary {item_id}", "description": "",
        "scope": [], "status": status, "priority": 5,
        "created_by": "main-loop", "created_at": "2026-06-01T00:00:00+00:00",
    }
    d.update(extra)
    return d


class WaitingDisplayTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.tmp = tempfile.mkdtemp(prefix="qmin-waiting-")
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

    def _write(self, items):
        self.queue_actual.parent.mkdir(parents=True, exist_ok=True)
        self.queue_actual.write_text(json.dumps(
            {"schema_version": 3, "items": items, "locked_scopes": {}}))
        self.appmod._cache.fetched_at = 0.0

    def test_waiting_on_pr_badge_and_section(self):
        self._write([_item(
            "q-w1", "waiting", waiting_at="2026-06-02T00:00:00+00:00",
            wait_on={"kind": "pr", "ref": "https://h/o/r/pull/7"},
            wait_deadline="2026-06-02T04:00:00+00:00")])
        payload = self.client.get("/api/queue").get_json()
        self.assertEqual([i["id"] for i in payload["blocked"]], ["q-w1"])
        self.assertEqual(payload["running"], [])
        it = payload["blocked"][0]
        self.assertTrue(it["badge_label"].startswith("waiting on pr:"))
        self.assertIn("Waiting on pr:https://h/o/r/pull/7", it["block_reason"])
        html = self.client.get("/").get_data(as_text=True)
        self.assertIn("waiting on pr:", html)

    def test_unacked_completion_badge(self):
        self._write([_item(
            "q-w2", "waiting", waiting_at="2026-06-02T00:00:00+00:00",
            wait_on={"kind": "coordinator", "ref": "ack"},
            pending_completion={"agent_id": "a", "at": "x",
                                "message_tail": "opened PR 12"})])
        it = self.client.get("/api/queue").get_json()["blocked"][0]
        self.assertEqual(it["badge_label"], "agent finished - awaiting ack")
        self.assertIn("UNACKED", it["block_reason"])
        self.assertIn("opened PR 12", it["block_reason"])

    def test_plain_blocked_unchanged(self):
        self._write([_item("q-b", "blocked", block_reason="human")])
        it = self.client.get("/api/queue").get_json()["blocked"][0]
        self.assertEqual(it["badge_label"], "blocked")
        self.assertEqual(it["block_reason"], "human")


if __name__ == "__main__":
    unittest.main()
