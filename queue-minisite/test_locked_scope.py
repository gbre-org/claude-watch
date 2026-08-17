#!/usr/bin/env python3
"""Tests for LOCKED rendering of scope-lock-parked pending items.

When an operator parks a scope with ``session-task queue lock <scope>``, every
pending item whose scope overlaps that lock is held: the dispatcher's spawn
gate (``_item_is_ready`` in session-task) consults ``locked_scopes`` and
refuses to spawn it. The minisite mirrors that readiness logic in-process, but
used to ignore ``locked_scopes`` entirely — so a locked-parked item still
rendered as READY with a primary FORCE START button. That mislabel made a
parked item look like a ready item nothing was forcing (Andrew #4430/#4432).

These tests pin the fix:

  * a pending item whose scope overlaps a locked scope reports
    ``ready_now == False`` and ``locked == True`` in the payload, carries the
    blocking lock token + reason + a copyable ``queue unlock`` command, and
    renders a LOCKED badge (never a READY badge).
  * a pending item whose scope does NOT overlap any lock is unaffected — it
    still reports ``ready_now == True`` and renders READY, not LOCKED.

Run::

    python3 queue-minisite/test_locked_scope.py
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
        "created_at": "2026-08-01T00:00:00+00:00",
        "group_id": item_id,
    }
    d.update(extra)
    return d


class LockedScopeTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.tmp = tempfile.mkdtemp(prefix="qmin-locked-scope-")
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

    def _write(self, items: list[dict], locked_scopes: dict | None = None) -> None:
        self.queue_actual.parent.mkdir(parents=True, exist_ok=True)
        with open(self.queue_actual, "w") as f:
            json.dump(
                {
                    "schema_version": 3,
                    "items": items,
                    "locked_scopes": locked_scopes or {},
                },
                f,
            )
        self.appmod._cache.fetched_at = 0.0

    def _payload(self) -> dict:
        return self.client.get("/api/queue").get_json()

    def _pending_by_id(self, qid: str) -> dict:
        for it in self._payload().get("pending", []):
            if it["id"] == qid:
                return it
        raise AssertionError(f"{qid} not in pending section")

    def _html(self) -> str:
        resp = self.client.get("/")
        self.assertEqual(resp.status_code, 200)
        return resp.get_data(as_text=True)

    # -- payload-level assertions ------------------------------------------

    def test_locked_item_is_not_ready_and_flagged_locked(self):
        """A pending item overlapping a locked scope is LOCKED, not READY."""
        self._write(
            [_item("q-locked", "pending", scope=["repo:widgets"])],
            locked_scopes={
                "repo:widgets": {
                    "reason": "holding for the release freeze",
                    "locked_at": "2026-08-16T10:00:00+00:00",
                }
            },
        )
        it = self._pending_by_id("q-locked")
        # The core bug: ready_now must flip False for a locked-parked item so
        # the READY badge (gated on ready_now) never shows.
        self.assertFalse(it["ready_now"], "locked item must NOT be ready_now")
        self.assertTrue(it["locked"], "locked item must set locked=True")
        self.assertIn("repo:widgets", it["lock_blockers"])
        self.assertIn("repo:widgets", it["lock_reason"])
        self.assertIn("holding for the release freeze", it["lock_reason"])
        self.assertIn(
            "session-task queue unlock repo:widgets", it["unlock_commands"]
        )

    def test_ready_item_without_lock_is_unaffected(self):
        """A pending item whose scope does not overlap any lock stays READY."""
        self._write(
            [_item("q-free", "pending", scope=["repo:gadgets"])],
            locked_scopes={
                "repo:widgets": {"reason": "unrelated", "locked_at": ""}
            },
        )
        it = self._pending_by_id("q-free")
        self.assertTrue(it["ready_now"], "non-overlapping item must be ready")
        self.assertFalse(it["locked"])
        self.assertEqual(it["lock_blockers"], [])
        self.assertEqual(it["unlock_commands"], [])

    def test_lock_overlap_uses_repo_covers_path_semantics(self):
        """A repo: lock parks a narrower path: item in the same repo."""
        self._write(
            [_item("q-path", "pending", scope=["path:widgets/src/a.py"])],
            locked_scopes={"repo:widgets": {"reason": "", "locked_at": ""}},
        )
        it = self._pending_by_id("q-path")
        self.assertTrue(it["locked"])
        self.assertFalse(it["ready_now"])

    # -- rendered-HTML assertions ------------------------------------------

    def test_locked_card_renders_badge_reason_and_unlock(self):
        """The LOCKED card shows a locked badge, the reason, and the unlock cmd.

        Rendering ready_now=False alone would silently drop BOTH the READY and
        the LOCKED signal, leaving a bare pending card that says nothing about
        why it will not spawn. The card must positively explain the lock and
        offer the way out.
        """
        self._write(
            [_item("q-locked", "pending", scope=["repo:widgets"])],
            locked_scopes={
                "repo:widgets": {
                    "reason": "release freeze in effect",
                    "locked_at": "2026-08-16T10:00:00+00:00",
                }
            },
        )
        html = self._html()
        # Distinct LOCKED badge (state-locked), consequence note, reason, and
        # the copyable unlock command.
        self.assertIn("state-locked", html)
        self.assertIn("scope locked", html)
        self.assertIn("release freeze in effect", html)
        self.assertIn("session-task queue unlock repo:widgets", html)
        # The `locked` modifier lands on the card + demotes force-start.
        self.assertIn("force-start-locked", html)
        # The READY badge text must NOT appear for a locked item. The pending
        # badge itself always renders; assert the ready group-head badge
        # (title text unique to the READY badge) is absent.
        self.assertNotIn("ready to spawn (group head, all deps done)", html)

    def test_ready_card_still_shows_ready_not_locked(self):
        """Control: a non-locked ready item renders READY, no lock affordances."""
        self._write(
            [_item("q-free", "pending", scope=["repo:gadgets"])],
        )
        html = self._html()
        self.assertIn("ready to spawn (group head, all deps done)", html)
        self.assertNotIn("state-locked", html)
        self.assertNotIn("scope locked", html)


if __name__ == "__main__":
    unittest.main(verbosity=2)
