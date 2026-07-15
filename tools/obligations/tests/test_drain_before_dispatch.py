#!/usr/bin/env python3
"""Tests for the ``drain_before_dispatch`` main-loop carve-out.

Background: the botchat mark-read gate is a full-block ``tool_pattern:"*"``
operator gate. Andrew wants the MAIN loop to be FORCED to drain + ack botchat
BEFORE it can spawn/manage task work -- otherwise the loop dispatches agents
while operator messages sit unread (botchat #1854 -> #1862 -> #1864 "ya").

The mechanism: an obligation row opts in with ``drain_before_dispatch: true``.
While that gate is FIRING (its predicate unsatisfied), the framework
deadlock-prevention floor (``_universal_recovery_exempt_match``):

  1. ELEVATES the firing gate's own ``exempt_patterns`` (its declared
     clear-path -- the botchat read/ack CLIs) into the floor, so the clear-path
     punches through EVERY co-firing gate; and
  2. DROPS the MAIN loop's task-dispatch surface (the ``Agent`` tool + the
     mutating ``session-task queue`` subcommands in
     ``MAIN_LOOP_DISPATCH_GATED_QUEUE_SUBCOMMANDS``) from the floor, so those
     calls fall through to per-row evaluation and the drain gate DENIES them.

Deadlock-safety (the property these tests pin -- see incident 2026-06-03):

  * The carve-out is OPT-IN. A gate that does NOT set ``drain_before_dispatch``
    (e.g. ``queue_ready_unspawned`` / ``event_must_act`` / ``stale_ready``,
    whose ONLY clear-path IS dispatch) NEVER narrows the dispatch floor. If it
    did, the loop could never dispatch to clear it -> hard deadlock.
  * The carve-out only bites WHILE the gate is unsatisfied. Once drained, the
    full floor is restored -> pure ordering ("drain first"), never a standing
    deadlock.
  * The recovery surface (``obligations override``, read-only + ``register`` +
    ``heartbeat`` ``session-task queue``, watcher-ctl, Read, ToolSearch,
    self-clear) and the gate's own clear-path stay floored the whole time. So
    the loop can ALWAYS (a) drain+ack botchat, (b) run recovery/override.

Loads the ``obligations`` CLI (no .py suffix) as a module via importlib, same
pattern as ``test_is_subagent_context.py``.

Run::

    uv run --python 3.11 --with pytest \\
        pytest tools/obligations/tests/test_drain_before_dispatch.py -v
"""

import importlib.machinery
import importlib.util
import json
from pathlib import Path

import pytest

HERE = Path(__file__).resolve().parent
OBLIGATIONS = HERE.parent / "obligations"


def _load_obligations():
    spec = importlib.util.spec_from_loader(
        "obligations_cli",
        importlib.machinery.SourceFileLoader("obligations_cli", str(OBLIGATIONS)),
    )
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


obl = _load_obligations()


# --------------------------------------------------------------------------
# Fixtures: point OBLIGATIONS_FILE at a temp file, expose a check() helper.
# --------------------------------------------------------------------------

@pytest.fixture
def state(tmp_path, monkeypatch):
    """Redirect the module's state file to a temp path and return a small
    controller with set()/check() helpers."""
    f = tmp_path / "obligations.json"
    monkeypatch.setattr(obl, "OBLIGATIONS_FILE", f)

    class Ctl:
        def __init__(self):
            self.now = obl._now()

        def set(self, obligations, overrides=None):
            f.write_text(json.dumps({
                "obligations": obligations,
                "overrides": overrides or [],
            }))

        def check(self, tool, command="", agent_id=None):
            ok, blocking = obl._check_core(tool, command, agent_id=agent_id)
            return ok, [b["id"] for b in blocking]

    return Ctl()


def _drain_gate(firing, *, exempts=None, drain=True, now=None):
    """A botchat-shaped full-block gate. ``firing`` True => evaluator exits 1
    (unread) so the predicate is UNSATISFIED (gate fires)."""
    return {
        "id": "ob-botchat",
        "tool_pattern": "*",
        "exempt_patterns": exempts if exempts is not None else [
            "Bash:botchat-send", "Bash:botchat-unread-check",
            "Bash:botchat-history", "Bash:botchat-show", "Read",
        ],
        "predicate": {"kind": "all_of", "params": {"predicates": [
            {"kind": "is_main_loop", "params": {}},
            {"kind": "evaluator", "params": {
                "cmd": "false" if firing else "true",
                "decision_mode": "exit_code",
                "allow_on_zero_exit": True,
                "timeout_ms": 2000,
            }},
        ]}},
        "enforcement": "gate", "ttl_secs": 0, "expires_at": None,
        "created_at": now or obl._now(), "created_by": "botchat",
        "satisfied_by": None, "deny_message": "unread botchat",
        "mandatory": False, "drain_before_dispatch": drain,
    }


