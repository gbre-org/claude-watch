#!/usr/bin/env python3
"""Tests for the `wedged` queue lifecycle state.

Covers:

  * `queue wedge <id> --reason ...` from running -> wedged
  * `queue wedge` refuses without --reason
  * `queue wedge` refuses on non-running statuses (pending, wedged, done,
    abandoned)
  * `queue unwedge <id>` from wedged -> running, refreshes heartbeat,
    preserves wedged_at + wedged_reason as audit
  * `queue unwedge` refuses on non-wedged statuses
  * a wedged item still owns its scope (peer with overlapping scope is
    blocked)
  * `queue done` and `queue abandon` accept wedged items (terminal exit)
  * `queue list` (default view) includes wedged items
  * `queue list --json` emits status='wedged'
  * `queue groups` reports wedged_count
  * `queue register` refuses to re-claim a wedged item

All tests run against a temp HOME so the live ~/.config/session/queue.json
is never touched.

Run:
    uv run --python 3.11 --with pytest \\
        pytest tools/session-task/tests/test_queue_wedged.py -v
"""

import json
import os
import subprocess
import sys
import tempfile
from pathlib import Path

SESSION_TASK = Path(__file__).resolve().parent.parent / "session-task"


def _env_for_tmp(tmp):
    env = os.environ.copy()
    env["HOME"] = tmp
    env["PINGME_SESSION_TASK"] = "0"
    env["CLAUDE_EVENT_SESSION_TASK"] = "0"
    return env


def _run(env, *argv, expect_exit=None):
    cmd = [sys.executable, str(SESSION_TASK)] + list(argv)
    r = subprocess.run(cmd, capture_output=True, text=True, env=env, timeout=15)
    if expect_exit is not None and r.returncode != expect_exit:
        raise RuntimeError(
            f"expected exit {expect_exit} got {r.returncode}: argv={argv}\n"
            f"stdout={r.stdout!r}\nstderr={r.stderr!r}"
        )
    return r


def _add(env, desc, scopes, *extra):
    args = ["queue", "add", desc, "--json"]
    for s in scopes:
        args.extend(["--scope", s])
    args.extend(extra)
    r = _run(env, *args, expect_exit=0)
    return json.loads(r.stdout)


def _show(env, qid):
    r = _run(env, "queue", "show", qid, expect_exit=0)
    return json.loads(r.stdout)


def _register(env, qid):
    r = _run(env, "queue", "register", qid, "--json", expect_exit=0)
    return json.loads(r.stdout)


# -------------------- wedge --------------------


def test_wedge_running_to_wedged():
    """A running item flips to wedged with reason + timestamp recorded."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        added = _add(env, "wedge me", ["repo:wedge-a"], "--summary", "wedge")
        qid = added["id"]
        _register(env, qid)

        r = _run(env, "queue", "wedge", qid, "--reason", "context_limit",
                 "--silent", "--json", expect_exit=0)
        out = json.loads(r.stdout)
        assert out["status"] == "wedged"
        assert out["wedged_reason"] == "context_limit"
        assert out["wedged_at"]  # non-empty timestamp

        shown = _show(env, qid)
        assert shown["status"] == "wedged"
        assert shown["wedged_reason"] == "context_limit"


def test_wedge_requires_reason():
    """`queue wedge` without --reason exits non-zero."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        added = _add(env, "no reason", ["repo:wedge-noreason"],
                     "--summary", "x")
        qid = added["id"]
        _register(env, qid)
        # argparse handles required=True; exit code 2 from argparse on
        # missing required arg.
        r = _run(env, "queue", "wedge", qid, "--silent")
        assert r.returncode != 0, (r.stdout, r.stderr)


def test_wedge_empty_reason_refused():
    """`queue wedge --reason ''` (empty/whitespace) is refused."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        added = _add(env, "empty reason", ["repo:wedge-empty"],
                     "--summary", "x")
        qid = added["id"]
        _register(env, qid)
        r = _run(env, "queue", "wedge", qid, "--reason", "   ",
                 "--silent")
        assert r.returncode != 0
        assert "reason is required" in r.stderr.lower()


def test_wedge_refused_on_pending():
    """A pending (unregistered) item cannot be wedged."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        added = _add(env, "pending", ["repo:wedge-pending"],
                     "--summary", "p")
        qid = added["id"]
        # Note: NOT registered.
        r = _run(env, "queue", "wedge", qid, "--reason", "context_limit",
                 "--silent")
        assert r.returncode != 0
        assert "must be running" in r.stderr.lower()
        # Status unchanged.
        shown = _show(env, qid)
        assert shown["status"] == "pending"


