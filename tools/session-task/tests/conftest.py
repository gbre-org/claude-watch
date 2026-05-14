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
os.environ.setdefault("CLAUDE_AGENTS_STATE_FALLBACK_BIN", "")
