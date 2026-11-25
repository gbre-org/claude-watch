#!/usr/bin/env python3
"""Passive CI rows (``kind: ci``) in queue-minisite.

Pins:

  * a CI row is shaped with ``kind``/``ci``/``ci_parent`` and is NEVER counted
    as an orphan (it has no owner);
  * ``live_log_mode`` is ``"ci"`` for an OPEN row only when a log command is
    configured, and ``""`` otherwise (an empty pane is worse than none);
  * the server HTML and ``static/refresh.js`` both emit ``data-ci-parent`` and
    the CI chip (the 5s refresh replaces the server paint);
  * ``/api/queue/<id>/stream`` runs the operator's command WITHOUT a shell,
    emits incrementally, ends when the row closes, reports a failing command
    as the retryable ``open-failed`` kind, and refuses unsafe substitutions;
  * ``multitail.js`` knows the ``ci`` mode, the parent attribute and nesting.

Run::

    python3 -m pytest queue-minisite/test_ci_rows.py -v
"""

from __future__ import annotations

import json
import os
import shutil
import stat
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
_DEFAULT_SESSION_TASK = HERE.parent / "tools" / "session-task" / "session-task"
SESSION_TASK = Path(os.environ.get("SESSION_TASK_BIN", str(_DEFAULT_SESSION_TASK)))


def _st(env: dict, *argv: str) -> str:
    r = subprocess.run([sys.executable, str(SESSION_TASK), *argv],
                       capture_output=True, text=True, env=env, timeout=20)
    if r.returncode != 0:
        raise RuntimeError(f"{argv} failed: {r.stderr}")
    return r.stdout.strip()


class CiRowsTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.tmp = tempfile.mkdtemp(prefix="qmin-ci-")
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
        os.environ["QUEUE_JSON"] = str(cls.queue_actual)
        os.environ["AGENT_STATE_JSON"] = str(Path(cls.tmp) / "no-agents.json")
        os.environ["AGENTS_JSONL_ROOT"] = str(Path(cls.tmp) / "agents-jsonl")
        os.environ["QUEUE_LOG_ARCHIVE_DIR"] = str(Path(cls.tmp) / "queue-logs")
        os.environ["SESSION_TASK_BIN"] = str(SESSION_TASK)
        os.environ.pop("QUEUE_MINISITE_CI_LOG_CMD", None)

        # Fake "log command": prints $1 lines from a counter file, and exits
        # non-zero when the file is absent. Also echoes its argv[2] so a test
        # can see that substitution happened without a shell.
        cls.logfile = Path(cls.tmp) / "ci.log"
        cls.cmd = Path(cls.tmp) / "ci-log"
        cls.cmd.write_text(
            "#!/bin/sh\n"
            f"[ -f {cls.logfile} ] || {{ echo 'no log yet' >&2; exit 2; }}\n"
            "echo \"args: $*\"\n"
            f"cat {cls.logfile}\n")
        cls.cmd.chmod(cls.cmd.stat().st_mode | stat.S_IEXEC)

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
            Path(str(self.queue_actual) + ".bak").unlink(missing_ok=True)
            shutil.rmtree(self.queue_actual.parent / "queue-snapshots",
                          ignore_errors=True)
        self.logfile.unlink(missing_ok=True)
        self.appmod.CI_LOG_CMD = ""
        self.appmod.CI_LOG_POLL_SECONDS = 0.05
        self.appmod.SSE_TAIL_POLL_SECONDS = 0.02
        self.appmod._cache.fetched_at = 0.0

    def _upsert(self, run="7", **kw) -> str:
        argv = ["queue", "ci", "upsert", "--provider", "prov", "--repo", "o/r",
                "--run", run]
        for k, v in kw.items():
            argv += ["--" + k.replace("_", "-"), v]
        self.appmod._cache.fetched_at = 0.0
        return _st(self.env, *argv).splitlines()[0]

    def _payload(self) -> dict:
        self.appmod._cache.fetched_at = 0.0
        return self.client.get("/api/queue").get_json()

    def _sse(self, qid: str) -> list[dict]:
        self.appmod._cache.fetched_at = 0.0
        r = self.client.get(f"/api/queue/{qid}/stream")
        out = []
        for raw in r.get_data().decode("utf-8", "replace").splitlines():
            if raw.startswith("data: "):
                out.append(json.loads(raw[6:]))
        return out

    # --- shaping ---------------------------------------------------------

    def test_shaped_row_and_no_orphan(self):
        q = self._upsert(url="https://ci.example/run/7", parent="q-2026-11-25-abcd")
        p = self._payload()
        row = next(r for r in p["running"] if r["id"] == q)
        self.assertEqual(row["kind"], "ci")
        self.assertEqual(row["ci_parent"], "q-2026-11-25-abcd")
        self.assertEqual(row["ci"]["run"], "7")
        self.assertEqual(row["ci"]["url"], "https://ci.example/run/7")
        self.assertFalse(row["is_starting"])
        self.assertEqual(p["orphan_count"], 0)

    def test_non_http_url_is_dropped(self):
        q = self._upsert(url="javascript:alert(1)")
        row = next(r for r in self._payload()["running"] if r["id"] == q)
        self.assertEqual(row["ci"]["url"], "")

    def test_live_log_mode_requires_command_and_open_row(self):
        q = self._upsert()
        row = next(r for r in self._payload()["running"] if r["id"] == q)
        self.assertEqual(row["live_log_mode"], "")
        self.appmod.CI_LOG_CMD = f"{self.cmd} {{run}}"
        row = next(r for r in self._payload()["running"] if r["id"] == q)
        self.assertEqual(row["live_log_mode"], "ci")
        _st(self.env, "queue", "ci", "close", "--id", q, "--outcome", "failure")
        p = self._payload()
        self.assertFalse([r for r in p["running"] if r["id"] == q])
        done = [r for r in p["abandoned_recent"] if r["id"] == q]
        self.assertEqual(done[0]["ci"]["outcome"], "failure")
        self.assertEqual(done[0]["live_log_mode"], "")

    # --- rendering parity ------------------------------------------------

    def test_html_and_refresh_js_emit_parent_attr_and_chip(self):
        self.appmod.CI_LOG_CMD = f"{self.cmd} {{run}}"
        q = self._upsert(parent="q-2026-11-25-abcd", url="https://ci.example/1")
        self.appmod._cache.fetched_at = 0.0
        html = self.client.get("/").get_data(as_text=True)
        self.assertIn('data-ci-parent="q-2026-11-25-abcd"', html)
        self.assertIn('data-live-log-mode="ci"', html)
        self.assertIn("ci-badge", html)
        self.assertIn('class="ci-link"', html)
        self.assertIn(q, html)
        js = (HERE / "static" / "refresh.js").read_text()
        self.assertIn("function ciTag", js)
        self.assertIn("data-ci-parent", js)
        self.assertIn("data-ci-row", js)

    def test_multitail_js_knows_ci_mode(self):
        js = (HERE / "static" / "multitail.js").read_text()
        for needle in ("data-ci-parent", "mt-pane-ci-child", "parentQid",
                       "'ci'"):
            self.assertIn(needle, js)
        css = (HERE / "static" / "style.css").read_text()
        self.assertIn(".mt-badge-ci", css)

    # --- stream ----------------------------------------------------------

    def test_stream_without_command(self):
        q = self._upsert()
        ev = self._sse(q)
        self.assertEqual(ev[0]["kind"], "no-log-source")

    def test_stream_failing_command_is_retryable_open_failed(self):
        self.appmod.CI_LOG_CMD = f"{self.cmd} {{run}}"
        q = self._upsert()
        ev = self._sse(q)
        self.assertTrue(any(e.get("kind") == "open-failed" for e in ev), ev)

    def _script(self, name, body):
        f = Path(self.tmp) / name
        f.write_text("#!/bin/sh\n" + body)
        f.chmod(f.stat().st_mode | stat.S_IXUSR)
        return f

    def test_pending_rc_streams_data_not_open_failed(self):
        """Exit 5 = pending: stdout is streamed as data, no open-failed
        (which would drive the pane to 'launch failed / no log')."""
        pend = self._script("pending-cmd", "echo '  - step Build [in_progress]'\nexit 5\n")
        self.appmod.CI_LOG_CMD = f"{pend} {{run}}"
        q = self._upsert()
        _st(self.env, "queue", "ci", "close", "--id", q, "--outcome", "success")
        ev = self._sse(q)
        self.assertFalse(any(e.get("type") == "error" for e in ev), ev)
        texts = [e["text"] for e in ev if e.get("type") == "event"]
        self.assertIn("  - step Build [in_progress]", texts)

    def test_pending_rc_with_no_output_gets_status_line(self):
        pend = self._script("pending-empty", "exit 5\n")
        self.appmod.CI_LOG_CMD = f"{pend} {{run}}"
        q = self._upsert()
        _st(self.env, "queue", "ci", "close", "--id", q, "--outcome", "success")
        ev = self._sse(q)
        self.assertFalse(any(e.get("type") == "error" for e in ev), ev)
        texts = [e["text"] for e in ev if e.get("type") == "event"]
        self.assertTrue(any("log not available yet" in t for t in texts), texts)

    def test_failure_error_is_one_clean_line_not_traceback(self):
        bad = self._script(
            "tb-cmd",
            "echo 'Traceback (most recent call last):' >&2\n"
            "echo '  File \"x.py\", line 1' >&2\n"
            "echo 'urllib.error.HTTPError: HTTP Error 404' >&2\nexit 1\n")
        self.appmod.CI_LOG_CMD = f"{bad} {{run}}"
        q = self._upsert()
        ev = self._sse(q)
        err = next(e for e in ev if e.get("kind") == "open-failed")
        self.assertNotIn("Traceback", err["error"])
        self.assertNotIn("File", err["error"])

    def test_stream_emits_lines_and_ends_when_closed(self):
        self.appmod.CI_LOG_CMD = f"{self.cmd} {{run}} {{job}}"
        q = self._upsert()
        self.logfile.write_text("alpha\nbeta\n")
        _st(self.env, "queue", "ci", "close", "--id", q, "--outcome", "success")
        ev = self._sse(q)
        self.assertEqual(ev[0]["kind"], "stream-start")
        self.assertEqual(ev[0]["mode"], "ci")
        texts = [e["text"] for e in ev if e.get("type") == "event"]
        # substitution happened, no shell: {job} was empty -> empty argv word
        self.assertEqual(texts[0].strip(), "args: 7")
        self.assertEqual(texts[1:3], ["alpha", "beta"])
        self.assertEqual(ev[-1]["kind"], "workload-end")

    def test_stream_is_incremental(self):
        self.appmod.CI_LOG_CMD = f"{self.cmd} {{run}}"
        q = self._upsert()
        self.logfile.write_text("one\n")
        gen = self.appmod._tail_ci_output(
            next(i for i in json.loads(self.queue_actual.read_text())["items"]
                 if i["id"] == q))
        frames = []
        for raw in gen:
            for line in raw.decode().splitlines():
                if line.startswith("data: "):
                    frames.append(json.loads(line[6:]))
            texts = [f.get("text") for f in frames if f.get("type") == "event"]
            if "one" in texts:
                break
        self.assertIn("one", texts)
        # grow the log, expect only the NEW line next
        self.logfile.write_text("one\ntwo\n")
        new = []
        for raw in gen:
            for line in raw.decode().splitlines():
                if line.startswith("data: "):
                    f = json.loads(line[6:])
                    if f.get("type") == "event":
                        new.append(f["text"])
            if "two" in new:
                break
        gen.close()
        self.assertEqual(new, ["two"])

    def test_unsafe_substitution_refused(self):
        self.appmod.CI_LOG_CMD = f"{self.cmd} {{job}}"
        q = self._upsert(run="8")
        # a job name that would become an option
        _st(self.env, "queue", "ci", "upsert", "--provider", "prov", "--repo",
            "o/r", "--run", "9", "--job=--evil")
        rows = json.loads(self.queue_actual.read_text())["items"]
        bad = next(r for r in rows if r["ci"]["job"] == "--evil")
        with self.assertRaises(ValueError):
            self.appmod._ci_log_argv(bad)
        ev = self._sse(bad["id"])
        self.assertEqual(ev[0]["kind"], "no-log-source")
        self.assertTrue(q)


if __name__ == "__main__":
    unittest.main()
