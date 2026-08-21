#!/usr/bin/env python3
"""Tests for `session-task queue force-start`.

Covers:
  * happy path: pending+blocked item promoted to running, scope-conflict
    blockers ignored, audit log written, JSON output shape correct.
  * refuse: item already running -> exit 1, descriptive stderr.
  * refuse: --reason omitted -> argparse exit 2.
  * refuse: empty --reason -> exit 1.
  * refuse: id not found -> exit 1.
  * audit log: row appended to QUEUE_FORCE_START_LOG with reason +
    overridden blockers.
  * claude-event emit: queue-running event written with force_started=true.

Run::

    uv run --python 3.11 --with pytest \\
        pytest tools/session-task/tests/test_queue_force_start.py -v

Or directly:
    python3 tools/session-task/tests/test_queue_force_start.py
"""

import json
import os
import subprocess
import sys
import tempfile
from pathlib import Path

SESSION_TASK = Path(__file__).resolve().parent.parent / "session-task"


def _force_start_leaf(ob):
    """Return the `force_started_unspawned` leaf predicate of an obligation
    row, or None.

    force-start registers the leaf WRAPPED as
    ``all_of [is_main_loop, force_started_unspawned]`` (main-loop-only
    scope); accept both the wrapped and a bare row so the assertions read
    the same either way.
    """
    pred = ob.get("predicate", {}) or {}
    if pred.get("kind") == "force_started_unspawned":
        return pred
    if pred.get("kind") == "all_of":
        for child in (pred.get("params", {}) or {}).get("predicates", []):
            if (isinstance(child, dict)
                    and child.get("kind") == "force_started_unspawned"):
                return child
    return None


def _force_start_obligations_for(ob_data, qid):
    return [
        ob for ob in ob_data.get("obligations", [])
        if (_force_start_leaf(ob) or {}).get("params", {}).get("queue_id")
        == qid
    ]


def _run_obligations_gate_hook(hook_path, env, payload):
    """Run the pre-tool-obligations-gate-hook on ``payload``; return the
    parsed decision dict (empty dict == allow)."""
    proc = subprocess.run(
        [sys.executable, str(hook_path)],
        input=json.dumps(payload),
        capture_output=True, text=True, env=env, timeout=10,
    )
    assert proc.returncode == 0, proc.stderr
    try:
        return json.loads(proc.stdout) if proc.stdout.strip() else {}
    except json.JSONDecodeError:
        return {}


def _decision(decision):
    return (decision.get("hookSpecificOutput", {}) or {}).get(
        "permissionDecision"
    )


def _env_for_tmp(tmp):
    env = dict(os.environ)
    env["HOME"] = str(tmp)
    Path(tmp, ".config/session").mkdir(parents=True, exist_ok=True)
    Path(tmp, ".config/claude").mkdir(parents=True, exist_ok=True)
    Path(tmp, "claude-events").mkdir(parents=True, exist_ok=True)
    # Force the audit log into the temp HOME so each test is isolated.
    env["QUEUE_FORCE_START_LOG"] = str(
        Path(tmp) / ".config" / "claude" / "queue-force-start.log"
    )
    # Force the per-force-start recovery bundle dir into temp too.
    env["FORCE_START_BUNDLE_DIR"] = str(
        Path(tmp) / ".config" / "session-task" / "force-start-bundles"
    )
    # Disable pingme noise during tests.
    env["PINGME_DISABLE"] = "1"
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


def _register(env, qid, *extra):
    return _run(env, "queue", "register", qid, *extra)


def _show(env, qid):
    r = _run(env, "queue", "show", qid, check=True)
    return json.loads(r.stdout)


# ---------------------------------------------------------------------------
# 1. Happy path: blocked-pending promoted, audit log written
# ---------------------------------------------------------------------------


def test_force_start_promotes_blocked_pending():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)

        # Establish a running item that blocks scope:foo.
        r1 = _add(env, "running", ["scope:foo"])
        d1 = json.loads(r1.stdout)
        assert _register(env, d1["id"], "--json").returncode == 0

        # Force-enqueue a blocked-pending sibling.
        r2 = _add(env, "blocked", ["scope:foo"], "--force-enqueue")
        d2 = json.loads(r2.stdout)
        assert d2["ready_now"] is False, "expected blocked-pending state"

        # spawn-check refuses (sanity)
        rc = _run(env, "queue", "spawn-check", d2["id"])
        assert rc.returncode == 2, rc.stderr

        # Force-start the blocked item.
        rs = _run(
            env, "queue", "force-start", d2["id"],
            "--reason", "operator decided", "--json",
        )
        assert rs.returncode == 0, f"force-start failed: {rs.stderr}"
        promoted = json.loads(rs.stdout)
        assert promoted["status"] == "running"
        assert promoted["force_started_reason"] == "operator decided"
        assert "force_started_at" in promoted
        assert isinstance(promoted["force_started_at"], int)
        # The original running item should appear in the overridden-blockers
        # list (cross-scope overlap).
        overridden_ids = [
            b["id"] for b in promoted["force_started_blockers_overridden"]
        ]
        assert d1["id"] in overridden_ids, (
            f"expected {d1['id']} in overridden blockers, got {overridden_ids}"
        )

        # Re-read via `queue show` to confirm persistence.
        shown = _show(env, d2["id"])
        assert shown["status"] == "running"
        assert shown["force_started_reason"] == "operator decided"


# ---------------------------------------------------------------------------
# 2. Refuse: item already running
# ---------------------------------------------------------------------------


