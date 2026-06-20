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



class _FakeBroker:
    """A tiny in-process SSE broker on 127.0.0.1:0 used to exercise the
    minisite's broker-live relay path. Serves GET /tail/<label> as an
    event-stream that replays a fixed set of complete lines (terminator
    stripped, matching the real broker's _send_line contract), then holds
    the connection open until the test flips the queue item terminal.
    """

    def __init__(self, lines):
        import threading
        from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

        self._lines = lines

        outer = self

        class Handler(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *a):
                pass

            def do_GET(self):
                if not self.path.startswith("/tail/"):
                    self.send_error(404)
                    return
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.send_header("Cache-Control", "no-cache")
                self.end_headers()
                try:
                    for ln in outer._lines:
                        self.wfile.write(("data: " + ln + "\n\n").encode())
                        self.wfile.flush()
                    # Hold open with keepalives; the client closes once the
                    # queue item goes terminal (its terminal check fires
                    # between reads).
                    import time as _t
                    for _ in range(200):
                        _t.sleep(0.05)
                        self.wfile.write(b": keep-alive\n\n")
                        self.wfile.flush()
                except (BrokenPipeError, ConnectionResetError, OSError):
                    return

        self.httpd = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.httpd.daemon_threads = True
        self.port = self.httpd.server_address[1]
        self.thread = threading.Thread(
            target=self.httpd.serve_forever, kwargs={"poll_interval": 0.05},
            daemon=True,
        )
        self.thread.start()

    @property
    def url(self):
        return "http://127.0.0.1:%d" % self.port

    def stop(self):
        try:
            self.httpd.shutdown()
        except Exception:
            pass


