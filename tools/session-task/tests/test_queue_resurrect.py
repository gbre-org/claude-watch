#!/usr/bin/env python3
"""Tests for the ``session-task queue resurrect`` subcommand.

After Claude Code crashes + restarts, in-flight agents die but their
queue items remain ``running`` (orphaned). PR #255 made claude-watch
correctly report ``queue_id: null`` for those ghost agents.
``queue resurrect <old-id>``:

* refuses unless the old item is in status ``running`` or ``wedged``
* finds the agent transcript by grepping all recently-active
  ``subagents/`` dirs for the literal ``Queue item: <old-id>`` marker
* prefers the OLDEST mtime when multiple matches (post-restart
  continuation transcripts re-cite the marker, but the original spawn
  prompt only lives in the pre-restart file)
* extracts the first user message containing the marker
* creates a new queue item with scope/summary/depends_on/priority
  copied + description prefixed with
  ``## RESURRECTED FROM <old-id> at <timestamp>``
* abandons the old item (default; ``--keep-old`` opts out)
* does NOT auto-register or auto-spawn

All tests run against a temp HOME so the live
``~/.config/session/queue.json`` is never touched.

Run::

    uv run --python 3.11 --with pytest \\
        pytest tools/session-task/tests/test_queue_resurrect.py -v
"""

import json
import os
import subprocess
import sys
import tempfile
import time
from pathlib import Path

SESSION_TASK = Path(__file__).resolve().parent.parent / "session-task"


def _env_for_tmp(tmp):
    tmp = Path(tmp)
    env = os.environ.copy()
    env["HOME"] = str(tmp)
    env["PINGME_SESSION_TASK"] = "0"
    env["CLAUDE_EVENT_SESSION_TASK"] = "0"
    env["QUEUE_LOG_ARCHIVE_DIR"] = str(tmp / "queue-logs")
    env["CLAUDE_AGENTS_STATE"] = str(tmp / "active-agents.json")
    env["CLAUDE_AGENTS_JSONL_ROOT"] = str(tmp / "projects")
    return env


def _run(env, *argv, expect_exit=0):
    cmd = [sys.executable, str(SESSION_TASK)] + list(argv)
    r = subprocess.run(cmd, capture_output=True, text=True, env=env, timeout=15)
    if expect_exit is not None and r.returncode != expect_exit:
        raise RuntimeError(
            f"unexpected exit {r.returncode} (want {expect_exit}): argv={argv}\n"
            f"stdout={r.stdout!r}\nstderr={r.stderr!r}"
        )
    return r


def _add(env, desc, scopes, *extra):
    args = ["queue", "add", desc, "--json"]
    for s in scopes:
        args.extend(["--scope", s])
    args.extend(extra)
    r = _run(env, *args)
    return json.loads(r.stdout)


def _show(env, qid):
    r = _run(env, "queue", "show", qid)
    return json.loads(r.stdout)


def _write_transcript(env, session_uuid, agent_id, lines, *, mtime=None):
    """Write a synthetic agent transcript and (optionally) set its mtime.

    Lines are JSONL records (already-encoded strings or dicts). Returns
    the path written.
    """
    sub = (
        Path(env["CLAUDE_AGENTS_JSONL_ROOT"])
        / session_uuid
        / "subagents"
    )
    sub.mkdir(parents=True, exist_ok=True)
    path = sub / f"agent-{agent_id}.jsonl"
    rendered = []
    for ln in lines:
        rendered.append(ln if isinstance(ln, str) else json.dumps(ln))
    path.write_text("\n".join(rendered) + "\n")
    if mtime is not None:
        os.utime(path, (mtime, mtime))
    return path


def _spawn_user_frame(text):
    """Build the JSONL record shape Claude Code's harness writes for the
    first user message (the agent-tool spawn prompt)."""
    return {
        "type": "user",
        "message": {
            "role": "user",
            "content": [{"type": "text", "text": text}],
        },
    }


