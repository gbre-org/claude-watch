#!/usr/bin/env python3
"""Regression tests for ``_is_subagent_context`` / ``is_main_loop`` /
``all_of`` under the FAIL-SAFE-TOWARD-SUBAGENT combinator.

Background (the recurring bug this pins): the botchat mark-read gate is wired
as ``all_of [is_main_loop, evaluator botchat-unread-check]`` -- it must fire
ONLY in the MAIN LOOP and NEVER inside a subagent. It leaked into subagents
THREE times (operator reports botchat #1232, #1492, #1658/#1659) because the
main-loop-vs-subagent decision mis-classified subagent tool calls as main loop.

The regressed root cause (verified against Claude Code 2.1.209): Claude Code
builds the hook payload as ``agent_type: ctx?.agentType ?? mainThreadAgentType``
-- a subagent whose per-call context lacks its own ``agentType`` gets the MAIN
THREAD's ``agent_type`` (``repl_main_thread`` / ``sdk``) even though it carries a
non-empty subagent ``agent_id``. The previous "agent_type is PRIMARY; classify
'main' => NOT a subagent regardless of agent_id" logic therefore returned "main
loop" for a genuine subagent -> the gate fired in the subagent -> leak.

The fix makes a NON-EMPTY ``agent_id`` authoritative for "subagent" (matching
Claude Code's own payload-schema instruction: "Use this field (not agent_type)
to distinguish subagent calls from main-thread calls"), OR'd with the
``agent_type``-subagent classification (so in-process teammates, which carry no
agent_id, are still caught by their bare-slug agent_type). Main loop is returned
ONLY when BOTH signals are clean.

Loads the ``obligations`` CLI (no .py suffix) as a module via importlib, same
pattern as ``test_tool_pattern_matches.py``.

Run::

    uv run --python 3.11 --with pytest \\
        pytest tools/obligations/tests/test_is_subagent_context.py -v
"""

import importlib.machinery
import importlib.util
from pathlib import Path

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
is_sub = obl._is_subagent_context
classify = obl._classify_agent_type
ev = obl._eval_predicate


# --- _classify_agent_type (unchanged helper, pinned) ---

def test_classify_main_thread():
    assert classify("repl_main_thread") == "main"
    assert classify("repl_main_thread:outputStyle:default") == "main"
    assert classify("sdk") == "main"


def test_classify_subagent_slug():
    assert classify("general-purpose") == "subagent"
    assert classify("Explore") == "subagent"
    assert classify("claude") == "subagent"
    assert classify("agent:abc123") == "subagent"
    assert classify("claude-container:README") == "subagent"


def test_classify_empty_is_none():
    assert classify("") is None
    assert classify(None) is None
    assert classify("   ") is None


# --- _is_subagent_context: the combinator ---

def test_main_loop_clean_both_signals():
    # No agent_id AND agent_type is main => main loop.
    assert is_sub("", "repl_main_thread") is False
    assert is_sub("", "sdk") is False
    assert is_sub(None, "repl_main_thread") is False


def test_bare_cli_no_signals_is_main_loop():
    # Direct CLI use / tests: neither signal => main loop (historical default).
    assert is_sub("", "") is False
    assert is_sub(None, None) is False
    assert is_sub("", None) is False


def test_subagent_via_agent_type_slug_no_agent_id():
    # In-process teammate: no agent_id, bare-slug agent_type => subagent.
    assert is_sub("", "general-purpose") is True
    assert is_sub("", "Explore") is True
    assert is_sub(None, "claude") is True


def test_subagent_via_agent_id_only():
    # Legacy shape: agent_id present, agent_type absent => subagent.
    assert is_sub("agent-abc", "") is True
    assert is_sub("agent-abc", None) is True


