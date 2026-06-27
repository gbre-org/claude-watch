#!/usr/bin/env python3
"""Queue-integration tests for `hostjob`.

Mirrors test_hostjob_stop.py: loads the `hostjob` script as a module via
SourceFileLoader, monkeypatches `mod.STATE_ROOT` to a tmpdir, and exercises
the queue-row behavior. Stdlib `unittest` only (these were authored before a pytest
config).

Because a real `session-task` may not be on the host PATH in the test env, we
inject a FAKE `session-task` shim onto PATH: a tiny shell script that records
its argv (one line per invocation) to a log file and, on `queue add --json`,
emits a JSON `{"id": "q-test-NNNN"}` on stdout. The reaper / `_register_queue_item`
resolve it via shutil.which, so the shim captures every queue call.

Covers (the Option A redefinition of --no-queue):
  1. default `run` (no --no-queue) -> register=True -> BOTH `queue add` AND
     `queue register` invoked; status.json has queue_id set.
  2. `run --no-queue` -> register=False -> `queue add` invoked but NO
     `queue register`; status.json STILL has queue_id set (THE CORE FIX:
     the row exists even with --no-queue).
  3. `--depends-on q-AAA,q-BBB` (and repeated) pass through to `queue add`
     as `--depends-on` args.
  4. `list` output includes the queue_id.

Run::

    python3 test_hostjob_queue.py
"""

from __future__ import annotations

import importlib.util
import io
import os
import shutil
import stat
import tempfile
import time
import unittest
from contextlib import redirect_stdout
from importlib.machinery import SourceFileLoader
from pathlib import Path

HERE = Path(__file__).resolve().parent
HOSTJOB = HERE.parent / "hostjob"


def _load_hostjob():
    loader = SourceFileLoader("hostjob_under_test_queue", str(HOSTJOB))
    spec = importlib.util.spec_from_loader("hostjob_under_test_queue", loader)
    mod = importlib.util.module_from_spec(spec)
    loader.exec_module(mod)
    return mod


# A fake `session-task` shim. Records every invocation's argv to $STQ_LOG
# (one space-joined line per call), and on `queue add ... --json` prints a
# JSON object with an "id" so _register_queue_item can parse a q-id.
_SHIM = r"""#!/usr/bin/env bash
log="${STQ_LOG:?STQ_LOG unset}"
printf '%s\n' "$*" >> "$log"
# Detect `queue add ... --json` and emit a q-id JSON on stdout.
if [ "$1" = "queue" ] && [ "$2" = "add" ]; then
    for a in "$@"; do
        if [ "$a" = "--json" ]; then
            echo '{"id": "q-test-0001"}'
            break
        fi
    done
fi
exit 0
"""


class _RunArgs:
    def __init__(self, label, cmd, cwd=None, no_queue=False, depends_on=None):
        self.label = label
        self.cmd = cmd
        self.cwd = cwd
        self.force = False
        self.no_queue = no_queue
        self.no_broker = True       # never spin up the live-tail broker
        self.depends_on = depends_on if depends_on is not None else []


class _ListArgs:
    pass


class QueueTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.mod = _load_hostjob()
        cls.tmp = tempfile.mkdtemp(prefix="hostjob-queue-test-")
        cls._orig_root = cls.mod.STATE_ROOT
        cls.mod.STATE_ROOT = cls.tmp

        # Install the fake session-task shim onto PATH.
        cls.bindir = os.path.join(cls.tmp, "bin")
        os.makedirs(cls.bindir, exist_ok=True)
        cls.shim = os.path.join(cls.bindir, "session-task")
        with open(cls.shim, "w") as f:
            f.write(_SHIM)
        os.chmod(cls.shim, 0o755)
        cls._orig_path = os.environ.get("PATH", "")
        os.environ["PATH"] = cls.bindir + os.pathsep + cls._orig_path

    @classmethod
    def tearDownClass(cls):
        cls.mod.STATE_ROOT = cls._orig_root
        os.environ["PATH"] = cls._orig_path
        shutil.rmtree(cls.tmp, ignore_errors=True)

    def setUp(self):
        # Fresh shim log per test.
        self.stq_log = os.path.join(self.tmp, "stq-%d.log" % id(self))
        if os.path.exists(self.stq_log):
            os.remove(self.stq_log)
        os.environ["STQ_LOG"] = self.stq_log

    def _shim_lines(self):
        try:
            with open(self.stq_log) as f:
                return [ln.rstrip("\n") for ln in f if ln.strip()]
        except FileNotFoundError:
            return []

    def _wait_terminal(self, label, timeout=8):
        """Run a fast worker; wait for the reaper to finish so finalize_queue
        (done/abandon) has fired and the status is terminal."""
        deadline = time.time() + timeout
        while time.time() < deadline:
            st = self.mod.read_status(label)
            if st and st.get("status") != "running":
                return st
            time.sleep(0.05)
        return self.mod.read_status(label)

    def test_default_run_adds_and_registers(self):
        label = "default-q"
        rc = self.mod.cmd_run(_RunArgs(label, ["true"], no_queue=False))
        self.assertEqual(rc, 0)
        st = self.mod.read_status(label)
        self.assertEqual(st.get("queue_id"), "q-test-0001", st)

        lines = self._shim_lines()
        add_calls = [l for l in lines if l.startswith("queue add")]
        reg_calls = [l for l in lines if l.startswith("queue register")]
        self.assertEqual(len(add_calls), 1, lines)
        self.assertEqual(len(reg_calls), 1,
                         "default run must REGISTER (claim scope): %r" % lines)
        # The add must carry scope + created-by + force-enqueue + json.
        self.assertIn("--scope hostjob:%s" % label, add_calls[0])
        self.assertIn("--created-by hostjob", add_calls[0])
        self.assertIn("--force-enqueue", add_calls[0])
        self.assertIn("--json", add_calls[0])
        self.assertIn("q-test-0001", reg_calls[0])

        # Let the reaper finish so finalize_queue runs (done on rc==0).
        self._wait_terminal(label)
        self.mod.cmd_clean(type("A", (), {"label": label, "all": False})())

    def test_no_queue_run_adds_but_does_not_register(self):
        # THE CORE FIX: --no-queue still creates a (pending) row with a q-id;
        # it only skips the scope-claiming `queue register`.
        label = "nq-q"
        rc = self.mod.cmd_run(_RunArgs(label, ["true"], no_queue=True))
        self.assertEqual(rc, 0)
        st = self.mod.read_status(label)
        self.assertEqual(st.get("queue_id"), "q-test-0001",
                         "--no-queue must STILL create a row with a q-id: %r" % st)

        lines = self._shim_lines()
        add_calls = [l for l in lines if l.startswith("queue add")]
        reg_calls = [l for l in lines if l.startswith("queue register")]
        self.assertEqual(len(add_calls), 1, lines)
        self.assertEqual(len(reg_calls), 0,
                         "--no-queue must NOT register (non-serializing): %r" % lines)

        self._wait_terminal(label)
        self.mod.cmd_clean(type("A", (), {"label": label, "all": False})())

    def test_depends_on_passes_through_to_queue_add(self):
        label = "dep-q"
        rc = self.mod.cmd_run(
            _RunArgs(label, ["true"], no_queue=True,
                     depends_on=["q-AAA", "q-BBB"]))
        self.assertEqual(rc, 0)
        lines = self._shim_lines()
        add_calls = [l for l in lines if l.startswith("queue add")]
        self.assertEqual(len(add_calls), 1, lines)
        self.assertIn("--depends-on q-AAA", add_calls[0])
        self.assertIn("--depends-on q-BBB", add_calls[0])

        self._wait_terminal(label)
        self.mod.cmd_clean(type("A", (), {"label": label, "all": False})())

    def test_list_shows_queue_id(self):
        label = "list-q"
        self.mod.cmd_run(_RunArgs(label, ["true"], no_queue=False))
        self._wait_terminal(label)
        buf = io.StringIO()
        with redirect_stdout(buf):
            self.mod.cmd_list(_ListArgs())
        out = buf.getvalue()
        self.assertIn(label, out)
        self.assertIn("q-test-0001", out,
                      "list output must include the queue_id: %r" % out)
        self.mod.cmd_clean(type("A", (), {"label": label, "all": False})())


if __name__ == "__main__":
    unittest.main(verbosity=2)
