"""`queue pg-status`: JSON backend reports backend=json (exit 0) and never
touches Postgres; postgres backend with a bad DSN fails loud (75 + event);
`--postgres` probe with a bad DSN fails (non-zero) with NO event and NO
queue mutation. Uses a temp HOME only -- never the live queue."""
import json
import os
import stat
import subprocess
import sys
import tempfile
from pathlib import Path

SESSION_TASK = Path(__file__).resolve().parent.parent / "session-task"
BAD_DSN = "postgresql://u:p@127.0.0.1:1/nodb"  # port 1: connection refused


def _setup(tmp, **extra):
    home = Path(tmp)
    cfg = home / ".config" / "session"
    cfg.mkdir(parents=True)
    qf = cfg / "queue.json"
    qf.write_text(json.dumps({"schema_version": 2, "locked_scopes": {},
                              "items": []}))
    bindir = home / "bin"
    bindir.mkdir()
    log = home / "events.log"
    shim = bindir / "claude-event"
    shim.write_text(f'#!/bin/sh\necho "$@" >> "{log}"\n')
    shim.chmod(shim.stat().st_mode | stat.S_IEXEC)
    env = os.environ.copy()
    env.pop("SESSION_TASK_QUEUE_BACKEND", None)
    env.pop("SESSION_TASK_PG_DSN", None)
    env.update(HOME=str(home), PATH=f"{bindir}:{env['PATH']}",
               PINGME_SESSION_TASK="0")
    env.pop("CLAUDE_EVENT_SESSION_TASK", None)
    env.update(extra)
    return env, qf, log


def _run(env, *argv):
    return subprocess.run([sys.executable, str(SESSION_TASK), "queue",
                           "pg-status", *argv], capture_output=True,
                          text=True, env=env, timeout=60)


def test_json_backend_reports_json_exit_0():
    with tempfile.TemporaryDirectory() as tmp:
        env, qf, log = _setup(tmp)
        before = qf.read_bytes()
        r = _run(env)
        assert r.returncode == 0, (r.stdout, r.stderr)
        assert r.stdout.startswith("backend=json"), r.stdout
        assert "UNRECOVERABLE" not in r.stderr
        assert not log.exists()
        assert qf.read_bytes() == before


def test_postgres_backend_bad_dsn_exit_75_with_event():
    with tempfile.TemporaryDirectory() as tmp:
        env, qf, log = _setup(tmp, SESSION_TASK_QUEUE_BACKEND="postgres",
                              SESSION_TASK_PG_DSN=BAD_DSN)
        r = _run(env)
        assert r.returncode == 75, (r.stdout, r.stderr)
        assert "connect failed" in r.stderr
        assert "queue backend is postgres and unusable" in r.stderr
        assert "queue-corrupt-unrecoverable" in log.read_text()


def test_postgres_probe_bad_dsn_nonzero_no_event_no_mutation():
    with tempfile.TemporaryDirectory() as tmp:
        env, qf, log = _setup(tmp, SESSION_TASK_PG_DSN=BAD_DSN)
        before = qf.read_bytes()
        r = _run(env, "--postgres")
        assert r.returncode not in (0, 75), (r.returncode, r.stdout, r.stderr)
        assert "UNREACHABLE" in r.stdout, r.stdout
        assert not log.exists(), "probe must not emit a claude-event"
        assert qf.read_bytes() == before
        assert sorted(p.name for p in qf.parent.iterdir()) == ["queue.json"]


def test_postgres_probe_without_dsn_nonzero_no_event():
    with tempfile.TemporaryDirectory() as tmp:
        env, qf, log = _setup(tmp)
        r = _run(env, "--postgres")
        assert r.returncode == 1
        assert "UNREACHABLE" in r.stdout
        assert not log.exists()
