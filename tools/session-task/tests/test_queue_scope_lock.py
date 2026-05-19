#!/usr/bin/env python3
"""Tests for the manual scope-lock subcommands (lock/unlock/locks).

Background (2026-05-08): operator wanted a way to enqueue an item but
hold it pending a real-world condition (hardware install, manual config
change, etc.). The lock subcommand parks any pending queue item whose
scope overlaps a locked token until `unlock` releases it.

Coverage:
    1. `lock <scope>` writes the token into queue.json and survives reload
    2. `queue add` of an item with a locked scope returns ready_now=false
    3. `queue ready` excludes items blocked solely by a lock
    4. `queue list --ready` excludes items blocked solely by a lock
    5. `queue spawn-check` refuses items blocked solely by a lock
    6. `unlock <scope>` flips items back to ready_now=true immediately
    7. Double-lock is idempotent (no error, locked_at preserved)
    8. Unlocking a non-locked scope is a no-op (exit 0)
    9. `queue locks` JSON shape is stable

Run:
    uv run --python 3.11 --with pytest \\
        pytest path/to/test_queue_scope_lock.py -v

Or directly: `python3 test_queue_scope_lock.py`.
"""

import json
import os
import subprocess
import sys
import tempfile
from pathlib import Path

SESSION_TASK = Path(__file__).resolve().parent.parent / "session-task"


def _env_for_tmp(tmp):
    env = dict(os.environ)
    env["HOME"] = str(tmp)
    env["PINGME_SESSION_TASK"] = "0"
    Path(tmp, ".config/session").mkdir(parents=True, exist_ok=True)
    return env


def _run(env, *argv, check=False, timeout=15):
    cmd = [sys.executable, str(SESSION_TASK)] + list(argv)
    r = subprocess.run(
        cmd, capture_output=True, text=True, env=env, timeout=timeout
    )
    if check and r.returncode != 0:
        raise RuntimeError(
            f"command failed rc={r.returncode}\n"
            f"  cmd: {' '.join(argv)}\n"
            f"  stdout: {r.stdout}\n"
            f"  stderr: {r.stderr}"
        )
    return r


def _add(env, desc, scope_args, *extra):
    cmd = ["queue", "add", desc, "--summary", "t", "--json"]
    for s in scope_args:
        cmd.extend(["--scope", s])
    cmd.extend(extra)
    return _run(env, *cmd)


# ---------------------------------------------------------------------------
# 1. lock writes a token into queue.json and persists across invocations
# ---------------------------------------------------------------------------


def test_lock_persists_in_queue_json():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)

        r = _run(env, "queue", "lock", "new-aqi-meter",
                 "--reason", "wait for hardware", "--json", check=True)
        d = json.loads(r.stdout)
        assert d["scope"] == "new-aqi-meter"
        assert d["reason"] == "wait for hardware"
        assert d["locked_at"]  # iso timestamp set
        assert d["already_locked"] is False

        # Second invocation should see the persisted lock.
        r2 = _run(env, "queue", "locks", "--json", check=True)
        rows = json.loads(r2.stdout)
        assert len(rows) == 1
        assert rows[0]["scope"] == "new-aqi-meter"
        assert rows[0]["reason"] == "wait for hardware"
        assert rows[0]["items_held"] == 0


# ---------------------------------------------------------------------------
# 2. queue add of an item with a locked scope returns ready_now=false
# ---------------------------------------------------------------------------


def test_queue_add_during_lock_blocks_ready():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)

        _run(env, "queue", "lock", "new-aqi-meter",
             "--reason", "wait for hardware", check=True)

        r = _add(env, "update ecowitt", ["new-aqi-meter"])
        assert r.returncode == 0, r.stderr
        d = json.loads(r.stdout)
        assert d["ready_now"] is False
        assert d["lock_blockers"] == ["new-aqi-meter"]
        assert "locks: new-aqi-meter" in d["spawn_instruction"]
        assert "BLOCKED" in d["spawn_instruction"]


