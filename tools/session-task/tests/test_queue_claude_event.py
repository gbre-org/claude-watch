#!/usr/bin/env python3
"""Tests for claude-event lifecycle emission on session-task queue ops.

Covers the 2026-04-24 addition (minus the ``queue-added`` emit, which
was intentionally dropped 2026-04-27 -- ``queue add`` is task creation,
not a state transition, and the main loop already holds the return
value, so the event was double-handling and noisy):

  * `queue register <id>` does NOT emit a claude-event (noise -- the
    caller just triggered it; removed 2026-05-29).
  * `queue done <id>` emits a ``queue-done`` claude-event with an
    ``elapsed_sec`` data field populated from ``started_at``.
  * `queue abandon <id> [--reason R]` emits a ``queue-abandoned``
    claude-event with ``elapsed_sec`` + ``reason`` populated.
  * A failing ``claude-event`` shim does NOT fail the underlying queue op
    (same invariant as pingme).
  * ``--silent`` on any of the three suppresses the claude-event emit
    (and the pingme emit as before).
  * ``CLAUDE_EVENT_SESSION_TASK=0`` env var suppresses emission.

The tests install a fake ``claude-event`` shim that records invocations
to a JSONL log file. A fake ``pingme`` shim is also installed but is
largely ignored here — we just need it present so the existing pingme
hooks don't spam stderr with "pingme: not found" noise.

Run:
    uv run --python 3.11 --with pytest \\
        pytest tools/session-task/tests/test_queue_claude_event.py -v

Or directly:
    python3 tools/session-task/tests/test_queue_claude_event.py
"""

import json
import os
import subprocess
import sys
import tempfile
import textwrap
from pathlib import Path

SESSION_TASK = Path(__file__).resolve().parent.parent / "session-task"


# ---------------------------------------------------------------------------
# Fake shims
# ---------------------------------------------------------------------------


def _install_fake_claude_event(bin_dir: Path, log_path: Path, exit_code: int = 0):
    """Drop a fake ``claude-event`` executable that logs argv as JSON.

    Format of each logged call: one JSON object per line with keys
    ``argv`` (the argv tail). The real tool writes a JSON file into
    ~/claude-events/; we don't need that side effect here because the
    queue lifecycle is solely responsible for CALLING the tool, not for
    reacting to the output file. The file-drop side effect is covered by
    separate claude-event CLI tests.
    """
    bin_dir.mkdir(parents=True, exist_ok=True)
    emitter = bin_dir / "claude-event"
    emitter.write_text(textwrap.dedent(f"""\
        #!/usr/bin/env python3
        import json, sys
        with open({str(log_path)!r}, "a") as f:
            f.write(json.dumps(sys.argv[1:]) + "\\n")
        sys.exit({exit_code})
        """))
    emitter.chmod(0o755)
    return emitter


def _install_fake_pingme(bin_dir: Path, log_path: Path | None = None, exit_code: int = 0):
    # session-task's queue hooks now shell out to ``queue-notify`` (dedicated
    # Pushover path); install the shim under that name. Helper keeps its
    # historical name to avoid churn.
    bin_dir.mkdir(parents=True, exist_ok=True)
    notifier = bin_dir / "queue-notify"
    sink = str(log_path) if log_path else "/dev/null"
    notifier.write_text(textwrap.dedent(f"""\
        #!/usr/bin/env python3
        import json, sys
        with open({sink!r}, "a") as f:
            f.write(json.dumps(sys.argv[1:]) + "\\n")
        sys.exit({exit_code})
        """))
    notifier.chmod(0o755)
    return notifier


