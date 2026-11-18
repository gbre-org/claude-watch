#!/usr/bin/env python3
"""Tests for the `waiting` queue state, agent-completion acks and `sweep`.

Covers:
  * `queue wait` parks a running item (no owner needed, scope released)
  * bad --on / reserved `coordinator:` kind refused
  * `queue agent-finished` records a PENDING COMPLETION (item -> waiting on
    the coordinator), never closes it, is idempotent, skips items the agent
    already moved
  * ack paths: `queue done` and `queue wait --on` clear the pending
    completion; `unwait` is refused while it is unacked
  * `queue sweep` reopens a waiting item when its event arrives (workload,
    pr merged / CI failure, event tag), closes it for --done-on-success, never
    resolves coordinator waits, ignores events from before the wait, and
    flags an overdue wait exactly once
  * `heartbeat --pid` still works on running items (back-compat)

Run:
    uv run --python 3.11 --with pytest \\
        pytest tools/session-task/tests/test_queue_waiting.py -v
"""

import json
import os
import subprocess
import sys
import tempfile
import time
from datetime import datetime, timedelta, timezone
from pathlib import Path

SESSION_TASK = Path(__file__).resolve().parent.parent / "session-task"


def _env(tmp):
    env = os.environ.copy()
    env["HOME"] = tmp
    env["PINGME_SESSION_TASK"] = "0"
    env["CLAUDE_EVENT_SESSION_TASK"] = "0"
    env["CLAUDE_EVENT_QUEUE"] = str(Path(tmp) / "events")
    env["CLAUDE_EVENT_LOG_DIR"] = str(Path(tmp) / "evlog")
    env["CLAUDE_AGENT_QUEUE_BINDINGS_PATH"] = str(Path(tmp) / "bindings.json")
    Path(env["CLAUDE_EVENT_QUEUE"]).mkdir()
    Path(env["CLAUDE_EVENT_LOG_DIR"]).mkdir()
    return env


def _run(env, *argv, expect_exit=0, stdin=None):
    r = subprocess.run([sys.executable, str(SESSION_TASK)] + list(argv),
                       capture_output=True, text=True, env=env, timeout=20,
                       input=stdin)
    if expect_exit is not None and r.returncode != expect_exit:
        raise RuntimeError(f"exit {r.returncode} != {expect_exit}: {argv}\n"
                           f"{r.stdout}\n{r.stderr}")
    return r


def _running(env, scope="repo:w-a"):
    q = json.loads(_run(env, "queue", "add", "work", "--scope", scope,
                        "--summary", "s", "--json").stdout)["id"]
    _run(env, "queue", "register", q, "--json")
    return q


def _show(env, q):
    return json.loads(_run(env, "queue", "show", q).stdout)


def _set(env, q, **fields):
    """Edit queue.json directly (agent ids, back-dated waits)."""
    path = Path(env["HOME"]) / ".config/session/queue.json"
    d = json.loads(path.read_text())
    for it in d["items"]:
        if it["id"] == q:
            it.update(fields)
    path.write_text(json.dumps(d))


def _emit(env, tag, message, data=None, ts=None):
    ev = {"timestamp_iso": (ts or datetime.now(timezone.utc)).isoformat(),
          "tag": tag, "source": "t", "message": message, "data": data or {}}
    (Path(env["CLAUDE_EVENT_QUEUE"]) / f"{time.time_ns()}_{tag}.json"
     ).write_text(json.dumps(ev))


def test_wait_parks_running_item_and_releases_scope():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env(tmp)
        q = _running(env)
        _set(env, q, agent_id="agent-1")
        r = _run(env, "queue", "wait", q, "--on", "pr:https://h/o/r/pull/7",
                 "--deadline", "2h", "--note", "ci", "--json")
        it = json.loads(r.stdout)
        assert it["status"] == "waiting"
        assert it["wait_on"] == {"kind": "pr", "ref": "https://h/o/r/pull/7"}
        assert "agent_id" not in it and it["last_agent_id"] == "agent-1"
        # scope released: a peer can register
        peer = json.loads(_run(env, "queue", "add", "peer", "--scope",
                               "repo:w-a", "--summary", "p", "--json").stdout)["id"]
        _run(env, "queue", "register", peer, "--json")
        listing = _run(env, "queue", "list").stdout
        assert "waiting:" in listing and "pr:https://h/o/r/pull/7" in listing