# ---------------------------------------------------------------------------
# 3. queue ready excludes items blocked solely by a lock
# ---------------------------------------------------------------------------


def test_queue_ready_excludes_locked():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)

        _run(env, "queue", "lock", "new-aqi-meter", check=True)
        r_add = _add(env, "update ecowitt", ["new-aqi-meter"])
        assert r_add.returncode == 0

        r_ready = _run(env, "queue", "ready", "--json", check=True)
        ready = json.loads(r_ready.stdout)
        assert ready == [], ready


# ---------------------------------------------------------------------------
# 4. queue list --ready excludes items blocked solely by a lock
# ---------------------------------------------------------------------------


def test_queue_list_ready_excludes_locked():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)

        _run(env, "queue", "lock", "new-aqi-meter", check=True)
        _add(env, "update ecowitt", ["new-aqi-meter"])

        r = _run(env, "queue", "list", "--ready", "--json", check=True)
        items = json.loads(r.stdout)
        assert items == [], items


# ---------------------------------------------------------------------------
# 5. queue spawn-check refuses items blocked solely by a lock
# ---------------------------------------------------------------------------


def test_spawn_check_refuses_locked_item():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)

        _run(env, "queue", "lock", "new-aqi-meter", check=True)
        r_add = _add(env, "update ecowitt", ["new-aqi-meter"])
        d = json.loads(r_add.stdout)
        qid = d["id"]

        r = _run(env, "queue", "spawn-check", qid, "--json")
        assert r.returncode == 2, (
            f"expected exit 2 (BLOCKED), got {r.returncode}\n"
            f"  stdout: {r.stdout}\n  stderr: {r.stderr}"
        )
        body = json.loads(r.stdout)
        assert body["ok"] is False
        assert any("locked" in reason for reason in body["reasons"]), body


# ---------------------------------------------------------------------------
# 6. unlock flips items to ready_now=true immediately
# ---------------------------------------------------------------------------


def test_unlock_releases_blocked_items():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)

        _run(env, "queue", "lock", "new-aqi-meter", check=True)
        r_add = _add(env, "update ecowitt", ["new-aqi-meter"])
        d = json.loads(r_add.stdout)
        qid = d["id"]

        # Confirm BLOCKED first.
        r_check = _run(env, "queue", "spawn-check", qid, "--json")
        assert r_check.returncode == 2

        # Unlock.
        r_un = _run(env, "queue", "unlock", "new-aqi-meter",
                    "--json", check=True)
        un = json.loads(r_un.stdout)
        assert un["was_locked"] is True

        # spawn-check should now succeed.
        r_check2 = _run(env, "queue", "spawn-check", qid, "--json", check=True)
        body = json.loads(r_check2.stdout)
        assert body["ok"] is True, body

        # And the item appears in queue ready.
        r_ready = _run(env, "queue", "ready", "--json", check=True)
        ready = json.loads(r_ready.stdout)
        assert len(ready) == 1
        assert ready[0]["id"] == qid


# ---------------------------------------------------------------------------
# 7. Double-lock is idempotent (no error; preserves original locked_at)
# ---------------------------------------------------------------------------


def test_double_lock_idempotent():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)

        r1 = _run(env, "queue", "lock", "new-aqi-meter",
                  "--reason", "first reason", "--json", check=True)
        d1 = json.loads(r1.stdout)
        assert d1["already_locked"] is False
        first_locked_at = d1["locked_at"]

        r2 = _run(env, "queue", "lock", "new-aqi-meter",
                  "--json", check=True)
        d2 = json.loads(r2.stdout)
        assert d2["already_locked"] is True
        # locked_at must NOT be reset when re-locking.
        assert d2["locked_at"] == first_locked_at
        # No new reason passed, prior reason preserved.
        assert d2["reason"] == "first reason"

        # Re-lock with a new reason updates the reason but keeps locked_at.
        r3 = _run(env, "queue", "lock", "new-aqi-meter",
                  "--reason", "updated reason", "--json", check=True)
        d3 = json.loads(r3.stdout)
        assert d3["already_locked"] is True
        assert d3["locked_at"] == first_locked_at
        assert d3["reason"] == "updated reason"