def _continuation_first_frame():
    """Build a degraded post-restart first-frame: a tool_result reply to
    the dead parent. This is what the new session-uuid dir contains
    after Claude Code crashes; it does NOT contain the Queue item:
    marker, which is why resurrect must prefer the oldest transcript."""
    return {
        "type": "user",
        "message": {
            "role": "user",
            "content": [
                {
                    "type": "tool_result",
                    "tool_use_id": "toolu_dead",
                    "content": "Exit code 137",
                }
            ],
        },
    }


# ---------------------------------------------------------------------------
# Happy paths
# ---------------------------------------------------------------------------


def test_resurrect_running_item_creates_new_abandons_old():
    """Baseline: resurrect a running item.

    Verifies the new item exists, has the same scope/summary/priority,
    the description carries the RESURRECTED FROM header + the original
    prompt, and the old item is abandoned with a resurrected_as field.
    """
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        item = _add(
            env,
            "do important work",
            ["repo:foo"],
            "--summary",
            "ten word headline",
            "--priority",
            "7",
        )
        old_id = item["id"]
        _run(env, "queue", "register", old_id)

        original_prompt = (
            f"Queue item: {old_id}\n\n"
            "Implement feature X. Read these files first..."
        )
        _write_transcript(
            env,
            session_uuid="sess-original",
            agent_id="aoriginal1234abcd",
            lines=[_spawn_user_frame(original_prompt)],
        )

        r = _run(env, "queue", "resurrect", old_id, "--json")
        out = json.loads(r.stdout)
        new_id = out["id"]
        assert new_id != old_id
        assert out["resurrected_from"] == old_id
        assert out["old_abandoned"] is True
        assert out["summary"] == "ten word headline"
        assert out["priority"] == 7
        assert out["scope"] == ["repo:foo"]

        new_shown = _show(env, new_id)
        assert new_shown["status"] == "pending"
        assert new_shown["resurrected_from"] == old_id
        assert new_shown["resurrect_source_transcript"].endswith(
            "agent-aoriginal1234abcd.jsonl"
        )
        # The description must carry the resurrect banner + original prompt.
        desc = new_shown["description"]
        assert desc.startswith("## RESURRECTED FROM ")
        assert old_id in desc
        assert new_id in desc
        assert "Implement feature X" in desc
        # `created_by` defaults to "resurrect" so audit-trail consumers can
        # filter the lineage.
        assert new_shown["created_by"] == "resurrect"

        old_shown = _show(env, old_id)
        assert old_shown["status"] == "abandoned"
        assert old_shown["resurrected_as"] == new_id
        assert new_id in old_shown.get("abandon_reason", "")


def test_resurrect_with_summary_override():
    """--summary replaces the inherited summary on the new item."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        item = _add(env, "first description", ["repo:bar"], "--summary", "old summary")
        old_id = item["id"]
        _run(env, "queue", "register", old_id)
        _write_transcript(
            env,
            "sess-bar",
            "abardead000000abc",
            [_spawn_user_frame(f"Queue item: {old_id}\nthe prompt")],
        )
        r = _run(
            env,
            "queue",
            "resurrect",
            old_id,
            "--summary",
            "fresh new headline",
            "--json",
        )
        out = json.loads(r.stdout)
        new = _show(env, out["id"])
        assert new["summary"] == "fresh new headline"


def test_resurrect_no_transcript_found_errors_with_hint():
    """No transcript containing the marker: clear error + override hint."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        item = _add(env, "no transcript yet", ["repo:gone"], "--summary", "x")
        old_id = item["id"]
        _run(env, "queue", "register", old_id)
        # No transcript written.
        r = _run(env, "queue", "resurrect", old_id, expect_exit=1)
        assert "no transcript found" in r.stderr
        assert "--from-transcript" in r.stderr


