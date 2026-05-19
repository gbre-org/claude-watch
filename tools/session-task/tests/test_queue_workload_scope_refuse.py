#!/usr/bin/env python3
"""Tests for the workload-scope manual-queueing refusal in cmd_queue_add.

Background: ``workload run <label>`` auto-creates a queue item with
scope ``["workload:<label>"]`` itself. If a caller ALSO queues a
``workload:<label>`` item manually BEFORE invoking ``workload run``,
the labels can drift (e.g. ``workload:promote-ready-oa-pitt`` vs the
runner's ``workload:promote-oa-pitt``) and two parallel queue rows
end up tracking one tmux pane. Andrew flagged the double-row on the
queue minisite 2026-05-19 (sig_ts 1779225355734): "can you update
tooling so it's hard if not impossible to do the manual queueing
when you shouldnt".

Behavior contract:

  * ``queue add --scope workload:<label>`` exits 3 with an ALL CAPS
    stderr banner pointing the caller at ``workload run <label>``.
  * ``queue add --scope workload:<label> --force-enqueue`` succeeds
    (the workload runner's own auto-add path uses this flag).
  * ``QUEUE_GATE_BYPASS=1`` env var bypasses the refusal (audited
    emergency escape hatch, same shape as the pre-agent-queue-gate
    hook).
  * Non-workload scopes (``scope:foo``, ``path:repo/sub``, ``*``)
    are unaffected -- the gate is targeted to the ``workload:``
    namespace only.

Run::

    uv run --python 3.11 --with pytest \\
        pytest tools/session-task/tests/test_queue_workload_scope_refuse.py -v
"""

import json
import os
import subprocess
import sys
import tempfile
from pathlib import Path

SESSION_TASK = Path(__file__).resolve().parent.parent / "session-task"


def _env_for_tmp(tmp):
    """Build env that points at tmp HOME with notifications suppressed."""
    tmp = Path(tmp)
    env = os.environ.copy()
    env["HOME"] = str(tmp)
    env["PINGME_SESSION_TASK"] = "0"
    env["CLAUDE_EVENT_SESSION_TASK"] = "0"
    # Make sure no inherited bypass env trips the bypass branch
    # unintentionally; specific tests set it back when they want it.
    env.pop("QUEUE_GATE_BYPASS", None)
    return env


def _run(env, *argv):
    cmd = [sys.executable, str(SESSION_TASK)] + list(argv)
    return subprocess.run(
        cmd, capture_output=True, text=True, env=env, timeout=15
    )


# ---------------------------------------------------------------------------
# (a) Manual --scope workload:<label> is refused (exit 3, banner)
# ---------------------------------------------------------------------------


def test_manual_workload_scope_is_refused_exit_3():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        r = _run(
            env,
            "queue",
            "add",
            "manual workload attempt",
            "--scope",
            "workload:promote-foo",
            "--summary",
            "manual attempt",
            "--json",
        )
        assert r.returncode == 3, (
            f"expected exit 3, got rc={r.returncode}\n"
            f"  stdout: {r.stdout}\n"
            f"  stderr: {r.stderr}"
        )
        # Banner is ALL CAPS, on stderr, mentions workload + workload run.
        assert "QUEUE ADD REFUSED" in r.stderr, r.stderr
        assert "MANUAL `workload:` SCOPE NOT ALLOWED" in r.stderr, r.stderr
        assert "workload run <label>" in r.stderr, r.stderr
        # Offending token surfaced.
        assert "workload:promote-foo" in r.stderr, r.stderr
        # No JSON on stdout — the refusal happens before the add succeeds.
        assert r.stdout.strip() == "", r.stdout


def test_manual_workload_scope_refusal_lists_all_offending_tokens():
    """A composite add with multiple workload tokens names each one."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        r = _run(
            env,
            "queue",
            "add",
            "two workload tokens",
            "--scope",
            "workload:alpha,workload:beta",
            "--summary",
            "two-token attempt",
            "--json",
        )
        assert r.returncode == 3, r.stderr
        assert "workload:alpha" in r.stderr, r.stderr
        assert "workload:beta" in r.stderr, r.stderr


def test_manual_workload_scope_refusal_even_with_other_scope_present():
    """Adding a benign scope alongside workload: doesn't excuse the gate."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        r = _run(
            env,
            "queue",
            "add",
            "workload mixed with scope",
            "--scope",
            "scope:legit",
            "--scope",
            "workload:promote-bar",
            "--summary",
            "mixed scope attempt",
            "--json",
        )
        assert r.returncode == 3, r.stderr
        assert "workload:promote-bar" in r.stderr, r.stderr


