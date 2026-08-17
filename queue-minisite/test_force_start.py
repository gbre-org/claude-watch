#!/usr/bin/env python3
"""End-to-end tests for the force-start endpoint on queue-minisite.

Spawns the Flask app in-process with QUEUE_PATH/SESSION_TASK_BIN/etc.
pointed at a tempdir, seeds queue.json, and exercises
``POST /api/queue/<id>/force-start`` end-to-end (the endpoint shells
out to the canonical session-task at
~/repos/claude-watch/tools/session-task/session-task — same binary the
container picks up via the bind mount in docker-compose.yml).

Cases:
  1. happy path — pending item promoted to running, response shape
     correct, queue.json mutated.
  2. missing reason — 400.
  3. wrong status (running) — 404.
  4. invalid id format — 400.

Run:
    python3 queue-minisite/test_force_start.py
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
# Canonical session-task lives in the parent claude-watch repo at
# ../tools/session-task/session-task. Allow override via SESSION_TASK_BIN
# for one-off paths.
_DEFAULT_SESSION_TASK = (
    HERE.parent / "tools" / "session-task" / "session-task"
)
SESSION_TASK = Path(os.environ.get("SESSION_TASK_BIN", str(_DEFAULT_SESSION_TASK)))


def _seed_queue(queue_path: Path, env: dict[str, str]) -> tuple[str, str]:
    """Create one running + one blocked-pending item via session-task,
    return (running_id, blocked_id).
    """
    cmd_base = [sys.executable, str(SESSION_TASK), "queue", "add"]

    def add(desc: str, scopes: list[str], *extra: str) -> dict:
        cmd = cmd_base + [desc, "--json"]
        for s in scopes:
            cmd.extend(["--scope", s])
        cmd.extend(extra)
        r = subprocess.run(cmd, capture_output=True, text=True, env=env, timeout=15)
        if r.returncode != 0:
            raise RuntimeError(f"add failed: {r.stderr}")
        return json.loads(r.stdout)

    def reg(qid: str) -> None:
        r = subprocess.run(
            [sys.executable, str(SESSION_TASK), "queue", "register", qid, "--json"],
            capture_output=True, text=True, env=env, timeout=15,
        )
        if r.returncode != 0:
            raise RuntimeError(f"register failed: {r.stderr}")

    d1 = add("first", ["repo:web"])
    reg(d1["id"])
    d2 = add("blocked", ["repo:web"], "--force-enqueue")
    return d1["id"], d2["id"]


class ForceStartEndpointTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.tmp = tempfile.mkdtemp(prefix="qmin-force-start-")
        cls.queue_path = Path(cls.tmp) / "queue.json"
        cls.queue_path.parent.mkdir(parents=True, exist_ok=True)

        # session-task / claude-event use HOME for default paths; isolate.
        cls.env = dict(os.environ)
        cls.env["HOME"] = cls.tmp
        Path(cls.tmp, ".config/session").mkdir(parents=True, exist_ok=True)
        Path(cls.tmp, ".config/claude").mkdir(parents=True, exist_ok=True)
        Path(cls.tmp, "claude-events").mkdir(parents=True, exist_ok=True)
        # Force the session-task default queue.json onto our managed path
        # by setting HOME (it derives from Path.home()).
        cls.env["QUEUE_FORCE_START_LOG"] = str(
            Path(cls.tmp) / ".config/claude/queue-force-start.log"
        )
        # Disable pingme noise during tests.
        cls.env["PINGME_DISABLE"] = "1"
        # Disable claude-event emits inside session-task to keep the test
        # purely about queue.json + HTTP.
        cls.env["CLAUDE_EVENT_SESSION_TASK"] = "0"

        # Apply the env to this process so the in-process Flask app's
        # subprocess invocations inherit it. (subprocess inherits the
        # current os.environ.)
        for k, v in cls.env.items():
            os.environ[k] = v

        # Resolve the actual path the vendored session-task will write.
        cls.queue_actual = Path(cls.tmp) / ".config/session/queue.json"
        # Point the app at it explicitly via QUEUE_JSON.
        os.environ["QUEUE_JSON"] = str(cls.queue_actual)
        # Container default agent-state path is /agents-state/...; in a
        # host test we just point at a non-existent file so the loader
        # returns the empty default.
        os.environ["AGENT_STATE_JSON"] = str(Path(cls.tmp) / "no-agents.json")
        # Same for the JSONL root and the archive dir.
        os.environ["AGENTS_JSONL_ROOT"] = str(Path(cls.tmp) / "no-jsonl")
        os.environ["QUEUE_LOG_ARCHIVE_DIR"] = str(
            Path(cls.tmp) / "queue-logs"
        )
        os.environ["SESSION_TASK_BIN"] = str(SESSION_TASK)

        # Import the app AFTER env is set.
        sys.path.insert(0, str(HERE))
        # Force a clean import.
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
        # Reset queue.json before each test. session-task creates it with
        # the canonical schema; just unlink and re-seed.
        if self.queue_actual.exists():
            self.queue_actual.unlink()
        self.running_id, self.blocked_id = _seed_queue(
            self.queue_actual, self.env,
        )
        # Bust the in-process read cache.
        self.appmod._cache.fetched_at = 0.0

    # -------------------------------------------------------------- 1
    def test_happy_path_promotes_pending(self):
        r = self.client.post(
            f"/api/queue/{self.blocked_id}/force-start",
            json={"reason": "operator-decided"},
        )
        self.assertEqual(r.status_code, 200, r.get_data(as_text=True))
        body = r.get_json()
        self.assertTrue(body.get("ok"), body)
        self.assertEqual(body["id"], self.blocked_id)
        self.assertEqual(body["action"], "force-start")
        # The CLI's annotated reason is what the endpoint writes — it
        # appends "(via UI by ...)" so we check the prefix.
        self.assertIn("operator-decided", body["reason"])

        # Inspect queue.json directly for the persisted fields.
        with open(self.queue_actual) as f:
            data = json.load(f)
        promoted = next(
            it for it in data["items"] if it["id"] == self.blocked_id
        )
        self.assertEqual(promoted["status"], "running")
        self.assertIn("operator-decided", promoted["force_started_reason"])
        self.assertIsInstance(promoted["force_started_at"], int)

    # -------------------------------------------------------------- 2
    def test_missing_reason_returns_400(self):
        r = self.client.post(
            f"/api/queue/{self.blocked_id}/force-start",
            json={},
        )
        self.assertEqual(r.status_code, 400, r.get_data(as_text=True))
        body = r.get_json()
        self.assertFalse(body.get("ok"))
        self.assertIn("reason", body.get("error", "").lower())

        # Empty-string reason also rejected.
        r2 = self.client.post(
            f"/api/queue/{self.blocked_id}/force-start",
            json={"reason": "   "},
        )
        self.assertEqual(r2.status_code, 400, r2.get_data(as_text=True))

    # -------------------------------------------------------------- 3
    def test_running_status_returns_404(self):
        # The running item's status is not pending; force-start must
        # refuse with 404.
        r = self.client.post(
            f"/api/queue/{self.running_id}/force-start",
            json={"reason": "trying anyway"},
        )
        self.assertEqual(r.status_code, 404, r.get_data(as_text=True))
        body = r.get_json()
        self.assertEqual(body.get("error"), "not pending")

    # -------------------------------------------------------------- 4
    def test_invalid_id_format_returns_400(self):
        r = self.client.post(
            "/api/queue/not-a-queue-id/force-start",
            json={"reason": "x"},
        )
        self.assertEqual(r.status_code, 400, r.get_data(as_text=True))
        body = r.get_json()
        self.assertIn("invalid", body.get("error", "").lower())

    # -------------------------------------------------------------- 5
    def test_force_start_registers_obligation(self):
        """Web-UI force-start should ALSO register a force_started_unspawned
        obligation in the host's ~/.config/claude/obligations.json. This
        is the gate that blocks the main loop until an Agent is spawned
        for the promoted queue id.

        In production the container shares the host's ~/.config/claude
        directory via a docker-compose bind mount + the obligations CLI
        is COPY'd into /usr/local/bin/obligations. In this test we run
        in-process on the host so we just need the host's `obligations`
        CLI on PATH.
        """
        # Drop the OBLIGATIONS_FORCE_START=0 suppression locally so the
        # obligation register actually runs for this single test.
        prior = os.environ.pop("OBLIGATIONS_FORCE_START", None)
        try:
            r = self.client.post(
                f"/api/queue/{self.blocked_id}/force-start",
                json={"reason": "obligation-test"},
            )
            self.assertEqual(r.status_code, 200, r.get_data(as_text=True))

            ob_path = Path(self.tmp) / ".config" / "claude" / "obligations.json"
            self.assertTrue(
                ob_path.exists(),
                f"expected obligations.json at {ob_path}",
            )
            with open(ob_path) as f:
                ob_data = json.load(f)
            matching = [
                ob for ob in ob_data.get("obligations", [])
                if (ob.get("predicate", {}).get("kind")
                    == "force_started_unspawned")
                and ob.get("predicate", {}).get("params", {}).get("queue_id")
                    == self.blocked_id
            ]
            self.assertTrue(
                matching,
                f"expected a force_started_unspawned obligation for "
                f"{self.blocked_id!r}, got "
                f"{[o.get('predicate') for o in ob_data.get('obligations',[])]}"
            )
            ob = matching[0]
            self.assertEqual(ob.get("tool_pattern"), "*")
            self.assertEqual(ob.get("enforcement", "gate"), "gate")
        finally:
            if prior is not None:
                os.environ["OBLIGATIONS_FORCE_START"] = prior

    # -------------------------------------------------------------- 6
    def test_stale_session_task_bin_returns_clear_error(self):
        """When SESSION_TASK_BIN is not OPENABLE, force-start must return an
        ACTIONABLE 500 rather than the opaque "session-task force-start
        failed" the raw rc=2 subprocess would produce.

        This is the exact production failure mode Andrew hit (#4386): the
        container bind-mounted session-task as a SINGLE FILE, so when the
        host script was replaced via atomic rename the mount went stale
        (pointed at the now-unlinked inode) and ``python3 /app/session-task
        ...`` died with ENOENT. The preflight turns that into a diagnostic
        naming the stale mount; the durable fix is the directory bind mount
        in docker-compose.yml.
        """
        missing = str(Path(self.tmp) / "gone" / "session-task")
        orig = self.appmod.SESSION_TASK_BIN
        self.appmod.SESSION_TASK_BIN = missing
        try:
            r = self.client.post(
                f"/api/queue/{self.blocked_id}/force-start",
                json={"reason": "operator-decided"},
            )
            self.assertEqual(r.status_code, 500, r.get_data(as_text=True))
            body = r.get_json()
            self.assertFalse(body.get("ok"))
            err = body.get("error", "")
            self.assertIn("not readable", err)
            self.assertIn("stale", err.lower())
            self.assertEqual(body.get("session_task_bin"), missing)

            # No half-promotion: the item stays pending, untouched.
            with open(self.queue_actual) as f:
                data = json.load(f)
            item = next(
                it for it in data["items"] if it["id"] == self.blocked_id
            )
            self.assertEqual(item["status"], "pending")
        finally:
            self.appmod.SESSION_TASK_BIN = orig


    # -------------------------------------------------------------- 7
    def test_co_run_leaves_overlapping_peer_running(self):
        """`co_run: true` force-starts ALONGSIDE the same-scope running peer
        WITHOUT autostopping it (Andrew #4529). The endpoint appends
        --no-interrupt to the session-task force-start argv; default
        (no co_run) still autostops the peer.
        """
        r = self.client.post(
            f"/api/queue/{self.blocked_id}/force-start",
            json={"reason": "scopes-safe", "co_run": True},
        )
        self.assertEqual(r.status_code, 200, r.get_data(as_text=True))
        body = r.get_json()
        self.assertTrue(body.get("ok"), body)
        self.assertTrue(body.get("co_run"), body)

        with open(self.queue_actual) as f:
            data = json.load(f)
        items = {it["id"]: it for it in data["items"]}
        # Target promoted + marked co-run.
        self.assertEqual(items[self.blocked_id]["status"], "running")
        self.assertTrue(items[self.blocked_id].get("force_started_co_run"))
        # The overlapping peer is STILL running (not autostopped).
        self.assertEqual(items[self.running_id]["status"], "running")
        self.assertNotIn(
            "autostopped_by_force_start", items[self.running_id]
        )

    # -------------------------------------------------------------- 8
    def test_default_force_start_autostops_overlapping_peer(self):
        """Regression: with no co_run flag, force-start still autostops the
        overlapping same-scope running peer (unchanged default behavior).
        """
        r = self.client.post(
            f"/api/queue/{self.blocked_id}/force-start",
            json={"reason": "interrupt-default"},
        )
        self.assertEqual(r.status_code, 200, r.get_data(as_text=True))
        self.assertFalse(r.get_json().get("co_run"), r.get_json())

        with open(self.queue_actual) as f:
            data = json.load(f)
        items = {it["id"]: it for it in data["items"]}
        self.assertEqual(items[self.blocked_id]["status"], "running")
        self.assertEqual(items[self.running_id]["status"], "abandoned")


if __name__ == "__main__":
    unittest.main(verbosity=2)
