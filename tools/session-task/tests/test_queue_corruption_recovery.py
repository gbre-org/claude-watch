#!/usr/bin/env python3
"""Corruption-recovery + atomic-write hardening tests for session-task queue.

Motivated by the 2026-08-16 incident: queue.json ballooned to 3.9MB and became
a byte-interleaved [valid-doc][trailing-fragment] file (concurrent host +
container writers whose advisory flock locks were incoherent across the
macOS/Docker bind-mount). json.loads rejected the whole file and the old code
SILENTLY reset the queue to empty, losing all in-flight items.

These tests verify the fix:
  * atomic temp+rename writes (never a torn/interleaved file),
  * layered recovery on a corrupt file (salvage leading doc -> .bak -> empty),
  * a LOUD alert on every recovery/reset (never silent),
  * concurrent same-domain writers never corrupt + never lose items.

Run:
    uv run --python 3.11 --with pytest \\
        pytest tools/session-task/tests/test_queue_corruption_recovery.py -v
"""

import importlib.machinery
import importlib.util
import json
import os
import subprocess
import sys
import tempfile
import threading
from pathlib import Path

SESSION_TASK = Path(__file__).resolve().parent.parent / "session-task"


def _load_module():
    """Import session-task (no .py extension) as a module for unit tests."""
    loader = importlib.machinery.SourceFileLoader("session_task_mod",
                                                  str(SESSION_TASK))
    spec = importlib.util.spec_from_loader("session_task_mod", loader)
    mod = importlib.util.module_from_spec(spec)
    loader.exec_module(mod)
    return mod


def _point_module_at(mod, tmp):
    """Redirect the module's queue path globals into a per-test tmp dir."""
    d = Path(tmp) / ".config" / "session"
    d.mkdir(parents=True, exist_ok=True)
    mod.QUEUE_FILE = d / "queue.json"
    mod.QUEUE_LOCK_FILE = d / "queue.json.lock"
    mod.QUEUE_BAK_FILE = d / "queue.json.bak"
    return d


def _queue_doc(ids):
    return {
        "schema_version": 2,
        "items": [
            {"id": i, "description": f"desc {i}", "scope": [f"repo:{i}"],
             "group_id": f"g-{i}", "status": "pending"}
            for i in ids
        ],
        "locked_scopes": {},
    }


# ---------------------------------------------------------------------------
# Unit tests: helpers (import module, redirect path globals)
# ---------------------------------------------------------------------------


def test_atomic_write_no_temp_left():
    mod = _load_module()
    with tempfile.TemporaryDirectory() as tmp:
        d = _point_module_at(mod, tmp)
        mod._atomic_write_text(mod.QUEUE_FILE, "hello\n")
        assert mod.QUEUE_FILE.read_text() == "hello\n"
        mod._atomic_write_text(mod.QUEUE_FILE, "world\n")
        assert mod.QUEUE_FILE.read_text() == "world\n"
        leftovers = [p.name for p in d.iterdir() if p.name.endswith(".tmp")]
        assert not leftovers, f"leftover temp files: {leftovers}"


def test_salvage_leading_json_recovers_before_trailing_fragment():
    mod = _load_module()
    doc = _queue_doc(["q-a", "q-b", "q-c"])
    valid = json.dumps(doc, indent=2) + "\n"
    corrupt = valid + 'd /Users/foo && echo bar",\n      "summary": "x"\n    }\n  ]\n}\n'
    try:
        json.loads(corrupt)
        raise AssertionError("corrupt fixture unexpectedly parsed clean")
    except json.JSONDecodeError:
        pass
    salvaged = mod._salvage_leading_json(corrupt)
    assert salvaged is not None
    assert [it["id"] for it in salvaged["items"]] == ["q-a", "q-b", "q-c"]


def test_salvage_leading_json_returns_none_on_leading_garbage():
    mod = _load_module()
    assert mod._salvage_leading_json("}{ not json at all") is None
    assert mod._salvage_leading_json('[1, 2, 3] trailing') is None


def test_recovery_salvages_leading_document():
    mod = _load_module()
    with tempfile.TemporaryDirectory() as tmp:
        d = _point_module_at(mod, tmp)
        doc = _queue_doc(["q-a", "q-b", "q-c"])
        valid = json.dumps(doc, indent=2) + "\n"
        corrupt = valid + 'd /x && echo y",\n }\n ]\n}\n'
        mod.QUEUE_FILE.write_text(corrupt)
        data, info = mod._read_queue_with_recovery()
        assert info is not None and info["mode"] == "salvage-leading"
        assert info["recovered_items"] == 3
        assert [it["id"] for it in data["items"]] == ["q-a", "q-b", "q-c"]
        quarantined = list(d.glob("queue.json.corrupt.*"))
        assert quarantined, "corrupt bytes were not quarantined"
        assert quarantined[0].read_text() == corrupt


def test_recovery_falls_back_to_bak_when_unsalvageable():
    mod = _load_module()
    with tempfile.TemporaryDirectory() as tmp:
        _point_module_at(mod, tmp)
        mod.QUEUE_BAK_FILE.write_text(json.dumps(_queue_doc(["q-x", "q-y"])) + "\n")
        mod.QUEUE_FILE.write_text("}}}garbage not recoverable{{{")
        data, info = mod._read_queue_with_recovery()
        assert info is not None and info["mode"] == "bak"
        assert info["recovered_items"] == 2
        assert [it["id"] for it in data["items"]] == ["q-x", "q-y"]