def test_force_start_refuses_already_running():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        r1 = _add(env, "first", ["scope:foo"])
        d1 = json.loads(r1.stdout)
        _register(env, d1["id"], "--json")

        rc = _run(
            env, "queue", "force-start", d1["id"],
            "--reason", "trying anyway",
        )
        assert rc.returncode == 1, (
            f"expected exit 1 on already-running, got {rc.returncode}\n"
            f"stderr: {rc.stderr}"
        )
        assert "must be pending" in rc.stderr, rc.stderr


# ---------------------------------------------------------------------------
# 3. Refuse: --reason omitted (argparse hard-fails)
# ---------------------------------------------------------------------------


def test_force_start_refuses_no_reason():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        r1 = _add(env, "blocked", ["scope:bar"])
        d1 = json.loads(r1.stdout)

        # No --reason at all -- argparse rejects with exit 2.
        rc = _run(env, "queue", "force-start", d1["id"])
        assert rc.returncode == 2, rc.stderr
        assert "reason" in (rc.stderr.lower() + rc.stdout.lower())


def test_force_start_refuses_empty_reason():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        r1 = _add(env, "blocked", ["scope:bar"])
        d1 = json.loads(r1.stdout)

        # Whitespace-only --reason -- our own check, exit 1.
        rc = _run(
            env, "queue", "force-start", d1["id"], "--reason", "   ",
        )
        assert rc.returncode == 1, rc.stderr
        assert "reason" in rc.stderr.lower()


# ---------------------------------------------------------------------------
# 4. Refuse: id not found
# ---------------------------------------------------------------------------


def test_force_start_refuses_not_found():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        rc = _run(
            env, "queue", "force-start", "q-does-not-exist",
            "--reason", "ghost",
        )
        assert rc.returncode == 1, rc.stderr
        assert "not found" in rc.stderr.lower()


# ---------------------------------------------------------------------------
# 5. Audit log row written
# ---------------------------------------------------------------------------


def test_force_start_writes_audit_log():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        log_path = Path(env["QUEUE_FORCE_START_LOG"])

        r1 = _add(env, "blocker", ["scope:bar"])
        d1 = json.loads(r1.stdout)
        _register(env, d1["id"], "--json")
        r2 = _add(env, "blocked", ["scope:bar"], "--force-enqueue")
        d2 = json.loads(r2.stdout)

        rs = _run(
            env, "queue", "force-start", d2["id"],
            "--reason", "audit-test", "--json",
        )
        assert rs.returncode == 0, rs.stderr

        assert log_path.exists(), "audit log file not created"
        rows = [
            json.loads(line)
            for line in log_path.read_text().splitlines() if line.strip()
        ]
        assert len(rows) == 1, rows
        row = rows[0]
        assert row["queue_id"] == d2["id"]
        assert row["reason"] == "audit-test"
        assert "blockers_overridden" in row
        overridden_ids = [b["id"] for b in row["blockers_overridden"]]
        assert d1["id"] in overridden_ids, overridden_ids
        # Timestamp is unix epoch (int) and matches what's on the queue item.
        assert isinstance(row["timestamp"], int)
        promoted = _show(env, d2["id"])
        assert row["timestamp"] == promoted["force_started_at"]


# ---------------------------------------------------------------------------
# 6. Claude-event emitted with force_started=true
# ---------------------------------------------------------------------------


def test_force_start_emits_claude_event():
    """The lifecycle emit should include `force_started=true` in the data
    block so downstream consumers (work-queue-exporter, external messaging
    integrations, etc.)
    can branch on the override path. We assert by reading the emitted
    JSON file out of the per-test claude-events dir.
    """
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        events_dir = Path(tmp) / "claude-events"

        r1 = _add(env, "blocker", ["scope:baz"])
        d1 = json.loads(r1.stdout)
        _register(env, d1["id"], "--json")
        # Drain any events from the register call so we only see the
        # force-start emit below.
        for f in events_dir.iterdir():
            f.unlink()

        r2 = _add(env, "blocked", ["scope:baz"], "--force-enqueue")
        d2 = json.loads(r2.stdout)

        rs = _run(
            env, "queue", "force-start", d2["id"],
            "--reason", "event-test", "--json",
        )
        assert rs.returncode == 0, rs.stderr

        emitted = []
        for f in sorted(events_dir.iterdir()):
            try:
                emitted.append(json.loads(f.read_text()))
            except (OSError, ValueError):
                continue

        # Find a queue-running event whose data carries force_started=true.
        # Note: claude-event's `--data KEY=VAL` flattens the value through a
        # shell argument, so booleans/lists land in the JSON event as their
        # `str()` rendering ("True", "[\"q-...\"]"). The semantic check is
        # that the field is present AND truthy after str-coercion.
        def _truthy(v):
            return str(v).lower() in ("true", "1", "yes")

        matching = [
            e for e in emitted
            if e.get("tag") == "queue-running"
            and _truthy((e.get("data") or {}).get("force_started"))
        ]
        assert matching, (
            f"expected a queue-running event with force_started=true, got "
            f"{[(e.get('tag'), (e.get('data') or {}).get('force_started')) for e in emitted]}"
        )
        ev = matching[0]
        assert ev["data"]["queue_id"] == d2["id"]
        assert ev["data"]["force_started_reason"] == "event-test"


# ---------------------------------------------------------------------------
# 7. Dedicated `force-start` claude-event emitted alongside `queue-running`
# ---------------------------------------------------------------------------