def _env_for_tmp(tmp, bin_dir=None, claude_event_session_task=None):
    env = dict(os.environ)
    env["HOME"] = str(tmp)
    if bin_dir is None:
        empty = Path(tmp) / ".empty_path"
        empty.mkdir(parents=True, exist_ok=True)
        env["PATH"] = str(empty)
    else:
        env["PATH"] = f"{bin_dir}:{env.get('PATH', '')}"
    if claude_event_session_task is not None:
        env["CLAUDE_EVENT_SESSION_TASK"] = claude_event_session_task
    elif "CLAUDE_EVENT_SESSION_TASK" in env:
        del env["CLAUDE_EVENT_SESSION_TASK"]
    Path(tmp, ".config/session").mkdir(parents=True, exist_ok=True)
    return env


def _run(env, *argv, check=False, timeout=15):
    cmd = [sys.executable, str(SESSION_TASK)] + list(argv)
    r = subprocess.run(cmd, capture_output=True, text=True, env=env,
                       timeout=timeout)
    if check and r.returncode != 0:
        raise RuntimeError(
            f"command failed rc={r.returncode}\n"
            f"  cmd: {' '.join(argv)}\n"
            f"  stdout: {r.stdout}\n"
            f"  stderr: {r.stderr}"
        )
    return r


def _add(env, desc, scopes, *extra):
    cmd = ["queue", "add", desc, "--json"]
    for s in scopes:
        cmd.extend(["--scope", s])
    cmd.extend(extra)
    return _run(env, *cmd)


def _read_shim_log(log_path: Path):
    if not log_path.exists():
        return []
    out = []
    for line in log_path.read_text().splitlines():
        line = line.strip()
        if not line:
            continue
        out.append(json.loads(line))
    return out


def _parse_claude_event_argv(argv):
    """Parse the argv the session-task hook passed to claude-event.

    claude-event signature:
        claude-event <message> --source queue --tag TAG
                     [--priority P] [--data KEY=VAL ...]
    """
    priority = "normal"
    source = None
    tag = None
    data = {}
    positional = []
    i = 0
    while i < len(argv):
        a = argv[i]
        if a == "--source":
            source = argv[i + 1]
            i += 2
        elif a == "--tag":
            tag = argv[i + 1]
            i += 2
        elif a == "--priority":
            priority = argv[i + 1]
            i += 2
        elif a == "--data":
            k, _, v = argv[i + 1].partition("=")
            data[k] = v
            i += 2
        else:
            positional.append(a)
            i += 1
    message = positional[0] if positional else None
    return {
        "message": message,
        "source": source,
        "tag": tag,
        "priority": priority,
        "data": data,
    }


# ---------------------------------------------------------------------------
# 1. queue add does NOT emit a claude-event (queue-added removed 2026-04-27)
# ---------------------------------------------------------------------------


def test_add_does_not_emit_claude_event():
    """``queue add`` is task creation, not a state transition.

    The ``queue-added`` emit was dropped 2026-04-27 ("too noisy" --
    Andrew). Verify ``queue add`` no longer shells out to claude-event
    so this regression doesn't sneak back in.
    """
    with tempfile.TemporaryDirectory() as tmp:
        bin_dir = Path(tmp) / "bin"
        ev_log = Path(tmp) / "claude-event.log"
        _install_fake_claude_event(bin_dir, ev_log)
        _install_fake_pingme(bin_dir)
        env = _env_for_tmp(tmp, bin_dir=bin_dir)

        summary = "test add emits no claude-event"
        _add(env, "add-test", ["repo:ce-add"], "--summary", summary)

        calls = _read_shim_log(ev_log)
        assert calls == [], (
            f"queue add should NOT emit a claude-event; got {calls}"
        )


# ---------------------------------------------------------------------------
# 2. queue register does NOT emit queue-running (removed 2026-05-29)
# ---------------------------------------------------------------------------


