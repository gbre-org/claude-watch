#!/usr/bin/env python3
"""Passive CI rows (`kind: ci`): upsert / close / list / check, hiding,
exemptions, and outcome-aware dependencies.

Run:
    uv run --python 3.11 --with pytest \\
        pytest tools/session-task/tests/test_queue_ci_rows.py -v
"""

import json
import stat
import sys
import tempfile
from datetime import datetime, timedelta, timezone
from pathlib import Path

import pytest

sys.path.insert(0, str(Path(__file__).resolve().parent))
from test_queue_waiting import _env, _run, _set  # noqa: E402


@pytest.fixture()
def env():
    with tempfile.TemporaryDirectory() as tmp:
        e = _env(tmp)
        # Record claude-event emits through a shim instead of the real bus.
        bindir = Path(tmp) / "bin"
        bindir.mkdir()
        shim = bindir / "claude-event"
        log = Path(tmp) / "emits.log"
        shim.write_text(
            "#!/bin/sh\necho \"$@\" >> " + str(log) + "\n")
        shim.chmod(shim.stat().st_mode | stat.S_IEXEC)
        e["PATH"] = str(bindir) + ":" + e["PATH"]
        e.pop("CLAUDE_EVENT_SESSION_TASK", None)
        e["EMIT_LOG"] = str(log)
        yield e


def _emits(env):
    p = Path(env["EMIT_LOG"])
    return p.read_text().splitlines() if p.exists() else []


def _upsert(env, run="7", job="", **kw):
    argv = ["queue", "ci", "upsert", "--provider", "prov", "--repo", "o/r",
            "--run", run]
    if job:
        argv += ["--job", job]
    for k, v in kw.items():
        argv += ["--" + k.replace("_", "-"), v]
    return _run(env, *argv).stdout.strip()


def _close(env, outcome, run="7", job=""):
    argv = ["queue", "ci", "close", "--provider", "prov", "--repo", "o/r",
            "--run", run, "--outcome", outcome]
    if job:
        argv += ["--job", job]
    return _run(env, *argv)


def _show(env, q):
    return json.loads(_run(env, "queue", "show", q).stdout)


def _add_dep(env, ref):
    return json.loads(_run(env, "queue", "add", "dependent", "--scope",
                           "repo:w-a", "--summary", "dep", "--depends-on", ref,
                           "--json").stdout)


def test_upsert_idempotent_and_prints_id(env):
    a = _upsert(env, status="queued", url="http://x/1")
    b = _upsert(env, status="in_progress", branch="feat")
    assert a == b and a.startswith("q-")
    row = _show(env, a)
    assert row["kind"] == "ci" and row["status"] == "running"
    assert row["scope"] == []
    assert row["ci"]["status"] == "in_progress"
    assert row["ci"]["url"] == "http://x/1"  # kept across updates
    assert row["ci"]["branch"] == "feat"
    rows = json.loads(_run(env, "queue", "ci", "list", "--json").stdout)
    assert len(rows) == 1
    # a different job is a different row
    assert _upsert(env, job="build") != a


def test_hidden_from_default_list_and_ready(env):
    ci = _upsert(env)
    default = json.loads(_run(env, "queue", "list", "--json").stdout)
    assert ci not in [i["id"] for i in default]
    allv = json.loads(_run(env, "queue", "list", "--all", "--json").stdout)
    assert ci not in [i["id"] for i in allv]
    ready = json.loads(_run(env, "queue", "list", "--ready", "--json").stdout)
    assert ci not in [i["id"] for i in ready]
    shown = json.loads(_run(env, "queue", "list", "--ci", "--json").stdout)
    assert ci in [i["id"] for i in shown]
    only = json.loads(_run(env, "queue", "list", "--ci-only", "--json").stdout)
    assert [i["id"] for i in only] == [ci]


def test_ci_row_does_not_affect_other_items(env):
    _upsert(env)
    out = json.loads(_run(env, "queue", "add", "x", "--scope", "*",
                          "--summary", "s", "--json").stdout)
    assert out["ready_now"] is True