def test_force_start_emits_dedicated_force_start_event():
    """A force-start should emit BOTH a `queue-running` event (for the
    standard lifecycle bus) AND a dedicated `force-start` event so
    `claude-event-watch` surfaces force-starts to the main loop with a
    distinct tag (Andrew DM 2026-05-02 19:54 ET: "force starting should
    both emit an event AND add a hard obligation to spawn").
    """
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        events_dir = Path(tmp) / "claude-events"

        r1 = _add(env, "blocker", ["scope:fs"])
        d1 = json.loads(r1.stdout)
        _register(env, d1["id"], "--json")
        # Drain register's events.
        for f in events_dir.iterdir():
            f.unlink()

        r2 = _add(env, "blocked", ["scope:fs"], "--force-enqueue")
        d2 = json.loads(r2.stdout)

        rs = _run(
            env, "queue", "force-start", d2["id"],
            "--reason", "fs-event-test", "--json",
        )
        assert rs.returncode == 0, rs.stderr

        emitted = []
        for f in sorted(events_dir.iterdir()):
            try:
                emitted.append(json.loads(f.read_text()))
            except (OSError, ValueError):
                continue

        force_events = [e for e in emitted if e.get("tag") == "force-start"]
        assert force_events, (
            f"expected a `force-start` event, got tags="
            f"{[e.get('tag') for e in emitted]}"
        )
        ev = force_events[0]
        data = ev.get("data") or {}
        assert data.get("queue_id") == d2["id"]
        assert data.get("force_started_reason") == "fs-event-test"


# ---------------------------------------------------------------------------
# 8. Force-start registers a `force_started_unspawned` obligation
# ---------------------------------------------------------------------------


def test_force_start_registers_obligation():
    """Force-start should register a hard-gate obligation that DENIES every
    non-exempt main-loop tool call until an Agent has been dispatched for
    the promoted queue id. Verified by inspecting the per-test
    obligations.json (HOME-isolated -- the live ~/.config/claude/
    obligations.json is never touched).
    """
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)

        r1 = _add(env, "blocker", ["scope:obx"])
        d1 = json.loads(r1.stdout)
        _register(env, d1["id"], "--json")
        r2 = _add(env, "blocked", ["scope:obx"], "--force-enqueue")
        d2 = json.loads(r2.stdout)

        rs = _run(
            env, "queue", "force-start", d2["id"],
            "--reason", "obligation-test", "--json",
        )
        assert rs.returncode == 0, rs.stderr

        ob_path = Path(tmp) / ".config" / "claude" / "obligations.json"
        assert ob_path.exists(), (
            f"expected obligations.json at {ob_path}, but it was not written"
        )
        ob_data = json.loads(ob_path.read_text())
        matching = _force_start_obligations_for(ob_data, d2["id"])
        assert matching, (
            f"expected a force_started_unspawned obligation for {d2['id']!r}, "
            f"got {[ob.get('predicate') for ob in ob_data.get('obligations',[])]}"
        )
        ob = matching[0]
        assert ob.get("tool_pattern") == "*"
        assert ob.get("enforcement", "gate") == "gate"
        assert ob.get("created_by", "").startswith("force-start:")
        # Main-loop-only scoping: the leaf must be wrapped in an all_of whose
        # FIRST child is the `is_main_loop` scope guard, so the gate is
        # inert for concurrently-running subagents (regression 2026-08-20:
        # a bare row denied three unrelated subagents in the seconds
        # between force-start and the main loop's Agent call).
        pred = ob["predicate"]
        assert pred.get("kind") == "all_of", pred
        children = pred.get("params", {}).get("predicates", [])
        assert children and children[0].get("kind") == "is_main_loop", pred
        leaf = _force_start_leaf(ob)
        assert leaf is not None, pred
        # Dispatch grace window: default 60s.
        assert leaf.get("params", {}).get("grace_secs") == 60, leaf
        assert ob.get("ttl_secs", 0) > 0  # has a TTL safety net


def test_force_start_obligation_suppressed_by_env():
    """`OBLIGATIONS_FORCE_START=0` skips the obligation register call.
    Used by upstream test harnesses (e.g. queue-minisite) that exercise
    the force-start endpoint without wanting to mutate obligations state.
    The claude-event still emits and the queue still flips -- ONLY the
    obligation register is suppressed.
    """
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        env["OBLIGATIONS_FORCE_START"] = "0"

        r1 = _add(env, "blocker", ["scope:obx2"])
        d1 = json.loads(r1.stdout)
        _register(env, d1["id"], "--json")
        r2 = _add(env, "blocked", ["scope:obx2"], "--force-enqueue")
        d2 = json.loads(r2.stdout)

        rs = _run(
            env, "queue", "force-start", d2["id"],
            "--reason", "ob-suppressed", "--json",
        )
        assert rs.returncode == 0, rs.stderr

        ob_path = Path(tmp) / ".config" / "claude" / "obligations.json"
        # File may exist from an unrelated read, but must not contain a
        # force_started_unspawned row for d2.
        if ob_path.exists():
            ob_data = json.loads(ob_path.read_text())
            matching = _force_start_obligations_for(ob_data, d2["id"])
            assert not matching, (
                f"expected NO obligation registered when "
                f"OBLIGATIONS_FORCE_START=0, got {matching}"
            )


# ---------------------------------------------------------------------------
# 9. Autostop overlapping running peers + recovery bundle
# ---------------------------------------------------------------------------


