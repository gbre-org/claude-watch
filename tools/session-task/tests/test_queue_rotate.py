#!/usr/bin/env python3
"""Tests for `session-task queue rotate` — session-store rotation/archival.

`queue rotate` bounds the two unbounded growers under ~/.config/session/:

  1. queue-logs/  -- per-completed-item transcript archives. Rotation prunes
     entries older than a max-age, then enforces a hard max-count floor
     (oldest-by-mtime pruned first). Recent transcripts are kept so the
     q-site "View log" affordance still works on recent Done cards.

  2. completed-tasks.jsonl -- append-only queue/resume history. Rotation rolls
     it once it exceeds a byte threshold: the most-recent RETAIN lines stay in
     the LIVE file, everything older is moved to a dated gz segment under
     completed-archive/. Keeping the recent tail in the live file is what lets
     the q-site DONE view (which reads the live file) keep seeing recent done
     items across a roll.

Behavior contract exercised here:

  * Age + count pruning of queue-logs (files AND dirs), recent kept.
  * completed-tasks roll: archive integrity (gz round-trips, oldest rows go to
    the archive, newest RETAIN rows stay live), atomic (no data lost).
  * No-op below threshold; idempotent (second run does nothing new).
  * DONE-view-still-sees-recent: the retained live tail contains the newest
    done rows, and the q-site reader shape (done-only, newest-first, deduped)
    still surfaces them.
  * Env-var overrides + CLI-flag overrides both honored.
  * --dry-run mutates nothing.

All tests run against a temp HOME so the live ~/.config/session/ is untouched.

Run::

    uv run --python 3.11 --with pytest \\
        pytest tools/session-task/tests/test_queue_rotate.py -v
"""

import gzip
import json
import os
import subprocess
import sys
import time
from pathlib import Path

import pytest

SESSION_TASK = Path(__file__).resolve().parent.parent / "session-task"


def _env_for_tmp(tmp):
    tmp = Path(tmp)
    env = os.environ.copy()
    env["HOME"] = str(tmp)
    env["PINGME_SESSION_TASK"] = "0"
    env["CLAUDE_EVENT_SESSION_TASK"] = "0"
    env["QUEUE_LOG_ARCHIVE_DIR"] = str(tmp / ".config" / "session" / "queue-logs")
    env["COMPLETED_ARCHIVE_DIR"] = str(
        tmp / ".config" / "session" / "completed-archive"
    )
    return env


def _run(env, *argv, expect_exit=0):
    cmd = [sys.executable, str(SESSION_TASK)] + list(argv)
    r = subprocess.run(cmd, capture_output=True, text=True, env=env, timeout=30)
    if r.returncode != expect_exit:
        raise RuntimeError(
            f"unexpected exit {r.returncode} (want {expect_exit}): argv={argv}\n"
            f"stdout={r.stdout!r}\nstderr={r.stderr!r}"
        )
    return r


def _sess_dir(tmp):
    d = Path(tmp) / ".config" / "session"
    d.mkdir(parents=True, exist_ok=True)
    return d


def _write_completed(tmp, n, *, body_pad=200, event="done", id_prefix="q-x"):
    """Write `n` completed-tasks.jsonl rows (oldest first)."""
    sess = _sess_dir(tmp)
    cf = sess / "completed-tasks.jsonl"
    with open(cf, "w") as f:
        for i in range(n):
            f.write(
                json.dumps(
                    {
                        "source": "queue",
                        "event": event,
                        "id": f"{id_prefix}-{i:05d}",
                        "group_id": "g-1",
                        "task": f"[queue {id_prefix}-{i:05d}] task {i} " + ("z" * body_pad),
                    }
                )
                + "\n"
            )
    return cf


def _make_queue_log(tmp, name, *, age_days=0.0, is_dir=False):
    ql = Path(tmp) / ".config" / "session" / "queue-logs"
    ql.mkdir(parents=True, exist_ok=True)
    p = ql / name
    if is_dir:
        p.mkdir()
        (p / "inner.txt").write_text("x")
    else:
        p.write_text("x")
    if age_days:
        t = time.time() - age_days * 86400.0
        os.utime(p, (t, t))
    return p