def test_register_does_not_emit_queue_running_event():
    """``queue register`` no longer emits a claude-event.

    The ``queue-running`` emit from register was noise -- the caller
    (main loop) just triggered the transition and already knows.
    Removed 2026-05-29. force-start still emits queue-running via its
    own code path.
    """
    with tempfile.TemporaryDirectory() as tmp:
        bin_dir = Path(tmp) / "bin"
        ev_log = Path(tmp) / "claude-event.log"
        _install_fake_claude_event(bin_dir, ev_log)
        _install_fake_pingme(bin_dir)
        env = _env_for_tmp(tmp, bin_dir=bin_dir)

        r1 = _add(env, "reg-test", ["repo:ce-reg"], "--summary",
                  "reg test summary")
        d1 = json.loads(r1.stdout)
        assert _read_shim_log(ev_log) == []

        _run(env, "queue", "register", d1["id"], "--json", check=True)

        calls = _read_shim_log(ev_log)
        assert calls == [], (
            f"queue register should NOT emit a claude-event; got {calls}"
        )


# ---------------------------------------------------------------------------
# 3. queue done emits queue-done with elapsed_sec populated
# ---------------------------------------------------------------------------


def test_done_emits_queue_done_event_with_elapsed_sec():
    with tempfile.TemporaryDirectory() as tmp:
        bin_dir = Path(tmp) / "bin"
        ev_log = Path(tmp) / "claude-event.log"
        _install_fake_claude_event(bin_dir, ev_log)
        _install_fake_pingme(bin_dir)
        env = _env_for_tmp(tmp, bin_dir=bin_dir)

        r1 = _add(env, "done-test", ["repo:ce-done"], "--summary",
                  "done test summary")
        d1 = json.loads(r1.stdout)
        _run(env, "queue", "register", d1["id"], "--json", check=True)
        _run(env, "queue", "done", d1["id"], check=True)

        calls = _read_shim_log(ev_log)
        # done only (register no longer emits since 2026-05-29)
        assert len(calls) == 1, calls
        parsed = _parse_claude_event_argv(calls[0])
        assert parsed["source"] == "queue"
        assert parsed["tag"] == "queue-done"
        assert parsed["data"]["queue_id"] == d1["id"]
        # elapsed_sec must be present and parseable as int >= 0.
        assert "elapsed_sec" in parsed["data"]
        assert int(parsed["data"]["elapsed_sec"]) >= 0


# ---------------------------------------------------------------------------
# 4. queue abandon emits queue-abandoned with reason
# ---------------------------------------------------------------------------


def test_abandon_emits_queue_abandoned_event():
    with tempfile.TemporaryDirectory() as tmp:
        bin_dir = Path(tmp) / "bin"
        ev_log = Path(tmp) / "claude-event.log"
        _install_fake_claude_event(bin_dir, ev_log)
        _install_fake_pingme(bin_dir)
        env = _env_for_tmp(tmp, bin_dir=bin_dir)

        r1 = _add(env, "abandon-test", ["repo:ce-abandon"], "--summary",
                  "abandon test summary")
        d1 = json.loads(r1.stdout)
        _run(env, "queue", "register", d1["id"], "--json", check=True)
        _run(env, "queue", "abandon", d1["id"], "--reason",
             "agent crashed in testing", check=True)

        calls = _read_shim_log(ev_log)
        # abandon only (register no longer emits since 2026-05-29)
        assert len(calls) == 1, calls
        parsed = _parse_claude_event_argv(calls[0])
        assert parsed["source"] == "queue"
        assert parsed["tag"] == "queue-abandoned"
        assert parsed["data"]["queue_id"] == d1["id"]
        assert parsed["data"]["reason"] == "agent crashed in testing"
        assert "elapsed_sec" in parsed["data"]
        assert int(parsed["data"]["elapsed_sec"]) >= 0


# ---------------------------------------------------------------------------
# 5. claude-event failure does NOT break the queue transition
# ---------------------------------------------------------------------------