def test_force_start_autostops_overlapping_running_peer():
    """A force-start must abandon every RUNNING item whose scope OVERLAPS
    the force-started item's scope, with a clear abandon_reason. Andrew
    2026-05-02 21:10 UTC: "force-start should ALSO autostop any RUNNING
    work whose scope OVERLAPS the force-started item".
    """
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)

        # Establish a running item that holds scope:foo. This is the peer
        # that should get autostopped.
        r1 = _add(env, "the-running-peer", ["scope:foo"])
        d1 = json.loads(r1.stdout)
        assert _register(env, d1["id"], "--json").returncode == 0

        # Force-enqueue a blocked-pending sibling on the same scope.
        r2 = _add(env, "the-interrupter", ["scope:foo"], "--force-enqueue")
        d2 = json.loads(r2.stdout)

        # Force-start: should autostop d1.
        rs = _run(
            env, "queue", "force-start", d2["id"],
            "--reason", "interrupting", "--json",
        )
        assert rs.returncode == 0, rs.stderr

        # d1 should now be abandoned with the autostop reason.
        d1_after = _show(env, d1["id"])
        assert d1_after["status"] == "abandoned", d1_after
        assert "autostopped by force-start" in (
            d1_after.get("abandon_reason", "")
        ), d1_after.get("abandon_reason")
        assert d1_after.get("autostopped_by_force_start") == d2["id"]

        # The promoted record should reference its autostopped peers.
        promoted_out = json.loads(rs.stdout)
        assert d1["id"] in promoted_out.get(
            "force_started_autostopped_peers", []
        ), promoted_out


def test_force_start_does_not_touch_disjoint_running_peer():
    """A running peer whose scope does NOT overlap the force-started item
    must be left RUNNING. The autostop is scope-overlap-driven.
    """
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)

        # Running peer on a disjoint scope.
        r1 = _add(env, "unrelated", ["scope:other"])
        d1 = json.loads(r1.stdout)
        assert _register(env, d1["id"], "--json").returncode == 0

        # Running peer on a SECOND disjoint scope, just for good measure.
        r2 = _add(env, "also-unrelated", ["scope:third"])
        d2 = json.loads(r2.stdout)
        assert _register(env, d2["id"], "--json").returncode == 0

        # Force-start a fresh item on scope:foo (overlaps neither).
        r3 = _add(env, "interrupter", ["scope:foo"])
        d3 = json.loads(r3.stdout)

        rs = _run(
            env, "queue", "force-start", d3["id"],
            "--reason", "no-conflict", "--json",
        )
        assert rs.returncode == 0, rs.stderr

        # Both unrelated peers should still be running.
        assert _show(env, d1["id"])["status"] == "running"
        assert _show(env, d2["id"])["status"] == "running"

        # Promoted record's autostopped-peers list is empty.
        promoted_out = json.loads(rs.stdout)
        assert promoted_out.get("force_started_autostopped_peers", []) == []


def test_force_start_writes_recovery_bundle_with_autostop():
    """When a peer is autostopped, the recovery bundle JSON must be written
    at FORCE_START_BUNDLE_DIR/<q-X>.json with the peer's queue context.
    """
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)

        r1 = _add(env, "running-peer", ["scope:bundle"])
        d1 = json.loads(r1.stdout)
        _register(env, d1["id"], "--json")
        r2 = _add(env, "interrupter", ["scope:bundle"], "--force-enqueue")
        d2 = json.loads(r2.stdout)

        rs = _run(
            env, "queue", "force-start", d2["id"],
            "--reason", "bundle-write", "--json",
        )
        assert rs.returncode == 0, rs.stderr

        bundle_dir = Path(env["FORCE_START_BUNDLE_DIR"])
        bundle_path = bundle_dir / f"{d2['id']}.json"
        assert bundle_path.exists(), (
            f"expected bundle at {bundle_path}, dir contents = "
            f"{list(bundle_dir.iterdir()) if bundle_dir.exists() else 'no dir'}"
        )

        bundle = json.loads(bundle_path.read_text())
        assert bundle["force_started_queue_id"] == d2["id"]
        assert bundle["force_started_reason"] == "bundle-write"
        peers = bundle["autostopped_peers"]
        assert len(peers) == 1, peers
        peer = peers[0]
        assert peer["queue_id"] == d1["id"]
        assert peer["summary"]
        assert peer["scope"] == ["scope:bundle"]
        assert peer["abandon_reason"].startswith("autostopped by force-start")
        # Repo snapshots and prompt are best-effort and may be empty in
        # the test sandbox (no claude-watch active-agents.json), but the
        # keys must exist.
        assert "repo_snapshots" in peer
        assert "original_prompt" in peer
        assert "agent_kill_outcome" in peer

        # Promoted JSON should also surface the bundle path.
        promoted_out = json.loads(rs.stdout)
        assert promoted_out.get("force_started_recovery_bundle_path") == \
            str(bundle_path), promoted_out


def test_force_start_writes_empty_bundle_when_no_autostop():
    """Bundle is always written (per spec — empty-list case is a useful
    "force-started in the clear" signal). Verify the empty-peers shape.
    """
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        r1 = _add(env, "lonely", ["scope:empty"])
        d1 = json.loads(r1.stdout)

        rs = _run(
            env, "queue", "force-start", d1["id"],
            "--reason", "empty-bundle", "--json",
        )
        assert rs.returncode == 0, rs.stderr

        bundle_dir = Path(env["FORCE_START_BUNDLE_DIR"])
        bundle_path = bundle_dir / f"{d1['id']}.json"
        assert bundle_path.exists()
        bundle = json.loads(bundle_path.read_text())
        assert bundle["autostopped_peers"] == []


