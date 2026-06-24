#!/usr/bin/env python3
"""Tests for --no-queue hostjob visibility in queue-minisite.

The minisite's `_render_payload` builds its buckets from queue.json items
ONLY. A hostjob run WITHOUT a queue row — e.g. `hostjob run --no-queue`, or
a main-loop hostjob (`cw-deploy-*`) — is therefore INVISIBLE while running,
because nothing in queue.json represents it.

The fix (q-2026-06-24-0368): scan the hostjob state dir
(`<HOSTJOB_LOG_DIR>/<label>/status.json`) and, for each RUNNING hostjob not
already represented by a real queue row, synthesize a VIRTUAL running queue
item and append it to the `running` bucket. Dedup against (a) the real
queue_id and (b) any real `hostjob:<label>` scope item so this complements
q-2026-06-24-2c98 (once that makes every hostjob create a real row, both
dedup conditions fire and the synthesizer is a no-op).

Run::

    python3 -m pytest queue-minisite/test_hostjob_visibility.py -v
"""

from __future__ import annotations

import json
import os
import sys
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
for _mod in list(sys.modules):
    if _mod in ("app", "claude_agents"):
        del sys.modules[_mod]
import app as appmod  # noqa: E402


def _write_status(root: Path, label: str, *, status: str, queue_id=None,
                  cmd=None, started_at=1782315446.5):
    d = root / label
    d.mkdir(parents=True, exist_ok=True)
    payload = {
        "label": label,
        "cmd": cmd if cmd is not None else ["sleep", "120"],
        "cwd": None,
        "status": status,
        "rc": None,
        "pid": 1234,
        "reaper_pid": 1233,
        "started_at": started_at,
        "ended_at": None,
        "queue_id": queue_id,
    }
    (d / "status.json").write_text(json.dumps(payload))


def _running_queue_item(qid, *, scope):
    return {
        "id": qid,
        "summary": "real running item",
        "scope": scope,
        "status": "running",
        "created_at": "2026-06-24T00:00:00+00:00",
        "registered_at": "2026-06-24T00:00:00+00:00",
        "created_by": "main-loop",
    }


class HostjobVisibilityTest(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self._dir = Path(self._tmp.name)
        self._orig_hostjob_dir = appmod.HOSTJOB_LOG_DIR
        self._orig_cached_queue = appmod._cached_queue
        appmod.HOSTJOB_LOG_DIR = str(self._dir)

    def tearDown(self):
        appmod.HOSTJOB_LOG_DIR = self._orig_hostjob_dir
        appmod._cached_queue = self._orig_cached_queue
        self._tmp.cleanup()

    def _render(self, items):
        appmod._cached_queue = lambda: ({"items": items}, None)
        with appmod.app.test_request_context("/"):
            return appmod._render_payload()

    def _find_running(self, state, *, id_=None, label=None):
        for r in state["running"]:
            if id_ is not None and r["id"] == id_:
                return r
            if label is not None and r.get("hostjob_label") == label:
                return r
        return None

    # Case 1: --no-queue running hostjob (queue_id=null) -> appears in running.
    def test_no_queue_running_hostjob_appears(self):
        _write_status(self._dir, "cw-deploy-abc", status="running",
                      queue_id=None, cmd=["make", "deploy"])
        state = self._render([])
        r = self._find_running(state, id_="hostjob:cw-deploy-abc")
        self.assertIsNotNone(r, "synthesized hostjob should appear in running")
        self.assertEqual(r["status"], "running")
        self.assertTrue(r["is_hostjob_virtual"])
        self.assertEqual(r["created_by"], "hostjob")
        self.assertEqual(r["hostjob_label"], "cw-deploy-abc")
        self.assertEqual(r["scope"], ["hostjob:cw-deploy-abc"])
        self.assertIn("make deploy", r["description"])
        self.assertEqual(state["totals"]["running"], 1)
        # "hostjob" surfaces in the sources facet.
        self.assertIn("hostjob", state["sources"])

    # Case 2: running hostjob whose queue_id IS a real queue item -> no dup.
    def test_hostjob_with_real_queue_id_not_double_counted(self):
        real = _running_queue_item("q-2026-06-24-aaaa", scope=["repo:x"])
        _write_status(self._dir, "somejob", status="running",
                      queue_id="q-2026-06-24-aaaa")
        state = self._render([real])
        # No synthetic dup with id hostjob:<label>.
        self.assertIsNone(self._find_running(state, id_="hostjob:somejob"))
        ids = [r["id"] for r in state["running"]]
        self.assertEqual(ids, ["q-2026-06-24-aaaa"])
        self.assertEqual(state["totals"]["running"], 1)

    # Case 3: running hostjob whose label already has a real hostjob:<label>
    # scope item -> not duplicated.
    def test_hostjob_with_real_scope_label_not_duplicated(self):
        real = _running_queue_item(
            "q-2026-06-24-bbbb", scope=["hostjob:dupjob"])
        # queue_id null so dedup must fire on the label, not the id.
        _write_status(self._dir, "dupjob", status="running", queue_id=None)
        state = self._render([real])
        self.assertIsNone(self._find_running(state, id_="hostjob:dupjob"))
        ids = [r["id"] for r in state["running"]]
        self.assertEqual(ids, ["q-2026-06-24-bbbb"])
        self.assertEqual(state["totals"]["running"], 1)

    # Case 4: done/crashed/stopped hostjobs -> NOT synthesized.
    def test_non_running_hostjobs_not_synthesized(self):
        _write_status(self._dir, "donejob", status="done", queue_id=None)
        _write_status(self._dir, "crashjob", status="crashed", queue_id=None)
        _write_status(self._dir, "stopjob", status="stopped", queue_id=None)
        state = self._render([])
        self.assertEqual(state["running"], [])
        self.assertEqual(state["totals"]["running"], 0)
        self.assertNotIn("hostjob", state["sources"])

    # Case 5: missing / empty HOSTJOB_LOG_DIR -> no crash, empty synthesis.
    def test_empty_and_missing_dir_no_crash(self):
        # Empty dir (setUp created it; it's empty).
        state = self._render([])
        self.assertEqual(state["running"], [])
        # Missing dir.
        appmod.HOSTJOB_LOG_DIR = str(self._dir / "does-not-exist")
        state2 = self._render([])
        self.assertEqual(state2["running"], [])

    # Sidecar files (broker.json, fd-*.wfapi.json) under the root must be
    # skipped without raising — the real hostjob dir mixes these in.
    def test_sidecar_files_skipped(self):
        (self._dir / "broker.json").write_text("{}")
        (self._dir / "fd-2440.wfapi.json").write_text("not json")
        _write_status(self._dir, "realjob", status="running", queue_id=None)
        state = self._render([])
        r = self._find_running(state, id_="hostjob:realjob")
        self.assertIsNotNone(r)
        self.assertEqual(state["totals"]["running"], 1)


if __name__ == "__main__":
    unittest.main()
