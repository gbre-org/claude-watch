#!/usr/bin/env python3
"""Tests for hostjob PENDING->RUNNING promotion in queue-minisite (Andrew
#8650/#8667: "every hostjob always visible + tailable + stoppable").

A hostjob whose queue row is still ``pending`` -- one that lost the launch-time
``queue register`` scope race, or was launched ``--no-queue`` -- can already
have a LIVE worker. Without promotion it renders in the pending backlog (with a
nonsensical "force start"), is not tailed as a hostjob, and its "abandon"
doesn't kill the worker. ``_shape`` consults the authoritative status.json (the
SAME source ``hostjob list`` / the demote path read) and, when the runner
EXPLICITLY reports the worker running, renders it exactly like a registered
running hostjob -- so it lands in the RUNNING section, tailable, with a working
Stop button.

Mirror of ``test_hostjob_status_reconcile.py`` (the opposite, demote direction).

Run::

    python3 queue-minisite/test_hostjob_promote_pending.py
"""

from __future__ import annotations

import json
import sys
import tempfile
import unittest
from datetime import datetime, timezone
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
for _mod in list(sys.modules):
    if _mod in ("app", "claude_agents"):
        del sys.modules[_mod]
import app as appmod  # noqa: E402


class HostjobPromoteTest(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self._orig_dir = appmod.HOSTJOB_LOG_DIR
        appmod.HOSTJOB_LOG_DIR = self._tmp.name

    def tearDown(self):
        appmod.HOSTJOB_LOG_DIR = self._orig_dir
        self._tmp.cleanup()

    def _write_status(self, label, data):
        d = Path(self._tmp.name) / label
        d.mkdir(parents=True, exist_ok=True)
        (d / "status.json").write_text(json.dumps(data))

    def _pending_item(self, label):
        # A hostjob row session-task added but never registered: no
        # registered_at / started_at, status still pending.
        return {
            "id": f"q-{label}",
            "summary": f"hostjob: {label}",
            "scope": [f"hostjob:{label}"],
            "status": "pending",
            "created_by": "hostjob",
            "created_at": "2026-06-01T00:00:00+00:00",
        }

    def _shape(self, item):
        now = datetime.now(timezone.utc)
        return appmod._shape(item, now, {}, items=[item], bindings={})

    def test_pending_with_running_status_promotes_to_running(self):
        self._write_status("livejob", {"status": "running", "rc": None, "pid": 12345})
        s = self._shape(self._pending_item("livejob"))
        self.assertEqual(s["status"], "running")
        self.assertTrue(s["is_reconciled_hostjob"])
        # Rendered as a hostjob -> tailable in the running section.
        self.assertEqual(s["hostjob_label"], "livejob")

    def test_pending_promote_anchors_age_on_status_json_start(self):
        epoch = datetime(2026, 6, 1, 12, 0, 0, tzinfo=timezone.utc).timestamp()
        self._write_status("tsjob", {"status": "running", "started_at": epoch})
        s = self._shape(self._pending_item("tsjob"))
        self.assertEqual(s["status"], "running")
        # started_at_iso is populated from the hostjob's own start, not left
        # blank (a pending row has no registered_at / started_at of its own).
        self.assertTrue(s["started_at_iso"])
        self.assertIn("2026-06-01T12:00:00", s["started_at_iso"])

    def test_pending_terminal_status_stays_pending(self):
        # A terminal status.json does NOT promote -- the reaper's own
        # done/abandon flip (which works on a pending row) is authoritative
        # for the terminal transition; we only ever promote on live `running`.
        self._write_status("donejob", {"status": "done", "rc": 0})
        s = self._shape(self._pending_item("donejob"))
        self.assertEqual(s["status"], "pending")
        self.assertFalse(s["is_reconciled_hostjob"])

    def test_pending_absent_status_stays_pending(self):
        # status.json not written yet (sub-second launch window) -> keep
        # pending; promotion kicks in once the runner records `running`.
        self._write_status("otherjob", {"status": "running"})  # populate surface
        s = self._shape(self._pending_item("nojob"))
        self.assertEqual(s["status"], "pending")
        self.assertFalse(s["is_reconciled_hostjob"])

    def test_non_hostjob_pending_untouched(self):
        item = {
            "id": "q-agent",
            "summary": "an agent task",
            "scope": ["repo:x"],
            "status": "pending",
            "created_at": "2026-06-01T00:00:00+00:00",
        }
        s = self._shape(item)
        self.assertEqual(s["status"], "pending")
        self.assertFalse(s["is_reconciled_hostjob"])


if __name__ == "__main__":
    unittest.main(verbosity=2)


class HostjobReviveClosedRowTest(HostjobPromoteTest):
    """A done/abandoned row whose worker is still running under that queue id
    renders RUNNING (visible) and records the stored status."""

    def _closed(self, label, status="done"):
        it = self._pending_item(label)
        it["status"] = status
        it["completed_at"] = "2026-06-01T00:05:00+00:00"
        return it

    def test_done_row_with_live_worker_renders_running(self):
        self._write_status("zlive", {"status": "running", "queue_id": "q-zlive"})
        for st in ("done", "abandoned", "quarantined"):
            s = self._shape(self._closed("zlive", st))
            self.assertEqual(s["status"], "running")
            self.assertEqual(s["hostjob_queue_status"], st)

    def test_done_row_owned_by_other_queue_id_stays_done(self):
        self._write_status("zold", {"status": "running", "queue_id": "q-other"})
        s = self._shape(self._closed("zold"))
        self.assertEqual(s["status"], "done")
        self.assertEqual(s["hostjob_queue_status"], "")

    def test_done_row_with_terminal_worker_stays_done(self):
        self._write_status("zdead", {"status": "done", "rc": 0, "queue_id": "q-zdead"})
        self.assertEqual(self._shape(self._closed("zdead"))["status"], "done")
