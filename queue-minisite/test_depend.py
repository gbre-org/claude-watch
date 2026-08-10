#!/usr/bin/env python3
"""End-to-end tests for the depend endpoint on queue-minisite.

Spawns the Flask app in-process with QUEUE_PATH/SESSION_TASK_BIN/etc.
pointed at a tempdir, seeds queue.json, and exercises:

  * POST /api/queue/depend — add a dep edge
  * DELETE /api/queue/<id>/depend — remove a dep edge

Run:
    python3 queue-minisite/test_depend.py
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
# Repo-root relative: queue-minisite/ lives inside the claude-watch
# checkout, so the CLI is a sibling under tools/. (The old form went
# up two levels and back down through a hardcoded "claude-watch"
# directory name, which only resolved when the checkout happened to
# be named that — it broke in worktrees and renamed clones.)
_DEFAULT_SESSION_TASK = HERE.parent / "tools" / "session-task" / "session-task"
SESSION_TASK = Path(os.environ.get("SESSION_TASK_BIN", str(_DEFAULT_SESSION_TASK)))


def _seed_queue(queue_path: Path, env: dict[str, str]) -> tuple[str, str, str]:
    """Create three pending items in different scopes (so groups are
    disjoint) and return their ids.
    """
    cmd_base = [sys.executable, str(SESSION_TASK), "queue", "add"]

    def add(desc: str, scopes: list[str], *extra: str) -> dict:
        cmd = cmd_base + [desc, "--summary", desc, "--json"]
        for s in scopes:
            cmd.extend(["--scope", s])
        cmd.extend(extra)
        r = subprocess.run(cmd, capture_output=True, text=True, env=env, timeout=15)
        if r.returncode != 0:
            raise RuntimeError(f"add failed: {r.stderr}")
        return json.loads(r.stdout)

    a = add("a", ["repo:a"])
    b = add("b", ["repo:b"])
    c = add("c", ["repo:c"])
    return a["id"], b["id"], c["id"]


class DependEndpointTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.tmp = tempfile.mkdtemp(prefix="qmin-depend-")
        cls.queue_path = Path(cls.tmp) / "queue.json"

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
        os.environ["QUEUE_JSON"] = str(cls.queue_actual)
        os.environ["AGENT_STATE_JSON"] = str(Path(cls.tmp) / "no-agents.json")
        os.environ["AGENTS_JSONL_ROOT"] = str(Path(cls.tmp) / "no-jsonl")
        os.environ["QUEUE_LOG_ARCHIVE_DIR"] = str(Path(cls.tmp) / "queue-logs")
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
        self.a_id, self.b_id, self.c_id = _seed_queue(
            self.queue_actual, self.env,
        )
        self.appmod._cache.fetched_at = 0.0

    def test_post_adds_dep_edge(self):
        r = self.client.post(
            "/api/queue/depend",
            json={"dragged_id": self.b_id, "target_id": self.a_id},
        )
        self.assertEqual(r.status_code, 200, r.get_data(as_text=True))
        body = r.get_json()
        self.assertTrue(body.get("ok"))
        self.assertEqual(body["dragged_id"], self.b_id)
        self.assertEqual(body["target_id"], self.a_id)
        self.assertEqual(body["depends_on"], [self.a_id])
        self.assertFalse(body["ready_now"])

        # Verify queue.json mutation. Deps are persisted as `task:<id>`
        # scope tokens (canonical since 2026-05-08); the legacy
        # `depends_on` list is migrated away on read and stripped on the
        # next write, so it must NOT be what we assert on disk.
        with open(self.queue_actual) as f:
            data = json.load(f)
        b = next(it for it in data["items"] if it["id"] == self.b_id)
        self.assertIn(f"task:{self.a_id}", b.get("scope") or [])
        self.assertFalse(b.get("depends_on"))

    def test_ci_gate_sabotage_check(self):
        # TEMPORARY: proves the new CI job actually fails the build.
        # Reverted in the next commit.
        self.assertEqual(1, 2, "deliberate failure: CI gate sabotage check")

    def test_self_dep_rejected_400(self):
        r = self.client.post(
            "/api/queue/depend",
            json={"dragged_id": self.a_id, "target_id": self.a_id},
        )
        self.assertEqual(r.status_code, 400, r.get_data(as_text=True))

    def test_invalid_id_format_400(self):
        r = self.client.post(
            "/api/queue/depend",
            json={"dragged_id": "not-valid", "target_id": self.a_id},
        )
        self.assertEqual(r.status_code, 400)

    def test_unknown_id_404(self):
        r = self.client.post(
            "/api/queue/depend",
            json={"dragged_id": "q-2099-99-99-zzzz", "target_id": self.a_id},
        )
        self.assertEqual(r.status_code, 404)

    def test_delete_removes_dep_edge(self):
        # Add first
        r = self.client.post(
            "/api/queue/depend",
            json={"dragged_id": self.b_id, "target_id": self.a_id},
        )
        self.assertEqual(r.status_code, 200)

        # Now remove
        r = self.client.delete(
            f"/api/queue/{self.b_id}/depend",
            json={"target_id": self.a_id},
        )
        self.assertEqual(r.status_code, 200, r.get_data(as_text=True))
        body = r.get_json()
        self.assertTrue(body.get("ok"))
        self.assertEqual(body["depends_on"], [])
        self.assertTrue(body["ready_now"])

        # The edge is gone from the persisted representation too: no
        # `task:<a>` scope token, and no legacy `depends_on` remnant.
        with open(self.queue_actual) as f:
            data = json.load(f)
        b = next(it for it in data["items"] if it["id"] == self.b_id)
        self.assertNotIn(f"task:{self.a_id}", b.get("scope") or [])
        self.assertFalse(b.get("depends_on"))

    def test_cross_group_dep_works(self):
        # b depends on a (repo:b -> repo:a, different groups)
        r = self.client.post(
            "/api/queue/depend",
            json={"dragged_id": self.b_id, "target_id": self.a_id},
        )
        self.assertEqual(r.status_code, 200)
        # c depends on b (third group)
        r = self.client.post(
            "/api/queue/depend",
            json={"dragged_id": self.c_id, "target_id": self.b_id},
        )
        self.assertEqual(r.status_code, 200)

        with open(self.queue_actual) as f:
            data = json.load(f)
        groups = {it["id"]: it["group_id"] for it in data["items"]}
        self.assertNotEqual(groups[self.a_id], groups[self.b_id])
        self.assertNotEqual(groups[self.b_id], groups[self.c_id])


if __name__ == "__main__":
    unittest.main(verbosity=2)
