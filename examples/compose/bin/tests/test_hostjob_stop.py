#!/usr/bin/env python3
"""Integration test for `hostjob stop`.

Mirrors test_hostjob_broker.py: loads the `hostjob` script as a module via
SourceFileLoader and exercises the stop subcommand end to end against a real
detached worker. Stdlib `unittest` only (authored before a pytest dir existed).

Covers:
  1. start a trivial `sleep` job, `stop` it -> worker pid is killed, state
     flips to a terminal "stopped", and `clean` then removes it.
  2. the recycled-pid guard: a status.json whose recorded pid is alive but
     whose cmd does NOT match the live cmdline is REFUSED (rc=1), and
     --force overrides the guard.
  3. stop on an already-gone pid marks the job stopped (not an error).
  4. stop on a missing label errors.
  5. the queue/event traffic a `run` generates is confined to the harness.

Operator state is isolated by `hostjob_testkit.install` — see that module for
why `--no-queue` is NOT an opt-out of the queue row.

Run::

    python3 -m pytest test_hostjob_stop.py -v
    # or
    python3 test_hostjob_stop.py
"""

from __future__ import annotations

import importlib.util
import sys
import time
import unittest
from importlib.machinery import SourceFileLoader
from pathlib import Path

HERE = Path(__file__).resolve().parent
HOSTJOB = HERE.parent / "hostjob"

# Importable when run directly (`python3 test_hostjob_stop.py`), not just
# under pytest's rootdir-prepend.
if str(HERE) not in sys.path:
    sys.path.insert(0, str(HERE))

import hostjob_testkit  # noqa: E402


def _load_hostjob():
    loader = SourceFileLoader("hostjob_under_test", str(HOSTJOB))
    spec = importlib.util.spec_from_loader("hostjob_under_test", loader)
    mod = importlib.util.module_from_spec(spec)
    loader.exec_module(mod)
    return mod


class _RunArgs:
    def __init__(self, label, cmd, cwd=None):
        self.label = label
        self.cmd = cmd
        self.cwd = cwd
        self.force = False
        # NB: --no-queue does NOT mean "no queue row" — it only skips the
        # scope-claiming `queue register`. The row is still created, which is
        # why isolation (hostjob_testkit) is what keeps this off the
        # operator's queue, not this flag.
        self.no_queue = True
        self.no_broker = True     # don't spin up the live-tail broker
        self.depends_on = []


class _StopArgs:
    def __init__(self, label=None, all=False, grace=2, force=False):
        self.label = label
        self.all = all
        self.grace = grace
        self.force = force


class _CleanArgs:
    def __init__(self, label=None, all=False):
        self.label = label
        self.all = all


class StopTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.mod = _load_hostjob()
        # Isolate EVERY piece of operator state a run touches: hostjob's
        # ~/.cache/hostjob job dirs, the session-task queue, and the
        # claude-event stream.
        cls.iso = hostjob_testkit.install(cls.mod, prefix="hostjob-stop-test-")

    @classmethod
    def tearDownClass(cls):
        cls.iso.teardown()

    def setUp(self):
        self.iso.reset_logs()

    def _wait_pid_dead(self, pid, timeout=5):
        deadline = time.time() + timeout
        while time.time() < deadline:
            if not self.mod.pid_alive(pid):
                return True
            time.sleep(0.05)
        return False

    def test_stop_kills_running_worker_and_clean_removes(self):
        label = "sleep-job"
        self.mod.cmd_run(_RunArgs(label, ["sleep", "120"]))
        # Wait for the reaper to record the worker pid.
        pid = None
        deadline = time.time() + 5
        while time.time() < deadline:
            st = self.mod.read_status(label)
            if st and st.get("pid"):
                pid = st["pid"]
                break
            time.sleep(0.05)
        self.assertIsNotNone(pid, "reaper never recorded a worker pid")
        self.assertTrue(self.mod.pid_alive(pid), "worker not alive before stop")

        rc = self.mod.cmd_stop(_StopArgs(label=label, grace=2))
        self.assertEqual(rc, 0, "stop returned non-zero")
        self.assertTrue(self._wait_pid_dead(pid), "worker pid still alive after stop")

        st = self.mod.read_status(label)
        self.assertEqual(st.get("status"), "stopped", st)
        self.assertIsNotNone(st.get("ended_at"))

        # clean removes the now-terminal job.
        rc = self.mod.cmd_clean(_CleanArgs(label=label))
        self.assertEqual(rc, 0)
        self.assertIsNone(self.mod.read_status(label), "clean did not remove state")

    def test_recycled_pid_guard_refuses_then_force_overrides(self):
        label = "guard-job"
        # Start a long-lived worker we can read a real live pid from.
        self.mod.cmd_run(_RunArgs(label, ["sleep", "120"]))
        pid = None
        deadline = time.time() + 5
        while time.time() < deadline:
            st = self.mod.read_status(label)
            if st and st.get("pid"):
                pid = st["pid"]
                break
            time.sleep(0.05)
        self.assertIsNotNone(pid)

        # Rewrite the recorded cmd to something that will NOT match the live
        # `sleep 120` cmdline -> the guard should refuse.
        st = self.mod.read_status(label)
        st["cmd"] = ["/usr/bin/totally-unrelated-binary", "--flag"]
        self.mod.write_status(label, st)

        rc = self.mod.cmd_stop(_StopArgs(label=label, grace=2))
        self.assertEqual(rc, 1, "guard should refuse a mismatched pid")
        self.assertTrue(self.mod.pid_alive(pid), "guard refusal must NOT kill")

        # --force overrides the guard and kills it.
        rc = self.mod.cmd_stop(_StopArgs(label=label, grace=2, force=True))
        self.assertEqual(rc, 0)
        self.assertTrue(self._wait_pid_dead(pid), "force-stop did not kill pid")
        self.mod.cmd_clean(_CleanArgs(label=label))

    def test_stop_already_gone_pid_marks_stopped(self):
        label = "ghost-job"
        # Hand-craft a status stuck 'running' with a pid that is not alive.
        dead_pid = 999999  # almost certainly not a live pid
        self.assertFalse(self.mod.pid_alive(dead_pid))
        self.mod.write_status(label, {
            "label": label, "cmd": ["sleep", "1"], "cwd": None,
            "status": "running", "rc": None, "pid": dead_pid,
            "reaper_pid": dead_pid, "started_at": time.time(),
            "ended_at": None, "queue_id": None,
        })
        rc = self.mod.cmd_stop(_StopArgs(label=label, grace=2))
        # pid gone => marked stopped, NOT an error.
        self.assertEqual(rc, 0)
        st = self.mod.read_status(label)
        self.assertIn(st.get("status"), ("stopped", "crashed"), st)
        self.mod.cmd_clean(_CleanArgs(label=label))

    def test_stop_missing_label_errors(self):
        rc = self.mod.cmd_stop(_StopArgs(label="no-such-job-xyz", grace=2))
        self.assertEqual(rc, 1)

    def test_run_queue_traffic_stays_inside_the_harness(self):
        """Regression guard: these tests used to leak rows into the operator's
        live queue. `run` DOES create a queue row even with --no-queue, so the
        only thing keeping it off the real queue is the isolation — assert the
        isolation is actually in force and that the row landed on the shim."""
        self.assertTrue(self.iso.shimmed(hostjob_testkit.SESSION_TASK),
                        "session-task must resolve to the harness shim")
        self.assertTrue(self.iso.shimmed(hostjob_testkit.CLAUDE_EVENT),
                        "claude-event must resolve to the harness shim")

        label = "isolation-job"
        self.mod.cmd_run(_RunArgs(label, ["true"]))
        adds = self.iso.calls_starting("queue add")
        self.assertEqual(len(adds), 1,
                         "run must create exactly one (isolated) queue row: %r"
                         % self.iso.calls())
        self.assertIn("--scope hostjob:%s" % label, adds[0])

        # Let the reaper land its terminal flip + hostjob-done event, and
        # confirm those went to the shims too rather than the real CLIs.
        deadline = time.time() + 8
        while time.time() < deadline:
            st = self.mod.read_status(label)
            if st and st.get("status") != "running":
                break
            time.sleep(0.05)
        deadline = time.time() + 5
        while time.time() < deadline:
            if self.iso.calls(hostjob_testkit.CLAUDE_EVENT):
                break
            time.sleep(0.05)
        self.assertTrue(
            self.iso.calls_starting("queue done")
            or self.iso.calls_starting("queue abandon"),
            "reaper's terminal queue flip must hit the shim: %r"
            % self.iso.calls())
        self.assertTrue(
            self.iso.calls_starting("emit", hostjob_testkit.CLAUDE_EVENT),
            "hostjob-done event must hit the shim: %r"
            % self.iso.calls(hostjob_testkit.CLAUDE_EVENT))
        self.mod.cmd_clean(_CleanArgs(label=label))


if __name__ == "__main__":
    unittest.main(verbosity=2)
