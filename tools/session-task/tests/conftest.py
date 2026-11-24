"""pytest auto-config for session-task tests.

Suppress real ``pingme`` push notifications for ALL tests in this dir.

Several tests build their subprocess env from ``dict(os.environ)``
without explicitly setting ``PINGME_SESSION_TASK=0``. If the outer
shell didn't set it either, the spawned ``session-task`` process will
shell out to the real ``pingme`` binary -- producing real push
notifications on every ``queue register`` / ``queue done`` /
``queue abandon`` during a test run.

Setting ``PINGME_SESSION_TASK=0`` via ``setdefault`` here means:

* Tests that DON'T explicitly manage this var inherit ``=0`` and stay
  silent (no push-notification noise to the maintainer's phone).
* ``test_queue_pingme.py`` keeps working because it explicitly DELETES
  the var when it wants pingme to fire AND installs a fake ``pingme``
  shim onto a controlled PATH -- so the un-suppressed code path can't
  reach the real binary.

Why ``CLAUDE_EVENT_SESSION_TASK`` is NOT suppressed here:

* claude-event writes go to ``$HOME/claude-events/``, and every test
  sets ``HOME`` to a tempdir -- so events land in the per-test tmpdir
  and never reach the real bus.
* Several tests (``test_queue_claude_event.py``, parts of
  ``test_queue_force_start.py``) ASSERT events are emitted; suppressing
  the var here would break them.

If a future test starts shelling out to a process that calls real
``claude-event`` against the real ``$HOME``, that test should set the
suppression itself (via the same ``env[...] = "0"`` pattern), not push
the burden up here.

Why ``CLAUDE_EVENT_QUEUE`` / ``CRON_EVENT_QUEUE`` ARE stripped here
(previously broken -- fixed 2026-09-16):

* The ``$HOME``-tempdir isolation claimed above only holds if
  ``claude-event`` actually falls through to ``$HOME``. It doesn't when
  either env var is already set in the OUTER shell: ``claude-event``
  resolves its queue dir as
  ``os.environ.get("CLAUDE_EVENT_QUEUE", os.environ.get("CRON_EVENT_QUEUE",
  str(Path.home() / "claude-events")))`` -- i.e. these vars take
  PRIORITY over ``$HOME``.
* Several test files build the subprocess env via ``os.environ.copy()``
  / ``dict(os.environ)`` and only override ``HOME``. If the ambient shell
  (e.g. a live container session) exports ``CLAUDE_EVENT_QUEUE``, that
  value rides straight through the copy and every claude-event emitted
  during the test run lands in the REAL live bus, not the per-test
  tmpdir -- silently defeating the isolation this docstring used to
  assume was automatic. (A couple of individual test files -- see
  ``test_queue_force_start_co_run.py`` / ``test_queue_force_start_new_scope.py``
  -- had already worked around this ad hoc with their own
  ``env.pop("CLAUDE_EVENT_QUEUE", None)``; most others hadn't.)
* Fix: pop both vars from THIS process's own ``os.environ`` at import
  time (not ``setdefault`` -- they are already set in the leaking case,
  so ``setdefault`` would no-op). Every test that copies from
  ``os.environ`` now copies from an already-scrubbed environment, so
  ``HOME`` becomes the only source of the queue dir again, with no
  per-test-file changes required. A test that genuinely needs a REAL
  ``CLAUDE_EVENT_QUEUE`` value should re-set it itself, explicitly, in
  that test.

Why ``CLAUDE_AGENTS_STATE_FALLBACK_BIN`` is suppressed here:

* When ``CLAUDE_AGENTS_STATE`` points at a missing / empty tmp file
  (the normal test setup), session-task's _load_active_agents_state
  falls back to invoking ``claude-watch active-agents --json`` off
  PATH. On a developer machine ``claude-watch`` IS on PATH, so the
  fallback returns the live host's agent map — silently overriding
  the test's curated empty state and breaking "no agent record"
  assertions.
* Tests that WANT to exercise the fallback path (``test_queue_archive``'s
  fallback-specific tests) explicitly install a shim on PATH and
  override the env back to ``"claude-watch"``.
"""
import os

os.environ.setdefault("PINGME_SESSION_TASK", "0")

# Make the transient-read retry backoff near-instant under test (production
# keeps its ~2s worst-case budget). The retry COUNT is unchanged, so the
# ENOENT/recovery tests still exercise every retry.
os.environ.setdefault("SESSION_TASK_READ_RETRY_SLEEP", "0.001")

# Strip (not setdefault -- see docstring above): these are typically
# ALREADY SET in the ambient shell, and the whole point is overriding an
# already-set value so tests that copy os.environ don't leak onto the
# real live event bus.
os.environ.pop("CLAUDE_EVENT_QUEUE", None)
os.environ.pop("CRON_EVENT_QUEUE", None)
os.environ.setdefault("CLAUDE_AGENTS_STATE_FALLBACK_BIN", "")
