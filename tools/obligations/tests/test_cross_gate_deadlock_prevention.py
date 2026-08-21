#!/usr/bin/env python3
"""Tests for cross-gate deadlock prevention via shared recovery-CLI exemptions.

PROBLEM (botchat #5168): the two gate obligations `agent_ack_pending` and
`event_must_act` each DENY non-exempt tool calls while armed, but NEITHER's
exempt-set included the OTHER's clearing command. When both were armed
simultaneously: `agent-ack` (which clears agent_ack_pending) was blocked by
event_must_act, and `event-ack` (which clears event_must_act) was blocked by
agent_ack_pending → neither could be cleared without `obligations override`.

FIX: `DEFAULT_EXEMPT_PATTERNS` (the shared recovery-CLI allowlist) MUST include
the clearing commands for EVERY gate-mode obligation (agent-ack, event-ack,
event-classify, claude-watch-ack, session-task queue, obligations, agent-msg,
agent-tail). Every gate-mode obligation MUST apply these exemptions, so no gate
can ever block the command that clears another gate.

This test asserts:
1. The DEFAULT_EXEMPT_PATTERNS list includes all known recovery CLIs
2. Every evaluator-backed gate (event_must_act, agent_ack_pending,
   queue_ready_unspawned) seeds with those exemptions
3. The subagent_queue_item_running gate also uses the same exemptions

Run::

    uv run --python 3.11 --with pytest \\
        pytest tools/obligations/tests/test_cross_gate_deadlock_prevention.py -v
"""

import importlib.machinery
import importlib.util
import json
import os
import re
import subprocess
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
OBLIGATIONS_DIR = HERE.parent
OBLIGATIONS_INIT = OBLIGATIONS_DIR / "obligations-init"


def _load_init_module():
    """Import the obligations-init script as a module (it has no .py suffix)."""
    spec = importlib.util.spec_from_loader(
        "obligations_init_mod",
        importlib.machinery.SourceFileLoader(
            "obligations_init_mod", str(OBLIGATIONS_INIT)),
    )
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


# The recovery CLIs that MUST be in DEFAULT_EXEMPT_PATTERNS and exempt
# from every gate. This is the governing set from the fix specification.
REQUIRED_RECOVERY_CLIS = [
    "session-task",      # queue inspection/registration (status/spawn-check/register/show/list)
    "obligations",       # escape hatch (list/show/status/check/override/satisfy)
    "claude-watch-ack",  # alert ack surface
    "claude-watch-dispatch",
    "agent-msg",         # inbox surface (ack/inbox/gc/disarm)
    "agent-tail",        # transcript inspection
    "agent-ack",         # CLEARS agent_ack_pending gate
    "event-ack",         # CLEARS event_must_act gate
    "event-classify",    # event triage
]


def test_default_exempt_patterns_includes_all_recovery_clis():
    """DEFAULT_EXEMPT_PATTERNS must include regex patterns that match all
    required recovery CLIs. This is the source-of-truth allowlist every gate
    MUST apply."""
    mod = _load_init_module()
    patterns = mod.DEFAULT_EXEMPT_PATTERNS

    assert isinstance(patterns, list), "DEFAULT_EXEMPT_PATTERNS must be a list"
    assert len(patterns) > 0, "DEFAULT_EXEMPT_PATTERNS must not be empty"

    # Collapse all patterns into one combined regex for simplicity. In reality
    # each pattern is its own --exempt-tool-pattern arg, so they're OR'd.
    # We'll check that each required CLI appears in at least one pattern.
    combined = " ".join(patterns)

    missing = []
    for cli in REQUIRED_RECOVERY_CLIS:
        # Check if the CLI name appears in any pattern. The actual patterns
        # are anchored regexes (e.g., r"Bash:^agent-ack\s+..."), so a
        # substring search for the CLI name is sufficient.
        if cli not in combined:
            missing.append(cli)

    assert not missing, (
        f"DEFAULT_EXEMPT_PATTERNS is missing recovery CLIs: {missing}. "
        "Every gate-clearing command must be in the shared allowlist to "
        "prevent cross-gate deadlock. See botchat #5168."
    )


