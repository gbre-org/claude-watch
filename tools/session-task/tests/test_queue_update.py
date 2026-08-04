#!/usr/bin/env python3
"""Tests for `queue update` -- in-place edit of a NOT-YET-SPAWNED item's spec.

Motivation (Andrew #2980): the main loop kept abandon+re-adding queue items
just to fix a task's description/prompt when requirements were clarified
mid-flight. `queue update <id>` edits description/summary/scope in place,
preserving item identity + audit trail, and REFUSES when a LIVE spawned agent
is bound to the item (editing a running agent's spec is meaningless).

Covers:
  * update --description / --summary / --scope edit a pending item in place.
  * update --desc-file (file and stdin) sets the description.
  * update REPLACES the scope list and recomputes readiness ordering.
  * update requires at least one field; --description/--desc-file exclusive;
    empty summary/description rejected.
  * update on a missing id exits 1; on done/abandoned exits 1.
  * GUARD: update REFUSES (exit 2) when a LIVE agent is bound to the item,
    and --force overrides it.
  * A registered-but-NOT-yet-spawned item (running status, no live agent
    record) is a VALID update target.

Run:
    uv run --python 3.11 --with pytest \\
        pytest tests/test_queue_update.py -v

Or directly:
    python3 tests/test_queue_update.py
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
    # Point active-agents state at a per-test file (starts absent => no live
    # agents) and disable the binary fallback so the developer host's real
    # claude-watch never bleeds in.
    env["CLAUDE_AGENTS_STATE"] = str(Path(tmp, "active-agents.json"))
    env["CLAUDE_AGENTS_STATE_FALLBACK_BIN"] = ""
    Path(tmp, ".config/session").mkdir(parents=True, exist_ok=True)
    return env


def _run(env, *argv, timeout=15, stdin=None):
    cmd = [sys.executable, str(SESSION_TASK)] + list(argv)
    return subprocess.run(cmd, capture_output=True, text=True, env=env,
                          timeout=timeout, input=stdin)


def _add(env, desc, scopes, *extra):
    cmd = ["queue", "add", desc, "--json"]
    for s in scopes:
        cmd.extend(["--scope", s])
    cmd.extend(extra)
    return json.loads(_run(env, *cmd).stdout)


def _show(env, qid):
    return json.loads(_run(env, "queue", "show", qid).stdout)


def _write_live_agent_state(env, qid, agent_id="agentxyz", alive=True):
    """Simulate claude-watch active-agents state binding a live agent to qid."""
    state = {"agents": [{"queue_id": qid, "agent_id": agent_id,
                         "alive": alive, "jsonl_age_seconds": 1}]}
    Path(env["CLAUDE_AGENTS_STATE"]).write_text(json.dumps(state))


# ---------------------------------------------------------------------------
# 1. Basic in-place edits.
# ---------------------------------------------------------------------------


def test_update_description_and_summary():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        a = _add(env, "old desc", ["repo:foo"], "--summary", "old summary")
        r = _run(env, "queue", "update", a["id"],
                 "--description", "new desc", "--summary", "new summary",
                 "--json")
        assert r.returncode == 0, r.stderr
        out = json.loads(r.stdout)
        assert out["changes"]["description"]["new"] == "new desc"
        assert out["changes"]["summary"]["new"] == "new summary"
        # Identity preserved.
        assert out["item"]["id"] == a["id"]
        assert "spec_updated_at" in out["item"]
        shown = _show(env, a["id"])
        assert shown["description"] == "new desc"
        assert shown["summary"] == "new summary"


def test_update_desc_file_and_stdin():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        a = _add(env, "old", ["repo:foo"])
        f = Path(tmp, "prompt.txt")
        f.write_text("multi\nline\nprompt\n")
        r = _run(env, "queue", "update", a["id"], "--desc-file", str(f))
        assert r.returncode == 0, r.stderr
        assert _show(env, a["id"])["description"] == "multi\nline\nprompt"

        b = _add(env, "old2", ["repo:bar"])
        r = _run(env, "queue", "update", b["id"], "--desc-file", "-",
                 stdin="from stdin")
        assert r.returncode == 0, r.stderr
        assert _show(env, b["id"])["description"] == "from stdin"


def test_update_scope_replaces_and_reorders():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        # Two items in the SAME scope: a is ready, b serialized behind it.
        a = _add(env, "A", ["repo:foo"])
        b = _add(env, "B", ["repo:foo"])
        assert a["ready_now"] is True and b["ready_now"] is False
        # Move b to a DISJOINT scope -> it leaves a's group and becomes ready.
        r = _run(env, "queue", "update", b["id"], "--scope", "repo:bar",
                 "--json")
        assert r.returncode == 0, r.stderr
        out = json.loads(r.stdout)
        assert out["changes"]["scope"]["new"] == ["repo:bar"]
        scb, rcb = json.loads(
            _run(env, "queue", "spawn-check", b["id"], "--json").stdout
        ), None
        assert scb["ok"] is True


# ---------------------------------------------------------------------------
# 2. Argument validation.
# ---------------------------------------------------------------------------


def test_update_requires_a_field():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        a = _add(env, "A", ["repo:foo"])
        r = _run(env, "queue", "update", a["id"])
        assert r.returncode == 1
        assert "nothing to update" in r.stderr


def test_update_desc_and_desc_file_exclusive():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        a = _add(env, "A", ["repo:foo"])
        r = _run(env, "queue", "update", a["id"],
                 "--description", "x", "--desc-file", "-")
        assert r.returncode == 1
        assert "mutually exclusive" in r.stderr


def test_update_empty_summary_rejected():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        a = _add(env, "A", ["repo:foo"])
        r = _run(env, "queue", "update", a["id"], "--summary", "   ")
        assert r.returncode == 1
        assert "summary must be non-empty" in r.stderr


def test_update_missing_id():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        r = _run(env, "queue", "update", "q-nope", "--summary", "x")
        assert r.returncode == 1
        assert "not found" in r.stderr


# ---------------------------------------------------------------------------
# 3. Terminal-state refusal.
# ---------------------------------------------------------------------------


def test_update_refuses_done_and_abandoned():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        a = _add(env, "A", ["repo:foo"])
        _run(env, "queue", "register", a["id"])
        _run(env, "queue", "done", a["id"])
        r = _run(env, "queue", "update", a["id"], "--summary", "x")
        assert r.returncode == 1
        assert "nothing to correct on a finished item" in r.stderr

        b = _add(env, "B", ["repo:bar"])
        _run(env, "queue", "abandon", b["id"], "--reason", "nope")
        r = _run(env, "queue", "update", b["id"], "--summary", "x", "--force")
        assert r.returncode == 1  # --force does NOT override terminal state


# ---------------------------------------------------------------------------
# 4. Live-agent guard.
# ---------------------------------------------------------------------------


def test_registered_not_yet_spawned_is_valid_target():
    """Registered (running) with NO live agent record => editable."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        a = _add(env, "A", ["repo:foo"])
        _run(env, "queue", "register", a["id"])
        # No active-agents state file => no live agent bound.
        r = _run(env, "queue", "update", a["id"], "--summary", "clarified")
        assert r.returncode == 0, r.stderr
        assert _show(env, a["id"])["summary"] == "clarified"