def _queue_ready_gate(firing, *, now=None):
    """A dispatch-RECOVERY gate (queue_ready_unspawned shape): tool_pattern
    Bash, whose ONLY clear-path is Agent/session-task dispatch. It does NOT
    opt into drain_before_dispatch and does NOT exempt botchat CLIs."""
    return {
        "id": "ob-qready",
        "tool_pattern": "Bash",
        "exempt_patterns": [
            r"Bash:^session-task\s+queue\s+(status|spawn-check|register|show|list)",
            r"Bash:^obligations\s+(list|show|status|check|override|satisfy)",
        ],
        "predicate": {"kind": "all_of", "params": {"predicates": [
            {"kind": "is_main_loop", "params": {}},
            {"kind": "evaluator", "params": {
                "cmd": "false" if firing else "true",
                "decision_mode": "exit_code",
                "allow_on_zero_exit": True,
            }},
        ]}},
        "enforcement": "gate", "ttl_secs": 0, "expires_at": None,
        "created_at": now or obl._now(), "created_by": "obligations-init",
        "satisfied_by": None, "deny_message": "ready unspawned",
        "mandatory": False, "drain_before_dispatch": False,
    }


# --------------------------------------------------------------------------
# 1. Drain gate FIRING (main loop): dispatch surface is DENIED.
# --------------------------------------------------------------------------

def test_drain_firing_denies_agent_spawn(state):
    state.set([_drain_gate(firing=True)])
    ok, ids = state.check("Agent")
    assert not ok and "ob-botchat" in ids


def test_drain_firing_denies_session_task_add(state):
    state.set([_drain_gate(firing=True)])
    ok, ids = state.check("Bash", "session-task queue add \"x\" --scope y")
    assert not ok and "ob-botchat" in ids


@pytest.mark.parametrize("sub", [
    "done", "abandon", "block", "unblock", "reprioritize", "depend", "promote",
])
def test_drain_firing_denies_mutating_queue_subcommands(state, sub):
    state.set([_drain_gate(firing=True)])
    ok, ids = state.check("Bash", f"session-task queue {sub} q-1")
    assert not ok and "ob-botchat" in ids


# --------------------------------------------------------------------------
# 2. Drain gate FIRING: recovery + clear-path surface stays FLOORED (allowed).
# --------------------------------------------------------------------------

@pytest.mark.parametrize("cmd", [
    "session-task queue status q-1",
    "session-task queue spawn-check q-1",
    "session-task queue register q-1",
    "session-task queue show q-1",
    "session-task queue list",
    "session-task queue heartbeat q-1",
    "obligations override \"x\" --duration 5m",
    "obligations list",
    "watcher-ctl run foo",
    "watcher-restart",
    "self-clear",
])
def test_drain_firing_allows_recovery_bash(state, cmd):
    state.set([_drain_gate(firing=True)])
    ok, _ = state.check("Bash", cmd)
    assert ok, f"recovery command must stay floored: {cmd}"


@pytest.mark.parametrize("cmd", [
    "botchat-send --mark-read 5 --ack 5",
    "botchat-unread-check --ids",
    "botchat-history",
    "botchat-show 5",
])
def test_drain_firing_allows_botchat_clear_path(state, cmd):
    state.set([_drain_gate(firing=True)])
    ok, _ = state.check("Bash", cmd)
    assert ok, f"botchat clear-path must be allowed: {cmd}"


def test_drain_firing_allows_read_and_toolsearch(state):
    state.set([_drain_gate(firing=True)])
    assert state.check("Read")[0]
    assert state.check("ToolSearch")[0]


def test_drain_firing_still_blocks_unrelated_bash(state):
    # The gate is a full-block *; a non-exempt, non-dispatch command is still
    # denied by the gate's normal tool_pattern:* behavior (unchanged).
    state.set([_drain_gate(firing=True)])
    ok, ids = state.check("Bash", "ls -la")
    assert not ok and "ob-botchat" in ids


# --------------------------------------------------------------------------
# 3. Drain gate SATISFIED: full floor restored (dispatch allowed again).
# --------------------------------------------------------------------------

def test_drain_satisfied_restores_dispatch_floor(state):
    state.set([_drain_gate(firing=False)])
    assert state.check("Agent")[0]
    assert state.check("Bash", "session-task queue add x")[0]


