#!/usr/bin/env python3
"""Tests for the `blocked` queue lifecycle state.

Covers:

  * `queue block <id> --reason ...` from running -> blocked
  * `queue block` refuses without --reason
  * `queue block` refuses on non-running statuses (pending, wedged,
    blocked, done, abandoned)
  * `queue unblock <id>` from blocked -> running, refreshes heartbeat,
    preserves blocked_at + block_reason as audit
  * `queue unblock` refuses on non-blocked statuses
  * a blocked item RELEASES its scope (peer with overlapping scope
    becomes ready -- blocked is not in-flight; #371)
  * `queue done` and `queue abandon` accept blocked items (terminal
    exit)
  * `queue list` (default view) includes blocked items
  * `queue list --json` emits status='blocked'
  * `queue groups` reports blocked_count
  * `queue register` refuses to re-claim a blocked item

State-machine coherence: only running -> blocked is allowed.
pending -> blocked and wedged -> blocked are explicitly REJECTED so the
state graph stays simple. pending items use `queue lock <scope>` for
real-world-condition holds; wedged items must first `unwedge` (back to
running) before they can transition to blocked.

All tests run against a temp HOME so the live ~/.config/session/queue.json
is never touched.

Run:
    uv run --python 3.11 --with pytest \\
        pytest tools/session-task/tests/test_queue_blocked.py -v
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


# -------------------- block --------------------


def test_block_running_to_blocked():
    """A running item flips to blocked with reason + timestamp recorded."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        added = _add(env, "block me", ["repo:block-a"], "--summary", "block")
        qid = added["id"]
        _register(env, qid)

        r = _run(env, "queue", "block", qid,
                 "--reason", "awaiting human greenlight",
                 "--silent", "--json", expect_exit=0)
        out = json.loads(r.stdout)
        assert out["status"] == "blocked"
        assert out["block_reason"] == "awaiting human greenlight"
        assert out["blocked_at"]

        shown = _show(env, qid)
        assert shown["status"] == "blocked"
        assert shown["block_reason"] == "awaiting human greenlight"


def test_block_requires_reason():
    """`queue block` without --reason exits non-zero (argparse required)."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        added = _add(env, "no reason", ["repo:block-noreason"],
                     "--summary", "x")
        qid = added["id"]
        _register(env, qid)
        r = _run(env, "queue", "block", qid, "--silent")
        assert r.returncode != 0, (r.stdout, r.stderr)


def test_block_empty_reason_refused():
    """`queue block --reason ''` (empty/whitespace) is refused."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        added = _add(env, "empty reason", ["repo:block-empty"],
                     "--summary", "x")
        qid = added["id"]
        _register(env, qid)
        r = _run(env, "queue", "block", qid, "--reason", "   ", "--silent")
        assert r.returncode != 0
        assert "reason is required" in r.stderr.lower()


def test_block_refused_on_pending():
    """A pending (unregistered) item cannot be blocked."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        added = _add(env, "pending", ["repo:block-pending"], "--summary", "p")
        qid = added["id"]
        # NOT registered.
        r = _run(env, "queue", "block", qid, "--reason", "x", "--silent")
        assert r.returncode != 0
        assert "must be running" in r.stderr.lower()
        shown = _show(env, qid)
        assert shown["status"] == "pending"


def test_block_refused_on_wedged():
    """A wedged item must be unwedged before it can be blocked.

    State-machine coherence: wedged -> blocked is rejected. The operator
    must first `unwedge` (back to running) and then `block`, so the
    wedge resolution is explicit and audited.
    """
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        added = _add(env, "wedged first", ["repo:block-wedged"],
                     "--summary", "w")
        qid = added["id"]
        _register(env, qid)
        _run(env, "queue", "wedge", qid, "--reason", "context_limit",
             "--silent", expect_exit=0)
        r = _run(env, "queue", "block", qid, "--reason", "x", "--silent")
        assert r.returncode != 0
        assert "must be running" in r.stderr.lower()
        shown = _show(env, qid)
        assert shown["status"] == "wedged"


def test_block_refused_on_already_blocked():
    """Blocking an already-blocked item exits non-zero."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        added = _add(env, "double", ["repo:block-dbl"], "--summary", "d")
        qid = added["id"]
        _register(env, qid)
        _run(env, "queue", "block", qid, "--reason", "first",
             "--silent", expect_exit=0)
        r = _run(env, "queue", "block", qid, "--reason", "second", "--silent")
        assert r.returncode != 0
        assert "must be running" in r.stderr.lower()