def test_resurrect_prefers_oldest_of_multiple_transcripts():
    """Multiple marker-bearing transcripts: pick the OLDEST mtime.

    The pre-restart transcript carries the canonical spawn prompt.
    A post-restart continuation may re-cite the queue id in a follow-up
    user message but has degraded preceding frames. We MUST pick the
    oldest file so the resurrected prompt is the original spawn.
    """
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        item = _add(env, "needs the original prompt", ["repo:multi"], "--summary", "m")
        old_id = item["id"]
        _run(env, "queue", "register", old_id)

        now = time.time()
        original_path = _write_transcript(
            env,
            session_uuid="sess-orig-uuid",
            agent_id="aoriginalspawn001",
            lines=[
                _spawn_user_frame(
                    f"Queue item: {old_id}\n\nORIGINAL SPAWN PROMPT — use this."
                )
            ],
            mtime=now - 3600,  # 1h ago
        )

        # Newer continuation transcript that ALSO mentions the marker
        # (e.g. follow-up user message after a self-clear). This file is
        # what we explicitly do NOT want resurrect to pick.
        continuation_path = _write_transcript(
            env,
            session_uuid="sess-after-restart",
            agent_id="acontinue99999000",
            lines=[
                _continuation_first_frame(),
                _spawn_user_frame(
                    f"follow-up note referencing Queue item: {old_id} "
                    "DEGRADED — do not pick me."
                ),
            ],
            mtime=now,  # newest
        )

        r = _run(env, "queue", "resurrect", old_id, "--json")
        out = json.loads(r.stdout)
        new = _show(env, out["id"])
        assert new["resurrect_source_transcript"] == str(original_path), (
            f"expected oldest transcript {original_path}, got "
            f"{new['resurrect_source_transcript']}"
        )
        assert "ORIGINAL SPAWN PROMPT" in new["description"]
        assert "DEGRADED" not in new["description"]


def test_resurrect_falls_back_to_first_user_message_when_no_marker():
    """--from-transcript override pointing at a marker-less file: falls back
    to the bare first user message (helpful for hand-written prompts)."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        item = _add(env, "marker-less recovery", ["repo:nomark"], "--summary", "nm")
        old_id = item["id"]
        _run(env, "queue", "register", old_id)
        # Transcript exists but has no marker; auto-search would skip it.
        # With --from-transcript we still resurrect using the first user msg.
        path = _write_transcript(
            env,
            "sess-nomark",
            "anomark000000xyz0",
            [_spawn_user_frame("hand-written prompt body without any marker")],
        )
        r = _run(
            env,
            "queue",
            "resurrect",
            old_id,
            "--from-transcript",
            str(path),
            "--json",
        )
        out = json.loads(r.stdout)
        new = _show(env, out["id"])
        assert "hand-written prompt body without any marker" in new["description"]


# ---------------------------------------------------------------------------
# Refusal paths
# ---------------------------------------------------------------------------


def test_resurrect_refuses_done_item():
    """A done item is terminal — resurrect refuses with a queue-add hint."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        item = _add(env, "completed work", ["repo:done"], "--summary", "d")
        old_id = item["id"]
        _run(env, "queue", "register", old_id)
        _run(env, "queue", "done", old_id)

        r = _run(env, "queue", "resurrect", old_id, expect_exit=1)
        assert "must be running or wedged" in r.stderr
        assert "queue add" in r.stderr


def test_resurrect_refuses_pending_item():
    """A pending item never owned the scope; queue add is the right tool."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        item = _add(env, "never registered", ["repo:pending"], "--summary", "p")
        old_id = item["id"]
        # No register.
        r = _run(env, "queue", "resurrect", old_id, expect_exit=1)
        assert "must be running or wedged" in r.stderr


def test_resurrect_refuses_abandoned_item():
    """An abandoned item is terminal — same shape as done."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        item = _add(env, "abandoned", ["repo:abnd"], "--summary", "a")
        old_id = item["id"]
        _run(env, "queue", "register", old_id)
        _run(env, "queue", "abandon", old_id, "--reason", "test")

        r = _run(env, "queue", "resurrect", old_id, expect_exit=1)
        assert "must be running or wedged" in r.stderr


