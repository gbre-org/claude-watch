#!/usr/bin/env python3
"""Operator-state isolation for the in-process `hostjob` tests.

Why this exists
---------------
`hostjob` is not a pure function: every `run` shells out to the operator's
real CLIs.

  * `session-task queue add` — EVERY job gets a first-class queue row
    (scope ``hostjob:<label>``, ``--created-by hostjob``, ``--force-enqueue``),
    followed by `queue register` unless ``--no-queue`` was passed, and a
    `queue done` / `queue abandon` from the detached reaper on exit.
  * `claude-event emit` — the reaper emits a ``hostjob-done`` event.

The trap: ``--no-queue`` does NOT mean "no queue row". It only skips the
scope-claiming `queue register`; the row is still created and stays
``pending``. A test that sets ``no_queue=True`` believing it opts out of the
queue therefore still writes into the operator's live queue, where the rows sit
until a human abandons them by hand — and a stale-ready sweep can then block
unrelated operator work. That is a test reaching out of its sandbox.

Isolation, not cleanup
----------------------
Teardown that abandons the rows afterwards still leaks whenever a run crashes,
is killed, or exits early. So the tests never create real rows in the first
place. `install()` puts two independent layers in front of the real state:

  1. **PATH shims** — recording stand-ins for `session-task` and
     `claude-event` in a temp bin dir at the front of ``$PATH``. `hostjob`
     resolves both via ``shutil.which``, so the shims capture every call, make
     assertions possible, and keep the tests fast and deterministic (they do
     not depend on the real CLIs being installed at all).
  2. **A throwaway ``$HOME``** — the belt-and-braces layer. Both CLIs derive
     their state from ``$HOME`` (``~/.config/session/queue.json``,
     ``~/claude-events``), so even a call that misses the shims — a tool we
     did not think to shim, or a detached reaper still exiting after the shim
     dir has been torn down — lands in the tempdir.

`hostjob`'s own job state (``STATE_ROOT``, normally ``~/.cache/hostjob``) is
repointed at the tempdir too.

The env is inherited by the detached reaper (`cmd_run` spawns it with
``env=dict(os.environ)``), so mutating ``os.environ`` in-process is enough to
isolate the asynchronous half as well.

Usage (stdlib ``unittest``, matching the existing test files)::

    class MyTest(unittest.TestCase):
        @classmethod
        def setUpClass(cls):
            cls.mod = _load_hostjob()
            cls.iso = hostjob_testkit.install(cls.mod)

        @classmethod
        def tearDownClass(cls):
            cls.iso.teardown()

`conftest.py` in this directory installs an outer session-scoped isolation as a
backstop, so a test file that forgets to call `install()` still cannot touch
operator state under pytest.
"""

from __future__ import annotations

import os
import shutil
import tempfile

__all__ = ["Isolation", "install", "SESSION_TASK", "CLAUDE_EVENT"]

SESSION_TASK = "session-task"
CLAUDE_EVENT = "claude-event"

# Env var each shim writes its argv log to. One line per invocation,
# space-joined argv (argv[0] excluded), append mode.
_LOG_ENV = {
    SESSION_TASK: "HOSTJOB_TEST_SESSION_TASK_LOG",
    CLAUDE_EVENT: "HOSTJOB_TEST_CLAUDE_EVENT_LOG",
}

# The q-id the fake `session-task` hands back from `queue add --json`, so
# `_register_queue_item` has something to parse and thread through status.json.
FAKE_QUEUE_ID = "q-test-0001"

_SHIM_HEADER = """#!/usr/bin/env bash
# Recording stand-in installed by hostjob_testkit — never the real CLI.
log="${%(logvar)s:?%(logvar)s unset}"
printf '%%s\\n' "$*" >> "$log"
"""

# `queue add ... --json` must emit a parseable q-id; everything else just logs.
_SHIM_BODY = {
    SESSION_TASK: """
if [ "$1" = "queue" ] && [ "$2" = "add" ]; then
    for a in "$@"; do
        if [ "$a" = "--json" ]; then
            echo '{"id": "%s"}'
            break
        fi
    done
fi
exit 0
""" % FAKE_QUEUE_ID,
    CLAUDE_EVENT: """
exit 0
""",
}