def test_force_start_event_carries_recovery_bundle_path():
    """The dedicated `force-start` claude-event's data must include both
    `recovery_bundle_path` and `autostopped_peers` so the main loop can
    paste them into the spawned Agent's prompt.
    """
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        events_dir = Path(tmp) / "claude-events"

        r1 = _add(env, "blocker", ["scope:evt"])
        d1 = json.loads(r1.stdout)
        _register(env, d1["id"], "--json")
        for f in events_dir.iterdir():
            f.unlink()

        r2 = _add(env, "blocked", ["scope:evt"], "--force-enqueue")
        d2 = json.loads(r2.stdout)

        rs = _run(
            env, "queue", "force-start", d2["id"],
            "--reason", "evt-test", "--json",
        )
        assert rs.returncode == 0, rs.stderr

        emitted = []
        for f in sorted(events_dir.iterdir()):
            try:
                emitted.append(json.loads(f.read_text()))
            except (OSError, ValueError):
                continue

        force_events = [e for e in emitted if e.get("tag") == "force-start"]
        assert force_events, [e.get("tag") for e in emitted]
        ev = force_events[0]
        data = ev.get("data") or {}
        # claude-event flattens list/None values via str() so we assert by
        # presence + truthy-substring rather than direct typed equality.
        bundle_path = data.get("recovery_bundle_path")
        assert bundle_path, data
        assert d2["id"] in str(bundle_path), data
        assert d1["id"] in str(data.get("autostopped_peers")), data


def test_force_start_audit_log_records_autostopped_peers():
    """The QUEUE_FORCE_START_LOG row must include the autostopped peers'
    queue ids so post-incident auditors can reconstruct the override.
    """
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        log_path = Path(env["QUEUE_FORCE_START_LOG"])

        r1 = _add(env, "blocker", ["scope:audit"])
        d1 = json.loads(r1.stdout)
        _register(env, d1["id"], "--json")
        r2 = _add(env, "blocked", ["scope:audit"], "--force-enqueue")
        d2 = json.loads(r2.stdout)

        rs = _run(
            env, "queue", "force-start", d2["id"],
            "--reason", "audit-autostop", "--json",
        )
        assert rs.returncode == 0, rs.stderr

        rows = [
            json.loads(line)
            for line in log_path.read_text().splitlines() if line.strip()
        ]
        assert rows
        row = rows[-1]
        assert row["queue_id"] == d2["id"]
        assert d1["id"] in row.get("autostopped_peers", []), row


def test_force_start_obligation_message_includes_bundle_path():
    """The deny banner persisted in obligations.json should reference the
    recovery bundle path so the main loop sees it on the gate fire.
    """
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)

        r1 = _add(env, "blocker", ["scope:obmsg"])
        d1 = json.loads(r1.stdout)
        _register(env, d1["id"], "--json")
        r2 = _add(env, "blocked", ["scope:obmsg"], "--force-enqueue")
        d2 = json.loads(r2.stdout)

        rs = _run(
            env, "queue", "force-start", d2["id"],
            "--reason", "deny-banner", "--json",
        )
        assert rs.returncode == 0, rs.stderr

        ob_path = Path(tmp) / ".config" / "claude" / "obligations.json"
        assert ob_path.exists(), "obligations.json not written"
        ob_data = json.loads(ob_path.read_text())
        matching = _force_start_obligations_for(ob_data, d2["id"])
        assert matching, "force_started_unspawned obligation not registered"
        # The obligations CLI stores the deny banner under `deny_message`
        # (writable via `obligations add --deny-msg ...`). Older deploys
        # may have used `deny_msg`; fall back gracefully.
        ob = matching[0]
        deny_msg = (
            ob.get("deny_message")
            or ob.get("deny_msg")
            or ob.get("message", "")
        )
        assert d2["id"] in deny_msg, f"deny_msg missing q-id: {deny_msg!r}"
        # Bundle path mentions the queue id (deterministic filename).
        assert f"{d2['id']}.json" in deny_msg, deny_msg


def test_force_start_repo_snapshot_captured_in_bundle():
    """When a scope token resolves to a real git repo, the bundle should
    capture `git status` / `git diff` for that working tree. We seed a
    tiny git repo under HOME=tmp and assert the snapshot lands.
    """
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        # Make ~/repos/myrepo a real git working tree with a dirty file.
        repos_dir = Path(tmp) / "repos" / "myrepo"
        repos_dir.mkdir(parents=True)
        subprocess.run(
            ["git", "init", "-q", str(repos_dir)],
            check=True, capture_output=True,
        )
        subprocess.run(
            ["git", "-C", str(repos_dir), "config", "user.email", "t@t"],
            check=True, capture_output=True,
        )
        subprocess.run(
            ["git", "-C", str(repos_dir), "config", "user.name", "t"],
            check=True, capture_output=True,
        )
        # Initial commit so HEAD exists.
        (repos_dir / "README.md").write_text("hello\n")
        subprocess.run(
            ["git", "-C", str(repos_dir), "add", "README.md"],
            check=True, capture_output=True,
        )
        subprocess.run(
            ["git", "-C", str(repos_dir), "commit", "-q", "-m", "init"],
            check=True, capture_output=True,
        )
        # Now leave a dirty modification + an untracked file.
        (repos_dir / "README.md").write_text("hello\nworld\n")
        (repos_dir / "scratch.txt").write_text("uncommitted\n")

        # Running peer scoped to that repo.
        r1 = _add(env, "peer", ["repo:myrepo"])
        d1 = json.loads(r1.stdout)
        _register(env, d1["id"], "--json")
        r2 = _add(env, "interrupter", ["repo:myrepo"], "--force-enqueue")
        d2 = json.loads(r2.stdout)

        rs = _run(
            env, "queue", "force-start", d2["id"],
            "--reason", "repo-snap", "--json",
        )
        assert rs.returncode == 0, rs.stderr

        bundle_path = Path(env["FORCE_START_BUNDLE_DIR"]) / f"{d2['id']}.json"
        bundle = json.loads(bundle_path.read_text())
        peers = bundle["autostopped_peers"]
        assert len(peers) == 1
        snaps = peers[0]["repo_snapshots"]
        assert snaps, "expected at least one repo snapshot"
        snap = snaps[0]
        assert "myrepo" in snap["path"]
        # Untracked + modified files should both surface in porcelain status.
        assert "scratch.txt" in snap["status"]
        assert "README.md" in snap["status"]
        # Diff carries the README.md edit.
        assert "world" in snap["diff"], snap["diff"]