def test_resurrect_refuses_missing_item():
    """Unknown queue id: exit 1 with a not-found error."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        r = _run(env, "queue", "resurrect", "q-does-not-exist", expect_exit=1)
        assert "not found" in r.stderr


# ---------------------------------------------------------------------------
# --keep-old
# ---------------------------------------------------------------------------


def test_resurrect_keep_old_leaves_old_running():
    """--keep-old: the old item stays in its previous status."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        item = _add(env, "keep me running", ["repo:keep"], "--summary", "k")
        old_id = item["id"]
        _run(env, "queue", "register", old_id)
        _write_transcript(
            env,
            "sess-keep",
            "akeepalive00000a0",
            [_spawn_user_frame(f"Queue item: {old_id}\nkeep-old test prompt")],
        )

        r = _run(env, "queue", "resurrect", old_id, "--keep-old", "--json")
        out = json.loads(r.stdout)
        assert out["old_abandoned"] is False

        old_shown = _show(env, old_id)
        assert old_shown["status"] == "running"
        assert "resurrected_as" not in old_shown
        # The new item is still created.
        new_shown = _show(env, out["id"])
        assert new_shown["resurrected_from"] == old_id


# ---------------------------------------------------------------------------
# Wedged item resurrect (status: wedged is allowed)
# ---------------------------------------------------------------------------


def test_resurrect_wedged_item_succeeds():
    """A wedged item is a valid resurrect candidate."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        item = _add(env, "wedged work", ["repo:wedge"], "--summary", "w")
        old_id = item["id"]
        _run(env, "queue", "register", old_id)
        _run(env, "queue", "wedge", old_id, "--reason", "stuck on api")
        _write_transcript(
            env,
            "sess-wedge",
            "awedged000000abce",
            [_spawn_user_frame(f"Queue item: {old_id}\nwedged-recovery prompt")],
        )

        r = _run(env, "queue", "resurrect", old_id, "--json")
        out = json.loads(r.stdout)
        old_shown = _show(env, old_id)
        assert old_shown["status"] == "abandoned"
        new_shown = _show(env, out["id"])
        assert "wedged-recovery prompt" in new_shown["description"]


# ---------------------------------------------------------------------------
# --from-transcript override
# ---------------------------------------------------------------------------


def test_resurrect_from_transcript_missing_file_errors():
    """--from-transcript pointing at a nonexistent path: clear error."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        item = _add(env, "x", ["repo:x"], "--summary", "x")
        _run(env, "queue", "register", item["id"])
        r = _run(
            env,
            "queue",
            "resurrect",
            item["id"],
            "--from-transcript",
            str(Path(tmp) / "no-such.jsonl"),
            expect_exit=1,
        )
        assert "does not exist" in r.stderr or "not a file" in r.stderr


def test_resurrect_from_transcript_takes_precedence_over_marker_search():
    """When --from-transcript is given, marker-grep auto-search is skipped."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        item = _add(env, "override path", ["repo:ovr"], "--summary", "o")
        old_id = item["id"]
        _run(env, "queue", "register", old_id)
        # Write TWO transcripts: one would be found by marker search,
        # one is the operator's preferred override file.
        _write_transcript(
            env,
            "sess-auto",
            "aautosearch00000a",
            [_spawn_user_frame(f"Queue item: {old_id}\nauto-found prompt")],
        )
        override_path = _write_transcript(
            env,
            "sess-override",
            "aoverridepath0000",
            [_spawn_user_frame("operator-curated override prompt")],
        )
        r = _run(
            env,
            "queue",
            "resurrect",
            old_id,
            "--from-transcript",
            str(override_path),
            "--json",
        )
        out = json.loads(r.stdout)
        new = _show(env, out["id"])
        assert new["resurrect_source_transcript"] == str(override_path)
        assert "operator-curated override prompt" in new["description"]
        assert "auto-found prompt" not in new["description"]


# ---------------------------------------------------------------------------
# depends_on / scope inheritance
# ---------------------------------------------------------------------------


def test_resurrect_carries_depends_on_via_scope_tokens():
    """Inherited task: scope tokens (the canonical depends_on storage) carry
    over to the new item; depends_on edges are preserved."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        # Create a dep target.
        dep = _add(env, "dep target", ["repo:dep"], "--summary", "dt")
        dep_id = dep["id"]
        # Create the item that depends on it.
        item = _add(
            env,
            "needs dep",
            ["repo:needs"],
            "--summary",
            "n",
            "--depends-on",
            dep_id,
        )
        old_id = item["id"]
        # Mark dep done so we can register the parent and walk it through
        # to running status. (Otherwise the spawn-gate refuses.)
        _run(env, "queue", "register", dep_id)
        _run(env, "queue", "done", dep_id)
        _run(env, "queue", "register", old_id)

        _write_transcript(
            env,
            "sess-deps",
            "adepsinherit0000a",
            [_spawn_user_frame(f"Queue item: {old_id}\ninherits-deps prompt")],
        )

        r = _run(env, "queue", "resurrect", old_id, "--json")
        out = json.loads(r.stdout)
        # carried_deps may be empty here because the dep is already done,
        # but the task: scope token should still be on the new item's
        # scope (the resurrect-time dep-validity sweep only STRIPS deps
        # that no longer EXIST, not deps that are done).
        new = _show(env, out["id"])
        scope_tokens = new["scope"]
        # repo:needs is the user scope, task:<dep> is the inherited dep.
        assert "repo:needs" in scope_tokens
        assert f"task:{dep_id}" in scope_tokens