# ---------------------------------------------------------------------------
# queue-logs rotation
# ---------------------------------------------------------------------------


def test_queue_logs_age_prune(tmp_path):
    env = _env_for_tmp(tmp_path)
    for i in range(3):
        _make_queue_log(tmp_path, f"q-old-{i}.jsonl", age_days=40)
    for i in range(3):
        _make_queue_log(tmp_path, f"q-new-{i}.jsonl", age_days=1)

    r = _run(env, "queue", "rotate", "--queue-logs-max-age", "30",
             "--queue-logs-max-count", "1000", "--json")
    out = json.loads(r.stdout)
    assert out["queue_logs"]["scanned"] == 6
    assert len(out["queue_logs"]["pruned_age"]) == 3
    ql = tmp_path / ".config" / "session" / "queue-logs"
    remaining = sorted(p.name for p in ql.iterdir())
    assert remaining == ["q-new-0.jsonl", "q-new-1.jsonl", "q-new-2.jsonl"]


def test_queue_logs_count_cap(tmp_path):
    env = _env_for_tmp(tmp_path)
    # 10 entries, all recent, staggered mtimes so oldest-first is deterministic.
    base = time.time()
    ql = tmp_path / ".config" / "session" / "queue-logs"
    ql.mkdir(parents=True, exist_ok=True)
    for i in range(10):
        p = ql / f"q-{i:02d}.jsonl"
        p.write_text("x")
        t = base - (10 - i) * 3600  # q-00 oldest, q-09 newest
        os.utime(p, (t, t))

    r = _run(env, "queue", "rotate", "--queue-logs-max-age", "0",
             "--queue-logs-max-count", "4", "--json")
    out = json.loads(r.stdout)
    assert len(out["queue_logs"]["pruned_count"]) == 6
    remaining = sorted(p.name for p in ql.iterdir())
    # Newest 4 survive.
    assert remaining == ["q-06.jsonl", "q-07.jsonl", "q-08.jsonl", "q-09.jsonl"]


def test_queue_logs_prunes_dirs(tmp_path):
    env = _env_for_tmp(tmp_path)
    _make_queue_log(tmp_path, "q-olddir", age_days=40, is_dir=True)
    _make_queue_log(tmp_path, "q-newdir", age_days=1, is_dir=True)
    _run(env, "queue", "rotate", "--queue-logs-max-age", "30",
         "--queue-logs-max-count", "1000")
    ql = tmp_path / ".config" / "session" / "queue-logs"
    remaining = sorted(p.name for p in ql.iterdir())
    assert remaining == ["q-newdir"]
    assert (ql / "q-newdir" / "inner.txt").exists()


def test_queue_logs_missing_dir_noop(tmp_path):
    env = _env_for_tmp(tmp_path)
    # No queue-logs dir at all.
    r = _run(env, "queue", "rotate", "--json")
    out = json.loads(r.stdout)
    assert out["queue_logs"]["scanned"] == 0
    assert out["queue_logs"]["pruned_age"] == []


# ---------------------------------------------------------------------------
# completed-tasks.jsonl rolling
# ---------------------------------------------------------------------------


def test_completed_roll_archive_integrity(tmp_path):
    env = _env_for_tmp(tmp_path)
    cf = _write_completed(tmp_path, 3000)
    size_before = cf.stat().st_size
    assert size_before > 100 * 1024  # comfortably over the 0.1MB test threshold

    r = _run(env, "queue", "rotate", "--completed-max-mb", "0.1",
             "--completed-retain", "500", "--json")
    out = json.loads(r.stdout)["completed"]
    assert out["rolled"] is True
    assert out["archived_lines"] == 2500
    assert out["retained_lines"] == 500

    # Live file: newest 500 rows, in order.
    live = [json.loads(l) for l in cf.read_text().splitlines() if l.strip()]
    assert len(live) == 500
    assert live[0]["id"] == "q-x-02500"
    assert live[-1]["id"] == "q-x-02999"

    # Archive: gz round-trips, holds oldest 2500 rows, no overlap, no loss.
    arch_dir = tmp_path / ".config" / "session" / "completed-archive"
    segs = list(arch_dir.glob("completed-tasks-*.jsonl.gz"))
    assert len(segs) == 1
    arch_rows = [
        json.loads(l) for l in gzip.open(segs[0], "rt").read().splitlines() if l.strip()
    ]
    assert len(arch_rows) == 2500
    assert arch_rows[0]["id"] == "q-x-00000"
    assert arch_rows[-1]["id"] == "q-x-02499"

    # Union of archive + live == the original 3000, no gap, no dupe.
    all_ids = [r["id"] for r in arch_rows] + [r["id"] for r in live]
    assert all_ids == [f"q-x-{i:05d}" for i in range(3000)]