# ---------------------------------------------------------------------------
# (b) Bypass paths: --force-enqueue and QUEUE_GATE_BYPASS=1
# ---------------------------------------------------------------------------


def test_force_enqueue_flag_bypasses_refusal():
    """--force-enqueue is what the workload runner itself passes."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        r = _run(
            env,
            "queue",
            "add",
            "workload runner auto-add path",
            "--scope",
            "workload:auto-test",
            "--summary",
            "runner add",
            "--force-enqueue",
            "--json",
        )
        assert r.returncode == 0, (
            f"expected exit 0 (force-enqueue bypass), got rc={r.returncode}\n"
            f"  stdout: {r.stdout}\n"
            f"  stderr: {r.stderr}"
        )
        d = json.loads(r.stdout)
        assert d["scope"] == ["workload:auto-test"], d
        assert "workload" not in r.stderr.lower() or "REFUSED" not in r.stderr


def test_queue_gate_bypass_env_var_bypasses_refusal():
    """QUEUE_GATE_BYPASS=1 env var bypasses the refusal."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        env["QUEUE_GATE_BYPASS"] = "1"
        r = _run(
            env,
            "queue",
            "add",
            "emergency bypass path",
            "--scope",
            "workload:emergency",
            "--summary",
            "env bypass",
            "--json",
        )
        assert r.returncode == 0, (
            f"expected exit 0 (env bypass), got rc={r.returncode}\n"
            f"  stdout: {r.stdout}\n"
            f"  stderr: {r.stderr}"
        )
        d = json.loads(r.stdout)
        assert d["scope"] == ["workload:emergency"], d


def test_queue_gate_bypass_env_var_zero_is_not_bypass():
    """QUEUE_GATE_BYPASS=0 (or empty) does NOT bypass."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        env["QUEUE_GATE_BYPASS"] = "0"
        r = _run(
            env,
            "queue",
            "add",
            "bypass-zero should still refuse",
            "--scope",
            "workload:still-refused",
            "--summary",
            "should refuse",
            "--json",
        )
        assert r.returncode == 3, (
            f"expected exit 3 (QUEUE_GATE_BYPASS=0 is no-bypass), "
            f"got rc={r.returncode}\n"
            f"  stdout: {r.stdout}\n"
            f"  stderr: {r.stderr}"
        )


# ---------------------------------------------------------------------------
# (c) Non-workload scopes pass through unchanged
# ---------------------------------------------------------------------------


def test_non_workload_scope_passes_through_unchanged():
    """scope:foo, path:repo/sub, *, repo:x etc. are all unaffected."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        for scope_tok in [
            "scope:foo",
            "repo:bar",
            "path:bar/baz",
            "file:/tmp/x",
            "resource:y",
            "*",
        ]:
            r = _run(
                env,
                "queue",
                "add",
                f"benign scope {scope_tok}",
                "--scope",
                scope_tok,
                "--summary",
                "benign",
                "--json",
            )
            assert r.returncode == 0, (
                f"expected exit 0 for {scope_tok}, got rc={r.returncode}\n"
                f"  stdout: {r.stdout}\n"
                f"  stderr: {r.stderr}"
            )
            d = json.loads(r.stdout)
            assert d["scope"] == [scope_tok], d


def test_token_containing_workload_substring_is_not_refused():
    """The gate matches the `workload:` prefix exactly, not 'workload' anywhere."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        # `scope:workloadish` is NOT a workload: scope; it's a regular
        # scope token that happens to contain the substring "workload".
        r = _run(
            env,
            "queue",
            "add",
            "scope token contains workload as substring",
            "--scope",
            "scope:workloadish",
            "--summary",
            "substring not prefix",
            "--json",
        )
        assert r.returncode == 0, (
            f"expected exit 0 (substring not prefix), got rc={r.returncode}\n"
            f"  stdout: {r.stdout}\n"
            f"  stderr: {r.stderr}"
        )


if __name__ == "__main__":
    sys.exit(subprocess.call(["pytest", "-v", __file__]))