def test_block_refused_on_done():
    """A done item cannot be blocked."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        added = _add(env, "done first", ["repo:block-done"], "--summary", "d")
        qid = added["id"]
        _register(env, qid)
        _run(env, "queue", "done", qid, "--silent", expect_exit=0)
        r = _run(env, "queue", "block", qid, "--reason", "x", "--silent")
        assert r.returncode != 0


# -------------------- set-block-reason (in-place reason edit) --------------------


def test_set_block_reason_updates_in_place():
    """`set-block-reason` on a blocked item refreshes block_reason in place,
    preserves blocked_at + status, and stamps block_reason_updated_at."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        added = _add(env, "evolving block", ["repo:setbr-update"],
                     "--summary", "sbr")
        qid = added["id"]
        _register(env, qid)
        _run(env, "queue", "block", qid, "--reason", "awaiting build #140",
             "--silent", expect_exit=0)
        before = _show(env, qid)
        orig_blocked_at = before["blocked_at"]
        assert orig_blocked_at

        r = _run(env, "queue", "set-block-reason", qid,
                 "--reason", "build #140 landed; awaiting #141 verdict",
                 "--json", expect_exit=0)
        out = json.loads(r.stdout)
        assert out["status"] == "blocked"
        assert out["block_reason"] == "build #140 landed; awaiting #141 verdict"
        # blocked_at preserved (audit history intact).
        assert out["blocked_at"] == orig_blocked_at
        assert out["block_reason_updated_at"]

        shown = _show(env, qid)
        assert shown["status"] == "blocked"
        assert shown["block_reason"] == "build #140 landed; awaiting #141 verdict"
        assert shown["blocked_at"] == orig_blocked_at
        assert shown["block_reason_updated_at"]


def test_set_block_reason_requires_nonempty_reason():
    """`set-block-reason --reason ''` (empty/whitespace) is refused."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        added = _add(env, "empty", ["repo:setbr-empty"], "--summary", "x")
        qid = added["id"]
        _register(env, qid)
        _run(env, "queue", "block", qid, "--reason", "first", "--silent",
             expect_exit=0)
        r = _run(env, "queue", "set-block-reason", qid, "--reason", "   ")
        assert r.returncode != 0
        assert "reason is required" in r.stderr.lower()
        # Original reason untouched.
        shown = _show(env, qid)
        assert shown["block_reason"] == "first"


def test_set_block_reason_refused_on_running():
    """A running (non-blocked) item refuses set-block-reason -- use `block`."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        added = _add(env, "running", ["repo:setbr-running"], "--summary", "x")
        qid = added["id"]
        _register(env, qid)
        r = _run(env, "queue", "set-block-reason", qid, "--reason", "x")
        assert r.returncode != 0
        assert "must be blocked" in r.stderr.lower()
        shown = _show(env, qid)
        assert shown["status"] == "running"
        assert "block_reason" not in shown or not shown.get("block_reason")


def test_set_block_reason_refused_on_pending():
    """A pending (unregistered) item refuses set-block-reason."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        added = _add(env, "pending", ["repo:setbr-pending"], "--summary", "x")
        qid = added["id"]
        r = _run(env, "queue", "set-block-reason", qid, "--reason", "x")
        assert r.returncode != 0
        assert "must be blocked" in r.stderr.lower()


def test_set_block_reason_refused_on_done():
    """A done (terminal) item refuses set-block-reason."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        added = _add(env, "done", ["repo:setbr-done"], "--summary", "x")
        qid = added["id"]
        _register(env, qid)
        _run(env, "queue", "done", qid, "--silent", expect_exit=0)
        r = _run(env, "queue", "set-block-reason", qid, "--reason", "x")
        assert r.returncode != 0
        assert "must be blocked" in r.stderr.lower()


def test_set_block_reason_refused_on_abandoned():
    """An abandoned (terminal) item refuses set-block-reason."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        added = _add(env, "abandon", ["repo:setbr-abandon"], "--summary", "x")
        qid = added["id"]
        _register(env, qid)
        _run(env, "queue", "abandon", qid, "--reason", "nope", "--silent",
             expect_exit=0)
        r = _run(env, "queue", "set-block-reason", qid, "--reason", "x")
        assert r.returncode != 0
        assert "must be blocked" in r.stderr.lower()


def test_block_on_running_still_transitions_regression():
    """REGRESSION GUARD: `block --reason` on a RUNNING item still works
    (running -> blocked) -- the new set-block-reason subcommand did not
    alter the block state machine."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        added = _add(env, "still blocks", ["repo:setbr-regress"],
                     "--summary", "x")
        qid = added["id"]
        _register(env, qid)
        r = _run(env, "queue", "block", qid, "--reason", "ext blocker",
                 "--silent", "--json", expect_exit=0)
        out = json.loads(r.stdout)
        assert out["status"] == "blocked"
        assert out["block_reason"] == "ext blocker"
        assert out["blocked_at"]
        # And block is still refused on an already-blocked item (unchanged).
        r2 = _run(env, "queue", "block", qid, "--reason", "second", "--silent")
        assert r2.returncode != 0
        assert "must be running" in r2.stderr.lower()