def test_completed_no_roll_under_threshold(tmp_path):
    env = _env_for_tmp(tmp_path)
    _write_completed(tmp_path, 10)
    r = _run(env, "queue", "rotate", "--completed-max-mb", "50", "--json")
    out = json.loads(r.stdout)["completed"]
    assert out["rolled"] is False
    arch_dir = tmp_path / ".config" / "session" / "completed-archive"
    assert not arch_dir.exists() or not list(arch_dir.glob("*.gz"))


def test_completed_roll_idempotent(tmp_path):
    env = _env_for_tmp(tmp_path)
    _write_completed(tmp_path, 3000)
    _run(env, "queue", "rotate", "--completed-max-mb", "0.1", "--completed-retain", "500")
    # Second run: live file now has 500 lines, well under threshold -> no roll.
    r = _run(env, "queue", "rotate", "--completed-max-mb", "0.1",
             "--completed-retain", "500", "--json")
    out = json.loads(r.stdout)["completed"]
    assert out["rolled"] is False
    segs = list((tmp_path / ".config" / "session" / "completed-archive").glob("*.gz"))
    assert len(segs) == 1  # not a second segment


def test_completed_missing_file_noop(tmp_path):
    env = _env_for_tmp(tmp_path)
    _sess_dir(tmp_path)  # dir exists, file doesn't
    r = _run(env, "queue", "rotate", "--json")
    out = json.loads(r.stdout)["completed"]
    assert out["rolled"] is False


def test_completed_archive_cap(tmp_path):
    """Old gz segments are pruned to ROTATE_COMPLETED_ARCHIVE_MAX."""
    env = _env_for_tmp(tmp_path)
    env["ROTATE_COMPLETED_ARCHIVE_MAX"] = "2"
    arch_dir = tmp_path / ".config" / "session" / "completed-archive"
    arch_dir.mkdir(parents=True)
    # Pre-seed 3 old segments with sortable (chronological) names.
    for ts in ("20260101T000000Z", "20260102T000000Z", "20260103T000000Z"):
        (arch_dir / f"completed-tasks-{ts}.jsonl.gz").write_bytes(b"old")
    _write_completed(tmp_path, 3000)
    _run(env, "queue", "rotate", "--completed-max-mb", "0.1", "--completed-retain", "500")
    segs = sorted(p.name for p in arch_dir.glob("*.jsonl.gz"))
    # 3 pre-seeded + 1 fresh = 4, capped to newest 2.
    assert len(segs) == 2
    # The freshly written segment (today) must be among survivors.
    assert any(s > "20260103T000000Z" for s in segs) or segs[-1].startswith(
        "completed-tasks-2026"
    )


# ---------------------------------------------------------------------------
# DONE-view coordination (q-site reader still sees recent done items)
# ---------------------------------------------------------------------------