# ---------------------------------------------------------------------------
# 8. Unlocking a non-locked scope is a no-op (exit 0)
# ---------------------------------------------------------------------------


def test_unlock_non_locked_is_noop():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)

        r = _run(env, "queue", "unlock", "never-locked", "--json")
        assert r.returncode == 0, (r.stdout, r.stderr)
        d = json.loads(r.stdout)
        assert d["was_locked"] is False
        assert d["scope"] == "never-locked"


# ---------------------------------------------------------------------------
# 9. queue locks JSON shape: scope, reason, locked_at, items_held
# ---------------------------------------------------------------------------


def test_queue_locks_json_shape():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)

        _run(env, "queue", "lock", "scope-a", "--reason", "ra", check=True)
        _run(env, "queue", "lock", "scope-b", "--reason", "rb", check=True)
        # Add two items: one held by scope-a, one not held at all (different
        # scope, no lock).
        _add(env, "held-item", ["scope-a"])
        _add(env, "free-item", ["scope-c"])

        r = _run(env, "queue", "locks", "--json", check=True)
        rows = json.loads(r.stdout)
        rows_by_scope = {row["scope"]: row for row in rows}
        assert "scope-a" in rows_by_scope
        assert "scope-b" in rows_by_scope
        assert rows_by_scope["scope-a"]["items_held"] == 1
        assert rows_by_scope["scope-b"]["items_held"] == 0
        assert rows_by_scope["scope-a"]["reason"] == "ra"
        assert rows_by_scope["scope-b"]["reason"] == "rb"
        for row in rows:
            assert "locked_at" in row and row["locked_at"]


# ---------------------------------------------------------------------------
# 10. Lock survives a fresh queue.json read (persistence smoke)
# ---------------------------------------------------------------------------


def test_lock_persists_across_invocations():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)

        _run(env, "queue", "lock", "persistent-scope",
             "--reason", "outlives the process", check=True)

        # Inspect queue.json directly to confirm storage.
        qjson = json.loads(
            (Path(tmp) / ".config" / "session" / "queue.json").read_text()
        )
        assert "locked_scopes" in qjson
        assert "persistent-scope" in qjson["locked_scopes"]
        assert qjson["locked_scopes"]["persistent-scope"]["reason"] == \
            "outlives the process"


# ---------------------------------------------------------------------------
# 11. Lock does not affect items in other (non-overlapping) scopes
# ---------------------------------------------------------------------------


def test_lock_only_affects_overlapping_scopes():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)

        _run(env, "queue", "lock", "scope-locked", check=True)

        # Item with a totally different scope should be ready_now=true.
        r = _add(env, "unaffected", ["scope-elsewhere"])
        assert r.returncode == 0, r.stderr
        d = json.loads(r.stdout)
        assert d["ready_now"] is True, d
        assert d["lock_blockers"] == []


# ---------------------------------------------------------------------------
# 12. task:<id> scope token: blocked while target running, ready on done
# ---------------------------------------------------------------------------


def test_task_scope_token_blocks_while_target_running():
    """An item carrying `task:<id>` in its scope is blocked until the
    target reaches `done`. This is the unified surface that replaces a
    separate `depends_on` field.
    """
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)

        # A is the target dep.
        r_a = _add(env, "first", ["repo:a"])
        a = json.loads(r_a.stdout)

        # B carries task:q-A as a scope token (no --depends-on used).
        r_b = _add(env, "second", ["repo:b", f"task:{a['id']}"])
        b = json.loads(r_b.stdout)
        assert b["ready_now"] is False, b
        assert a["id"] in b["depends_on"], b
        assert a["id"] in b["dep_blockers"], b
        # Different repo scopes -> separate groups (task: tokens don't
        # serialize via groups).
        assert a["group_id"] != b["group_id"]

        # Finish A; B flips ready.
        subprocess.run(
            [sys.executable, str(SESSION_TASK), "queue", "register", a["id"]],
            env=env, capture_output=True, text=True, timeout=15, check=True,
        )
        subprocess.run(
            [sys.executable, str(SESSION_TASK), "queue", "done", a["id"]],
            env=env, capture_output=True, text=True, timeout=15, check=True,
        )

        r_show = _run(env, "queue", "show", b["id"], check=True)
        b_show = json.loads(r_show.stdout)
        assert b_show["ready_now"] is True, b_show
        assert b_show["dep_blockers"] == [], b_show