# -------------------- unblock --------------------


def test_unblock_blocked_to_running():
    """`queue unblock` flips blocked -> running and refreshes heartbeat."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        added = _add(env, "recover", ["repo:block-rec"], "--summary", "r")
        qid = added["id"]
        _register(env, qid)
        _run(env, "queue", "block", qid, "--reason", "awaiting greenlight",
             "--silent", expect_exit=0)

        before = _show(env, qid)
        assert before["status"] == "blocked"

        r = _run(env, "queue", "unblock", qid, "--silent", "--json",
                 expect_exit=0)
        out = json.loads(r.stdout)
        assert out["status"] == "running"
        assert out["unblocked_at"]
        # Heartbeat refreshed so exporter doesn't immediately re-flag stale.
        assert out["last_heartbeat_at"] == out["unblocked_at"]

        # Audit: blocked_at + block_reason preserved on the row.
        shown = _show(env, qid)
        assert shown["status"] == "running"
        assert shown["block_reason"] == "awaiting greenlight"
        assert shown["blocked_at"]
        assert shown["unblocked_at"]


def test_unblock_refused_on_running():
    """Cannot unblock a healthy running item."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        added = _add(env, "healthy", ["repo:block-healthy"], "--summary", "h")
        qid = added["id"]
        _register(env, qid)
        r = _run(env, "queue", "unblock", qid, "--silent")
        assert r.returncode != 0
        assert "must be blocked" in r.stderr.lower()