def test_failing_claude_event_does_not_block_queue_op():
    with tempfile.TemporaryDirectory() as tmp:
        bin_dir = Path(tmp) / "bin"
        ev_log = Path(tmp) / "claude-event.log"
        # Exit code 17 == shim recorded the call but returned non-zero.
        _install_fake_claude_event(bin_dir, ev_log, exit_code=17)
        _install_fake_pingme(bin_dir)
        env = _env_for_tmp(tmp, bin_dir=bin_dir)

        r1 = _add(env, "fail-ce", ["repo:ce-fail"], "--summary",
                  "failing emitter shim")
        assert r1.returncode == 0, (
            f"queue add must not propagate claude-event failure, "
            f"stderr={r1.stderr!r}"
        )
        d1 = json.loads(r1.stdout)

        rr = _run(env, "queue", "register", d1["id"], "--json")
        assert rr.returncode == 0, rr.stderr
        rd = _run(env, "queue", "done", d1["id"])
        assert rd.returncode == 0, rd.stderr

        # The shim still recorded every invocation despite exit 17.
        # Only done lands in the shim log (register no longer emits
        # since 2026-05-29, queue add since 2026-04-27).
        calls = _read_shim_log(ev_log)
        tags = [_parse_claude_event_argv(c)["tag"] for c in calls]
        assert tags == ["queue-done"], tags


# ---------------------------------------------------------------------------
# 6. Env-var suppression works
# ---------------------------------------------------------------------------


def test_env_var_suppresses_claude_event():
    with tempfile.TemporaryDirectory() as tmp:
        bin_dir = Path(tmp) / "bin"
        ev_log = Path(tmp) / "claude-event.log"
        _install_fake_claude_event(bin_dir, ev_log)
        _install_fake_pingme(bin_dir)
        env = _env_for_tmp(tmp, bin_dir=bin_dir,
                           claude_event_session_task="0")

        r1 = _add(env, "suppress-test", ["repo:ce-suppress"], "--summary",
                  "env suppressed")
        d1 = json.loads(r1.stdout)
        _run(env, "queue", "register", d1["id"], "--json", check=True)
        _run(env, "queue", "done", d1["id"], check=True)

        calls = _read_shim_log(ev_log)
        assert calls == [], f"expected no claude-event emits, got {calls}"


# ---------------------------------------------------------------------------
# 7. Idempotent terminal transitions
#
# A workload that was killed via `workload kill` triggers an abandon
# transition from claude-watch even if the wrapper script also raced
# ahead and ran its own `emit-done` transition. The second abandon must
# be a no-op: same stored `abandoned_at`, same `abandon_reason`, no
# duplicate `queue-abandoned` claude-event, no duplicate pingme. Same
# guarantee applies to `queue done`. Andrew DM 2026-05-13.
# ---------------------------------------------------------------------------