def test_THE_LEAK_agent_id_present_but_agent_type_fell_back_to_main():
    # THE REGRESSION: Claude Code 2.1.209 sends a subagent tool call with a
    # non-empty agent_id AND agent_type='repl_main_thread' (the payload builder
    # falls back to mainThreadAgentType when the subagent's per-call context
    # carries no agentType). This MUST be classified as a subagent -- the
    # previous agent_type-primary logic wrongly said "main loop" here, firing
    # the botchat gate inside the subagent.
    assert is_sub("agent-abc", "repl_main_thread") is True
    assert is_sub("agent-abc", "repl_main_thread:outputStyle:default") is True
    assert is_sub("agent-abc", "sdk") is True


def test_agent_id_authoritative_even_with_subagent_type():
    # agent_id present AND a subagent slug => subagent (belt and suspenders).
    assert is_sub("agent-abc", "general-purpose") is True


# --- is_main_loop predicate wraps _is_subagent_context ---

def _is_main_loop(agent_id, agent_type):
    ok, _why = ev({"kind": "is_main_loop", "params": {}},
                  "Bash", "echo hi",
                  agent_id=agent_id, agent_type=agent_type)
    return ok


def test_is_main_loop_predicate_matches_context():
    # Genuine main loop => is_main_loop satisfied.
    assert _is_main_loop("", "repl_main_thread") is True
    # The leak case => is_main_loop must NOT be satisfied (it's a subagent).
    assert _is_main_loop("agent-abc", "repl_main_thread") is False
    # Subagent slug => not main loop.
    assert _is_main_loop("", "general-purpose") is False


# --- end-to-end: all_of [is_main_loop, <deny-child>] must NOT fire in subagent ---

def _all_of_main_loop_gate(agent_id, agent_type):
    """Model the botchat gate: all_of [is_main_loop, evaluator(exit!=0)].

    The second child is an ``evaluator`` whose command exits non-zero, so it
    ALWAYS fails (mirrors ``botchat-unread-check`` reporting unread mail). It
    is deliberately NOT an ``is_main_loop`` child, so it does NOT trip the
    all_of scope-guard short-circuit (which only fires on a failing
    ``is_main_loop`` child).

      - MAIN LOOP: first child (is_main_loop) PASSES => evaluated to the
        second child, which FAILS => outer all_of DENIES (ok=False). Proves
        the gate is live in the main loop.
      - SUBAGENT: first child (is_main_loop) FAILS => scope-guard
        short-circuit => the whole all_of is INACTIVE (satisfied/ALLOW,
        ok=True). Proves NO leak into the subagent.
    """
    pred = {
        "kind": "all_of",
        "params": {
            "predicates": [
                {"kind": "is_main_loop", "params": {}},
                {"kind": "evaluator", "params": {
                    "cmd": "exit 1",
                    "decision_mode": "exit_code",
                    "allow_on_zero_exit": True,
                    "timeout_ms": 2000,
                }},
            ]
        },
    }
    ok, why = ev(pred, "Bash", "echo hi",
                 agent_id=agent_id, agent_type=agent_type)
    return ok, why


def test_gate_active_in_main_loop_but_inactive_in_subagent():
    # MAIN LOOP: first child (is_main_loop) passes, second child
    # (is_main_loop negate) FAILS => outer all_of DENIES (ok=False). The gate
    # is live in the main loop.
    ok_main, _ = _all_of_main_loop_gate("", "repl_main_thread")
    assert ok_main is False

    # SUBAGENT (the leak case): first child (is_main_loop) FAILS => scope-guard
    # short-circuit => outer all_of is INACTIVE => ALLOW (ok=True). NO LEAK.
    ok_leak, why_leak = _all_of_main_loop_gate("agent-abc", "repl_main_thread")
    assert ok_leak is True, why_leak
    assert "scope-guard" in why_leak

    # SUBAGENT via bare slug (in-process teammate): same -- no leak.
    ok_slug, why_slug = _all_of_main_loop_gate("", "general-purpose")
    assert ok_slug is True, why_slug
    assert "scope-guard" in why_slug