# ---------------------------------------------------------------------------
# 13. queue lock task:<id> blocks any pending item carrying task:<id>
# ---------------------------------------------------------------------------


def test_queue_lock_task_id_blocks_dependents():
    """Locking `task:<id>` parks every pending item carrying that token
    in its scope. The lock machinery is reused as-is — no new code path
    for task locks.
    """
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)

        # A is some queue id we'll lock the task: token on.
        r_a = _add(env, "anchor", ["repo:a"])
        a = json.loads(r_a.stdout)
        # Done so it can't block on its own merits.
        _run(env, "queue", "register", a["id"], check=True)
        _run(env, "queue", "done", a["id"], check=True)

        # Lock task:<a-id>.
        _run(env, "queue", "lock", f"task:{a['id']}",
             "--reason", "manual gate before B", check=True)

        # B carries task:<a-id>. Even though A is done, the lock parks B.
        r_b = _add(env, "after-a", ["repo:b", f"task:{a['id']}"])
        b = json.loads(r_b.stdout)
        assert b["ready_now"] is False, b
        assert f"task:{a['id']}" in b["lock_blockers"], b
        assert "locks: " in b["spawn_instruction"]

        # Unlocking the task: token releases B.
        _run(env, "queue", "unlock", f"task:{a['id']}", check=True)
        r_check = _run(env, "queue", "spawn-check", b["id"], "--json", check=True)
        body = json.loads(r_check.stdout)
        assert body["ok"] is True, body


# ---------------------------------------------------------------------------
# 14. Mix of task: and regular scope: blocked iff EITHER condition holds
# ---------------------------------------------------------------------------


def test_mixed_scope_task_and_regular_blocks_independently():
    """Item with scope=[repo:x, task:q-A] is blocked if EITHER any
    running peer overlaps repo:x OR q-A is not yet done. Each gate is
    independent.
    """
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)

        # Set up a dep target A and a scope-conflict source X.
        r_a = _add(env, "dep-target", ["repo:dep"])
        a = json.loads(r_a.stdout)
        r_x = _add(env, "scope-peer", ["repo:shared"])
        x = json.loads(r_x.stdout)
        # X is running and holds repo:shared.
        _run(env, "queue", "register", x["id"], check=True)

        # B has both repo:shared (conflicts with X) AND task:q-A.
        # As of 2026-05-19 the default soft-serializes; --force-enqueue
        # is a no-op preserved for back-compat.
        r_b = _add(env, "double-blocked", ["repo:shared", f"task:{a['id']}"],
                   "--force-enqueue")
        b = json.loads(r_b.stdout)
        assert b["ready_now"] is False, b
        # B is blocked by both running peer X (in same merged group) AND
        # by the unmet task:q-A dep.
        assert a["id"] in b["dep_blockers"], b
        # X is in serialized_after (same scope group, X running).
        assert x["id"] in b["serialized_after"], b

        # Finish X (the scope-conflict source). B is still blocked by A.
        _run(env, "queue", "done", x["id"], check=True)
        r_show1 = _run(env, "queue", "show", b["id"], check=True)
        b1 = json.loads(r_show1.stdout)
        assert b1["ready_now"] is False, b1
        assert a["id"] in b1["dep_blockers"], b1

        # Now finish A. B becomes ready.
        _run(env, "queue", "register", a["id"], check=True)
        _run(env, "queue", "done", a["id"], check=True)
        r_show2 = _run(env, "queue", "show", b["id"], check=True)
        b2 = json.loads(r_show2.stdout)
        assert b2["ready_now"] is True, b2
        assert b2["dep_blockers"] == [], b2