def test_abandon_is_idempotent_on_already_abandoned():
    """Second `queue abandon` on an already-abandoned item is a no-op.

    Without this, a workload-kill race that fires two abandon calls
    would (a) clobber `abandoned_at` and `abandon_reason` with the
    later call's values, and (b) emit a duplicate `queue-abandoned`
    claude-event that the main loop would handle twice.
    """
    with tempfile.TemporaryDirectory() as tmp:
        bin_dir = Path(tmp) / "bin"
        ev_log = Path(tmp) / "claude-event.log"
        _install_fake_claude_event(bin_dir, ev_log)
        _install_fake_pingme(bin_dir)
        env = _env_for_tmp(tmp, bin_dir=bin_dir)

        r1 = _add(env, "abandon-idem", ["repo:ce-aidem"], "--summary",
                  "abandon idempotency")
        d1 = json.loads(r1.stdout)
        _run(env, "queue", "register", d1["id"], "--json", check=True)

        _run(env, "queue", "abandon", d1["id"], "--reason", "first reason",
             check=True)

        # Snapshot persisted state after first abandon so we can verify
        # the second call doesn't mutate it.
        r_show1 = _run(env, "queue", "show", d1["id"], check=True)
        item_after_first = json.loads(r_show1.stdout)
        first_at = item_after_first["abandoned_at"]
        first_reason = item_after_first.get("abandon_reason")
        assert item_after_first["status"] == "abandoned"
        assert first_reason == "first reason"

        # Second abandon — different reason, must NOT clobber state.
        r2 = _run(env, "queue", "abandon", d1["id"], "--reason",
                  "second reason (should be ignored)")
        assert r2.returncode == 0, (
            f"second abandon should exit 0; got rc={r2.returncode}\n"
            f"stdout={r2.stdout}\nstderr={r2.stderr}"
        )
        assert "already abandoned" in r2.stdout, r2.stdout

        r_show2 = _run(env, "queue", "show", d1["id"], check=True)
        item_after_second = json.loads(r_show2.stdout)
        assert item_after_second["abandoned_at"] == first_at, (
            "abandoned_at must NOT be overwritten by repeat abandon"
        )
        assert item_after_second.get("abandon_reason") == first_reason, (
            "abandon_reason must NOT be overwritten by repeat abandon"
        )

        # claude-event log: first abandon only. The repeat abandon must
        # not emit a duplicate `queue-abandoned`. Register no longer emits.
        calls = _read_shim_log(ev_log)
        tags = [
            _parse_claude_event_argv(c).get("tag") for c in calls
        ]
        assert tags == ["queue-abandoned"], (
            f"expected exactly one queue-abandoned emit, got tags={tags}"
        )


def test_abandon_is_idempotent_after_done():
    """`queue abandon` on a done item must not flip status or re-emit.

    Asymmetric race: the wrapper completes naturally (transitions to
    `done`), then `workload kill` arrives anyway (slow tmux teardown)
    and tries to `abandon`. The done status must be preserved and no
    duplicate event fires.
    """
    with tempfile.TemporaryDirectory() as tmp:
        bin_dir = Path(tmp) / "bin"
        ev_log = Path(tmp) / "claude-event.log"
        _install_fake_claude_event(bin_dir, ev_log)
        _install_fake_pingme(bin_dir)
        env = _env_for_tmp(tmp, bin_dir=bin_dir)

        r1 = _add(env, "done-then-abandon", ["repo:ce-dta"], "--summary",
                  "done then abandon")
        d1 = json.loads(r1.stdout)
        _run(env, "queue", "register", d1["id"], "--json", check=True)
        _run(env, "queue", "done", d1["id"], check=True)

        # Now try to abandon — should be a no-op.
        r2 = _run(env, "queue", "abandon", d1["id"], "--reason", "late kill")
        assert r2.returncode == 0
        assert "already done" in r2.stdout, r2.stdout

        r_show = _run(env, "queue", "show", d1["id"], check=True)
        item = json.loads(r_show.stdout)
        assert item["status"] == "done", item
        assert item.get("abandon_reason") is None, item
        assert item.get("abandoned_at") is None, item

        calls = _read_shim_log(ev_log)
        tags = [
            _parse_claude_event_argv(c).get("tag") for c in calls
        ]
        assert tags == ["queue-done"], (
            f"abandon-on-done should not emit; tags={tags}"
        )


# ---------------------------------------------------------------------------
# Entry point for direct invocation
# ---------------------------------------------------------------------------


def _all_tests():
    return [
        test_add_does_not_emit_claude_event,
        test_register_does_not_emit_queue_running_event,
        test_done_emits_queue_done_event_with_elapsed_sec,
        test_abandon_emits_queue_abandoned_event,
        test_failing_claude_event_does_not_block_queue_op,
        test_env_var_suppresses_claude_event,
        test_abandon_is_idempotent_on_already_abandoned,
        test_abandon_is_idempotent_after_done,
    ]


if __name__ == "__main__":
    fail = 0
    for t in _all_tests():
        try:
            t()
            print(f"PASS: {t.__name__}")
        except Exception as e:
            fail += 1
            print(f"FAIL: {t.__name__}: {e}")
    sys.exit(0 if fail == 0 else 1)
