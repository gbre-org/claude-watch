#!/usr/bin/env python3
"""Tests for the item-level ``agent_id`` owner fallback in ``_classify_owner``.

Owner-attribution gap (#3615/#3617): a subagent RESUMED onto a rotated
queue id (d6cb -> e8cd rebind) keeps its ORIGINAL spawn marker, so
claude-watch's active-agents map still keys it under the OLD qid and
``agent_by_qid.get(item_id)`` misses it — the dashboard shows "owner
unknown" though the agent is alive and running the item.

``session-task queue register`` now stamps the true owner's ``agent_id``
on the item. ``_classify_owner`` honors that stamp: it recovers the
agent's liveness from the same active-agents state (the rebound agent is
present under its old qid) and reports ``mode='agent'`` instead of
``'unknown'``.

Run::

    python3 queue-minisite/test_owner_stamp_fallback.py
"""

from __future__ import annotations

import os
import sys
import tempfile
import unittest
from datetime import datetime, timezone
from pathlib import Path

HERE = Path(__file__).resolve().parent


class ClassifyOwnerStampTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.tmp = tempfile.mkdtemp()
        # _classify_owner is a pure function, but importing app reads a few
        # env-configured paths at import time — point them at harmless
        # nonexistent files.
        os.environ["QUEUE_JSON"] = str(Path(cls.tmp) / "queue.json")
        os.environ["AGENT_STATE_JSON"] = str(Path(cls.tmp) / "no-agents.json")
        os.environ["AGENTS_JSONL_ROOT"] = str(Path(cls.tmp) / "no-jsonl")
        os.environ["QUEUE_LOG_ARCHIVE_DIR"] = str(Path(cls.tmp) / "no-archive")
        os.environ["WORKLOAD_LOG_DIR"] = str(Path(cls.tmp) / "no-workloads")
        sys.path.insert(0, str(HERE))
        import app as appmod  # noqa: E402

        cls.app = appmod
        cls.now = datetime(2026, 8, 11, 12, 0, 0, tzinfo=timezone.utc)

    def test_active_agents_record_still_wins_for_own_qid(self):
        """Baseline: a record keyed on the item's own qid takes precedence
        over any stamped id (unchanged behavior)."""
        item = {"id": "q-2026-08-11-aaaa", "agent_id": "astamped00000000"}
        agent_by_qid = {
            "q-2026-08-11-aaaa": {
                "agent_id": "adirect000000000",
                "alive": True,
                "jsonl_age_seconds": 5,
            }
        }
        owner = self.app._classify_owner(item, self.now, agent_by_qid)
        self.assertEqual(owner["mode"], "agent")
        self.assertEqual(owner["agent_id"], "adirect000000000")

    def test_rebound_owner_recovered_from_old_qid_record(self):
        """The rebound agent's record lives under its OLD qid; the stamp
        lets us recover its liveness and attribute the NEW item."""
        item = {"id": "q-2026-08-11-e8cd", "agent_id": "arebound00000000"}
        # active-agents still keys the agent under the original qid d6cb.
        agent_by_qid = {
            "q-2026-08-11-d6cb": {
                "agent_id": "arebound00000000",
                "alive": True,
                "jsonl_age_seconds": 12,
            }
        }
        owner = self.app._classify_owner(item, self.now, agent_by_qid)
        self.assertEqual(owner["mode"], "agent")
        self.assertEqual(owner["agent_id"], "arebound00000000")
        self.assertTrue(owner["alive"])
        self.assertEqual(owner["jsonl_age_seconds"], 12)
        self.assertFalse(owner["is_starting"])

    def test_stamped_owner_not_in_state_still_shows_owner(self):
        """Owner stamped but absent from active-agents: surface the known
        owner (not 'unknown'); alive is None so no orphan badge fires."""
        item = {"id": "q-2026-08-11-e8cd", "agent_id": "aghost0000000000"}
        owner = self.app._classify_owner(item, self.now, {})
        self.assertEqual(owner["mode"], "agent")
        self.assertEqual(owner["agent_id"], "aghost0000000000")
        self.assertIsNone(owner["alive"])

    def test_no_stamp_no_record_is_unknown(self):
        """No stamp and no record: the existing 'owner unknown' path."""
        item = {"id": "q-2026-08-11-e8cd"}
        owner = self.app._classify_owner(item, self.now, {})
        self.assertEqual(owner["mode"], "unknown")
        self.assertEqual(owner["agent_id"], "")


if __name__ == "__main__":
    unittest.main()