# ---------------------------------------------------------------------------
# 10. End-to-end: force-start + Agent spawn via hook clears the obligation
# ---------------------------------------------------------------------------


def test_force_start_then_hook_clears_obligation():
    """Regression: the bug where dispatched subagents got blocked by their
    own force-start obligation because claude-watch's once-a-minute
    active-agents poller hadn't refreshed yet.

    Flow:
      1. Force-start a blocked-pending queue item -- registers a
         `force_started_unspawned` obligation that DENIES `*`.
      2. Confirm a Bash tool call is denied by the obligations gate
         (pre-tool-obligations-gate-hook).
      3. Run the pre-agent-queue-gate-hook with a valid `Queue item: q-X`
         marker -- it writes a pending-spawn record at the configured
         path.
      4. Re-run the Bash tool call against the obligations gate -- the
         predicate now sees the fresh pending-spawn entry and ALLOWS,
         even though active-agents.json hasn't been refreshed yet.

    This covers the q-X dispatch race that previously required manual
    `obligations override --duration` workarounds.

    The dispatch grace window is DISABLED here
    (``OBLIGATIONS_FORCE_START_GRACE_SECS=0``) so step 2 asserts the
    post-grace DENY deterministically; the grace itself is covered by
    ``test_force_start_obligation_grace_window``.
    """
    repo_root = Path(__file__).resolve().parent.parent.parent.parent
    pre_agent_hook = repo_root / "tools" / "hooks" / "pre-agent-queue-gate-hook"
    pre_obligations_hook = (
        repo_root / "tools" / "hooks" / "pre-tool-obligations-gate-hook"
    )
    obligations_cli = repo_root / "tools" / "obligations" / "obligations"
    assert pre_agent_hook.exists(), pre_agent_hook
    assert pre_obligations_hook.exists(), pre_obligations_hook
    assert obligations_cli.exists(), obligations_cli

    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        env["OBLIGATIONS_FORCE_START_GRACE_SECS"] = "0"
        # Direct the pending-spawns sidecar into the temp HOME so the
        # test can both write it (via the agent hook) and read it (via
        # the predicate).
        pending_path = Path(tmp) / ".config" / "claude" / "pending-spawns.json"
        env["CLAUDE_PENDING_SPAWNS_PATH"] = str(pending_path)
        # Make sure the hook subprocesses see HOME-isolated session-task
        # state (queue.json + obligations.json live under HOME/.config).
        # The hook also needs `session-task` on PATH; we already have it
        # via the parent env but it doesn't hurt to also point at the
        # repo copy via SESSION_TASK_PATH if the hook honours it. The
        # current hook shells `session-task` directly, so we rely on
        # whatever the test runner has on PATH (the repo's
        # tools/session-task/session-task is symlinked into ~/bin).

        # 1. Force-start a blocked-pending item.
        r1 = _add(env, "blocker", ["scope:e2e"])
        d1 = json.loads(r1.stdout)
        _register(env, d1["id"], "--json")
        r2 = _add(env, "blocked", ["scope:e2e"], "--force-enqueue")
        d2 = json.loads(r2.stdout)
        target_qid = d2["id"]

        rs = _run(
            env, "queue", "force-start", target_qid,
            "--reason", "e2e-clear-test", "--json",
        )
        assert rs.returncode == 0, rs.stderr

        # The obligation should have been registered.
        ob_path = Path(tmp) / ".config" / "claude" / "obligations.json"
        assert ob_path.exists(), "obligation row must be written"

        # 2. Run the pre-tool-obligations-gate-hook for a Bash call --
        # expect DENY because no agent has been observed yet and there's
        # no pending-spawn record.
        bash_payload = {
            "tool_name": "Bash",
            "tool_input": {"command": "ls"},
        }
        proc = subprocess.run(
            [sys.executable, str(pre_obligations_hook)],
            input=json.dumps(bash_payload),
            capture_output=True, text=True, env=env, timeout=10,
        )
        assert proc.returncode == 0, proc.stderr
        try:
            decision = json.loads(proc.stdout) if proc.stdout.strip() else {}
        except json.JSONDecodeError:
            decision = {}
        hso = decision.get("hookSpecificOutput", {}) or {}
        assert hso.get("permissionDecision") == "deny", (
            f"expected DENY pre-spawn, got: {decision}"
        )
        assert target_qid in (
            hso.get("permissionDecisionReason", "")
            + decision.get("systemMessage", "")
        ), "deny banner should name the queue id"

        # 3. Run the pre-agent-queue-gate-hook to simulate an Agent
        # spawn for the target queue id. This writes the pending-spawn
        # record.
        agent_payload = {
            "tool_name": "Agent",
            "tool_input": {
                "subagent_type": "general-purpose",
                "prompt": (
                    f"Investigate the thing.\n\nQueue item: {target_qid}\n"
                ),
            },
        }
        proc = subprocess.run(
            [sys.executable, str(pre_agent_hook)],
            input=json.dumps(agent_payload),
            capture_output=True, text=True, env=env, timeout=10,
        )
        assert proc.returncode == 0, proc.stderr
        try:
            agent_decision = (
                json.loads(proc.stdout) if proc.stdout.strip() else {}
            )
        except json.JSONDecodeError:
            agent_decision = {}
        # Agent spawn must be allowed (no decision override).
        assert agent_decision.get("hookSpecificOutput", {}).get(
            "permissionDecision"
        ) != "deny", f"Agent spawn was blocked: {agent_decision}"

        # Sidecar file should now hold a pending-spawn record for the qid.
        assert pending_path.exists(), (
            f"pending-spawns sidecar not written at {pending_path}"
        )
        sidecar = json.loads(pending_path.read_text())
        pending_entries = sidecar.get("pending", []) if isinstance(
            sidecar, dict
        ) else []
        matching = [
            e for e in pending_entries if e.get("queue_id") == target_qid
        ]
        assert matching, (
            f"expected a pending-spawn entry for {target_qid!r}, got: {sidecar}"
        )

        # 4. Re-run the obligations gate hook for a Bash call -- now
        # expect ALLOW because the predicate sees the fresh pending-spawn
        # record.
        proc = subprocess.run(
            [sys.executable, str(pre_obligations_hook)],
            input=json.dumps(bash_payload),
            capture_output=True, text=True, env=env, timeout=10,
        )
        assert proc.returncode == 0, proc.stderr
        try:
            decision_after = (
                json.loads(proc.stdout) if proc.stdout.strip() else {}
            )
        except json.JSONDecodeError:
            decision_after = {}
        hso_after = decision_after.get("hookSpecificOutput", {}) or {}
        assert hso_after.get("permissionDecision") != "deny", (
            f"expected ALLOW after pending-spawn record was written, got: "
            f"{decision_after}"
        )