def test_evaluator_gates_all_use_default_exempt_patterns(tmp_path, monkeypatch):
    """Every evaluator-backed gate seeder (event_must_act, agent_ack_pending,
    queue_ready_unspawned) must register with DEFAULT_EXEMPT_PATTERNS applied,
    so every gate exempts every other gate's clearing commands."""
    mod = _load_init_module()

    # We'll intercept `obligations add` calls and capture the --exempt-tool-pattern
    # args passed to each gate. A gate that seeds without DEFAULT_EXEMPT_PATTERNS
    # is a regression.
    captured = {}

    def fake_run(argv, *a, **kw):
        class R:
            returncode = 0
            stdout = '{"id": "ob-test-001"}'
            stderr = ""
        # argv is [cli, "add", "--tool-pattern", ..., "--exempt-tool-pattern", ...]
        # Extract the deny_msg tag to identify which gate this is.
        try:
            deny_idx = argv.index("--deny-msg")
            deny_msg = argv[deny_idx + 1] if deny_idx + 1 < len(argv) else ""
        except (ValueError, IndexError):
            deny_msg = ""

        # Collect all --exempt-tool-pattern values.
        exempts = []
        i = 0
        while i < len(argv):
            if argv[i] == "--exempt-tool-pattern" and i + 1 < len(argv):
                exempts.append(argv[i + 1])
                i += 2
            else:
                i += 1

        captured[deny_msg] = exempts
        return R()

    # Also stub _row_already_present to always return False so the seeders
    # actually fire the `obligations add` call.
    monkeypatch.setattr(mod, "_row_already_present", lambda *a, **kw: False)
    monkeypatch.setattr(mod.subprocess, "run", fake_run)

    # Seed the three evaluator-backed gates that must have DEFAULT_EXEMPT_PATTERNS.
    mod.seed_event_must_act("obligations", dry_run=False, force=False, verbose=False)
    mod.seed_agent_ack_pending("obligations", dry_run=False, force=False, verbose=False)
    mod.seed_queue_ready_unspawned("obligations", dry_run=False, force=False, verbose=False)

    # Verify each gate was registered with exempts.
    gates_under_test = [
        (mod.EVENT_MUST_ACT_TAG, "event_must_act"),
        (mod.AGENT_ACK_PENDING_TAG, "agent_ack_pending"),
        (mod.QUEUE_READY_UNSPAWNED_TAG, "queue_ready_unspawned"),
    ]

    for tag, name in gates_under_test:
        exempts = captured.get(tag, [])
        assert len(exempts) > 0, (
            f"{name} gate was seeded with ZERO --exempt-tool-pattern args. "
            f"It must include DEFAULT_EXEMPT_PATTERNS to prevent cross-gate deadlock."
        )

        # Collapse the exempts into one string for substring search (same
        # approach as test_default_exempt_patterns_includes_all_recovery_clis).
        combined = " ".join(exempts)

        # Check that the PRIMARY recovery CLIs appear. We don't demand byte-for-byte
        # equality with DEFAULT_EXEMPT_PATTERNS (agent_ack_pending has additional
        # botchat exempts, which is fine), but the CRITICAL cross-gate recovery
        # CLIs must be present.
        critical = ["agent-ack", "event-ack", "event-classify",
                    "session-task", "obligations"]
        missing = [cli for cli in critical if cli not in combined]

        assert not missing, (
            f"{name} gate is missing critical recovery-CLI exemptions: {missing}. "
            f"This will cause cross-gate deadlock when this gate and another are "
            f"armed simultaneously. The gate must exempt DEFAULT_EXEMPT_PATTERNS. "
            f"See botchat #5168."
        )