def test_wedge_refused_on_already_wedged():
    """Wedging an already-wedged item exits non-zero (must transition first)."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        added = _add(env, "double-wedge", ["repo:wedge-dbl"],
                     "--summary", "d")
        qid = added["id"]
        _register(env, qid)
        _run(env, "queue", "wedge", qid, "--reason", "context_limit",
             "--silent", expect_exit=0)
        # Second wedge: refused.
        r = _run(env, "queue", "wedge", qid, "--reason", "again",
                 "--silent")
        assert r.returncode != 0
        assert "must be running" in r.stderr.lower()


def test_wedge_refused_on_done():
    """A done item cannot be wedged."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        added = _add(env, "done first", ["repo:wedge-done"],
                     "--summary", "d")
        qid = added["id"]
        _register(env, qid)
        _run(env, "queue", "done", qid, "--silent", expect_exit=0)
        r = _run(env, "queue", "wedge", qid, "--reason", "x",
                 "--silent")
        assert r.returncode != 0


# -------------------- unwedge --------------------


def test_unwedge_wedged_to_running():
    """`queue unwedge` flips wedged -> running and refreshes heartbeat."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        added = _add(env, "recover", ["repo:wedge-rec"], "--summary", "r")
        qid = added["id"]
        _register(env, qid)
        _run(env, "queue", "wedge", qid, "--reason", "context_limit",
             "--silent", expect_exit=0)

        # Confirm wedged state before recovery.
        before = _show(env, qid)
        assert before["status"] == "wedged"

        r = _run(env, "queue", "unwedge", qid, "--silent", "--json",
                 expect_exit=0)
        out = json.loads(r.stdout)
        assert out["status"] == "running"
        assert out["unwedged_at"]
        # Heartbeat refreshed by unwedge so the exporter doesn't immediately
        # re-flag the item as stale.
        assert out["last_heartbeat_at"] == out["unwedged_at"]

        # Audit: wedged_at + wedged_reason preserved on the row.
        shown = _show(env, qid)
        assert shown["status"] == "running"
        assert shown["wedged_reason"] == "context_limit"
        assert shown["wedged_at"]
        assert shown["unwedged_at"]


def test_unwedge_refused_on_running():
    """Cannot unwedge a healthy running item."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        added = _add(env, "healthy", ["repo:wedge-healthy"],
                     "--summary", "h")
        qid = added["id"]
        _register(env, qid)
        r = _run(env, "queue", "unwedge", qid, "--silent")
        assert r.returncode != 0
        assert "must be wedged" in r.stderr.lower()


def test_unwedge_refused_on_pending():
    """Cannot unwedge an unregistered item."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        added = _add(env, "pending", ["repo:wedge-uw-pend"],
                     "--summary", "x")
        qid = added["id"]
        r = _run(env, "queue", "unwedge", qid, "--silent")
        assert r.returncode != 0


# -------------------- scope ownership --------------------


def test_wedged_item_still_owns_scope():
    """A wedged item blocks a peer item with overlapping scope from spawning.

    Pre-2026-05-19 this hard-failed (exit 3). Now soft-serializes: peer is
    enqueued, ready_now=false, serialized_after points at the wedged owner.
    """
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        a = _add(env, "first", ["repo:scope-share"], "--summary", "a")
        a_id = a["id"]
        _register(env, a_id)
        _run(env, "queue", "wedge", a_id, "--reason", "context_limit",
             "--silent", expect_exit=0)

        # Adding a peer with the same scope soft-serializes (exit 0,
        # blocked for spawn) since wedged items still own their scope.
        r = _run(env, "queue", "add", "second", "--scope", "repo:scope-share",
                 "--summary", "b", "--json")
        assert r.returncode == 0, (
            f"expected exit 0 (soft-serialize behind wedged peer), got "
            f"{r.returncode}: stderr={r.stderr!r}"
        )
        d = json.loads(r.stdout)
        assert d["ready_now"] is False, d
        assert a_id in d["serialized_after"], d


def test_wedged_item_blocks_spawn_check_of_pending_peer():
    """spawn-check on a pending peer of a wedged owner reports blocked."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        # Disjoint scope at add-time so the second item lands as pending
        # rather than being hard-refused. Then we force a scope overlap by
        # adding the conflicting scope manually -- nope, actually queue add
        # has no post-creation scope edit. Instead, add A and B to the SAME
        # scope but at-add B will be queued (pending) since A is still
        # pending too (no register yet).
        a = _add(env, "first", ["repo:spawn-block"], "--summary", "a")
        b = _add(env, "second", ["repo:spawn-block"], "--summary", "b")
        # B is in the same group, blocked by A as group head.
        assert a["group_id"] == b["group_id"]
        _register(env, a["id"])
        _run(env, "queue", "wedge", a["id"], "--reason", "x", "--silent",
             expect_exit=0)
        # spawn-check on B must report blocked.
        r = _run(env, "queue", "spawn-check", b["id"], "--json")
        assert r.returncode != 0, (
            "spawn-check on pending peer of wedged owner must refuse"
        )