class Isolation:
    """Handle returned by `install`. Restores everything in `teardown`."""

    def __init__(self, tmp, bindir, logs, saved_env, mod=None, saved_state_root=None):
        self.tmp = tmp
        self.bindir = bindir
        self.logs = logs                    # {tool_name: log path}
        self._saved_env = saved_env         # {var: original value or None}
        self._mod = mod
        self._saved_state_root = saved_state_root
        self._torn_down = False

    # -- assertions helpers -------------------------------------------------

    def calls(self, tool=SESSION_TASK):
        """Every invocation of `tool` so far, one space-joined argv per entry."""
        try:
            with open(self.logs[tool]) as f:
                return [ln.rstrip("\n") for ln in f if ln.strip()]
        except FileNotFoundError:
            return []

    def calls_starting(self, prefix, tool=SESSION_TASK):
        """Invocations of `tool` whose argv line starts with `prefix`."""
        return [ln for ln in self.calls(tool) if ln.startswith(prefix)]

    def reset_logs(self):
        """Drop every recorded invocation (call from `setUp` for per-test logs)."""
        for path in self.logs.values():
            try:
                os.remove(path)
            except FileNotFoundError:
                pass

    def shimmed(self, tool=SESSION_TASK):
        """True if `tool` currently resolves to this Isolation's shim."""
        found = shutil.which(tool)
        return bool(found) and os.path.dirname(os.path.abspath(found)) == self.bindir

    # -- lifecycle ----------------------------------------------------------

    def teardown(self):
        if self._torn_down:
            return
        self._torn_down = True
        if self._mod is not None and self._saved_state_root is not None:
            self._mod.STATE_ROOT = self._saved_state_root
        for var, old in self._saved_env.items():
            if old is None:
                os.environ.pop(var, None)
            else:
                os.environ[var] = old
        shutil.rmtree(self.tmp, ignore_errors=True)


def _write_shim(bindir, tool):
    path = os.path.join(bindir, tool)
    with open(path, "w") as f:
        f.write(_SHIM_HEADER % {"logvar": _LOG_ENV[tool]})
        f.write(_SHIM_BODY[tool])
    os.chmod(path, 0o755)
    return path


def install(mod=None, prefix="hostjob-test-"):
    """Isolate every piece of operator state a `hostjob` run would touch.

    `mod` is an already-loaded `hostjob` module whose ``STATE_ROOT`` should be
    repointed at the tempdir; pass ``None`` to isolate only the environment
    (what `conftest.py`'s backstop does).

    Returns an `Isolation`; call ``.teardown()`` when done.
    """
    tmp = tempfile.mkdtemp(prefix=prefix)
    bindir = os.path.join(tmp, "bin")
    os.makedirs(bindir, exist_ok=True)

    logs = {}
    for tool in (SESSION_TASK, CLAUDE_EVENT):
        _write_shim(bindir, tool)
        logs[tool] = os.path.join(tmp, "%s.calls.log" % tool)

    saved_env = {}
    for var in ("PATH", "HOME", *_LOG_ENV.values()):
        saved_env[var] = os.environ.get(var)

    os.environ["PATH"] = bindir + os.pathsep + (saved_env["PATH"] or "")
    # Throwaway HOME: session-task's ~/.config/session/queue.json and
    # claude-event's ~/claude-events both hang off it, so an unshimmed call
    # still cannot reach operator state.
    os.environ["HOME"] = tmp
    for tool, var in _LOG_ENV.items():
        os.environ[var] = logs[tool]

    saved_state_root = None
    if mod is not None:
        saved_state_root = mod.STATE_ROOT
        mod.STATE_ROOT = os.path.join(tmp, "hostjob-state")

    return Isolation(tmp, bindir, logs, saved_env, mod, saved_state_root)