def _force_start_for_gate_tests(tmp, env, scope_tag):
    """Force-start a blocked-pending item in the temp HOME; return its qid
    plus the paths the obligations gate hook reads."""
    repo_root = Path(__file__).resolve().parent.parent.parent.parent
    pre_obligations_hook = (
        repo_root / "tools" / "hooks" / "pre-tool-obligations-gate-hook"
    )
    assert pre_obligations_hook.exists(), pre_obligations_hook
    # Keep the predicate's agents/pending lookups off the live host files:
    # the pending-spawns sidecar is env-redirectable, and a test qid can
    # never appear in the host's active-agents.json anyway.
    env["CLAUDE_PENDING_SPAWNS_PATH"] = str(
        Path(tmp) / ".config" / "claude" / "pending-spawns.json"
    )
    r1 = _add(env, "blocker", [f"scope:{scope_tag}"])
    d1 = json.loads(r1.stdout)
    _register(env, d1["id"], "--json")
    r2 = _add(env, "blocked", [f"scope:{scope_tag}"], "--force-enqueue")
    d2 = json.loads(r2.stdout)
    rs = _run(
        env, "queue", "force-start", d2["id"],
        "--reason", f"{scope_tag}-test", "--json",
    )
    assert rs.returncode == 0, rs.stderr
    ob_path = Path(tmp) / ".config" / "claude" / "obligations.json"
    assert ob_path.exists(), "obligation row must be written"
    return d2["id"], pre_obligations_hook


def test_force_start_obligation_does_not_gate_subagents():
    """Regression (2026-08-20): the FORCE-START obligation is evaluated on
    EVERY PreToolUse, including tool calls issued by subagents that were
    already running. Between the main loop's `force-start` and its `Agent`
    call, three unrelated subagents were denied with "spawn the agent for
    q=X" -- a gate they could neither cause nor satisfy -- and fell back
    to audited bypasses.

    The obligation is now registered as
    ``all_of [is_main_loop, force_started_unspawned]``: a subagent caller
    (non-empty ``agent_id``, or a subagent ``agent_type`` slug) must be
    ALLOWED while the same call from the main loop is DENIED.

    Grace disabled so the main-loop DENY is immediate.
    """
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        env["OBLIGATIONS_FORCE_START_GRACE_SECS"] = "0"
        qid, hook = _force_start_for_gate_tests(tmp, env, "subgate")
        bash = {"tool_name": "Bash", "tool_input": {"command": "ls"}}

        # Main loop (no agent_id / agent_type) -> DENY naming the qid.
        main = _run_obligations_gate_hook(hook, env, bash)
        assert _decision(main) == "deny", f"expected main-loop DENY: {main}"
        assert qid in (
            main.get("hookSpecificOutput", {}).get("permissionDecisionReason", "")
            + main.get("systemMessage", "")
        )

        # Subagent by agent_id -> ALLOW (scope guard inactive).
        sub = _run_obligations_gate_hook(
            hook, env, dict(bash, agent_id="agent-other-work"),
        )
        assert _decision(sub) != "deny", (
            f"subagent must not be gated by another item's force-start: {sub}"
        )

        # Subagent by agent_type slug only (in-process teammate) -> ALLOW.
        sub2 = _run_obligations_gate_hook(
            hook, env, dict(bash, agent_type="general-purpose"),
        )
        assert _decision(sub2) != "deny", (
            f"agent_type-slug subagent must not be gated: {sub2}"
        )

        # Sanity: the main loop is STILL denied after the subagent calls
        # (the subagent path must not have satisfied/cleared the row).
        main2 = _run_obligations_gate_hook(hook, env, bash)
        assert _decision(main2) == "deny", main2