# -------------------- terminal transitions from wedged --------------------


def test_done_from_wedged():
    """`queue done` accepts a wedged item (operator marks complete)."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        added = _add(env, "wedge then done", ["repo:wedge-done2"],
                     "--summary", "wd")
        qid = added["id"]
        _register(env, qid)
        _run(env, "queue", "wedge", qid, "--reason", "context_limit",
             "--silent", expect_exit=0)
        _run(env, "queue", "done", qid, "--silent", expect_exit=0)
        shown = _show(env, qid)
        assert shown["status"] == "done"
        # Audit fields preserved.
        assert shown.get("wedged_reason") == "context_limit"


def test_abandon_from_wedged():
    """`queue abandon` accepts a wedged item (operator gives up)."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        added = _add(env, "give up", ["repo:wedge-abandon"],
                     "--summary", "g")
        qid = added["id"]
        _register(env, qid)
        _run(env, "queue", "wedge", qid, "--reason", "context_limit",
             "--silent", expect_exit=0)
        _run(env, "queue", "abandon", qid, "--reason",
             "operator gave up after wedge", "--silent", expect_exit=0)
        shown = _show(env, qid)
        assert shown["status"] == "abandoned"
        assert shown["abandon_reason"] == "operator gave up after wedge"
        assert shown.get("wedged_reason") == "context_limit"


# -------------------- list / show / groups visibility --------------------


def test_queue_list_default_includes_wedged():
    """Default `queue list` (no --all) shows wedged items."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        added = _add(env, "show me", ["repo:wedge-list"], "--summary", "s")
        qid = added["id"]
        _register(env, qid)
        _run(env, "queue", "wedge", qid, "--reason", "context_limit",
             "--silent", expect_exit=0)
        r = _run(env, "queue", "list", "--json", expect_exit=0)
        items = json.loads(r.stdout)
        ids = [it["id"] for it in items]
        assert qid in ids, (
            f"wedged item must appear in default queue list; got ids={ids}"
        )
        # Status string is verbatim 'wedged' for downstream consumers.
        wedged = [it for it in items if it["id"] == qid][0]
        assert wedged["status"] == "wedged"
        assert wedged["wedged_reason"] == "context_limit"


def test_queue_groups_reports_wedged_count():
    """`queue groups --json` includes wedged_count per group."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        a = _add(env, "wedge", ["repo:wedge-grp"], "--summary", "a")
        _register(env, a["id"])
        _run(env, "queue", "wedge", a["id"], "--reason", "x", "--silent",
             expect_exit=0)
        r = _run(env, "queue", "groups", "--json", expect_exit=0)
        groups = json.loads(r.stdout)
        assert len(groups) == 1
        g = groups[0]
        assert g["wedged_count"] == 1
        assert g["running_count"] == 0
        assert g["pending_count"] == 0


# -------------------- register / re-spawn --------------------


def test_register_refused_on_wedged():
    """`queue register` refuses a wedged item -- must unwedge or abandon first."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        added = _add(env, "no double", ["repo:wedge-rereg"],
                     "--summary", "x")
        qid = added["id"]
        _register(env, qid)
        _run(env, "queue", "wedge", qid, "--reason", "context_limit",
             "--silent", expect_exit=0)
        r = _run(env, "queue", "register", qid)
        assert r.returncode == 2, (r.stdout, r.stderr)
        assert "WEDGED" in r.stderr
