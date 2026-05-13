#!/usr/bin/env python3
"""End-to-end tests for ``GET /api/queue/<id>/meta`` on queue-minisite.

Companion to ``test_workload_archive.py``. Seeds queue.json + an
archived subagent JSONL + a parent-session JSONL containing a
``queue-operation`` enqueue record with the agent's task-notification
payload, then asserts the endpoint joins those sources into a single
metadata blob containing the agent's return text, token usage, tool
count, and runtime.

Run::

    python3 queue-minisite/test_meta.py
"""

from __future__ import annotations

import json
import os
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
_DEFAULT_SESSION_TASK = (
    HERE.parent.parent / "claude-watch" / "tools" / "session-task" / "session-task"
)
SESSION_TASK = Path(os.environ.get("SESSION_TASK_BIN", str(_DEFAULT_SESSION_TASK)))


def _add(env: dict, queue_actual: Path, desc: str, scopes: list[str]) -> dict:
    cmd = [sys.executable, str(SESSION_TASK), "queue", "add", desc,
           "--summary", desc, "--json"]
    for s in scopes:
        cmd.extend(["--scope", s])
    r = subprocess.run(cmd, capture_output=True, text=True, env=env, timeout=15)
    if r.returncode != 0:
        raise RuntimeError(f"add failed: {r.stderr}")
    return json.loads(r.stdout)


def _mark_done(queue_path: Path, item_id: str, archive_path: str | None = None) -> None:
    """Flip an item to status=done and (optionally) stamp log_archive_path."""
    with open(queue_path) as f:
        data = json.load(f)
    for it in data["items"]:
        if it["id"] == item_id:
            it["status"] = "done"
            it["started_at"] = "2026-05-11T20:57:03+00:00"
            it["registered_at"] = "2026-05-11T20:57:03+00:00"
            it["completed_at"] = "2026-05-11T21:02:03+00:00"
            if archive_path:
                it["log_archive_path"] = archive_path
    with open(queue_path, "w") as f:
        json.dump(data, f)


def _seed_archive_and_parent(
    archive_dir: Path,
    jsonl_root: Path,
    qid: str,
    *,
    session_id: str,
    agent_id: str,
    return_text: str,
    total_tokens: int,
    tool_uses: int,
    duration_ms: int,
) -> str:
    """Drop a subagent JSONL into the archive dir + a parent session JSONL
    containing the task-notification enqueue record.

    Returns the relative archive filename (for stamping into queue.json).
    """
    archive_dir.mkdir(parents=True, exist_ok=True)
    jsonl_root.mkdir(parents=True, exist_ok=True)

    # Subagent archive — first line is enough; we just need sessionId +
    # agentId so the endpoint can resolve the parent.
    first_rec = {
        "type": "user",
        "uuid": "00000000-0000-0000-0000-000000000001",
        "timestamp": "2026-05-11T20:57:03Z",
        "sessionId": session_id,
        "agentId": agent_id,
        "message": {"role": "user", "content": [{"type": "text", "text": "init"}]},
    }
    arc_name = f"{qid}.jsonl"
    (archive_dir / arc_name).write_text(json.dumps(first_rec) + "\n")

    # Parent session JSONL — single queue-operation enqueue record with
    # the task-notification payload the harness writes when a background
    # subagent terminates.
    task_notif = (
        "<task-notification>\n"
        f"<task-id>{agent_id}</task-id>\n"
        "<tool-use-id>toolu_TESTSEED</tool-use-id>\n"
        "<status>completed</status>\n"
        "<summary>Agent \"test fixture\" completed</summary>\n"
        f"<result>{return_text}</result>\n"
        "<usage>"
        f"<total_tokens>{total_tokens}</total_tokens>"
        f"<tool_uses>{tool_uses}</tool_uses>"
        f"<duration_ms>{duration_ms}</duration_ms>"
        "</usage>\n"
        "</task-notification>"
    )
    parent_rec = {
        "type": "queue-operation",
        "operation": "enqueue",
        "timestamp": "2026-05-11T21:02:31Z",
        "sessionId": session_id,
        "content": task_notif,
    }
    parent_path = jsonl_root / f"{session_id}.jsonl"
    parent_path.write_text(json.dumps(parent_rec) + "\n")

    return arc_name


class MetaEndpointTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.tmp = tempfile.mkdtemp(prefix="qmin-meta-")
        cls.env = dict(os.environ)
        cls.env["HOME"] = cls.tmp
        Path(cls.tmp, ".config/session").mkdir(parents=True, exist_ok=True)
        Path(cls.tmp, ".config/claude").mkdir(parents=True, exist_ok=True)
        Path(cls.tmp, "claude-events").mkdir(parents=True, exist_ok=True)
        cls.env["PINGME_SESSION_TASK"] = "0"
        cls.env["CLAUDE_EVENT_SESSION_TASK"] = "0"

        for k, v in cls.env.items():
            os.environ[k] = v

        cls.queue_actual = Path(cls.tmp) / ".config/session/queue.json"
        cls.archive_dir = Path(cls.tmp) / "queue-logs"
        cls.jsonl_root = Path(cls.tmp) / "agents-jsonl"
        os.environ["QUEUE_JSON"] = str(cls.queue_actual)
        os.environ["AGENT_STATE_JSON"] = str(Path(cls.tmp) / "no-agents.json")
        os.environ["AGENTS_JSONL_ROOT"] = str(cls.jsonl_root)
        os.environ["QUEUE_LOG_ARCHIVE_DIR"] = str(cls.archive_dir)
        cls.workload_dir = Path(cls.tmp) / "workloads"
        cls.workload_dir.mkdir(parents=True, exist_ok=True)
        os.environ["WORKLOAD_LOG_DIR"] = str(cls.workload_dir)
        os.environ["SESSION_TASK_BIN"] = str(SESSION_TASK)

        sys.path.insert(0, str(HERE))
        for mod in list(sys.modules):
            if mod in ("app", "claude_agents"):
                del sys.modules[mod]
        import app as appmod  # noqa: E402
        cls.appmod = appmod
        cls.client = appmod.app.test_client()

    @classmethod
    def tearDownClass(cls):
        shutil.rmtree(cls.tmp, ignore_errors=True)

    def setUp(self):
        if self.queue_actual.exists():
            self.queue_actual.unlink()
        if self.archive_dir.exists():
            for p in self.archive_dir.iterdir():
                if p.is_file():
                    p.unlink()
        if self.jsonl_root.exists():
            for p in self.jsonl_root.iterdir():
                if p.is_file():
                    p.unlink()
        if self.workload_dir.exists():
            for p in self.workload_dir.iterdir():
                if p.is_file():
                    p.unlink()
        self.appmod._cache.fetched_at = 0.0

    # ---------- 400 / 404 ----------

    def test_invalid_id_format_returns_400(self):
        r = self.client.get("/api/queue/not!valid/meta")
        self.assertEqual(r.status_code, 400)

    def test_unknown_id_returns_404(self):
        # Need a queue.json — otherwise the read fails before the id
        # lookup. Add a dummy unrelated item.
        _add(self.env, self.queue_actual, "decoy", ["repo:test"])
        self.appmod._cache.fetched_at = 0.0
        r = self.client.get("/api/queue/q-nosuch-id/meta")
        self.assertEqual(r.status_code, 404)

    # ---------- pending item (no archive, no agent return text) ----------

    def test_pending_item_shape(self):
        item = _add(self.env, self.queue_actual, "pending fixture", ["repo:test"])
        qid = item["id"]
        self.appmod._cache.fetched_at = 0.0

        r = self.client.get(f"/api/queue/{qid}/meta")
        self.assertEqual(r.status_code, 200)
        p = r.get_json()
        self.assertTrue(p["ok"])
        self.assertEqual(p["id"], qid)
        self.assertEqual(p["status"], "pending")
        self.assertEqual(p["summary"], "pending fixture")
        self.assertIn("repo:test", p["scope"])
        # No archive → no agent block.
        self.assertIsNone(p["agent"])
        # Runtime is null until the item starts.
        self.assertIsNone(p["runtime_seconds"])

    # ---------- done item with agent return text ----------

    def test_done_item_surfaces_agent_return_value(self):
        item = _add(self.env, self.queue_actual, "agent fixture", ["repo:test"])
        qid = item["id"]
        arc = _seed_archive_and_parent(
            self.archive_dir,
            self.jsonl_root,
            qid,
            session_id="fea2cd3a-76ac-4ec1-b562-15f09872854d",
            agent_id="a0b6897cbdd7a9478",
            return_text=(
                "=== Test fixture agent ===\n"
                "[INVESTIGATING] ...\n"
                "[COMMITTED] all green\n"
                "\n"
                "[PHASE-TIMING] investigation 1m | total 1m"
            ),
            total_tokens=88058,
            tool_uses=49,
            duration_ms=308886,
        )
        _mark_done(self.queue_actual, qid, archive_path=arc)
        self.appmod._cache.fetched_at = 0.0

        r = self.client.get(f"/api/queue/{qid}/meta")
        self.assertEqual(r.status_code, 200)
        p = r.get_json()
        self.assertTrue(p["ok"])
        self.assertEqual(p["status"], "done")
        self.assertEqual(p["runtime_seconds"], 300.0)

        agent = p["agent"]
        self.assertIsNotNone(agent)
        self.assertEqual(agent["agent_id"], "a0b6897cbdd7a9478")
        self.assertEqual(agent["parent_session_id"], "fea2cd3a-76ac-4ec1-b562-15f09872854d")
        self.assertEqual(agent["return_status"], "completed")
        self.assertIn("[PHASE-TIMING]", agent["return_text"])
        self.assertEqual(agent["usage_total_tokens"], 88058)
        self.assertEqual(agent["usage_tool_uses"], 49)
        self.assertEqual(agent["usage_duration_ms"], 308886)

    def test_done_item_missing_parent_jsonl_returns_archive_anchor_only(self):
        """When the parent JSONL is gone, the agent block still surfaces the
        anchor (agent_id + session_id) so the front-end can render the
        partial info — return_text is just absent."""
        item = _add(self.env, self.queue_actual, "no parent fixture", ["repo:test"])
        qid = item["id"]
        # Seed only the archive — no parent JSONL.
        first_rec = {
            "type": "user",
            "uuid": "00000000-0000-0000-0000-000000000001",
            "timestamp": "2026-05-11T20:57:03Z",
            "sessionId": "deadbeef-dead-beef-dead-beefdeadbeef",
            "agentId": "abcd1234abcd1234a",
            "message": {"role": "user", "content": []},
        }
        self.archive_dir.mkdir(parents=True, exist_ok=True)
        arc = f"{qid}.jsonl"
        (self.archive_dir / arc).write_text(json.dumps(first_rec) + "\n")
        _mark_done(self.queue_actual, qid, archive_path=arc)
        self.appmod._cache.fetched_at = 0.0

        r = self.client.get(f"/api/queue/{qid}/meta")
        self.assertEqual(r.status_code, 200)
        p = r.get_json()
        agent = p["agent"]
        self.assertIsNotNone(agent)
        self.assertEqual(agent["agent_id"], "abcd1234abcd1234a")
        # return_text key absent (helper only adds when the lookup succeeds).
        self.assertNotIn("return_text", agent)

    # ---------- dependents via task: scope token ----------

    def test_dependents_via_task_scope_token(self):
        a = _add(self.env, self.queue_actual, "blocker", ["repo:test"])
        # b has a:q-... in its scope (encodes a dep on a).
        b = _add(self.env, self.queue_actual, "blocked",
                 ["repo:test", f"task:{a['id']}"])
        self.appmod._cache.fetched_at = 0.0

        r = self.client.get(f"/api/queue/{a['id']}/meta")
        self.assertEqual(r.status_code, 200)
        p = r.get_json()
        self.assertIn(b["id"], p["dependents"])

    # ---------- script_capture surfaced from workload sidecar ----------

    def test_script_capture_surfaced_for_workload_item(self):
        # Workload-bound queue items carry a `workload:<label>` scope.
        # When the workload-run CLI captures a script at start time it
        # writes /tmp/claude-workloads/<label>.script.json; the meta
        # endpoint loads that file and includes it as `script_capture`.
        item = _add(
            self.env,
            self.queue_actual,
            "wl fixture",
            ["workload:test-wl-1"],
        )
        qid = item["id"]

        capture = {
            "path": "/tmp/foo.sh",
            "interpreter": "bash",
            "size_bytes": 42,
            "truncated": False,
            "binary": False,
            "content": "#!/bin/bash\necho hi\n",
            "sha256": "abc123def",
        }
        (self.workload_dir / "test-wl-1.script.json").write_text(json.dumps(capture))
        self.appmod._cache.fetched_at = 0.0

        r = self.client.get(f"/api/queue/{qid}/meta")
        self.assertEqual(r.status_code, 200)
        p = r.get_json()
        self.assertEqual(p["workload_label"], "test-wl-1")
        self.assertIsNotNone(p["script_capture"])
        self.assertEqual(p["script_capture"]["interpreter"], "bash")
        self.assertEqual(p["script_capture"]["path"], "/tmp/foo.sh")
        self.assertEqual(p["script_capture"]["content"], "#!/bin/bash\necho hi\n")

    def test_script_capture_null_when_sidecar_missing(self):
        # Workload item with no .script.json on disk (older workloads,
        # non-script invocations, capture refused for safety). The
        # meta payload still has the key, set to None.
        item = _add(
            self.env,
            self.queue_actual,
            "wl no-capture",
            ["workload:test-wl-2"],
        )
        qid = item["id"]
        self.appmod._cache.fetched_at = 0.0

        r = self.client.get(f"/api/queue/{qid}/meta")
        self.assertEqual(r.status_code, 200)
        p = r.get_json()
        self.assertEqual(p["workload_label"], "test-wl-2")
        self.assertIsNone(p["script_capture"])

    def test_script_capture_null_for_non_workload_item(self):
        # No `workload:` scope token → the loader is never invoked.
        item = _add(self.env, self.queue_actual, "plain item", ["repo:test"])
        qid = item["id"]
        self.appmod._cache.fetched_at = 0.0

        r = self.client.get(f"/api/queue/{qid}/meta")
        self.assertEqual(r.status_code, 200)
        p = r.get_json()
        self.assertEqual(p["workload_label"], "")
        self.assertIsNone(p["script_capture"])

    def test_script_capture_rejects_malformed_sidecar(self):
        # A garbage file at the sidecar path should fail-soft to None,
        # not crash the endpoint.
        item = _add(
            self.env,
            self.queue_actual,
            "wl malformed",
            ["workload:test-wl-3"],
        )
        qid = item["id"]
        (self.workload_dir / "test-wl-3.script.json").write_text("{not valid json")
        self.appmod._cache.fetched_at = 0.0

        r = self.client.get(f"/api/queue/{qid}/meta")
        self.assertEqual(r.status_code, 200)
        p = r.get_json()
        self.assertIsNone(p["script_capture"])


if __name__ == "__main__":
    unittest.main(verbosity=2)