def test_wait_rejects_bad_spec_and_reserved_kind():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env(tmp)
        q = _running(env)
        _run(env, "queue", "wait", q, "--on", "nonsense", expect_exit=1)
        _run(env, "queue", "wait", q, "--on", "coordinator:ack", expect_exit=1)
        _run(env, "queue", "wait", q, "--on", "pr:x", "--deadline", "soon",
             expect_exit=1)
        assert _show(env, q)["status"] == "running"


def test_agent_finished_records_pending_completion_never_closes():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env(tmp)
        q = _running(env)
        _set(env, q, agent_id="agent-9")
        r = _run(env, "queue", "agent-finished", "--agent-id", "agent-9",
                 "--message-stdin", stdin="all done, PR opened")
        assert q in r.stdout
        it = _show(env, q)
        assert it["status"] == "waiting"
        assert it["wait_on"]["kind"] == "coordinator"
        assert it["pending_completion"]["agent_id"] == "agent-9"
        assert "PR opened" in it["pending_completion"]["message_tail"]
        # idempotent
        r2 = _run(env, "queue", "agent-finished", "--agent-id", "agent-9")
        assert "pending-completions=0" in r2.stdout
        # ownerless-running impossible: nothing is running
        assert '"running"' not in json.dumps(
            [i["status"] for i in json.loads(
                _run(env, "queue", "list", "--json").stdout)])


def test_agent_finished_uses_binding_and_skips_moved_items():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env(tmp)
        bound = _running(env, "repo:w-b")
        moved = _running(env, "repo:w-c")
        Path(env["CLAUDE_AGENT_QUEUE_BINDINGS_PATH"]).write_text(json.dumps(
            {"bindings": {"agent-b": {"queue_id": bound}}}))
        _set(env, moved, agent_id="agent-b")
        _run(env, "queue", "wait", moved, "--on", "event:x")
        _run(env, "queue", "agent-finished", "--agent-id", "agent-b")
        assert _show(env, bound)["pending_completion"]
        assert "pending_completion" not in _show(env, moved)
        assert _show(env, moved)["wait_on"]["kind"] == "event"


def test_ack_by_done_and_by_wait_clear_pending_completion():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env(tmp)
        a = _running(env, "repo:w-d")
        b = _running(env, "repo:w-e")
        _set(env, a, agent_id="ag-a")
        _set(env, b, agent_id="ag-b")
        _run(env, "queue", "agent-finished", "--agent-id", "ag-a")
        _run(env, "queue", "agent-finished", "--agent-id", "ag-b")
        # unwait refused while unacked
        _run(env, "queue", "unwait", a, expect_exit=1)
        _run(env, "queue", "done", a)
        it = _show(env, a)
        assert it["status"] == "done" and "pending_completion" not in it
        assert it["completion_acked_by"] == "done"
        _run(env, "queue", "wait", b, "--on", "workload:job1")
        it = _show(env, b)
        assert it["status"] == "waiting" and it["wait_on"]["kind"] == "workload"
        assert "pending_completion" not in it
        assert it["completion_acked_by"].startswith("wait:workload")
        _run(env, "queue", "unwait", b)
        assert _show(env, b)["status"] == "pending"


def test_sweep_workload_done_reopens_or_closes():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env(tmp)
        a = _running(env, "repo:w-f")
        b = _running(env, "repo:w-g")
        _run(env, "queue", "wait", a, "--on", "workload:job-a")
        _run(env, "queue", "wait", b, "--on", "workload:job-b",
             "--done-on-success")
        _emit(env, "workload-done", "done", {"label": "job-a", "exit_code": 0})
        _emit(env, "workload-done", "done", {"label": "job-b", "exit_code": 0})
        r = _run(env, "queue", "sweep", "--json")
        out = json.loads(r.stdout)
        assert [x["id"] for x in out["woke"]] == [a]
        assert [x["id"] for x in out["closed"]] == [b]
        assert _show(env, a)["status"] == "pending"
        assert _show(env, b)["status"] == "done"