def test_resurrect_strips_dangling_deps():
    """If a task: scope token refers to a queue id that no longer exists
    (was pruned), the resurrect strips it with a warning rather than
    enqueueing a permanently-blocked item."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        item = _add(env, "dangling-dep parent", ["repo:dang"], "--summary", "d")
        old_id = item["id"]
        _run(env, "queue", "register", old_id)
        # Manually inject a task: scope token referring to a phantom id.
        # The cleanest path is to edit queue.json directly because
        # `queue add --scope task:<unknown>` would refuse via the
        # add-time dep-validity sweep.
        qjson = Path(tmp) / ".config" / "session" / "queue.json"
        data = json.loads(qjson.read_text())
        for it in data["items"]:
            if it["id"] == old_id:
                if "task:q-phantom-id" not in it["scope"]:
                    it["scope"].append("task:q-phantom-id")
                break
        qjson.write_text(json.dumps(data))

        _write_transcript(
            env,
            "sess-dang",
            "adangling000abcde",
            [_spawn_user_frame(f"Queue item: {old_id}\ndangling test")],
        )

        r = _run(env, "queue", "resurrect", old_id, "--json")
        assert "Stripping" in r.stderr or "unknown queue id" in r.stderr
        out = json.loads(r.stdout)
        new = _show(env, out["id"])
        assert "task:q-phantom-id" not in new["scope"]


# ---------------------------------------------------------------------------
# Output / non-JSON path
# ---------------------------------------------------------------------------


def test_resurrect_non_json_output_includes_essentials():
    """The non-JSON happy-path output prints the new id, source, old-id
    status, and a spawn_instruction. Operators rely on this for the
    register-and-spawn follow-up."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        item = _add(env, "non-json", ["repo:nj"], "--summary", "nj")
        old_id = item["id"]
        _run(env, "queue", "register", old_id)
        _write_transcript(
            env,
            "sess-nj",
            "anonjson000abcdef",
            [_spawn_user_frame(f"Queue item: {old_id}\nnj test prompt")],
        )
        r = _run(env, "queue", "resurrect", old_id)
        assert "resurrected:" in r.stdout
        assert old_id in r.stdout
        assert "ready_now=" in r.stdout
        # The new item should be ready_now=true (no scope conflicts; old
        # was abandoned in the same RMW).
        assert "ready_now=true" in r.stdout


# ---------------------------------------------------------------------------
# Reason audit field
# ---------------------------------------------------------------------------


def test_resurrect_reason_propagates_to_audit_fields():
    """--reason TEXT appears in both the new item's resurrected_reason
    and the old item's abandon_reason."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        item = _add(env, "reasoned", ["repo:rsn"], "--summary", "r")
        old_id = item["id"]
        _run(env, "queue", "register", old_id)
        _write_transcript(
            env,
            "sess-reason",
            "areason0000abcdef",
            [_spawn_user_frame(f"Queue item: {old_id}\nreason test")],
        )
        r = _run(
            env,
            "queue",
            "resurrect",
            old_id,
            "--reason",
            "api-500-cascade",
            "--json",
        )
        out = json.loads(r.stdout)
        new = _show(env, out["id"])
        assert new["resurrected_reason"] == "api-500-cascade"
        old = _show(env, old_id)
        assert "api-500-cascade" in old["abandon_reason"]