def test_subagent_queue_item_running_also_uses_default_exempt_patterns(
        tmp_path, monkeypatch):
    """The subagent_queue_item_running gate (the subagent lifetime gate) must
    also apply DEFAULT_EXEMPT_PATTERNS so a subagent hitting this gate can
    still run recovery CLIs (agent-ack, event-ack, etc.) to diagnose the issue."""
    mod = _load_init_module()

    captured_exempts = []

    def fake_run(argv, *a, **kw):
        class R:
            returncode = 0
            stdout = '{"id": "ob-test-001"}'
            stderr = ""
        i = 0
        while i < len(argv):
            if argv[i] == "--exempt-tool-pattern" and i + 1 < len(argv):
                captured_exempts.append(argv[i + 1])
                i += 2
            else:
                i += 1
        return R()

    monkeypatch.setattr(mod, "_row_already_present", lambda *a, **kw: False)
    monkeypatch.setattr(mod.subprocess, "run", fake_run)

    mod.seed_subagent_queue_item_running(
        "obligations", dry_run=False, force=False, verbose=False)

    assert len(captured_exempts) > 0, (
        "subagent_queue_item_running gate was seeded with ZERO exempts. "
        "It must include DEFAULT_EXEMPT_PATTERNS."
    )

    combined = " ".join(captured_exempts)
    critical = ["agent-ack", "event-ack", "session-task", "obligations"]
    missing = [cli for cli in critical if cli not in combined]

    assert not missing, (
        f"subagent_queue_item_running gate is missing recovery-CLI exemptions: "
        f"{missing}. A subagent hitting this gate must still be able to run "
        f"recovery CLIs. It must exempt DEFAULT_EXEMPT_PATTERNS."
    )


def test_agent_ack_and_event_ack_mutually_exempt(tmp_path, monkeypatch):
    """The core deadlock scenario: agent_ack_pending must exempt event-ack,
    and event_must_act must exempt agent-ack. This is the direct fix for
    botchat #5168."""
    mod = _load_init_module()

    captured = {}

    def fake_run(argv, *a, **kw):
        class R:
            returncode = 0
            stdout = '{"id": "ob-test-001"}'
            stderr = ""
        try:
            deny_idx = argv.index("--deny-msg")
            deny_msg = argv[deny_idx + 1] if deny_idx + 1 < len(argv) else ""
        except (ValueError, IndexError):
            deny_msg = ""

        exempts = []
        i = 0
        while i < len(argv):
            if argv[i] == "--exempt-tool-pattern" and i + 1 < len(argv):
                exempts.append(argv[i + 1])
                i += 2
            else:
                i += 1

        captured[deny_msg] = exempts
        return R()

    monkeypatch.setattr(mod, "_row_already_present", lambda *a, **kw: False)
    monkeypatch.setattr(mod.subprocess, "run", fake_run)

    mod.seed_event_must_act("obligations", dry_run=False, force=False, verbose=False)
    mod.seed_agent_ack_pending("obligations", dry_run=False, force=False, verbose=False)

    event_must_act_exempts = " ".join(captured.get(mod.EVENT_MUST_ACT_TAG, []))
    agent_ack_pending_exempts = " ".join(captured.get(mod.AGENT_ACK_PENDING_TAG, []))

    assert "agent-ack" in event_must_act_exempts, (
        "event_must_act gate must exempt agent-ack (the command that clears "
        "agent_ack_pending). Without this, a simultaneously-armed "
        "agent_ack_pending blocks agent-ack, deadlocking the two gates. "
        "See botchat #5168."
    )

    assert "event-ack" in agent_ack_pending_exempts, (
        "agent_ack_pending gate must exempt event-ack (the command that clears "
        "event_must_act). Without this, a simultaneously-armed event_must_act "
        "blocks event-ack, deadlocking the two gates. See botchat #5168."
    )