def test_sweep_failure_does_not_close_done_on_success():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env(tmp)
        a = _running(env, "repo:w-h")
        _run(env, "queue", "wait", a, "--on", "workload:j", "--done-on-success")
        _emit(env, "workload-done", "failed", {"label": "j", "exit_code": 2})
        out = json.loads(_run(env, "queue", "sweep", "--json").stdout)
        assert out["closed"] == [] and out["woke"][0]["success"] is False
        assert _show(env, a)["status"] == "pending"


def test_sweep_pr_merged_and_consumed_log_and_event_tag():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env(tmp)
        a = _running(env, "repo:w-i")
        b = _running(env, "repo:w-j")
        _run(env, "queue", "wait", a, "--on", "pr:https://h/o/r/pull/42",
             "--done-on-success")
        _run(env, "queue", "wait", b, "--on", "event:train-landed")
        # PR event arrives via the CONSUMED log, not the pending dir
        ev = {"timestamp_iso": datetime.now(timezone.utc).isoformat(),
              "tag": "pr-status-change", "message": "PR #42 merged",
              "data": {}}
        (Path(env["CLAUDE_EVENT_LOG_DIR"]) / "consumed.jsonl").write_text(
            json.dumps(ev) + "\n")
        _emit(env, "train-landed", "landed")
        out = json.loads(_run(env, "queue", "sweep", "--json").stdout)
        assert [x["id"] for x in out["closed"]] == [a]
        assert [x["id"] for x in out["woke"]] == [b]


def test_sweep_ignores_unrelated_and_stale_events_and_coordinator_waits():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env(tmp)
        a = _running(env, "repo:w-k")
        c = _running(env, "repo:w-l")
        _set(env, c, agent_id="ag-c")
        _run(env, "queue", "wait", a, "--on", "workload:job-z")
        _run(env, "queue", "agent-finished", "--agent-id", "ag-c")
        old = datetime.now(timezone.utc) - timedelta(hours=3)
        _emit(env, "workload-done", "old", {"label": "job-z", "exit_code": 0},
              ts=old)
        _emit(env, "workload-done", "other", {"label": "different"})
        _emit(env, "pr-status-change", "PR #1 merged")
        out = json.loads(_run(env, "queue", "sweep", "--json").stdout)
        assert out["woke"] == [] and out["closed"] == []
        assert _show(env, a)["status"] == "waiting"
        assert _show(env, c)["status"] == "waiting"


def test_sweep_dry_run_writes_nothing():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env(tmp)
        a = _running(env, "repo:w-m")
        _run(env, "queue", "wait", a, "--on", "event:go")
        _emit(env, "go", "now")
        out = json.loads(_run(env, "queue", "sweep", "-n", "--json").stdout)
        assert len(out["woke"]) == 1
        assert _show(env, a)["status"] == "waiting"


def test_overdue_flagged_once():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env(tmp)
        a = _running(env, "repo:w-n")
        _run(env, "queue", "wait", a, "--on", "event:never")
        past = (datetime.now(timezone.utc) - timedelta(minutes=5)).isoformat()
        _set(env, a, wait_deadline=past)
        first = json.loads(_run(env, "queue", "sweep", "--json").stdout)
        assert [x["id"] for x in first["overdue"]] == [a]
        second = json.loads(_run(env, "queue", "sweep", "--json").stdout)
        assert second["overdue"] == []
        # still waiting (overdue never auto-resolves)
        assert _show(env, a)["status"] == "waiting"


def test_heartbeat_pid_backcompat():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env(tmp)
        q = _running(env)
        _run(env, "queue", "heartbeat", q, "--pid", str(os.getpid()))
        assert _show(env, q)["status"] == "running"
