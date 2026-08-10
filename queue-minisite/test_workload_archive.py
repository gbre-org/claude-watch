#!/usr/bin/env python3
"""End-to-end tests for the workload-archive replay path in queue-minisite.

Companion to ``test_depend.py``. Spawns the Flask app in-process with
QUEUE_PATH/SESSION_TASK_BIN/QUEUE_LOG_ARCHIVE_DIR/etc. pointed at a
tempdir, seeds queue.json + an archived ``<qid>.workload.txt`` file,
and exercises:

  * ``GET /api/queue/<id>/archive`` for a workload-bound queue item
    (``log_archive_path`` ends in ``.workload.txt``) — must stream
    SSE ``workload_line`` events.
  * Same endpoint for an agent-bound item (``.jsonl``) — must keep
    streaming the JSONL replay shape (regression guard).
  * 404 when the queue item is missing or has no archive stamp.
  * Front-end shape: ``has_archive`` boolean is True for both
    archive shapes (jsonl AND workload.txt) so the View-log button
    renders.

Run::

    python3 queue-minisite/test_workload_archive.py
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
# Repo-root relative: queue-minisite/ lives inside the claude-watch
# checkout, so the CLI is a sibling under tools/. (The old form went
# up two levels and back down through a hardcoded "claude-watch"
# directory name, which only resolved when the checkout happened to
# be named that — it broke in worktrees and renamed clones.)
_DEFAULT_SESSION_TASK = HERE.parent / "tools" / "session-task" / "session-task"
SESSION_TASK = Path(os.environ.get("SESSION_TASK_BIN", str(_DEFAULT_SESSION_TASK)))


def _add(env: dict, queue_actual: Path, desc: str, scopes: list[str]) -> dict:
    cmd = [sys.executable, str(SESSION_TASK), "queue", "add", desc,
           "--summary", desc, "--json"]
    for s in scopes:
        cmd.extend(["--scope", s])
    # `queue add` refuses a hand-written `workload:` scope — in real use
    # the workload runner creates those rows itself. These tests only
    # need a workload-shaped ROW to exercise the archive endpoint, so
    # take the documented escape hatch rather than standing up a real
    # workload runner.
    if any(s.startswith("workload:") for s in scopes):
        cmd.append("--force-enqueue")
    r = subprocess.run(cmd, capture_output=True, text=True, env=env, timeout=15)
    if r.returncode != 0:
        raise RuntimeError(f"add failed: {r.stderr}")
    return json.loads(r.stdout)


def _stamp_archive(item_id: str, archive_dir: Path, queue_path: Path,
                   *, suffix: str, content: str):
    """Write the archive file and patch queue.json to point at it.

    Mirrors what ``session-task`` does at queue-done time. We do the
    write directly (rather than running the helper) so the test stays
    fast and doesn't depend on a working active-agents.json.
    """
    archive_dir.mkdir(parents=True, exist_ok=True)
    fname = f"{item_id}{suffix}"
    (archive_dir / fname).write_text(content)
    with open(queue_path) as f:
        data = json.load(f)
    for it in data["items"]:
        if it["id"] == item_id:
            it["log_archive_path"] = fname
            it["status"] = "done"
            break
    with open(queue_path, "w") as f:
        json.dump(data, f)


class WorkloadArchiveEndpointTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.tmp = tempfile.mkdtemp(prefix="qmin-workload-archive-")
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
        cls.archive_dir = Path(cls.tmp) / "queue-logs"
        os.environ["QUEUE_JSON"] = str(cls.queue_actual)
        os.environ["AGENT_STATE_JSON"] = str(Path(cls.tmp) / "no-agents.json")
        os.environ["AGENTS_JSONL_ROOT"] = str(Path(cls.tmp) / "no-jsonl")
        os.environ["QUEUE_LOG_ARCHIVE_DIR"] = str(cls.archive_dir)
        os.environ["WORKLOAD_LOG_DIR"] = str(Path(cls.tmp) / "no-workloads")
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
        # Clean any prior archive files so each test starts fresh.
        if self.archive_dir.exists():
            for p in self.archive_dir.iterdir():
                if p.is_file():
                    p.unlink()
        self.appmod._cache.fetched_at = 0.0

    # ---------- helpers ----------

    def _read_sse(self, body_bytes: bytes) -> list[dict]:
        """Parse SSE 'data: {...}' lines from the response body into JSON."""
        events = []
        for raw in body_bytes.decode("utf-8", errors="replace").splitlines():
            if raw.startswith("data: "):
                payload = raw[len("data: "):]
                events.append(json.loads(payload))
        return events

    # ---------- workload archive happy path ----------

    def test_workload_archive_streams_workload_line_events(self):
        wkl = _add(self.env, self.queue_actual,
                   "workload archive E2E", ["workload:wkl-e2e"])
        wid = wkl["id"]
        _stamp_archive(
            wid,
            self.archive_dir,
            self.queue_actual,
            suffix=".workload.txt",
            content="hello world\nline two\nline three\n",
        )

        # Force the cache to reread queue.json after our direct mutation.
        self.appmod._cache.fetched_at = 0.0

        r = self.client.get(f"/api/queue/{wid}/archive")
        self.assertEqual(r.status_code, 200, r.get_data(as_text=True))
        self.assertEqual(
            r.headers.get("Content-Type", "").split(";")[0],
            "text/event-stream",
        )

        events = self._read_sse(r.get_data())
        # First frame: stream-start meta, mode=archive-workload.
        self.assertEqual(events[0]["type"], "meta")
        self.assertEqual(events[0]["kind"], "stream-start")
        self.assertEqual(events[0]["mode"], "archive-workload")

        # Three workload_line event frames.
        line_events = [
            e for e in events
            if e.get("type") == "event" and e.get("kind") == "workload_line"
        ]
        self.assertEqual(len(line_events), 3, line_events)
        self.assertEqual(line_events[0]["text"], "hello world")
        self.assertEqual(line_events[1]["text"], "line two")
        self.assertEqual(line_events[2]["text"], "line three")

        # Last frame: archive-end with line count.
        self.assertEqual(events[-1]["type"], "meta")
        self.assertEqual(events[-1]["kind"], "archive-end")
        self.assertEqual(events[-1]["lines"], 3)

    # ---------- agent archive regression ----------

    def test_agent_jsonl_archive_still_streams_jsonl_events(self):
        """Existing agent JSONL replay path must not regress."""
        agent_item = _add(self.env, self.queue_actual,
                          "agent archive E2E", ["repo:agent-e2e"])
        aid = agent_item["id"]
        # JSONL content — one user message line.
        jsonl_payload = json.dumps({
            "type": "user",
            "message": {"role": "user", "content": "hi"},
        })
        _stamp_archive(
            aid,
            self.archive_dir,
            self.queue_actual,
            suffix=".jsonl",
            content=jsonl_payload + "\n",
        )

        self.appmod._cache.fetched_at = 0.0

        r = self.client.get(f"/api/queue/{aid}/archive")
        self.assertEqual(r.status_code, 200)

        events = self._read_sse(r.get_data())
        self.assertEqual(events[0]["type"], "meta")
        self.assertEqual(events[0]["kind"], "stream-start")
        self.assertEqual(events[0]["mode"], "archive")  # NOT archive-workload

        # The parsed JSONL event should NOT be a workload_line.
        non_meta = [e for e in events if e.get("type") != "meta"]
        self.assertTrue(non_meta, events)
        self.assertNotEqual(non_meta[0].get("kind"), "workload_line")

    # ---------- direct_passthrough=True bytes-encoding regression ----------

    def test_format_sse_returns_bytes(self):
        """``_format_sse`` and ``_format_sse_comment`` must return bytes.

        The streaming Response is created with ``direct_passthrough=True``
        so werkzeug emits each yielded chunk to the WSGI socket verbatim
        (no buffering, no flush coalescing). WSGI requires byte chunks
        per PEP-3333; yielding strs raises
        ``TypeError('...' is not a byte')`` from gunicorn's
        gthread worker write() path and the SSE stream silently 200s
        with an empty body. Regression guard.
        """
        b = self.appmod._format_sse({"type": "meta", "kind": "test"})
        self.assertIsInstance(b, bytes, f"_format_sse returned {type(b).__name__}, expected bytes")
        # Round-trip: bytes start with `data: ` and end with `\n\n`.
        self.assertTrue(b.startswith(b"data: "), b)
        self.assertTrue(b.endswith(b"\n\n"), b)

        c = self.appmod._format_sse_comment("ka 1s")
        self.assertIsInstance(c, bytes, f"_format_sse_comment returned {type(c).__name__}, expected bytes")
        self.assertTrue(c.startswith(b": "), c)
        self.assertTrue(c.endswith(b"\n\n"), c)

    # ---------- 404 paths ----------

    def test_archive_404_when_no_archive_stamp(self):
        """Item exists but has no log_archive_path -> 404."""
        item = _add(self.env, self.queue_actual,
                    "no archive item", ["repo:no-arc"])
        qid = item["id"]
        self.appmod._cache.fetched_at = 0.0
        r = self.client.get(f"/api/queue/{qid}/archive")
        self.assertEqual(r.status_code, 404)

    def test_archive_404_when_unknown_id(self):
        """Queue id doesn't exist -> 404."""
        r = self.client.get("/api/queue/q-9999-99-99-zzzz/archive")
        self.assertEqual(r.status_code, 404)

    def test_archive_400_on_bad_id_format(self):
        r = self.client.get("/api/queue/not-a-real-id/archive")
        self.assertEqual(r.status_code, 400)

    # ---------- has_archive flag (front-end shape) ----------

    def test_has_archive_true_for_workload_txt(self):
        """The list payload's has_archive flag must be True for .workload.txt files too."""
        wkl = _add(self.env, self.queue_actual,
                   "wkl flag check", ["workload:wkl-flag"])
        wid = wkl["id"]
        _stamp_archive(
            wid,
            self.archive_dir,
            self.queue_actual,
            suffix=".workload.txt",
            content="x\n",
        )

        self.appmod._cache.fetched_at = 0.0

        r = self.client.get("/api/queue")
        self.assertEqual(r.status_code, 200, r.get_data(as_text=True))
        body = r.get_json()
        # Find the item across all status buckets.
        items = []
        for k in ("running", "pending", "done_recent", "abandoned_recent",
                  "done", "abandoned"):
            v = body.get(k)
            if isinstance(v, list):
                items.extend(v)
        target = next((it for it in items if it["id"] == wid), None)
        self.assertIsNotNone(target,
                             f"queue id {wid} not found in payload (keys: {list(body.keys())})")
        self.assertTrue(target.get("has_archive"),
                        f"expected has_archive=True for workload archive: {target}")

    def test_has_archive_true_for_jsonl(self):
        """Regression guard: agent .jsonl archives still flag has_archive=True."""
        item = _add(self.env, self.queue_actual,
                    "jsonl flag check", ["repo:jsonl-flag"])
        qid = item["id"]
        _stamp_archive(
            qid,
            self.archive_dir,
            self.queue_actual,
            suffix=".jsonl",
            content="{}\n",
        )

        self.appmod._cache.fetched_at = 0.0
        r = self.client.get("/api/queue")
        body = r.get_json()
        items = []
        for k in ("running", "pending", "done_recent", "abandoned_recent",
                  "done", "abandoned"):
            v = body.get(k)
            if isinstance(v, list):
                items.extend(v)
        target = next((it for it in items if it["id"] == qid), None)
        self.assertIsNotNone(target)
        self.assertTrue(target.get("has_archive"))


if __name__ == "__main__":
    unittest.main(verbosity=2)
