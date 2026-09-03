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


# ---------------------------------------------------------------------------
# MonitorArm: the monitor-mode cure exemption (sibling of the recovery CLIs)
# ---------------------------------------------------------------------------
#
# For a `mode=monitor` watcher `watcher-ctl run <name>` does not exec anything
# -- it prints the Monitor-tool command the main loop must ARM. That Monitor
# call is the monitor-mode form of `watcher-ctl run`, i.e. the CURE for a DOWN
# monitor-mode watcher. 2026-08-21: the `watchers_healthy` gate denied exactly
# that call ("watchers unhealthy -- restart them first") -- a gate blocking its
# own cure, the same deadlock class this file exists for. `MonitorArm` is the
# tool_pattern token that lets the arm through: the Monitor tool whose command
# is EXACTLY (whitespace / trailing ` 2>&1` aside) the monitor_cmd of an
# ENABLED monitor-mode watcher per `watcher-ctl list --json`. It sits in BOTH
# the shared DEFAULT_EXEMPT_PATTERNS allowlist and the obligations CLI's
# universal recovery floor, and is fail-CLOSED (no watcher-ctl => no match).

OBLIGATIONS_CLI = OBLIGATIONS_DIR / "obligations"


def _load_obligations_module():
    """Import the obligations CLI script (no .py suffix) as a module."""
    spec = importlib.util.spec_from_loader(
        "obligations_cli_mod",
        importlib.machinery.SourceFileLoader(
            "obligations_cli_mod", str(OBLIGATIONS_CLI)),
    )
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


MONITOR_CMD = "/opt/claude-container/watchers/claude-event-watch.sh --mode monitor"

WATCHER_LIST_JSON = [
    {"name": "claude-event-watch", "pattern": "x", "min_count": 1,
     "enabled": True,
     "start_cmd": "/opt/claude-container/watchers/claude-event-watch.sh",
     "mode": "monitor", "monitor_cmd": MONITOR_CMD,
     "layer": "base", "overridden": ["mode"]},
    {"name": "disabled-mon", "pattern": "d", "min_count": 1, "enabled": False,
     "start_cmd": "disabled-mon", "mode": "monitor",
     "monitor_cmd": "disabled-mon --mode monitor",
     "layer": "override", "overridden": []},
    {"name": "botchat-wait", "pattern": "b", "min_count": 1, "enabled": True,
     "start_cmd": "botchat-wait", "mode": "oneshot",
     "monitor_cmd": "botchat-wait --mode monitor",
     "layer": "base", "overridden": []},
]


def _write_fake_watcher_ctl(bin_dir: Path, body=None):
    bin_dir.mkdir(parents=True, exist_ok=True)
    script = bin_dir / "watcher-ctl"
    if body is None:
        body = (
            "#!/bin/sh\n"
            "[ \"$1\" = list ] || exit 1\n"
            "cat <<'EOF'\n" + json.dumps(WATCHER_LIST_JSON) + "\nEOF\n"
        )
    script.write_text(body)
    script.chmod(0o755)
    return script


def _sandbox_env(tmp_path: Path, bin_dir: Path) -> dict:
    env = os.environ.copy()
    env["HOME"] = str(tmp_path / "home")
    (tmp_path / "home").mkdir(exist_ok=True)
    env["PATH"] = f"{bin_dir}{os.pathsep}{OBLIGATIONS_DIR}{os.pathsep}{env.get('PATH', '')}"
    env["OBLIGATIONS_DISABLE_PINGME"] = "1"
    env["OBLIGATIONS_DISABLE_CLAUDE_EVENT"] = "1"
    return env


def _cli(env, *args):
    return subprocess.run(
        [str(OBLIGATIONS_CLI), *args], capture_output=True, text=True,
        env=env, timeout=20, check=False,
    )