def test_dep_ready_only_on_success(env):
    ci = _upsert(env)
    dep = _add_dep(env, ci)
    assert dep["ready_now"] is False
    _close(env, "success")
    assert _show(env, ci)["status"] == "done"
    assert _show(env, dep["id"])["ready_now"] is True


@pytest.mark.parametrize("outcome", ["failure", "cancelled", "skipped"])
def test_dep_blocked_on_non_success(env, outcome):
    ci = _upsert(env)
    dep = _add_dep(env, ci)
    _close(env, outcome)
    row = _show(env, dep["id"])
    assert row["status"] == "blocked"
    assert ci in row["block_reason"] and outcome in row["block_reason"]
    assert row["ready_now"] is False
    assert any("queue-dep-failed" in e for e in _emits(env))
    # blocked, not silently ready
    ready = json.loads(_run(env, "queue", "list", "--ready", "--json").stdout)
    assert dep["id"] not in [i["id"] for i in ready]


def test_dep_added_after_failure_blocks_immediately(env):
    ci = _upsert(env)
    _close(env, "failure")
    dep = _add_dep(env, ci)
    assert _show(env, dep["id"])["status"] == "blocked"


def test_dep_by_run_reference_creates_placeholder_then_reuses(env):
    dep = _add_dep(env, "ci:prov:o/r#7")
    ref_ids = dep["depends_on"]
    assert len(ref_ids) == 1
    # the syncer's first upsert lands on the same row
    assert _upsert(env) == ref_ids[0]
    assert dep["ready_now"] is False
    _close(env, "success")
    assert _show(env, dep["id"])["ready_now"] is True


def test_job_ref(env):
    dep = _add_dep(env, "ci:prov:o/r#7:build")
    assert _upsert(env, job="build") == dep["depends_on"][0]


def test_normal_items_unchanged_done_abandon(env):
    a = json.loads(_run(env, "queue", "add", "a", "--scope", "repo:z",
                        "--summary", "s", "--json").stdout)["id"]
    b = json.loads(_run(env, "queue", "add", "b", "--scope", "repo:y",
                        "--summary", "s", "--depends-on", a,
                        "--json").stdout)
    assert b["ready_now"] is False
    _run(env, "queue", "register", a)
    _run(env, "queue", "done", a)
    assert _show(env, b["id"])["ready_now"] is True
    # abandoned normal dep stays a silent permanent blocker (unchanged)
    c = json.loads(_run(env, "queue", "add", "c", "--scope", "repo:x",
                        "--summary", "s", "--json").stdout)["id"]
    d = json.loads(_run(env, "queue", "add", "d", "--scope", "repo:w",
                        "--summary", "s", "--depends-on", c,
                        "--json").stdout)["id"]
    _run(env, "queue", "abandon", c)
    row = _show(env, d)
    assert row["status"] == "pending" and row["ready_now"] is False
    assert not any("queue-dep-failed" in e for e in _emits(env))


def test_close_idempotent_and_conflict(env):
    ci = _upsert(env)
    assert _close(env, "failure").stdout.strip() == ci
    assert _close(env, "failure").stdout.strip() == ci


def test_close_conflicting_outcome_exit_3(env):
    _upsert(env)
    _close(env, "failure")
    r = _run(env, "queue", "ci", "close", "--provider", "prov", "--repo",
             "o/r", "--run", "7", "--outcome", "success", expect_exit=3)
    assert "reopen" in r.stderr


def test_close_unknown_run_creates_closed_row(env):
    r = _close(env, "success", run="99")
    row = _show(env, r.stdout.strip())
    assert row["status"] == "done" and row["ci"]["outcome"] == "success"