def test_update_refuses_live_spawned_agent():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        a = _add(env, "A", ["repo:foo"])
        _run(env, "queue", "register", a["id"])
        _write_live_agent_state(env, a["id"], alive=True)
        r = _run(env, "queue", "update", a["id"], "--summary", "x")
        assert r.returncode == 2
        assert "LIVE SPAWNED AGENT" in r.stderr
        # Spec unchanged.
        assert _show(env, a["id"])["summary"] != "x"


def test_update_force_overrides_live_agent():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        a = _add(env, "A", ["repo:foo"])
        _run(env, "queue", "register", a["id"])
        _write_live_agent_state(env, a["id"], alive=True)
        r = _run(env, "queue", "update", a["id"], "--summary", "forced",
                 "--force")
        assert r.returncode == 0, r.stderr
        assert _show(env, a["id"])["summary"] == "forced"


def test_dead_agent_binding_does_not_block():
    """A bound-but-DEAD agent (alive=false) is not a live spawn => editable."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        a = _add(env, "A", ["repo:foo"])
        _run(env, "queue", "register", a["id"])
        _write_live_agent_state(env, a["id"], alive=False)
        r = _run(env, "queue", "update", a["id"], "--summary", "revived")
        assert r.returncode == 0, r.stderr
        assert _show(env, a["id"])["summary"] == "revived"


if __name__ == "__main__":
    failures = 0
    for name, fn in sorted(globals().items()):
        if name.startswith("test_") and callable(fn):
            try:
                fn()
                print(f"PASS {name}")
            except Exception as e:  # noqa: BLE001
                failures += 1
                print(f"FAIL {name}: {e}")
    sys.exit(1 if failures else 0)
