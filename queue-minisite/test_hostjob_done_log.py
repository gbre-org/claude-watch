#!/usr/bin/env python3
"""Tests for DONE/terminal hostjob log viewing in queue-minisite.

Companion to ``test_hostjob_stream.py``. Covers the fix that lets the
q-site show a hostjob log AFTER the job finishes. The ``hostjob`` runner
leaves its per-label log at ``<HOSTJOB_LOG_DIR>/<label>/log`` and never
archives it (unlike subagent/workload archives, which session-task copies
into ``QUEUE_LOG_ARCHIVE_DIR`` + stamps as ``log_archive_path``). That log
file PERSISTS on disk after the job exits (until ``hostjob clean``), so a
terminal (done/abandoned) hostjob queue item can still surface its full
log via the same ``/stream`` tail a running item used.

Surfaces under test:

  * ``GET /api/queue/<id>/meta`` exposes ``has_hostjob_log`` = True when
    the per-label log file exists, False when it is absent (cleaned).
  * The index page renders the View-log affordance
    (``data-log-mode="hostjob"`` + ``data-hostjob-label``) on a DONE
    hostjob row whose log file exists, and OMITS it when the file is gone.
  * ``GET /api/queue/<id>/stream`` on a terminal hostjob whose log file is
    gone emits a graceful ``open-failed`` SSE error frame (HTTP 200, not a
    500) so the modal shows an error line rather than crashing.

Run::

    python3 -m pytest queue-minisite/test_hostjob_done_log.py -v
"""

from __future__ import annotations

import json
import os
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
_DEFAULT_SESSION_TASK = (
    HERE.parent.parent / "claude-watch" / "tools" / "session-task" / "session-task"
)
SESSION_TASK = Path(os.environ.get("SESSION_TASK_BIN", str(_DEFAULT_SESSION_TASK)))


def _add(env: dict, desc: str, scopes: list[str]) -> dict:
    cmd = [sys.executable, str(SESSION_TASK), "queue", "add", desc,
           "--summary", desc, "--json"]
    for s in scopes:
        cmd.extend(["--scope", s])
    r = subprocess.run(cmd, capture_output=True, text=True, env=env, timeout=15)
    if r.returncode != 0:
        raise RuntimeError(f"add failed: {r.stderr}")
    return json.loads(r.stdout)


def _set_status(queue_path: Path, item_id: str, status: str) -> None:
    """Flip a queue item to an arbitrary status (simulates the hostjob
    runner flipping the bound item done/abandoned on completion)."""
    with open(queue_path) as f:
        data = json.load(f)
    for it in data["items"]:
        if it["id"] == item_id:
            it["status"] = status
    with open(queue_path, "w") as f:
        json.dump(data, f)


class HostjobDoneLogTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.tmp = tempfile.mkdtemp(prefix="qmin-hostjob-done-")
        cls.env = dict(os.environ)
        cls.env["HOME"] = cls.tmp
        Path(cls.tmp, ".config/session").mkdir(parents=True, exist_ok=True)
        Path(cls.tmp, ".config/claude").mkdir(parents=True, exist_ok=True)
        Path(cls.tmp, "claude-events").mkdir(parents=True, exist_ok=True)
        cls.env["PINGME_SESSION_TASK"] = "0"
        cls.env["CLAUDE_EVENT_SESSION_TASK"] = "0"
        for k, v in cls.env.items():
            os.environ[k] = v

        cls.queue_actual = Path(cls.tmp) / ".config/session/queue.json"
        cls.hostjob_dir = Path(cls.tmp) / "hostjobs"
        cls.hostjob_dir.mkdir(parents=True, exist_ok=True)
        os.environ["QUEUE_JSON"] = str(cls.queue_actual)
        os.environ["AGENT_STATE_JSON"] = str(Path(cls.tmp) / "no-agents.json")
        os.environ["AGENTS_JSONL_ROOT"] = str(Path(cls.tmp) / "agents-jsonl")
        os.environ["QUEUE_LOG_ARCHIVE_DIR"] = str(Path(cls.tmp) / "queue-logs")
        os.environ["WORKLOAD_LOG_DIR"] = str(Path(cls.tmp) / "no-workloads")
        os.environ["HOSTJOB_LOG_DIR"] = str(cls.hostjob_dir)
        os.environ["SESSION_TASK_BIN"] = str(SESSION_TASK)

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
        if self.queue_actual.exists():
            self.queue_actual.unlink()
        if self.hostjob_dir.exists():
            for p in self.hostjob_dir.iterdir():
                if p.is_dir():
                    shutil.rmtree(p)
                else:
                    p.unlink()
        self.appmod._cache.fetched_at = 0.0

    def _seed_log(self, label: str, text: str) -> Path:
        jobdir = self.hostjob_dir / label
        jobdir.mkdir(parents=True, exist_ok=True)
        log = jobdir / "log"
        log.write_text(text)
        return log

    def _read_sse(self, body_bytes: bytes) -> list[dict]:
        events = []
        for raw in body_bytes.decode("utf-8", errors="replace").splitlines():
            if raw.startswith("data: "):
                events.append(json.loads(raw[len("data: "):]))
        return events

    # ---------- /meta surfaces has_hostjob_log ----------

    def test_meta_has_hostjob_log_true_when_file_present(self):
        label = "done-with-log"
        item = _add(self.env, "hostjob done", [f"hostjob:{label}"])
        qid = item["id"]
        self._seed_log(label, "hello\nworld\n")
        _set_status(self.queue_actual, qid, "done")
        self.appmod._cache.fetched_at = 0.0

        r = self.client.get(f"/api/queue/{qid}/meta")
        self.assertEqual(r.status_code, 200, r.get_data(as_text=True))
        p = r.get_json()
        self.assertEqual(p["hostjob_label"], label)
        self.assertTrue(p["has_hostjob_log"], p)

    def test_meta_has_hostjob_log_false_when_file_absent(self):
        label = "done-no-log"
        item = _add(self.env, "hostjob done", [f"hostjob:{label}"])
        qid = item["id"]
        # No log file seeded — simulates a cleaned job.
        _set_status(self.queue_actual, qid, "done")
        self.appmod._cache.fetched_at = 0.0

        r = self.client.get(f"/api/queue/{qid}/meta")
        self.assertEqual(r.status_code, 200)
        p = r.get_json()
        self.assertEqual(p["hostjob_label"], label)
        self.assertFalse(p["has_hostjob_log"], p)

    def test_meta_has_hostjob_log_false_for_plain_item(self):
        item = _add(self.env, "plain", ["repo:test"])
        qid = item["id"]
        _set_status(self.queue_actual, qid, "done")
        self.appmod._cache.fetched_at = 0.0
        r = self.client.get(f"/api/queue/{qid}/meta")
        self.assertEqual(r.status_code, 200)
        self.assertFalse(r.get_json()["has_hostjob_log"])

    # ---------- index page renders the View-log affordance ----------

    def test_index_done_hostjob_row_is_clickable_when_log_present(self):
        label = "idx-done"
        item = _add(self.env, "hostjob done idx", [f"hostjob:{label}"])
        qid = item["id"]
        self._seed_log(label, "one\ntwo\n")
        _set_status(self.queue_actual, qid, "done")
        self.appmod._cache.fetched_at = 0.0

        r = self.client.get("/")
        self.assertEqual(r.status_code, 200)
        html = r.get_data(as_text=True)
        # The row exists, is marked clickable, and carries the hostjob
        # log-mode + label so the front-end opens the hostjob tail.
        self.assertIn(f'data-queue-id="{qid}"', html)
        self.assertIn('data-log-mode="hostjob"', html)
        self.assertIn(f'data-hostjob-label="{label}"', html)
        # The hostjob-log badge is rendered for terminal hostjob rows.
        self.assertIn("hostjob log", html)

    def test_index_done_hostjob_row_not_clickable_when_log_absent(self):
        label = "idx-done-gone"
        item = _add(self.env, "hostjob done gone", [f"hostjob:{label}"])
        qid = item["id"]
        # No log file — the affordance must NOT render for this item.
        _set_status(self.queue_actual, qid, "done")
        self.appmod._cache.fetched_at = 0.0

        r = self.client.get("/")
        self.assertEqual(r.status_code, 200)
        html = r.get_data(as_text=True)
        self.assertIn(f'data-queue-id="{qid}"', html)
        # No hostjob log-mode affordance for this specific (logless) label.
        self.assertNotIn(f'data-hostjob-label="{label}"', html)

    # ---------- /stream is graceful when the log file is gone ----------

    def test_stream_terminal_hostjob_missing_log_emits_open_failed(self):
        """A terminal hostjob whose log file was cleaned must NOT 500 —
        the tail opens the (missing) path, fails, and emits a single
        ``open-failed`` error SSE frame so the modal shows an error line."""
        label = "stream-gone"
        item = _add(self.env, "hostjob gone stream", [f"hostjob:{label}"])
        qid = item["id"]
        # No log file seeded.
        _set_status(self.queue_actual, qid, "done")
        self.appmod._cache.fetched_at = 0.0

        self.appmod.SSE_TAIL_MAX_IDLE_SECONDS = 0.1
        self.appmod.SSE_TAIL_POLL_SECONDS = 0.05
        self.appmod.SSE_TAIL_MAX_LIFETIME_SECONDS = 5.0

        r = self.client.get(f"/api/queue/{qid}/stream")
        self.assertEqual(r.status_code, 200, r.get_data(as_text=True))
        events = self._read_sse(r.get_data())
        self.assertGreater(len(events), 0, "stream returned NO events")
        # stream-start meta first (mode=hostjob), then an open-failed error.
        self.assertEqual(events[0].get("kind"), "stream-start")
        self.assertEqual(events[0].get("mode"), "hostjob")
        kinds = [e.get("kind") for e in events if e.get("type") == "error"]
        self.assertIn("open-failed", kinds, events)

    def test_stream_terminal_hostjob_with_log_backfills_full_file(self):
        """The done-item happy path: a terminal hostjob whose log file
        still exists backfills the FULL file then ends on the terminal
        status — this is what makes the done log viewable."""
        label = "stream-present"
        item = _add(self.env, "hostjob present stream", [f"hostjob:{label}"])
        qid = item["id"]
        self._seed_log(label, "alpha\nbeta\ngamma\n")
        _set_status(self.queue_actual, qid, "done")
        self.appmod._cache.fetched_at = 0.0

        self.appmod.SSE_TAIL_MAX_IDLE_SECONDS = 0.1
        self.appmod.SSE_TAIL_POLL_SECONDS = 0.05
        self.appmod.SSE_TAIL_MAX_LIFETIME_SECONDS = 5.0

        r = self.client.get(f"/api/queue/{qid}/stream")
        self.assertEqual(r.status_code, 200, r.get_data(as_text=True))
        events = self._read_sse(r.get_data())
        line_events = [e for e in events if e.get("type") == "event"]
        texts = [e["text"] for e in line_events]
        self.assertEqual(texts, ["alpha", "beta", "gamma"], events)
        kinds = [e.get("kind") for e in events]
        self.assertIn("workload-end", kinds)


if __name__ == "__main__":
    unittest.main(verbosity=2)