def test_unblock_refused_on_pending():
    """Cannot unblock an unregistered item."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        added = _add(env, "pending", ["repo:block-uw-pend"], "--summary", "x")
        qid = added["id"]
        r = _run(env, "queue", "unblock", qid, "--silent")
        assert r.returncode != 0


def test_unblock_refused_on_wedged():
    """Cannot unblock a wedged item (it's not blocked)."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        added = _add(env, "wedged", ["repo:block-uw-wedge"], "--summary", "x")
        qid = added["id"]
        _register(env, qid)
        _run(env, "queue", "wedge", qid, "--reason", "context_limit",
             "--silent", expect_exit=0)
        r = _run(env, "queue", "unblock", qid, "--silent")
        assert r.returncode != 0
        assert "must be blocked" in r.stderr.lower()


# -------------------- scope ownership (blocked RELEASES scope) --------------------


def test_blocked_item_releases_scope():
    """A blocked item RELEASES its scope -- a peer with overlapping scope
    becomes ready and may spawn.

    Per design (#371), `queue block` releases the scope lock: blocked items
    are EXEMPT from the orphaned-running alert because they hold no live
    slot, so a pending peer in the same scope/group must be free to become
    ready and spawn rather than be starved behind the external blocker.
    This matches the proven-correct web UI `_compute_ready_now`, which gates
    readiness on ("pending","running") membership only -- blocked (like a
    terminal item) does not count as in-flight.
    """
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        a = _add(env, "first", ["repo:bscope-share"], "--summary", "a")
        a_id = a["id"]
        _register(env, a_id)
        _run(env, "queue", "block", a_id, "--reason", "ext blocker",
             "--silent", expect_exit=0)

        # Adding a peer with the same scope succeeds and is READY: the blocked
        # owner no longer holds the scope (it is not in serialized_after, and
        # there is no running_scope_conflict), so the peer can spawn now.
        r = _run(env, "queue", "add", "second", "--scope",
                 "repo:bscope-share", "--summary", "b", "--json")
        assert r.returncode == 0, (
            f"expected exit 0 (blocked peer releases scope), got "
            f"{r.returncode}: stderr={r.stderr!r}"
        )
        d = json.loads(r.stdout)
        assert d["ready_now"] is True, d
        assert a_id not in d["serialized_after"], d
        # The blocked owner is not a running conflict either.
        assert a_id not in [
            c.get("id") for c in d.get("running_scope_conflicts", [])
        ], d


def test_blocked_owner_does_not_block_spawn_check_of_pending_peer():
    """spawn-check on a pending peer of a blocked owner REPORTS READY.

    Counterpart to `test_blocked_item_releases_scope` at the spawn-check
    surface: because `queue block` releases the scope lock (#371), the
    blocked owner does not gate its pending peer -- the peer is the live
    group head and spawn-check approves it (exit 0, ok=true).
    """
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        a = _add(env, "first", ["repo:bspawn-block"], "--summary", "a")
        b = _add(env, "second", ["repo:bspawn-block"], "--summary", "b")
        assert a["group_id"] == b["group_id"]
        _register(env, a["id"])
        _run(env, "queue", "block", a["id"], "--reason", "x", "--silent",
             expect_exit=0)
        r = _run(env, "queue", "spawn-check", b["id"], "--json", expect_exit=0)
        out = json.loads(r.stdout)
        assert out["ok"] is True, out
        assert out["status"] == "pending", out


# -------------------- terminal transitions from blocked --------------------


def test_done_from_blocked():
    """`queue done` accepts a blocked item (work was actually complete)."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        added = _add(env, "block then done", ["repo:block-done2"],
                     "--summary", "bd")
        qid = added["id"]
        _register(env, qid)
        _run(env, "queue", "block", qid, "--reason", "awaiting greenlight",
             "--silent", expect_exit=0)
        _run(env, "queue", "done", qid, "--silent", expect_exit=0)
        shown = _show(env, qid)
        assert shown["status"] == "done"
        # Audit fields preserved.
        assert shown.get("block_reason") == "awaiting greenlight"


def test_abandon_from_blocked():
    """`queue abandon` accepts a blocked item."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        added = _add(env, "give up", ["repo:block-abandon"], "--summary", "g")
        qid = added["id"]
        _register(env, qid)
        _run(env, "queue", "block", qid, "--reason", "awaiting greenlight",
             "--silent", expect_exit=0)
        _run(env, "queue", "abandon", qid, "--reason",
             "operator gave up after block", "--silent", expect_exit=0)
        shown = _show(env, qid)
        assert shown["status"] == "abandoned"
        assert shown["abandon_reason"] == "operator gave up after block"
        assert shown.get("block_reason") == "awaiting greenlight"


# -------------------- list / show / groups visibility --------------------


def test_queue_list_default_includes_blocked():
    """Default `queue list` (no --all) shows blocked items."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        added = _add(env, "show me", ["repo:block-list"], "--summary", "s")
        qid = added["id"]
        _register(env, qid)
        _run(env, "queue", "block", qid, "--reason", "ext",
             "--silent", expect_exit=0)
        r = _run(env, "queue", "list", "--json", expect_exit=0)
        items = json.loads(r.stdout)
        ids = [it["id"] for it in items]
        assert qid in ids, (
            f"blocked item must appear in default queue list; got ids={ids}"
        )
        blocked = [it for it in items if it["id"] == qid][0]
        assert blocked["status"] == "blocked"
        assert blocked["block_reason"] == "ext"


def test_queue_groups_reports_blocked_count():
    """`queue groups --json` includes blocked_count per group."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        a = _add(env, "block", ["repo:block-grp"], "--summary", "a")
        _register(env, a["id"])
        _run(env, "queue", "block", a["id"], "--reason", "x", "--silent",
             expect_exit=0)
        r = _run(env, "queue", "groups", "--json", expect_exit=0)
        groups = json.loads(r.stdout)
        assert len(groups) == 1
        g = groups[0]
        assert g["blocked_count"] == 1
        assert g["running_count"] == 0
        assert g["pending_count"] == 0
        assert g["wedged_count"] == 0


# -------------------- register / re-spawn --------------------


def test_register_refused_on_blocked():
    """`queue register` refuses a blocked item -- must unblock or abandon first."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        added = _add(env, "no double", ["repo:block-rereg"], "--summary", "x")
        qid = added["id"]
        _register(env, qid)
        _run(env, "queue", "block", qid, "--reason", "ext", "--silent",
             expect_exit=0)
        r = _run(env, "queue", "register", qid)
        assert r.returncode == 2, (r.stdout, r.stderr)
        assert "BLOCKED" in r.stderr


# -------------------- round-trip lifecycle --------------------


def test_block_unblock_done_round_trip():
    """End-to-end: pending -> running -> blocked -> running -> done."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        added = _add(env, "round-trip", ["repo:block-rt"], "--summary", "rt")
        qid = added["id"]

        # pending -> running
        _register(env, qid)
        assert _show(env, qid)["status"] == "running"

        # running -> blocked
        _run(env, "queue", "block", qid, "--reason", "awaiting greenlight",
             "--silent", expect_exit=0)
        assert _show(env, qid)["status"] == "blocked"

        # blocked -> running
        _run(env, "queue", "unblock", qid, "--silent", expect_exit=0)
        assert _show(env, qid)["status"] == "running"

        # running -> done
        _run(env, "queue", "done", qid, "--silent", expect_exit=0)
        shown = _show(env, qid)
        assert shown["status"] == "done"
        # Full audit trail preserved.
        assert shown["block_reason"] == "awaiting greenlight"
        assert shown["blocked_at"]
        assert shown["unblocked_at"]
