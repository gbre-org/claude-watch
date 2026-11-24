"""Postgres queue backend tests. Need a THROWAWAY server: set
SESSION_TASK_TEST_PG_DSN (e.g. postgresql://u:p@127.0.0.1:55432/db). Skipped
otherwise. Never point at the live queue store (tables are dropped)."""
import json
import os
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

TOOL = Path(__file__).resolve().parent.parent
SESSION_TASK = TOOL / "session-task"
DSN = os.environ.get("SESSION_TASK_TEST_PG_DSN")


def _skip():
    if not DSN:
        try:
            import pytest
            pytest.skip("SESSION_TASK_TEST_PG_DSN not set")
        except ImportError:
            pass
        return True
    return False


def _env(tmp, **extra):
    e = os.environ.copy()
    e.update(HOME=str(tmp), PINGME_SESSION_TASK="0",
             CLAUDE_EVENT_SESSION_TASK="0",
             SESSION_TASK_QUEUE_BACKEND="postgres", SESSION_TASK_PG_DSN=DSN)
    e.update(extra)
    return e


def _run(env, *argv, timeout=60):
    return subprocess.run([sys.executable, str(SESSION_TASK)] + list(argv),
                          capture_output=True, text=True, env=env,
                          timeout=timeout)


def _reset_pg():
    sys.path.insert(0, str(TOOL))
    import pgwire
    c = pgwire.PgConnection(DSN)
    c.query("DROP TABLE IF EXISTS queue_state, queue_history")
    c.close()


def _migrate(tmp):
    d = Path(tmp) / ".config" / "session"
    d.mkdir(parents=True, exist_ok=True)
    (d / "queue.json").write_text(json.dumps({
        "schema_version": 2, "locked_scopes": {},
        "items": [{"id": "q-seed", "description": "seed", "scope": ["repo:s"],
                   "group_id": "g-s", "status": "pending", "priority": 5}]}))
    r = _run(_env(tmp), "queue", "pg-migrate")
    assert r.returncode == 0, r.stderr
    return d


def test_pgwire_scram_roundtrip():
    if _skip():
        return
    sys.path.insert(0, str(TOOL))
    import pgwire
    c = pgwire.PgConnection(DSN)
    assert c.query("SELECT 1, NULL, 'a''b'") == [("1", None, "a'b")]
    try:
        c.query("SELEC nope")
    except pgwire.PgError:
        pass
    else:
        raise AssertionError("expected PgError")
    assert c.query("SELECT 2") == [("2",)]  # connection usable after error
    c.close()


def test_migrate_idempotent_and_backup():
    if _skip():
        return
    _reset_pg()
    with tempfile.TemporaryDirectory() as tmp:
        d = _migrate(tmp)
        assert list(d.glob("queue.json.pre-pg-migrate.*"))
        r = _run(_env(tmp), "queue", "pg-migrate")
        assert r.returncode == 0 and "no-op" in r.stdout, r.stdout


def test_add_list_roundtrip_and_mirror():
    if _skip():
        return
    _reset_pg()
    with tempfile.TemporaryDirectory() as tmp:
        d = _migrate(tmp)
        r = _run(_env(tmp), "queue", "add", "pg task", "--scope", "repo:x",
                 "--json")
        assert r.returncode == 0, r.stderr
        r = _run(_env(tmp), "queue", "list", "--json")
        ids = sorted(i["id"] for i in json.loads(r.stdout))
        assert "q-seed" in ids and len(ids) == 2, ids
        mirror = json.loads((d / "queue.json").read_text())
        assert len(mirror["items"]) == 2  # file readers still see current state


def test_unreachable_pg_fails_loud_never_empty():
    if _skip():
        return
    with tempfile.TemporaryDirectory() as tmp:
        d = _migrate(tmp) if False else Path(tmp) / ".config" / "session"
        d.mkdir(parents=True, exist_ok=True)
        (d / "queue.json").write_text('{"items":[{"id":"q-keep"}]}')
        r = _run(_env(tmp, SESSION_TASK_PG_DSN="postgresql://x:y@127.0.0.1:1/z"),
                 "queue", "list", "--json")
        assert r.returncode == 75, (r.returncode, r.stderr)
        assert "refusing" in r.stderr
        assert "q-keep" in (d / "queue.json").read_text()  # untouched


def test_missing_row_fails_loud_not_created():
    if _skip():
        return
    _reset_pg()
    sys.path.insert(0, str(TOOL))
    import pgwire
    c = pgwire.PgConnection(DSN)
    c.query("CREATE TABLE queue_state (id int primary key, doc jsonb, "
            "version bigint, updated_at timestamptz default now())")
    c.close()
    with tempfile.TemporaryDirectory() as tmp:
        r = _run(_env(tmp), "queue", "add", "x", "--scope", "repo:a")
        assert r.returncode == 75, r.stderr
        assert "pg-migrate" in r.stderr


def test_concurrent_writers_no_lost_updates():
    if _skip():
        return
    _reset_pg()
    with tempfile.TemporaryDirectory() as tmp:
        _migrate(tmp)
        n, errs = 24, []

        def w(i):
            r = _run(_env(tmp), "queue", "add", f"t{i}", "--scope",
                     f"repo:r{i}", "--json")
            if r.returncode != 0:
                errs.append((i, r.stderr))
        ts = [threading.Thread(target=w, args=(i,)) for i in range(n)]
        [t.start() for t in ts]
        [t.join(120) for t in ts]
        assert not errs, errs
        r = _run(_env(tmp), "queue", "list", "--json")
        ids = {i["id"] for i in json.loads(r.stdout)}
        assert len(ids) == n + 1, len(ids)


def test_kill9_mid_command_is_atomic():
    if _skip():
        return
    import random
    import signal
    _reset_pg()
    with tempfile.TemporaryDirectory() as tmp:
        _migrate(tmp)
        for i in range(10):
            p = subprocess.Popen(
                [sys.executable, str(SESSION_TASK), "queue", "add", f"k{i}",
                 "--scope", f"repo:k{i}", "--json"], env=_env(tmp),
                stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            time.sleep(random.uniform(0, 0.2))
            p.send_signal(signal.SIGKILL)
            p.wait()
        # a killed holder must not wedge the row lock (server drops session)
        r = _run(_env(tmp), "queue", "list", "--json", timeout=90)
        assert r.returncode == 0, r.stderr
        assert any(i["id"] == "q-seed" for i in json.loads(r.stdout))


def test_export_json_rollback():
    if _skip():
        return
    _reset_pg()
    with tempfile.TemporaryDirectory() as tmp:
        d = _migrate(tmp)
        _run(_env(tmp), "queue", "add", "z", "--scope", "repo:z", "--json")
        (d / "queue.json").write_text("stale")
        r = _run(_env(tmp), "queue", "pg-export-json")
        assert r.returncode == 0, r.stderr
        assert len(json.loads((d / "queue.json").read_text())["items"]) == 2


if __name__ == "__main__":
    sys.exit(subprocess.call([sys.executable, "/tmp/runtests.py", __file__]))