def test_closed_row_ignored_by_upsert_until_reopen(env):
    ci = _upsert(env)
    _close(env, "failure")
    _upsert(env, status="running")
    assert _show(env, ci)["status"] == "abandoned"
    _run(env, "queue", "ci", "upsert", "--provider", "prov", "--repo", "o/r",
         "--run", "7", "--reopen")
    row = _show(env, ci)
    assert row["status"] == "running" and row["ci"]["outcome"] == ""


def test_parent_link_and_list_filter(env):
    _upsert(env, parent="q-2026-11-25-aaaa")
    _upsert(env, run="8")
    rows = json.loads(_run(env, "queue", "ci", "list", "--parent",
                           "q-2026-11-25-aaaa", "--json").stdout)
    assert len(rows) == 1 and rows[0]["parent"] == "q-2026-11-25-aaaa"


def _backdate(env, q, **ci_fields):
    path = Path(env["HOME"]) / ".config/session/queue.json"
    d = json.loads(path.read_text())
    for it in d["items"]:
        if it["id"] == q:
            it["ci"].update(ci_fields)
            if "created_at" in ci_fields:
                it["created_at"] = ci_fields["created_at"]
    path.write_text(json.dumps(d))


def _ago(minutes):
    return (datetime.now(timezone.utc) - timedelta(minutes=minutes)).isoformat()


def test_check_fresh_row_is_quiet(env):
    _upsert(env)
    r = _run(env, "queue", "ci", "check")
    assert r.stdout.strip() == ""
    assert not any("queue-ci-stale" in e for e in _emits(env))


def test_check_stale_sync_fires_once(env):
    ci = _upsert(env)
    _backdate(env, ci, last_synced=_ago(30))
    r = _run(env, "queue", "ci", "check", "--stale-sync-min", "10")
    assert "stale-sync" in r.stdout
    assert len([e for e in _emits(env) if "queue-ci-stale" in e]) == 1
    _run(env, "queue", "ci", "check", "--stale-sync-min", "10")
    assert len([e for e in _emits(env) if "queue-ci-stale" in e]) == 1


def test_check_max_run(env):
    ci = _upsert(env)
    _backdate(env, ci, created_at=_ago(500))
    r = _run(env, "queue", "ci", "check", "--max-run-min", "60")
    assert "max-run" in r.stdout
    assert len([e for e in _emits(env) if "queue-ci-stale" in e]) == 1


def test_check_ignores_closed_rows_and_dry_run(env):
    ci = _upsert(env)
    _backdate(env, ci, last_synced=_ago(30))
    r = _run(env, "queue", "ci", "check", "--stale-sync-min", "10",
             "--dry-run")
    assert "stale-sync" in r.stdout and not _emits(env)
    _close(env, "success")
    r = _run(env, "queue", "ci", "check", "--stale-sync-min", "10")
    assert r.stdout.strip() == ""


def test_check_env_defaults(env):
    ci = _upsert(env)
    _backdate(env, ci, last_synced=_ago(30))
    env["SESSION_TASK_CI_STALE_SYNC_MIN"] = "5"
    assert "stale-sync" in _run(env, "queue", "ci", "check").stdout


def test_ci_rows_not_in_banner_groups(env):
    _upsert(env)
    groups = json.loads(_run(env, "queue", "groups", "--json").stdout)
    assert groups == []


def test_upsert_stamps_syncer_owner_and_pr_url(env):
    q = _upsert(env, created_by="pr-watch", pr_url="https://example.test/pr/1")
    row = _show(env, q)
    assert row["owner"] == "pr-watch syncer (prov)"
    assert row["ci"]["pr_url"] == "https://example.test/pr/1"
    # Re-upsert keeps the stamp (and backfills rows created without one).
    _upsert(env)
    assert _show(env, q)["owner"] == "pr-watch syncer (prov)"


def test_upsert_owner_defaults_to_ci_sync(env):
    q = _upsert(env)
    assert _show(env, q)["owner"] == "ci-sync syncer (prov)"


def test_banner_never_flags_ci_row_as_no_owner(env):
    _upsert(env)
    out = _run(env, "queue", "banner").stdout
    assert "NO OWNER" not in out