# --------------------------------------------------------------------------
# 4. Deadlock-safety: a NON-opt-in gate never narrows the dispatch floor.
# --------------------------------------------------------------------------

def test_non_optin_gate_firing_keeps_dispatch_floored(state):
    # A dispatch-recovery gate (queue_ready_unspawned) firing alone must NOT
    # drop Agent / session-task from the floor -- else the loop can't dispatch
    # to clear it (incident 2026-06-03).
    state.set([_queue_ready_gate(firing=True)])
    assert state.check("Agent")[0], "Agent must stay floored for a non-drain gate"
    assert state.check("Bash", "session-task queue add x")[0]


# --------------------------------------------------------------------------
# 5. CO-FIRING: drain gate + dispatch-recovery gate. The property that would
#    have deadlocked WITHOUT the clear-path elevation.
# --------------------------------------------------------------------------

def test_cofiring_botchat_clear_path_punches_through_queue_gate(state):
    # Both firing. queue_ready_unspawned (tool_pattern Bash) does NOT exempt
    # botchat CLIs; WITHOUT clear-path elevation it would block the very
    # botchat-send that clears the drain gate -> deadlock. With elevation, the
    # clear-path is allowed.
    state.set([_drain_gate(firing=True), _queue_ready_gate(firing=True)])
    assert state.check("Bash", "botchat-send --mark-read 5")[0]
    assert state.check("Bash", "botchat-unread-check --ids")[0]
    assert state.check("Bash", "botchat-history")[0]
    # Override always available.
    assert state.check("Bash", "obligations override \"x\" --duration 5m")[0]
    # Dispatch is drain-gated (drain first) -- correct, not a deadlock: the
    # loop drains botchat, the drain gate clears, THEN Agent is un-gated.
    assert not state.check("Agent")[0]


def test_cofiring_after_drain_cleared_dispatch_recovers(state):
    # Drain satisfied, queue_ready still firing: Agent is floored again by the
    # universal recovery floor (dispatch IS queue_ready's clear-path).
    state.set([_drain_gate(firing=False), _queue_ready_gate(firing=True)])
    assert state.check("Agent")[0]


# --------------------------------------------------------------------------
# 6. An active override that bypasses the drain row => not firing => floor
#    restored (the loop deliberately chose the escape hatch).
# --------------------------------------------------------------------------

def test_active_override_restores_dispatch_floor(state):
    d = _drain_gate(firing=True)
    ov = {"id": "ov1", "scope": "all", "reason": "r",
          "created_by": "t", "expires_at": state.now + 300}
    state.set([d], overrides=[ov])
    assert state.check("Agent")[0]


# --------------------------------------------------------------------------
# 7. Subagent context: drain gates are all_of[is_main_loop, ...] and never
#    fire in a subagent, so the carve-out never bites there.
# --------------------------------------------------------------------------

def test_subagent_dispatch_not_carved(state):
    state.set([_drain_gate(firing=True)])
    assert state.check("Agent", agent_id="agent-xyz")[0]
    assert state.check("Bash", "session-task queue add x", agent_id="agent-xyz")[0]


# --------------------------------------------------------------------------
# 8. cmd_add persists drain_before_dispatch; _firing_drain_gates reads it.
# --------------------------------------------------------------------------

def test_add_persists_drain_flag(state):
    import argparse
    args = argparse.Namespace(
        tool_pattern="*", exempt_tool_pattern=["Read"], predicate="file_exists",
        params=json.dumps({"path": "/nonexistent-xyz", "negate": True}),
        ttl=0, enforcement="gate", deny_msg="d", satisfied_by_tool=None,
        satisfied_by_cmd_regex=None, created_by="t", mandatory=False,
        drain_before_dispatch=True, json=True,
    )
    obl.cmd_add(args)
    data = obl._read_only()
    row = data["obligations"][0]
    assert row["drain_before_dispatch"] is True


def test_add_defaults_drain_flag_false(state):
    import argparse
    args = argparse.Namespace(
        tool_pattern="Bash", exempt_tool_pattern=None, predicate="file_exists",
        params=json.dumps({"path": "/nonexistent-xyz"}),
        ttl=0, enforcement="gate", deny_msg="d", satisfied_by_tool=None,
        satisfied_by_cmd_regex=None, created_by="t", mandatory=False,
        json=True,
    )
    obl.cmd_add(args)
    data = obl._read_only()
    row = data["obligations"][0]
    assert row.get("drain_before_dispatch") is False


if __name__ == "__main__":
    import sys
    sys.exit(pytest.main([__file__, "-v"]))