class HostjobBrokerStreamTest(unittest.TestCase):
    """Broker-live relay + fallback coverage for _tail_hostjob_output.

    Reuses HostjobMinisiteTest's env layout but reloads `app` with
    HOSTJOB_BROKER_URL pointed at (a) an in-process fake broker and (b) a
    dead port, asserting the live relay works and the dead-broker case
    cleanly falls back to the file-poll path (never errors out)."""

    def setUp(self):
        self._saved_environ = dict(os.environ)
        self._saved_modules = {
            m: sys.modules[m] for m in ("app", "claude_agents") if m in sys.modules
        }
        self.tmp = tempfile.mkdtemp(prefix="qmin-hostjob-broker-")
        self.broker = None

    def tearDown(self):
        if self.broker is not None:
            self.broker.stop()
        os.environ.clear()
        os.environ.update(self._saved_environ)
        for m in ("app", "claude_agents"):
            sys.modules.pop(m, None)
        sys.modules.update(self._saved_modules)
        shutil.rmtree(self.tmp, ignore_errors=True)

    def _boot_app(self, broker_url):
        env = dict(os.environ)
        env["HOME"] = self.tmp
        Path(self.tmp, ".config/session").mkdir(parents=True, exist_ok=True)
        Path(self.tmp, ".config/claude").mkdir(parents=True, exist_ok=True)
        Path(self.tmp, "claude-events").mkdir(parents=True, exist_ok=True)
        env["PINGME_SESSION_TASK"] = "0"
        env["CLAUDE_EVENT_SESSION_TASK"] = "0"
        self.queue_actual = Path(self.tmp) / ".config/session/queue.json"
        self.hostjob_dir = Path(self.tmp) / "hostjobs"
        self.hostjob_dir.mkdir(parents=True, exist_ok=True)
        env["QUEUE_JSON"] = str(self.queue_actual)
        env["AGENT_STATE_JSON"] = str(Path(self.tmp) / "no-agents.json")
        env["AGENTS_JSONL_ROOT"] = str(Path(self.tmp) / "agents-jsonl")
        env["QUEUE_LOG_ARCHIVE_DIR"] = str(Path(self.tmp) / "queue-logs")
        env["WORKLOAD_LOG_DIR"] = str(Path(self.tmp) / "no-workloads")
        env["HOSTJOB_LOG_DIR"] = str(self.hostjob_dir)
        env["HOSTJOB_BROKER_URL"] = broker_url
        env["SESSION_TASK_BIN"] = str(SESSION_TASK)
        os.environ.clear()
        os.environ.update(env)
        sys.path.insert(0, str(HERE))
        for mod in ("app", "claude_agents"):
            sys.modules.pop(mod, None)
        import app as appmod  # noqa: E402
        appmod.SSE_TAIL_MAX_IDLE_SECONDS = 0.5
        appmod.SSE_TAIL_POLL_SECONDS = 0.05
        appmod.SSE_TAIL_MAX_LIFETIME_SECONDS = 5.0
        return appmod

    def _read_sse(self, body_bytes):
        events = []
        for raw in body_bytes.decode("utf-8", errors="replace").splitlines():
            if raw.startswith("data: "):
                events.append(json.loads(raw[len("data: "):]))
        return events

    def test_broker_live_relay(self):
        """With a reachable broker, the live delta relays through the broker
        SSE (not the file-poll path) into workload_line frames."""
        label = "broker-live-job"
        self.broker = _FakeBroker(["live one", "live two", "live three"])
        appmod = self._boot_app(self.broker.url)
        item = _add(self.env_for(appmod), self.queue_actual,
                    "broker live", [f"hostjob:{label}"])
        qid = item["id"]
        _register(self.env_for(appmod), qid)
        jobdir = self.hostjob_dir / label
        jobdir.mkdir(parents=True, exist_ok=True)
        # File holds the SAME bytes (broker is the live mirror of the file).
        (jobdir / "log").write_text("live one\nlive two\nlive three\n")
        # NOT yet terminal at open; flip it terminal shortly so the relay
        # loop's terminal check closes the stream after delivering lines.
        _set_status(self.queue_actual, qid, "running")
        appmod._cache.fetched_at = 0.0

        import threading
        def _flip():
            import time as _t
            _t.sleep(0.6)
            _set_status(self.queue_actual, qid, "done")
            appmod._cache.fetched_at = 0.0
        threading.Thread(target=_flip, daemon=True).start()

        r = appmod.app.test_client().get(f"/api/queue/{qid}/stream")
        self.assertEqual(r.status_code, 200, r.get_data(as_text=True))
        events = self._read_sse(r.get_data())
        kinds = [e.get("kind") for e in events]
        # No broker-fallback frame -> we stayed on the broker path.
        self.assertNotIn("broker-fallback", kinds,
                         f"expected broker-live path, got {kinds}")
        line_texts = [e["text"] for e in events
                      if e.get("type") == "event" and e.get("kind") == "workload_line"]
        # Backfill (3 from file) + live (3 from broker) = the lines appear;
        # assert the live broker lines were relayed at least once.
        for want in ("live one", "live two", "live three"):
            self.assertIn(want, line_texts, line_texts)
        self.assertIn("workload-end", kinds)

    def test_fallback_when_broker_dead(self):
        """With an UNREACHABLE broker URL, the stream must still backfill the
        file and terminate cleanly via the file-poll path — never error."""
        label = "broker-dead-job"
        # Pick a port nothing is listening on.
        import socket
        s = socket.socket(); s.bind(("127.0.0.1", 0)); dead = s.getsockname()[1]; s.close()
        appmod = self._boot_app(f"http://127.0.0.1:{dead}")
        item = _add(self.env_for(appmod), self.queue_actual,
                    "broker dead", [f"hostjob:{label}"])
        qid = item["id"]
        _register(self.env_for(appmod), qid)
        jobdir = self.hostjob_dir / label
        jobdir.mkdir(parents=True, exist_ok=True)
        (jobdir / "log").write_text("file line a\nfile line b\n")
        _set_status(self.queue_actual, qid, "done")
        appmod._cache.fetched_at = 0.0

        r = appmod.app.test_client().get(f"/api/queue/{qid}/stream")
        self.assertEqual(r.status_code, 200, r.get_data(as_text=True))
        events = self._read_sse(r.get_data())
        kinds = [e.get("kind") for e in events]
        # Fallback meta frame present (broker unreachable).
        self.assertIn("broker-fallback", kinds, kinds)
        line_texts = [e["text"] for e in events
                      if e.get("type") == "event" and e.get("kind") == "workload_line"]
        self.assertEqual(line_texts[:2], ["file line a", "file line b"], line_texts)
        # Cleanly terminated via the file path.
        self.assertIn("workload-end", kinds)

    def env_for(self, appmod):
        # session-task add/register helpers need HOME + the silencing vars
        # in their subprocess env; reuse the live process env (already set
        # by _boot_app).
        return dict(os.environ)



if __name__ == "__main__":
    unittest.main(verbosity=2)