def _check_rc(env, cmd, tool="Monitor"):
    cs = (json.dumps({"command": cmd, "description": "x", "persistent": True})
          if tool == "Monitor" else cmd)
    return _cli(env, "check", "--tool", tool, "--command-string", cs,
                "--json").returncode


def test_default_exempt_patterns_includes_monitor_arm():
    """The shared allowlist carries the monitor-mode cure next to the
    recovery CLIs -- so every gate seeded from it exempts the arm."""
    mod = _load_init_module()
    assert "MonitorArm" in mod.DEFAULT_EXEMPT_PATTERNS


def test_universal_recovery_floor_includes_monitor_arm():
    """Rows NOT seeded by obligations-init (e.g. an operator-registered
    `watchers_healthy` gate) are covered by the framework floor."""
    ob = _load_obligations_module()
    assert ob.MONITOR_ARM_PATTERN == "MonitorArm"
    assert ob.MONITOR_ARM_PATTERN in ob.UNIVERSAL_RECOVERY_EXEMPT_PATTERNS
    # It must be a recognised tool_pattern token, not a bare tool name that
    # would only match a tool literally called "MonitorArm".
    assert not ob._tool_pattern_matches("MonitorArm", "MonitorArm", "")


def test_normalize_monitor_cmd_collapses_whitespace_and_trailing_redirect():
    ob = _load_obligations_module()
    n = ob._normalize_monitor_cmd
    assert n("a  --mode   monitor 2>&1") == "a --mode monitor"
    assert n("  a --mode monitor  ") == "a --mode monitor"
    assert n("a --mode monitor 2>&1 2>&1") == "a --mode monitor"
    # A redirect elsewhere is part of the command (not stripped).
    assert n("a 2>&1 --x") == "a 2>&1 --x"
    assert n("") is None
    assert n(None) is None
    assert n(42) is None


def test_monitor_arm_pattern_matches_only_configured_monitor_cmd(monkeypatch):
    """Pure matcher semantics with the config lookup stubbed: exact match on
    the Monitor tool's command (JSON tool_input or raw string), nothing
    else."""
    ob = _load_obligations_module()
    monkeypatch.setattr(ob, "_configured_monitor_cmds",
                        lambda: {"x --mode monitor": "x"})
    m = ob._tool_pattern_matches
    payload = json.dumps({"command": "x --mode monitor 2>&1",
                          "description": "x", "persistent": True})
    assert m("MonitorArm", "Monitor", payload)
    assert m("MonitorArm", "Monitor", "x --mode monitor")          # raw form
    assert m("MonitorArm", "Monitor", "x   --mode monitor  2>&1")  # sloppy ws
    assert not m("MonitorArm", "Monitor", json.dumps({"command": "sleep 9"}))
    assert not m("MonitorArm", "Monitor",
                 json.dumps({"command": "x --mode monitor --more"}))
    assert not m("MonitorArm", "Monitor",
                 json.dumps({"command": "y x --mode monitor"}))
    assert not m("MonitorArm", "Bash", "x --mode monitor")         # wrong tool
    assert not m("MonitorArm", "Monitor", "")
    assert not m("MonitorArm", "Monitor", json.dumps({"task_id": "t"}))
    # Empty config (no watcher-ctl / nothing in monitor mode) => never matches.
    monkeypatch.setattr(ob, "_configured_monitor_cmds", lambda: {})
    assert not m("MonitorArm", "Monitor", payload)


def test_configured_monitor_cmds_reads_enabled_monitor_entries_only(
        tmp_path, monkeypatch):
    """The resolver takes `watcher-ctl list --json` and keeps ONLY enabled
    mode=monitor entries; a oneshot watcher's derived monitor_cmd and a
    disabled monitor watcher are not cures."""
    ob = _load_obligations_module()
    bin_dir = tmp_path / "bin"
    _write_fake_watcher_ctl(bin_dir)
    monkeypatch.setenv("PATH", f"{bin_dir}{os.pathsep}{os.environ.get('PATH', '')}")
    assert ob._configured_monitor_cmds() == {MONITOR_CMD: "claude-event-watch"}
    # Fail-closed: non-zero exit / garbage output => {}.
    _write_fake_watcher_ctl(bin_dir, "#!/bin/sh\nexit 3\n")
    assert ob._configured_monitor_cmds() == {}
    _write_fake_watcher_ctl(bin_dir, "#!/bin/sh\necho not-json\n")
    assert ob._configured_monitor_cmds() == {}