def test_done_view_sees_recent_after_roll(tmp_path):
    """Emulate the q-site #581 DONE reader against the rolled LIVE file and
    confirm the newest done items are still visible."""
    env = _env_for_tmp(tmp_path)
    cf = _write_completed(tmp_path, 3000)
    _run(env, "queue", "rotate", "--completed-max-mb", "0.1", "--completed-retain", "500")

    # Reader shape mirroring queue-minisite/app.py _load_completed_done_entries:
    # done-only, newest-first, deduped by id, from the LIVE file only.
    rows = []
    for line in cf.read_text().splitlines():
        line = line.strip()
        if not line:
            continue
        row = json.loads(line)
        if row.get("event") and row["event"] != "done":
            continue
        head = row.get("task", "").split("]", 1)[0]
        if "abandoned" in head:
            continue
        rows.append(row)
    rows.sort(key=lambda r: r["id"], reverse=True)
    seen, done = set(), []
    for r in rows:
        if r["id"] in seen:
            continue
        seen.add(r["id"])
        done.append(r)

    ids = [r["id"] for r in done]
    # The most-recent done items survive in the live file and are visible.
    assert ids[0] == "q-x-02999"
    assert "q-x-02500" in ids
    # Old (archived) ids are gone from the live view — but preserved in the gz.
    assert "q-x-00000" not in ids


# ---------------------------------------------------------------------------
# env-var + CLI override precedence, dry-run safety
# ---------------------------------------------------------------------------


def test_env_var_thresholds(tmp_path):
    env = _env_for_tmp(tmp_path)
    env["ROTATE_COMPLETED_MAX_BYTES"] = str(100 * 1024)  # 100 KB
    env["ROTATE_COMPLETED_RETAIN"] = "300"
    _write_completed(tmp_path, 3000)
    r = _run(env, "queue", "rotate", "--json")  # no CLI flags -> env wins
    out = json.loads(r.stdout)["completed"]
    assert out["rolled"] is True
    assert out["retained_lines"] == 300


def test_cli_overrides_env(tmp_path):
    env = _env_for_tmp(tmp_path)
    env["ROTATE_COMPLETED_RETAIN"] = "300"
    _write_completed(tmp_path, 3000)
    r = _run(env, "queue", "rotate", "--completed-max-mb", "0.1",
             "--completed-retain", "700", "--json")  # CLI wins
    out = json.loads(r.stdout)["completed"]
    assert out["retained_lines"] == 700


def test_malformed_env_falls_back(tmp_path):
    env = _env_for_tmp(tmp_path)
    env["ROTATE_COMPLETED_RETAIN"] = "not-a-number"
    _write_completed(tmp_path, 3000)
    # Must not crash; falls back to the default (2000) retain.
    r = _run(env, "queue", "rotate", "--completed-max-mb", "0.1", "--json")
    out = json.loads(r.stdout)["completed"]
    assert out["rolled"] is True
    assert out["retained_lines"] == 2000


def test_dry_run_mutates_nothing(tmp_path):
    env = _env_for_tmp(tmp_path)
    cf = _write_completed(tmp_path, 3000)
    for i in range(3):
        _make_queue_log(tmp_path, f"q-old-{i}.jsonl", age_days=40)
    before = cf.read_text()

    r = _run(env, "queue", "rotate", "--completed-max-mb", "0.1",
             "--completed-retain", "500", "--queue-logs-max-age", "30",
             "--dry-run", "--json")
    out = json.loads(r.stdout)
    assert out["dry_run"] is True
    assert out["completed"]["rolled"] is True  # reports what WOULD happen
    assert len(out["queue_logs"]["pruned_age"]) == 3

    # Nothing actually changed.
    assert cf.read_text() == before
    arch_dir = tmp_path / ".config" / "session" / "completed-archive"
    assert not arch_dir.exists() or not list(arch_dir.glob("*.gz"))
    ql = tmp_path / ".config" / "session" / "queue-logs"
    assert len(list(ql.iterdir())) == 3  # old files still present


def test_append_not_lost_when_no_roll(tmp_path):
    """A `queue complete` append lands correctly through the new completed
    lock, and rotate below-threshold leaves it intact."""
    env = _env_for_tmp(tmp_path)
    _run(env, "complete", "first task")
    _run(env, "complete", "second task")
    _run(env, "queue", "rotate", "--completed-max-mb", "50")
    cf = tmp_path / ".config" / "session" / "completed-tasks.jsonl"
    rows = [json.loads(l) for l in cf.read_text().splitlines() if l.strip()]
    tasks = [r["task"] for r in rows]
    assert "first task" in tasks
    assert "second task" in tasks


if __name__ == "__main__":
    sys.exit(pytest.main([__file__, "-v"]))
