#!/usr/bin/env python3
"""Tests for hostjob status reconciliation in queue-minisite.

A `hostjob` (`examples/compose/bin/hostjob`) records its authoritative
lifecycle in ``<HOSTJOB_LOG_DIR>/<label>/status.json`` — the same file
``hostjob list`` reads to print ``done rc=N``. The reaper's flip of the bound
queue item to done/abandon on worker exit is only fail-soft, so a dropped flip
(or a later ``hostjob clean`` that removes the terminal state dir) leaves the
queue row stuck ``running`` forever. The minisite then renders the finished
hostjob in the running column indefinitely (Andrew, botchat #1527).

The minisite can't repair the upstream ``queue.json`` (that flip lives in
`examples/compose/bin/hostjob`), so it reconciles at render time:

  * ``_hostjob_effective_terminal`` consults status.json and, for a hostjob
    whose queue status is ``running`` but whose status.json is terminal (or
    whose state dir has been cleaned away past the starting window), returns
    the corrected effective status.
  * ``_shape`` DEMOTES the running item to that terminal status and sets
    ``is_reconciled_hostjob`` (+ errored / exit-code flags), so it lands in the
    done / abandoned bucket instead of running.

Run::

    python3 -m pytest queue-minisite/test_hostjob_status_reconcile.py -v
"""

from __future__ import annotations

import json
import sys
import tempfile
import unittest
from datetime import datetime, timedelta, timezone
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
for _mod in list(sys.modules):
    if _mod in ("app", "claude_agents"):
        del sys.modules[_mod]
import app as appmod  # noqa: E402


class HostjobReconcileTest(unittest.TestCase):
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

    def _running_item(self, label, *, age_seconds):
        # Registered `age_seconds` ago; queue.json still says running.
        started = datetime.now(timezone.utc) - timedelta(seconds=age_seconds)
        return {
            "id": f"q-{label}",
            "summary": f"hostjob: {label}",
            "scope": [f"hostjob:{label}"],
            "status": "running",
            "created_at": started.isoformat(),
            "registered_at": started.isoformat(),
            "started_at": started.isoformat(),
        }

    def _shape(self, item):
        now = datetime.now(timezone.utc)
        return appmod._shape(item, now, {}, items=[item], bindings={})

    # -- status.json present, terminal ------------------------------------

    def test_done_rc0_reconciles_running_to_done(self):
        self._write_status("okjob", {"status": "done", "rc": 0})
        s = self._shape(self._running_item("okjob", age_seconds=300))
        self.assertEqual(s["status"], "done")
        self.assertTrue(s["is_reconciled_hostjob"])
        self.assertFalse(s["is_errored_hostjob"])
        self.assertEqual(s["hostjob_exit_code"], "0")

    def test_done_nonzero_rc_reconciles_running_to_errored_abandoned(self):
        self._write_status("failjob", {"status": "done", "rc": 2})
        s = self._shape(self._running_item("failjob", age_seconds=300))
        self.assertEqual(s["status"], "abandoned")
        self.assertTrue(s["is_reconciled_hostjob"])
        self.assertTrue(s["is_errored_hostjob"])
        self.assertEqual(s["hostjob_exit_code"], "2")

    def test_crashed_reconciles_running_to_errored_abandoned(self):
        self._write_status("crashjob", {"status": "crashed", "rc": None})
        s = self._shape(self._running_item("crashjob", age_seconds=300))
        self.assertEqual(s["status"], "abandoned")
        self.assertTrue(s["is_reconciled_hostjob"])
        self.assertTrue(s["is_errored_hostjob"])

    def test_status_json_still_running_keeps_running(self):
        # The runner's own record says running — trust the reaper to flip;
        # do NOT second-guess (no cross-namespace pid check).
        self._write_status("livejob", {"status": "running", "rc": None, "pid": 999999})
        s = self._shape(self._running_item("livejob", age_seconds=300))
        self.assertEqual(s["status"], "running")
        self.assertFalse(s["is_reconciled_hostjob"])

    # -- status.json absent (cleaned / launch race) -----------------------

    def test_missing_state_dir_past_starting_window_reconciles_to_done(self):
        # No status.json + old registration => finished-and-cleaned.
        s = self._shape(self._running_item("gonejob", age_seconds=3600))
        self.assertEqual(s["status"], "done")
        self.assertTrue(s["is_reconciled_hostjob"])
        self.assertEqual(s["hostjob_exit_code"], "")

    def test_missing_state_dir_within_starting_window_keeps_running(self):
        # Just launched — status.json may land a beat after the queue row
        # registers. Must NOT mis-flip.
        s = self._shape(self._running_item("freshjob", age_seconds=5))
        self.assertEqual(s["status"], "running")
        self.assertFalse(s["is_reconciled_hostjob"])

    # -- non-hostjob items untouched --------------------------------------

    def test_non_hostjob_running_item_untouched(self):
        item = {
            "id": "q-agent",
            "summary": "an agent",
            "scope": ["repo:test"],
            "status": "running",
            "created_at": "2026-07-13T00:00:00+00:00",
            "registered_at": "2026-07-13T00:00:00+00:00",
        }
        s = self._shape(item)
        self.assertEqual(s["status"], "running")
        self.assertFalse(s["is_reconciled_hostjob"])

    def test_already_done_hostjob_not_reconciled(self):
        # Queue already terminal — reconciliation only acts on `running`.
        self._write_status("dj", {"status": "done", "rc": 0})
        item = {
            "id": "q-dj",
            "summary": "hostjob: dj",
            "scope": ["hostjob:dj"],
            "status": "done",
            "completed_at": "2026-07-13T00:00:00+00:00",
            "created_at": "2026-07-13T00:00:00+00:00",
        }
        s = self._shape(item)
        self.assertEqual(s["status"], "done")
        self.assertFalse(s["is_reconciled_hostjob"])

    # -- render-level bucketing -------------------------------------------

    def test_reconciled_done_lands_in_done_bucket_not_running(self):
        self._write_status("bkt", {"status": "done", "rc": 0})
        items = [self._running_item("bkt", age_seconds=300)]
        orig = appmod._cached_queue
        appmod._cached_queue = lambda: ({"items": items}, None)
        try:
            with appmod.app.test_request_context("/"):
                state = appmod._render_payload()
        finally:
            appmod._cached_queue = orig
        running_ids = [r["id"] for r in state["running"]]
        done_ids = [d["id"] for d in state["done_recent"]]
        self.assertNotIn("q-bkt", running_ids)
        self.assertIn("q-bkt", done_ids)


if __name__ == "__main__":
    unittest.main()