# ---------------------------------------------------------------------------
# 15. --depends-on sugar translates to task:<id> scope tokens on add
# ---------------------------------------------------------------------------


def test_depends_on_sugar_materializes_as_task_scope_token():
    """`session-task queue add ... --depends-on q-A` is parser-level
    sugar that appends `task:q-A` to scope. The on-disk record stores the
    dep as a scope token; the legacy `depends_on` field is no longer
    written.
    """
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)

        r_a = _add(env, "first", ["repo:a"])
        a = json.loads(r_a.stdout)
        r_b = _add(env, "second", ["repo:b"], "--depends-on", a["id"])
        b = json.loads(r_b.stdout)

        # The JSON output preserves the depends_on field for back-compat.
        assert b["depends_on"] == [a["id"]], b
        assert a["id"] in b["dep_blockers"], b

        # On disk, the dep is materialized as task:<id> in scope; the
        # legacy depends_on field is NOT written.
        qjson = json.loads(
            (Path(tmp) / ".config" / "session" / "queue.json").read_text()
        )
        b_record = next(it for it in qjson["items"] if it["id"] == b["id"])
        assert f"task:{a['id']}" in b_record["scope"], b_record
        assert "depends_on" not in b_record, b_record


# ---------------------------------------------------------------------------
# 16. Legacy depends_on field is migrated to task: scope tokens on read
# ---------------------------------------------------------------------------


def test_legacy_depends_on_field_migrated_on_read():
    """A queue.json written by an older session-task with `depends_on`
    on items is transparently migrated: on next read, each dep is
    materialized as a `task:<id>` scope token and the legacy field is
    dropped. Behavior remains identical.
    """
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)

        # Bootstrap two items, then hand-craft a legacy queue.json so we
        # can test the read-time migration.
        r_a = _add(env, "first", ["repo:a"])
        a = json.loads(r_a.stdout)
        r_b = _add(env, "second", ["repo:b"])
        b = json.loads(r_b.stdout)

        qpath = Path(tmp) / ".config" / "session" / "queue.json"
        qdata = json.loads(qpath.read_text())
        b_rec = next(it for it in qdata["items"] if it["id"] == b["id"])
        # Strip any task: tokens that may already be there (none, but be
        # explicit) and add a legacy depends_on field.
        b_rec["scope"] = [t for t in b_rec["scope"] if not t.startswith("task:")]
        b_rec["depends_on"] = [a["id"]]
        qpath.write_text(json.dumps(qdata, indent=2) + "\n")

        # First read should migrate. queue show on B exposes the dep via
        # depends_on (back-compat field) and surfaces it as a blocker.
        r_show = _run(env, "queue", "show", b["id"], check=True)
        b_show = json.loads(r_show.stdout)
        assert b_show["depends_on"] == [a["id"]], b_show
        assert a["id"] in b_show["dep_blockers"], b_show
        assert b_show["ready_now"] is False, b_show

        # Any write op (e.g. set-summary) persists the migration. Use
        # set-summary to trigger a write that doesn't change semantics.
        _run(env, "queue", "set-summary", b["id"], "post-migration", check=True)
        qdata2 = json.loads(qpath.read_text())
        b_rec2 = next(it for it in qdata2["items"] if it["id"] == b["id"])
        assert f"task:{a['id']}" in b_rec2["scope"], b_rec2
        assert "depends_on" not in b_rec2, b_rec2


# ---------------------------------------------------------------------------
# 17. queue depend --add stamps task: scope token (not legacy field)
# ---------------------------------------------------------------------------