def test_recovery_resets_empty_as_last_resort():
    mod = _load_module()
    with tempfile.TemporaryDirectory() as tmp:
        _point_module_at(mod, tmp)
        mod.QUEUE_FILE.write_text("}}}garbage{{{")
        data, info = mod._read_queue_with_recovery()
        assert info is not None and info["mode"] == "empty"
        assert info["recovered_items"] == 0
        assert data["items"] == []


def test_clean_parse_reports_no_recovery():
    mod = _load_module()
    with tempfile.TemporaryDirectory() as tmp:
        _point_module_at(mod, tmp)
        mod.QUEUE_FILE.write_text(json.dumps(_queue_doc(["q-a"])) + "\n")
        data, info = mod._read_queue_with_recovery()
        assert info is None
        assert [it["id"] for it in data["items"]] == ["q-a"]


# ---------------------------------------------------------------------------
# End-to-end tests: real `session-task` subprocess against a temp HOME
# ---------------------------------------------------------------------------


def _env_for_tmp(tmp):
    env = os.environ.copy()
    env["HOME"] = str(tmp)
    env["PINGME_SESSION_TASK"] = "0"
    # Keep claude-event emits off the real bus; stderr alert still fires.
    env["CLAUDE_EVENT_SESSION_TASK"] = "0"
    return env


def _run(env, *argv, timeout=20):
    cmd = [sys.executable, str(SESSION_TASK)] + list(argv)
    return subprocess.run(cmd, capture_output=True, text=True, env=env,
                          timeout=timeout)


def _write_queue(tmp, text):
    d = Path(tmp) / ".config" / "session"
    d.mkdir(parents=True, exist_ok=True)
    (d / "queue.json").write_text(text)
    return d


def test_e2e_corrupt_file_salvages_and_alerts_loudly():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        doc = _queue_doc(["q-a", "q-b", "q-c"])
        corrupt = json.dumps(doc, indent=2) + "\n" + 'd /x && echo y",\n }\n]\n}\n'
        d = _write_queue(tmp, corrupt)
        r = _run(env, "queue", "list", "--json")
        assert r.returncode == 0, r.stderr
        items = json.loads(r.stdout)
        got = sorted(it["id"] for it in items)
        assert got == ["q-a", "q-b", "q-c"], (got, r.stderr)
        # LOUD alert on stderr (never silent).
        assert "CORRUPT" in r.stderr and "salvaged" in r.stderr, r.stderr
        # Corrupt bytes preserved.
        assert list(d.glob("queue.json.corrupt.*")), "no quarantine file"


def test_e2e_unrecoverable_reset_alerts_loudly():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        _write_queue(tmp, "}}}total garbage no json{{{")
        r = _run(env, "queue", "list", "--json")
        assert r.returncode == 0, r.stderr
        assert json.loads(r.stdout) == []
        assert "CORRUPT" in r.stderr and "reset to EMPTY" in r.stderr, r.stderr
        assert "IN-FLIGHT QUEUE STATE LOST" in r.stderr, r.stderr


def test_e2e_write_snapshots_bak_and_stays_parseable():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        r = _run(env, "queue", "add", "task one", "--scope", "repo:foo", "--json")
        assert r.returncode == 0, r.stderr
        d = Path(tmp) / ".config" / "session"
        # queue.json parses.
        json.loads((d / "queue.json").read_text())
        # A last-good .bak snapshot was written and parses to the same items.
        bak = d / "queue.json.bak"
        assert bak.exists(), "no .bak snapshot after a successful write"
        bak_data = json.loads(bak.read_text())
        assert len(bak_data["items"]) == 1


def test_e2e_concurrent_writers_never_corrupt_never_lose():
    """N concurrent same-domain `queue add`s: flock serializes them and the
    atomic write means queue.json is always parseable with all N items."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        Path(tmp, ".config/session").mkdir(parents=True, exist_ok=True)
        n = 12
        errors = []

        def worker(i):
            try:
                r = _run(env, "queue", "add", f"task {i}",
                         "--scope", f"repo:r{i}", "--json")
                if r.returncode != 0:
                    errors.append((i, r.returncode, r.stderr))
            except Exception as e:  # noqa: BLE001
                errors.append((i, "exc", repr(e)))

        threads = [threading.Thread(target=worker, args=(i,)) for i in range(n)]
        for t in threads:
            t.start()
        for t in threads:
            t.join(timeout=40)

        assert not errors, f"add errors: {errors}"
        d = Path(tmp) / ".config" / "session"
        data = json.loads((d / "queue.json").read_text())  # must parse
        assert len(data["items"]) == n, f"lost items: {len(data['items'])}/{n}"
        ids = [it["id"] for it in data["items"]]
        assert len(set(ids)) == n, f"duplicate ids: {ids}"
        # No corruption occurred -> no quarantine files.
        assert not list(d.glob("queue.json.corrupt.*")), "unexpected quarantine"


if __name__ == "__main__":
    import pytest
    sys.exit(pytest.main([__file__, "-v"]))
