#!/usr/bin/env python3
"""Tests for hostjob support in queue-minisite.

Companion to ``test_live_stream.py`` (agent /stream) and the workload
coverage in ``test_meta.py`` / ``test_workload_archive.py``. Covers the
hostjob-specific surfaces added when ``hostjob`` became a first-class
queue/minisite citizen:

  * ``GET /api/queue/<id>/meta`` surfaces ``hostjob_label`` for a
    ``hostjob:<label>`` queue item.
  * ``GET /api/queue/<id>/stream`` tails ``<HOSTJOB_LOG_DIR>/<label>/log``
    (note the per-label-DIR layout, NOT a flat ``<label>.output``) and
    emits the same SSE wire format as the workload tail, using the queue
    item's terminal status as the end-of-stream signal (hostjob has no
    ``.exit`` sidecar).

Run::

    python3 -m pytest queue-minisite/test_hostjob_stream.py -v
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


def _add(env: dict, queue_actual: Path, desc: str, scopes: list[str]) -> dict:
    cmd = [sys.executable, str(SESSION_TASK), "queue", "add", desc,
           "--summary", desc, "--json"]
    for s in scopes:
        cmd.extend(["--scope", s])
    r = subprocess.run(cmd, capture_output=True, text=True, env=env, timeout=15)
    if r.returncode != 0:
        raise RuntimeError(f"add failed: {r.stderr}")
    return json.loads(r.stdout)


def _register(env: dict, qid: str) -> None:
    cmd = [sys.executable, str(SESSION_TASK), "queue", "register", qid, "--json"]
    r = subprocess.run(cmd, capture_output=True, text=True, env=env, timeout=15)
    if r.returncode != 0:
        raise RuntimeError(f"register failed: {r.stderr}")


def _set_status(queue_path: Path, item_id: str, status: str) -> None:
    """Flip a queue item to an arbitrary status (used to simulate the
    hostjob runner flipping done/abandoned — the stream's stop signal)."""
    with open(queue_path) as f:
        data = json.load(f)
    for it in data["items"]:
        if it["id"] == item_id:
            it["status"] = status
    with open(queue_path, "w") as f:
        json.dump(data, f)


class HostjobMinisiteTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.tmp = tempfile.mkdtemp(prefix="qmin-hostjob-")
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

    def _read_sse(self, body_bytes: bytes) -> list[dict]:
        events = []
        for raw in body_bytes.decode("utf-8", errors="replace").splitlines():
            if raw.startswith("data: "):
                events.append(json.loads(raw[len("data: "):]))
        return events

    # ---------- /meta surfaces hostjob_label ----------

    def test_meta_surfaces_hostjob_label(self):
        item = _add(self.env, self.queue_actual,
                    "hostjob meta", ["hostjob:my-host-job"])
        qid = item["id"]
        self.appmod._cache.fetched_at = 0.0
        r = self.client.get(f"/api/queue/{qid}/meta")
        self.assertEqual(r.status_code, 200, r.get_data(as_text=True))
        p = r.get_json()
        self.assertEqual(p["hostjob_label"], "my-host-job")
        # workload_label stays empty for a hostjob item.
        self.assertEqual(p["workload_label"], "")

    def test_meta_hostjob_label_empty_for_plain_item(self):
        item = _add(self.env, self.queue_actual, "plain", ["repo:test"])
        qid = item["id"]
        self.appmod._cache.fetched_at = 0.0
        r = self.client.get(f"/api/queue/{qid}/meta")
        self.assertEqual(r.status_code, 200)
        self.assertEqual(r.get_json()["hostjob_label"], "")

    # ---------- /stream tails the hostjob log ----------

    def test_stream_tails_hostjob_log(self):
        """A running hostjob item streams its <label>/log file as
        workload_line SSE frames, then terminates once the queue item
        flips to a terminal status (hostjob has no .exit file)."""
        label = "tail-host-job"
        item = _add(self.env, self.queue_actual,
                    "hostjob stream", [f"hostjob:{label}"])
        qid = item["id"]
        _register(self.env, qid)
        # Seed the per-label-DIR log file: <HOSTJOB_LOG_DIR>/<label>/log
        jobdir = self.hostjob_dir / label
        jobdir.mkdir(parents=True, exist_ok=True)
        (jobdir / "log").write_text("line one\nline two\n")
        # Flip the item terminal so the tail's stop signal fires promptly
        # (the runner would normally do this on completion).
        _set_status(self.queue_actual, qid, "done")
        self.appmod._cache.fetched_at = 0.0

        self.appmod.SSE_TAIL_MAX_IDLE_SECONDS = 0.1
        self.appmod.SSE_TAIL_POLL_SECONDS = 0.05
        self.appmod.SSE_TAIL_MAX_LIFETIME_SECONDS = 5.0

        r = self.client.get(f"/api/queue/{qid}/stream")
        self.assertEqual(r.status_code, 200, r.get_data(as_text=True))
        self.assertEqual(
            r.headers.get("Content-Type", "").split(";")[0],
            "text/event-stream",
        )
        events = self._read_sse(r.get_data())
        self.assertGreater(len(events), 0, "hostjob stream returned NO events")
        # First frame: stream-start meta with mode=hostjob + log path.
        self.assertEqual(events[0]["type"], "meta")
        self.assertEqual(events[0]["kind"], "stream-start")
        self.assertEqual(events[0].get("mode"), "hostjob")
        self.assertIn(f"{label}/log", events[0].get("path", ""), events[0])
        # Backfilled the two log lines.
        line_events = [e for e in events if e.get("type") == "event"]
        self.assertEqual(
            len(line_events), 2,
            f"expected 2 log lines, got {len(line_events)}",
        )
        self.assertEqual(line_events[0]["text"], "line one")
        self.assertEqual(line_events[1]["text"], "line two")
        # Terminal frame at end (workload-end kind, reused wire format).
        kinds = [e.get("kind") for e in events]
        self.assertIn("workload-end", kinds)

    def test_stream_400_on_bad_qid_format(self):
        r = self.client.get("/api/queue/not!valid/stream")
        self.assertEqual(r.status_code, 400)


class HostjobLogDirDefaultTest(unittest.TestCase):
    """Regression for the live-log RECONNECTING bug: the minisite defaulted
    HOSTJOB_LOG_DIR to a non-existent `/hostjobs` bind-mount, so the hostjob
    tail built `/hostjobs/<label>/log` and got stuck emitting
    `open-failed: [Errno 2] No such file or directory`. The correct default
    is the hostjob runner's STATE_ROOT, `~/.cache/hostjob`.

    Does its OWN isolated env setup/teardown (separate from
    HostjobMinisiteTest, which always sets HOSTJOB_LOG_DIR and so never
    exercised the buggy default).
    """

    def setUp(self):
        self._saved_environ = dict(os.environ)
        self._saved_modules = {
            m: sys.modules[m] for m in ("app", "claude_agents") if m in sys.modules
        }
        self.tmp = tempfile.mkdtemp(prefix="qmin-hostjob-default-")

    def tearDown(self):
        os.environ.clear()
        os.environ.update(self._saved_environ)
        for m in ("app", "claude_agents"):
            sys.modules.pop(m, None)
        sys.modules.update(self._saved_modules)
        shutil.rmtree(self.tmp, ignore_errors=True)

    def test_default_resolves_to_cache_hostjob(self):
        # Fresh HOME, HOSTJOB_LOG_DIR UNSET — exercise the module-level default.
        os.environ["HOME"] = self.tmp
        os.environ.pop("HOSTJOB_LOG_DIR", None)
        Path(self.tmp, ".config/session").mkdir(parents=True, exist_ok=True)
        Path(self.tmp, ".config/claude").mkdir(parents=True, exist_ok=True)
        os.environ["PINGME_SESSION_TASK"] = "0"
        os.environ["CLAUDE_EVENT_SESSION_TASK"] = "0"
        os.environ["QUEUE_JSON"] = str(Path(self.tmp) / ".config/session/queue.json")
        os.environ["SESSION_TASK_BIN"] = str(SESSION_TASK)

        sys.path.insert(0, str(HERE))
        for mod in ("app", "claude_agents"):
            sys.modules.pop(mod, None)
        import app as appmod  # noqa: E402

        expected = os.path.join(os.path.expanduser("~"), ".cache", "hostjob")
        self.assertEqual(
            appmod.HOSTJOB_LOG_DIR, expected,
            f"default HOSTJOB_LOG_DIR should be ~/.cache/hostjob, "
            f"got {appmod.HOSTJOB_LOG_DIR!r}",
        )
        # The original bug: defaulting to the bogus /hostjobs bind-mount.
        self.assertNotEqual(appmod.HOSTJOB_LOG_DIR, "/hostjobs")



if __name__ == "__main__":
    unittest.main(verbosity=2)
