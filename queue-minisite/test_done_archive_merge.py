#!/usr/bin/env python3
"""Tests for the DONE view / completed-tasks archive merge.

The DONE section used to source solely from the ``done`` items resident in
``queue.json``. When ``queue.json`` corrupted and reset to empty, the live
done tail was wiped and the section rendered "DONE 0/0" — the whole
historical record vanished from the view.

The fix UNIONs the live ``queue.json`` done items with the persistent
append-only completed-tasks archive (``completed-tasks.jsonl``, the record
session-task writes on every queue done/abandon). This makes the DONE view
reset-proof and reflect the full completion history. Dedup is by queue id
with the live ``queue.json`` entry winning over its archive echo. Only DONE
rows are pulled from the archive (abandon / merge / block / … lifecycle rows
are dropped). The rendered window is capped at ``RECENT_DONE_LIMIT``, newest
first, while ``totals.done`` reports the full union count.

These tests pin that contract on ``/api/queue`` (the JSON the SPA refresh
tick reads).

Run::

    python3 queue-minisite/test_done_archive_merge.py
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


def _done_item(item_id: str, completed_at: str, summary: str = "") -> dict:
    return {
        "id": item_id,
        "summary": summary or f"live {item_id}",
        "description": "",
        "scope": [],
        "status": "done",
        "priority": 5,
        "created_by": "main-loop",
        "created_at": completed_at,
        "registered_at": completed_at,
        "completed_at": completed_at,
    }


def _archive_line(
    item_id: str,
    completed_at: str,
    task: str,
    *,
    event: str | None = None,
) -> str:
    row: dict = {
        "task": task,
        "completed_at": completed_at,
        "source": "queue",
        "id": item_id,
        "group_id": "g-x",
    }
    if event is not None:
        row["event"] = event
    return json.dumps(row)


def _write_archive(path: Path, lines: list[str]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with open(path, "w") as f:
        f.write("\n".join(lines) + "\n")


class DoneArchiveMergeTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.tmp = tempfile.mkdtemp(prefix="qmin-done-archive-")
        session_dir = Path(cls.tmp) / ".config/session"
        cls.queue_actual = session_dir / "queue.json"
        cls.archive_actual = session_dir / "completed-tasks.jsonl"
        os.environ["QUEUE_JSON"] = str(cls.queue_actual)
        os.environ["COMPLETED_TASKS_JSONL"] = str(cls.archive_actual)
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
        for k in ("COMPLETED_TASKS_JSONL",):
            os.environ.pop(k, None)

    def setUp(self):
        # Reset both the queue cache and the archive (mtime/size) cache so
        # each test sees the file it just wrote.
        self.appmod._cache.fetched_at = 0.0
        self.appmod._archive_cache.mtime = -1.0
        self.appmod._archive_cache.size = -1

    def _payload(self) -> dict:
        self.setUp()
        r = self.client.get("/api/queue")
        self.assertEqual(r.status_code, 200)
        return r.get_json()

    def test_empty_queue_shows_archive_history(self):
        """The reset scenario: queue.json empty, archive holds history."""
        _write_queue(self.queue_actual, [])
        _write_archive(
            self.archive_actual,
            [
                _archive_line("q-a", "2026-05-01T00:00:00+00:00", "[queue q-a] first"),
                _archive_line("q-b", "2026-05-02T00:00:00+00:00", "[queue q-b] second"),
                _archive_line("q-c", "2026-05-03T00:00:00+00:00", "[queue q-c] third"),
            ],
        )
        body = self._payload()
        self.assertEqual(body["totals"]["done"], 3)
        ids = [d["id"] for d in body["done_recent"]]
        # Newest-first.
        self.assertEqual(ids, ["q-c", "q-b", "q-a"])
        # All sourced from archive; prefix stripped for the summary.
        self.assertTrue(all(d["from_archive"] for d in body["done_recent"]))
        self.assertEqual(body["done_recent"][0]["summary"], "third")

    def test_live_queue_entry_wins_over_archive_echo(self):
        """An id in BOTH queue.json (done) and archive appears once, live-side."""
        _write_queue(
            self.queue_actual,
            [_done_item("q-dup", "2026-06-01T00:00:00+00:00", summary="LIVE version")],
        )
        _write_archive(
            self.archive_actual,
            [
                # Echo of the live item (session-task logs on done) + genuinely
                # archive-only history.
                _archive_line("q-dup", "2026-06-01T00:00:00+00:00", "[queue q-dup] ARCHIVE version"),
                _archive_line("q-old", "2026-05-01T00:00:00+00:00", "[queue q-old] older"),
            ],
        )
        body = self._payload()
        # Union count: 1 live + 1 archive-only (echo deduped away).
        self.assertEqual(body["totals"]["done"], 2)
        by_id = {d["id"]: d for d in body["done_recent"]}
        self.assertEqual(sorted(by_id), ["q-dup", "q-old"])
        # Live entry wins: not flagged from_archive, live summary retained.
        self.assertFalse(by_id["q-dup"]["from_archive"])
        self.assertEqual(by_id["q-dup"]["summary"], "LIVE version")
        self.assertTrue(by_id["q-old"]["from_archive"])

    def test_abandon_and_lifecycle_rows_excluded(self):
        """Only DONE rows from the archive feed the DONE section."""
        _write_queue(self.queue_actual, [])
        _write_archive(
            self.archive_actual,
            [
                _archive_line("q-done", "2026-05-02T00:00:00+00:00", "[queue q-done] ok"),
                # Structured abandon row.
                _archive_line(
                    "q-ab",
                    "2026-05-03T00:00:00+00:00",
                    "[queue q-ab abandoned] nope",
                    event="abandon",
                ),
                # Lifecycle (block) row.
                _archive_line(
                    "q-blk",
                    "2026-05-04T00:00:00+00:00",
                    "[queue q-blk] parked",
                    event="block",
                ),
                # Legacy abandon row (no event field; marker in the prefix).
                _archive_line(
                    "q-leg", "2026-05-05T00:00:00+00:00", "[queue q-leg abandoned] legacy"
                ),
            ],
        )
        body = self._payload()
        ids = [d["id"] for d in body["done_recent"]]
        self.assertEqual(ids, ["q-done"])
        self.assertEqual(body["totals"]["done"], 1)

    def test_free_text_abandoned_word_not_misclassified(self):
        """A done task whose BODY contains 'abandoned' is still shown."""
        _write_queue(self.queue_actual, [])
        _write_archive(
            self.archive_actual,
            [
                _archive_line(
                    "q-review",
                    "2026-05-06T00:00:00+00:00",
                    "[queue q-review] Review the abandoned PRs and report",
                ),
            ],
        )
        body = self._payload()
        self.assertEqual([d["id"] for d in body["done_recent"]], ["q-review"])
        self.assertEqual(body["totals"]["done"], 1)

    def test_recent_window_capped_total_reports_full_union(self):
        """done_recent caps at RECENT_DONE_LIMIT; totals.done is the union."""
        limit = self.appmod.RECENT_DONE_LIMIT
        n = limit + 15
        _write_queue(self.queue_actual, [])
        lines = [
            _archive_line(
                f"q-{i:04d}",
                f"2026-05-01T00:{i // 60:02d}:{i % 60:02d}+00:00",
                f"[queue q-{i:04d}] task {i}",
            )
            for i in range(n)
        ]
        _write_archive(self.archive_actual, lines)
        body = self._payload()
        self.assertEqual(body["totals"]["done"], n)
        self.assertEqual(len(body["done_recent"]), limit)
        # Newest-first: the highest-indexed (latest ts) ids lead.
        self.assertEqual(body["done_recent"][0]["id"], f"q-{n - 1:04d}")

    def test_missing_archive_is_graceful(self):
        """No archive file => DONE sources solely from queue.json (prior behavior)."""
        # Point at a nonexistent archive for this test only.
        self.appmod.COMPLETED_TASKS_PATH = str(Path(self.tmp) / "does-not-exist.jsonl")
        try:
            _write_queue(
                self.queue_actual,
                [_done_item("q-live", "2026-06-02T00:00:00+00:00")],
            )
            body = self._payload()
            self.assertEqual(body["totals"]["done"], 1)
            self.assertEqual([d["id"] for d in body["done_recent"]], ["q-live"])
        finally:
            self.appmod.COMPLETED_TASKS_PATH = str(self.archive_actual)


if __name__ == "__main__":
    unittest.main(verbosity=2)