def test_monitor_arm_applies_at_gate_eval_time(tmp_path):
    """END-TO-END through the real CLI (`obligations check`), not just the
    matcher: with a `*` gate FIRING, the Monitor arm of the configured
    monitor_cmd is allowed (exit 0) while any other Monitor command and an
    unrelated Bash call are still blocked (exit 2). This is the universal
    floor -- no per-row exempt list needed."""
    bin_dir = tmp_path / "bin"
    _write_fake_watcher_ctl(bin_dir)
    env = _sandbox_env(tmp_path, bin_dir)
    add = _cli(env, "add", "--tool-pattern", "*", "--predicate", "file_exists",
               "--params", json.dumps({"path": str(tmp_path / "missing")}),
               "--ttl", "300", "--deny-msg", "unrelated gate", "--json")
    assert add.returncode == 0, add.stderr
    assert _check_rc(env, MONITOR_CMD + " 2>&1") == 0
    assert _check_rc(env, MONITOR_CMD) == 0
    assert _check_rc(env, "sleep 3600") == 2
    assert _check_rc(env, "botchat-wait --mode monitor 2>&1") == 2
    assert _check_rc(env, "disabled-mon --mode monitor 2>&1") == 2
    assert _check_rc(env, "ls", tool="Bash") == 2
    # Fail-closed at eval time too: watcher-ctl breaks => the arm is gated.
    _write_fake_watcher_ctl(bin_dir, "#!/bin/sh\nexit 1\n")
    assert _check_rc(env, MONITOR_CMD + " 2>&1") == 2


def test_default_exempt_patterns_are_honored_at_gate_eval_time(tmp_path):
    """The cross-gate mechanism is per-row `exempt_patterns` seeded from
    DEFAULT_EXEMPT_PATTERNS -- prove the CLI honours such a row at EVAL time:
    a failing gate registered with DEFAULT_EXEMPT_PATTERNS lets every
    recovery CLI through and still blocks an unrelated command. (Seeding the
    list is the necessary half; this is the sufficiency half.)"""
    mod = _load_init_module()
    bin_dir = tmp_path / "bin"
    _write_fake_watcher_ctl(bin_dir)
    env = _sandbox_env(tmp_path, bin_dir)
    args = ["add", "--tool-pattern", "*", "--predicate", "file_exists",
            "--params", json.dumps({"path": str(tmp_path / "missing")}),
            "--ttl", "300", "--deny-msg", "gate with default exempts", "--json"]
    for pat in mod.DEFAULT_EXEMPT_PATTERNS:
        args += ["--exempt-tool-pattern", pat]
    add = _cli(env, *args)
    assert add.returncode == 0, add.stderr
    # Universal-floor commands pass regardless; the ones ONLY the per-row
    # list covers are the real proof: agent-ack / event-ack / event-classify
    # / claude-watch-ack / agent-msg / agent-tail.
    for cmd in ("agent-ack list", "event-ack ack 1", "event-classify x",
                "claude-watch-ack 1", "agent-msg inbox", "agent-tail abc",
                "session-task queue status", "obligations list"):
        assert _check_rc(env, cmd, tool="Bash") == 0, (
            f"{cmd!r} should be exempt via DEFAULT_EXEMPT_PATTERNS")
    assert _check_rc(env, MONITOR_CMD + " 2>&1") == 0
    assert _check_rc(env, "ls", tool="Bash") == 2, "unrelated command must still be gated"
    assert _check_rc(env, "sleep 1") == 2