def test_force_start_obligation_grace_window():
    """The main loop gets a dispatch grace window (default 60s) after a
    force-start: `force-start` -> `queue register` -> `Agent` must not
    trip over its own gate. Once the window has elapsed with no agent
    observed, the main loop IS denied (a force-started item must never sit
    `running` with no agent); a live agent / pending-spawn still clears it.
    """
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        env.pop("OBLIGATIONS_FORCE_START_GRACE_SECS", None)  # default 60
        qid, hook = _force_start_for_gate_tests(tmp, env, "grace")
        bash = {"tool_name": "Bash", "tool_input": {"command": "ls"}}

        # Immediately after force-start: inside the grace -> ALLOW.
        d0 = _run_obligations_gate_hook(hook, env, bash)
        assert _decision(d0) != "deny", f"expected grace ALLOW: {d0}"

        # Rewind the queue item's force_started_at by 2 min -> grace
        # elapsed -> DENY.
        qpath = Path(tmp) / ".config" / "session" / "queue.json"
        qdata = json.loads(qpath.read_text())
        for it in qdata.get("items", []):
            if it.get("id") == qid:
                assert isinstance(it.get("force_started_at"), int), it
                it["force_started_at"] -= 120
        qpath.write_text(json.dumps(qdata))
        d1 = _run_obligations_gate_hook(hook, env, bash)
        assert _decision(d1) == "deny", f"expected post-grace DENY: {d1}"

        # A subagent is still unaffected after the grace.
        d_sub = _run_obligations_gate_hook(
            hook, env, dict(bash, agent_id="agent-elsewhere"),
        )
        assert _decision(d_sub) != "deny", d_sub

        # A fresh pending-spawn record for the qid clears it for the main
        # loop (same evidence the pre-agent-queue-gate-hook writes).
        pending_path = Path(env["CLAUDE_PENDING_SPAWNS_PATH"])
        pending_path.parent.mkdir(parents=True, exist_ok=True)
        import time as _time
        pending_path.write_text(json.dumps({"pending": [{
            "queue_id": qid, "registered_at": int(_time.time()), "pid": 1,
        }]}))
        d2 = _run_obligations_gate_hook(hook, env, bash)
        assert _decision(d2) != "deny", f"expected ALLOW after spawn: {d2}"


def test_force_start_grace_env_override():
    """``OBLIGATIONS_FORCE_START_GRACE_SECS`` sets the leaf's ``grace_secs``;
    ``0`` omits it (no grace)."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        env["OBLIGATIONS_FORCE_START_GRACE_SECS"] = "15"
        r1 = _add(env, "blocker", ["scope:genv"])
        d1 = json.loads(r1.stdout)
        _register(env, d1["id"], "--json")
        r2 = _add(env, "blocked", ["scope:genv"], "--force-enqueue")
        d2 = json.loads(r2.stdout)
        rs = _run(env, "queue", "force-start", d2["id"],
                  "--reason", "genv", "--json")
        assert rs.returncode == 0, rs.stderr
        ob_path = Path(tmp) / ".config" / "claude" / "obligations.json"
        ob_data = json.loads(ob_path.read_text())
        leaf = _force_start_leaf(_force_start_obligations_for(ob_data, d2["id"])[0])
        assert leaf["params"].get("grace_secs") == 15, leaf

    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        env["OBLIGATIONS_FORCE_START_GRACE_SECS"] = "0"
        r1 = _add(env, "blocker", ["scope:genv0"])
        d1 = json.loads(r1.stdout)
        _register(env, d1["id"], "--json")
        r2 = _add(env, "blocked", ["scope:genv0"], "--force-enqueue")
        d2 = json.loads(r2.stdout)
        rs = _run(env, "queue", "force-start", d2["id"],
                  "--reason", "genv0", "--json")
        assert rs.returncode == 0, rs.stderr
        ob_path = Path(tmp) / ".config" / "claude" / "obligations.json"
        ob_data = json.loads(ob_path.read_text())
        leaf = _force_start_leaf(_force_start_obligations_for(ob_data, d2["id"])[0])
        assert "grace_secs" not in leaf["params"], leaf


# ---------------------------------------------------------------------------
# Entry point for direct invocation
# ---------------------------------------------------------------------------


def _all_tests():
    return [
        test_force_start_promotes_blocked_pending,
        test_force_start_refuses_already_running,
        test_force_start_refuses_no_reason,
        test_force_start_refuses_empty_reason,
        test_force_start_refuses_not_found,
        test_force_start_writes_audit_log,
        test_force_start_emits_claude_event,
        test_force_start_emits_dedicated_force_start_event,
        test_force_start_registers_obligation,
        test_force_start_obligation_suppressed_by_env,
        test_force_start_autostops_overlapping_running_peer,
        test_force_start_does_not_touch_disjoint_running_peer,
        test_force_start_writes_recovery_bundle_with_autostop,
        test_force_start_writes_empty_bundle_when_no_autostop,
        test_force_start_event_carries_recovery_bundle_path,
        test_force_start_audit_log_records_autostopped_peers,
        test_force_start_obligation_message_includes_bundle_path,
        test_force_start_repo_snapshot_captured_in_bundle,
        test_force_start_then_hook_clears_obligation,
        test_force_start_obligation_does_not_gate_subagents,
        test_force_start_obligation_grace_window,
        test_force_start_grace_env_override,
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