def test_queue_depend_add_writes_task_scope_token():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)

        r_a = _add(env, "a", ["repo:a"])
        a = json.loads(r_a.stdout)
        r_b = _add(env, "b", ["repo:b"])
        b = json.loads(r_b.stdout)

        _run(env, "queue", "depend", b["id"], "--add", a["id"], check=True)

        # Check on-disk shape: task:<a-id> in scope, no depends_on field.
        qjson = json.loads(
            (Path(tmp) / ".config" / "session" / "queue.json").read_text()
        )
        b_record = next(it for it in qjson["items"] if it["id"] == b["id"])
        assert f"task:{a['id']}" in b_record["scope"], b_record
        assert "depends_on" not in b_record, b_record

        # show still surfaces depends_on field for back-compat.
        r_show = _run(env, "queue", "show", b["id"], check=True)
        b_show = json.loads(r_show.stdout)
        assert b_show["depends_on"] == [a["id"]], b_show


# ---------------------------------------------------------------------------
# 18. queue depend --clear strips only task: tokens (regular scope kept)
# ---------------------------------------------------------------------------


def test_queue_depend_clear_preserves_regular_scope():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)

        r_a = _add(env, "a", ["repo:a"])
        a = json.loads(r_a.stdout)
        r_b = _add(env, "b", ["repo:b", "resource:thing"], "--depends-on", a["id"])
        b = json.loads(r_b.stdout)
        assert any(t.startswith("task:") for t in b["scope"])

        _run(env, "queue", "depend", b["id"], "--clear", check=True)

        qjson = json.loads(
            (Path(tmp) / ".config" / "session" / "queue.json").read_text()
        )
        b_record = next(it for it in qjson["items"] if it["id"] == b["id"])
        # Regular tokens preserved, task: gone.
        assert "repo:b" in b_record["scope"]
        assert "resource:thing" in b_record["scope"]
        assert not any(t.startswith("task:") for t in b_record["scope"])


# ---------------------------------------------------------------------------
# 19. task: scope tokens do NOT trigger same-scope conflict on add
# ---------------------------------------------------------------------------


def test_task_scope_does_not_trigger_running_conflict():
    """Two items both carrying `task:q-A` should NOT collide as a
    running-scope conflict. task: is an inert dep marker, not a
    serialization key.
    """
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)

        # Long-running anchor never used as dep target -- just to keep A
        # alive.
        r_a = _add(env, "dep-target", ["repo:dep"])
        a = json.loads(r_a.stdout)

        # B depends on A and is running.
        r_b = _add(env, "first-dependent", ["repo:b", f"task:{a['id']}"])
        b = json.loads(r_b.stdout)
        # B can't actually run while A is pending, but we can manipulate
        # state directly via queue.json -- or alternatively, test that
        # add-ing C with the same task: token does NOT exit 3 even though
        # B is running. Force-running: register A, then register B.
        _run(env, "queue", "register", a["id"], check=True)
        _run(env, "queue", "done", a["id"], check=True)
        # B is now ready, register it.
        _run(env, "queue", "register", b["id"], check=True)

        # C carries task:<a-id> too, but a is done now -- so the dep is
        # satisfied. The point of this test is that B running with the
        # same task: token doesn't cause an exit-3 add conflict.
        r_c = _add(env, "second-dependent", ["repo:c", f"task:{a['id']}"])
        # Should succeed (rc=0), NOT exit 3.
        assert r_c.returncode == 0, r_c.stderr
        c = json.loads(r_c.stdout)
        # Different scope (repo:b vs repo:c) -> separate groups; B's task:
        # token isn't a conflict.
        assert c["group_id"] != b["group_id"]


if __name__ == "__main__":
    import traceback
    tests = [v for k, v in list(globals().items()) if k.startswith("test_")]
    failed = 0
    for t in tests:
        try:
            t()
            print(f"PASS {t.__name__}")
        except Exception:
            failed += 1
            print(f"FAIL {t.__name__}")
            traceback.print_exc()
    if failed:
        print(f"\n{failed}/{len(tests)} failed")
        sys.exit(1)
    print(f"\n{len(tests)}/{len(tests)} passed")
