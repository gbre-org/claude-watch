#!/usr/bin/env python3
"""Remote-queue tests for `hostjob` (HOSTJOB_QUEUE_REMOTE).

When local `session-task` is absent, hostjob can create/close its queue row on
a remote queue host over ssh. A stub `ssh` shim on PATH stands in for the
transport (records its argv; honours HOSTJOB_TEST_SSH_FAIL=1 to simulate an
unreachable host). Covers: local path unchanged + preferred; remote reachable;
remote unreachable -> job still runs, warning on stderr, row recorded missing.

Run::  python3 test_hostjob_queue_remote.py
"""

from __future__ import annotations

import importlib.util
import io
import os
import sys
import time
import unittest
from contextlib import redirect_stderr, redirect_stdout
from importlib.machinery import SourceFileLoader
from pathlib import Path

HERE = Path(__file__).resolve().parent
HOSTJOB = HERE.parent / "hostjob"
if str(HERE) not in sys.path:
    sys.path.insert(0, str(HERE))

import hostjob_testkit  # noqa: E402

SSH_LOG_ENV = "HOSTJOB_TEST_SSH_LOG"
SSH_SHIM = """#!/usr/bin/env bash
printf '%s\\n' "$*" >> "${HOSTJOB_TEST_SSH_LOG:?}"
if [ -n "$HOSTJOB_TEST_SSH_FAIL" ]; then
  echo 'ssh: connect refused' >&2
  exit 255
fi
case "$*" in
  *"queue add"*--json*) echo '{"id": "q-remote-0001"}';;
esac
exit 0
"""


def _load():
    loader = SourceFileLoader("hostjob_under_test_qremote", str(HOSTJOB))
    spec = importlib.util.spec_from_loader("hostjob_under_test_qremote", loader)
    mod = importlib.util.module_from_spec(spec)
    loader.exec_module(mod)
    return mod


class _Args:
    def __init__(self, label, cmd):
        self.label, self.cmd = label, cmd
        self.cwd = None
        self.force = False
        self.no_queue = False
        self.no_broker = True
        self.depends_on = []


class RemoteQueueTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.mod = _load()
        cls.iso = hostjob_testkit.install(cls.mod, prefix="hostjob-qremote-test-")
        cls.ssh_log = os.path.join(cls.iso.tmp, "ssh.log")
        ssh = os.path.join(cls.iso.bindir, "ssh")
        with open(ssh, "w") as f:
            f.write(SSH_SHIM)
        os.chmod(ssh, 0o755)
        os.environ[SSH_LOG_ENV] = cls.ssh_log
        cls.st_shim = os.path.join(cls.iso.bindir, "session-task")
        # Hide EVERY other session-task on PATH (conftest's outer shim, a real
        # install) so "local absent" is genuinely absent; our bindir is kept.
        cls.saved_path = os.environ["PATH"]
        os.environ["PATH"] = os.pathsep.join(
            d for d in cls.saved_path.split(os.pathsep)
            if d == cls.iso.bindir
            or not os.path.exists(os.path.join(d, "session-task")))
        with open(cls.st_shim) as f:
            cls.st_shim_src = f.read()

    @classmethod
    def tearDownClass(cls):
        os.environ["PATH"] = cls.saved_path
        for v in (SSH_LOG_ENV, "HOSTJOB_TEST_SSH_FAIL", "HOSTJOB_QUEUE_REMOTE"):
            os.environ.pop(v, None)
        cls.iso.teardown()

    def setUp(self):
        self.iso.reset_logs()
        if os.path.exists(self.ssh_log):
            os.remove(self.ssh_log)
        os.environ.pop("HOSTJOB_TEST_SSH_FAIL", None)
        os.environ.pop("HOSTJOB_QUEUE_REMOTE", None)
        self._local(False)

    def _local(self, present):
        if present and not os.path.exists(self.st_shim):
            with open(self.st_shim, "w") as f:
                f.write(self.st_shim_src)
            os.chmod(self.st_shim, 0o755)
        elif not present and os.path.exists(self.st_shim):
            os.remove(self.st_shim)

    def _ssh_lines(self):
        try:
            with open(self.ssh_log) as f:
                return [ln.rstrip("\n") for ln in f if ln.strip()]
        except FileNotFoundError:
            return []

    def _wait_terminal(self, label, timeout=8):
        end = time.time() + timeout
        while time.time() < end:
            st = self.mod.read_status(label)
            if st and st.get("status") != "running":
                return st
            time.sleep(0.05)
        return self.mod.read_status(label)

    def _wait_ssh(self, needle, timeout=5):
        end = time.time() + timeout
        while time.time() < end:
            hits = [ln for ln in self._ssh_lines() if needle in ln]
            if hits:
                return hits
            time.sleep(0.05)
        return []

    def _run(self, label, cmd):
        out, err = io.StringIO(), io.StringIO()
        with redirect_stdout(out), redirect_stderr(err):
            rc = self.mod.cmd_run(_Args(label, cmd))
        return rc, out.getvalue(), err.getvalue()

    def _clean(self, label):
        self.mod.cmd_clean(type("A", (), {"label": label, "all": False})())

    def test_local_path_unchanged_and_preferred(self):
        self._local(True)
        os.environ["HOSTJOB_QUEUE_REMOTE"] = "ssh://op@queue.example"
        rc, _, _ = self._run("loc", ["true"])
        self.assertEqual(rc, 0)
        self._wait_terminal("loc")
        st = self.mod.read_status("loc")
        self.assertEqual(st["queue_id"], "q-test-0001")
        self.assertEqual(st["queue_backend"], self.st_shim)
        self.assertIsNone(st.get("queue_missing"))
        self.assertEqual(self._ssh_lines(), [],
                         "ssh must not be used when local session-task exists")
        lines = self.iso.calls(hostjob_testkit.SESSION_TASK)
        self.assertTrue([ln for ln in lines if ln.startswith("queue add")], lines)
        self.assertTrue([ln for ln in lines if ln.startswith("queue register")], lines)
        self._clean("loc")

    def test_remote_reachable(self):
        os.environ["HOSTJOB_QUEUE_REMOTE"] = "ssh://op@queue.example:2222"
        rc, out, err = self._run("rem", ["true"])
        self.assertEqual(rc, 0, err)
        self._wait_terminal("rem")
        st = self.mod.read_status("rem")
        self.assertEqual(st["queue_id"], "q-remote-0001")
        self.assertIsInstance(st["queue_backend"], dict)
        self.assertIsNone(st.get("queue_missing"))
        self.assertIn("q-remote-0001", out)
        add = self._wait_ssh("queue add")
        self.assertEqual(len(add), 1, self._ssh_lines())
        self.assertIn("-p 2222", add[0])
        self.assertIn("BatchMode=yes", add[0])
        self.assertIn("op@queue.example", add[0])
        self.assertIn("--scope hostjob:rem", add[0])
        self.assertTrue(self._wait_ssh("queue register q-remote-0001"))
        # reaper closes the remote row on exit (rc 0 -> done)
        self.assertTrue(self._wait_ssh("queue done q-remote-0001"),
                        self._ssh_lines())
        self._clean("rem")

    def _write_cfg(self, text):
        d = os.path.join(self.iso.tmp, ".config", "claude-container")
        os.makedirs(d, exist_ok=True)
        with open(os.path.join(d, "hostjob-queue-remote"), "w") as f:
            f.write(text)
        self._old_home = os.environ.get("HOME")
        os.environ["HOME"] = self.iso.tmp
        def _restore():
            os.environ["HOME"] = self._old_home
            os.remove(os.path.join(d, "hostjob-queue-remote"))
            os.environ.pop("HOSTJOB_QUEUE_REMOTE_CMD", None)
        self.addCleanup(_restore)

    def test_file_cmd_used(self):
        self._write_cfg("# c\nssh://op@queue.example\ncmd=/opt/bin/session-task\n")
        be = self.mod._queue_backend()[0]
        self.assertEqual(be["cmd"], "/opt/bin/session-task")
        self.assertIn("op@queue.example", be["ssh"])
        rc, _, err = self._run("filecmd", ["true"])
        self.assertEqual(rc, 0, err)
        add = self._wait_ssh("queue add")
        self.assertIn("/opt/bin/session-task queue add", add[0])
        self._wait_terminal("filecmd")
        self._clean("filecmd")

    def test_env_cmd_overrides_file(self):
        self._write_cfg("ssh://op@queue.example\ncmd=/opt/bin/session-task\n")
        os.environ["HOSTJOB_QUEUE_REMOTE_CMD"] = "/env/session-task"
        self.assertEqual(self.mod._queue_backend()[0]["cmd"], "/env/session-task")

    def test_cmd_line_first_still_finds_url(self):
        self._write_cfg("cmd=/opt/bin/session-task\nssh://op@queue.example\n")
        self.assertEqual(self.mod._remote_setting(), "ssh://op@queue.example")

    def test_old_single_line_file_still_works(self):
        self._write_cfg("ssh://op@queue.example\n")
        be = self.mod._queue_backend()[0]
        self.assertEqual(be["cmd"], "session-task")
        self.assertEqual(self.mod._remote_setting(), "ssh://op@queue.example")

    def test_remote_failure_abandons_with_exit_code(self):
        os.environ["HOSTJOB_QUEUE_REMOTE"] = "ssh://op@queue.example"
        self._run("remfail", ["false"])
        self._wait_terminal("remfail")
        ab = self._wait_ssh("queue abandon")
        self.assertTrue(ab, self._ssh_lines())
        self.assertIn("hostjob exit 1", ab[0])
        self._clean("remfail")

    def test_remote_unreachable_job_runs_and_warns(self):
        os.environ["HOSTJOB_QUEUE_REMOTE"] = "ssh://op@queue.example"
        os.environ["HOSTJOB_TEST_SSH_FAIL"] = "1"
        rc, out, err = self._run("down", ["true"])
        self.assertEqual(rc, 0, "queue errors must never fail the job")
        st = self._wait_terminal("down")
        self.assertEqual(st["status"], "done")
        self.assertEqual(st["rc"], 0)
        self.assertIsNone(st["queue_id"])
        self.assertIn("exited 255", st["queue_missing"])
        self.assertIn("WARNING", err)
        self.assertIn("UNqueued", err)
        self.assertIn("NO ROW", out)
        self._clean("down")

    def test_no_local_no_remote_records_missing(self):
        rc, out, err = self._run("none", ["true"])
        self.assertEqual(rc, 0)
        st = self._wait_terminal("none")
        self.assertEqual(st["status"], "done")
        self.assertIn("no HOSTJOB_QUEUE_REMOTE", st["queue_missing"])
        self.assertIn("WARNING", err)
        self._clean("none")

    def test_invalid_remote_spec(self):
        os.environ["HOSTJOB_QUEUE_REMOTE"] = "http://nope"
        rc, _, err = self._run("bad", ["true"])
        self.assertEqual(rc, 0)
        st = self._wait_terminal("bad")
        self.assertIn("invalid HOSTJOB_QUEUE_REMOTE", st["queue_missing"])
        self._clean("bad")

    def test_remote_args_are_shell_quoted(self):
        be = self.mod._remote_backend("ssh://op@h")
        argv = self.mod._st_argv(be, "queue", "add", "a b; rm -rf /")
        self.assertEqual(argv[-1], "session-task queue add 'a b; rm -rf /'")


if __name__ == "__main__":
    unittest.main()
