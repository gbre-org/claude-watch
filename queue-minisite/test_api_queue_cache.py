#!/usr/bin/env python3
"""Tests for the ``GET /api/queue`` cost controls.

The dashboard polls ``/api/queue`` continuously and every build shapes every
queue row, so an unbounded poller on a large store pinned the container's CPU
(botchat #7169). Pinned here:

* the rendered body is built once and reused across requests (and clients);
* the body carries an ETag and ``If-None-Match`` answers ``304`` with no body;
* clock-only drift (ages, fetched_at) does not change the ETag, a real queue
  change does;
* an unchanged queue.json is not re-parsed after the TTL lapses;
* a write-route cache bust (``fetched_at = 0``) is honoured immediately;
* the terminal windows are bounded by default and ``?history=N`` widens them
  (capped), bad values are 400.

Run::

    python3 queue-minisite/test_api_queue_cache.py
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
    }
    d.update(extra)
    return d


def _done(i: int) -> dict:
    return _item(
        f"q-done-{i:04d}",
        "done",
        completed_at=f"2026-08-02T00:{i // 60:02d}:{i % 60:02d}+00:00",
    )


class ApiQueueCacheTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.tmp = tempfile.mkdtemp(prefix="qmin-api-cache-")
        cls.queue = Path(cls.tmp) / ".config/session/queue.json"
        os.environ["QUEUE_JSON"] = str(cls.queue)
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
        self.queue.parent.mkdir(parents=True, exist_ok=True)
        with open(self.queue, "w") as f:
            json.dump({"schema_version": 3, "items": items, "locked_scopes": {}}, f)
        self.appmod._cache.fetched_at = 0.0  # what the write routes do

    def setUp(self):
        self._write([_item("q-p1", "pending"), _done(1)])
        # Count payload builds.
        self.builds = 0
        real = self.appmod._render_payload

        def counting(*a, **kw):
            self.builds += 1
            return real(*a, **kw)

        self._real = real
        self.appmod._render_payload = counting

    def tearDown(self):
        self.appmod._render_payload = self._real

    def test_body_built_once_across_requests(self):
        for _ in range(5):
            self.assertEqual(self.client.get("/api/queue").status_code, 200)
        self.assertEqual(self.builds, 1)

    def test_etag_304_without_body(self):
        r = self.client.get("/api/queue")
        etag = r.headers["ETag"]
        self.assertTrue(etag)
        r2 = self.client.get("/api/queue", headers={"If-None-Match": etag})
        self.assertEqual(r2.status_code, 304)
        self.assertEqual(r2.data, b"")
        r3 = self.client.get("/api/queue", headers={"If-None-Match": '"nope"'})
        self.assertEqual(r3.status_code, 200)

    def test_etag_ignores_clock_drift_but_tracks_changes(self):
        etag = self.client.get("/api/queue").headers["ETag"]
        # Force a rebuild with only the clock moved.
        self.appmod._api_cache.built_at = 0.0
        self.assertEqual(self.client.get("/api/queue").headers["ETag"], etag)
        self.assertEqual(self.builds, 2)
        # A real change flips it.
        self._write([_item("q-p1", "pending"), _item("q-p2", "pending"), _done(1)])
        self.assertNotEqual(self.client.get("/api/queue").headers["ETag"], etag)

    def test_write_route_bust_is_honoured_immediately(self):
        self.client.get("/api/queue")
        self._write([_item("q-p9", "pending")])
        ids = [i["id"] for i in self.client.get("/api/queue").get_json()["pending"]]
        self.assertEqual(ids, ["q-p9"])

    def test_unchanged_file_not_reparsed_after_ttl(self):
        self.client.get("/api/queue")
        reads = []
        real_read = self.appmod._read_queue
        self.appmod._read_queue = lambda: (reads.append(1), real_read())[1]
        try:
            self.appmod._cache.fetched_at -= self.appmod.CACHE_TTL_SECONDS + 1
            self.appmod._api_cache.built_at = 0.0
            self.client.get("/api/queue")
        finally:
            self.appmod._read_queue = real_read
        self.assertEqual(reads, [])

    def test_changed_file_is_reparsed_after_ttl(self):
        self.client.get("/api/queue")
        # Rewrite WITHOUT the bust, then let the TTL lapse.
        with open(self.queue, "w") as f:
            json.dump(
                {"schema_version": 3, "items": [_item("q-new", "pending")],
                 "locked_scopes": {}}, f)
        self.appmod._cache.fetched_at -= self.appmod.CACHE_TTL_SECONDS + 1
        self.appmod._api_cache.built_at = 0.0
        ids = [i["id"] for i in self.client.get("/api/queue").get_json()["pending"]]
        self.assertEqual(ids, ["q-new"])

    def test_history_param_bounds_terminal_rows(self):
        self._write([_item("q-run", "running")] + [_done(i) for i in range(80)])
        d = self.client.get("/api/queue").get_json()
        self.assertEqual(len(d["done_recent"]), self.appmod.RECENT_DONE_LIMIT)
        self.assertEqual(d["totals"]["done"], 80)  # totals stay complete
        self.assertEqual(len(d["running"]), 1)  # non-terminal always present
        d = self.client.get("/api/queue?history=60").get_json()
        self.assertEqual(len(d["done_recent"]), 60)
        d = self.client.get("/api/queue?history=0").get_json()
        self.assertEqual(d["done_recent"], [])
        self.assertEqual(len(d["running"]), 1)
        d = self.client.get("/api/queue?history=999999").get_json()
        self.assertEqual(len(d["done_recent"]), 80)  # capped, not an error
        self.assertEqual(self.client.get("/api/queue?history=abc").status_code, 400)

    def test_history_views_do_not_poison_default_cache(self):
        self._write([_item("q-run", "running")] + [_done(i) for i in range(80)])
        self.client.get("/api/queue?history=60")
        d = self.client.get("/api/queue").get_json()
        self.assertEqual(len(d["done_recent"]), self.appmod.RECENT_DONE_LIMIT)

    def test_cache_control_no_cache(self):
        r = self.client.get("/api/queue")
        self.assertEqual(r.headers.get("Cache-Control"), "no-cache")
        self.assertEqual(r.mimetype, "application/json")


if __name__ == "__main__":
    unittest.main()
