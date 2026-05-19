#!/usr/bin/env python3
"""Regression test for Bug q-2026-05-17-87b5: queue minisite must honor
``task:<id>`` scope tokens as dependency edges, not only the legacy
``depends_on`` field.

Symptom (2026-05-17): the minisite rendered the READY badge on a pending
item whose dependency was a running peer, because session-task encodes
deps as ``task:<id>`` scope tokens (canonical since 2026-05-08) and the
minisite's ``_compute_ready_now`` / ``_shape`` only inspected the legacy
``depends_on`` list.

Test plan (mirrors the production case):

  1. Seed two items in disjoint scopes (so they're in different groups).
  2. Register one to running.
  3. Add a second pending item with ``--scope task:<running-id>``
     (NOT ``--depends-on`` — that path also writes the legacy field).
  4. Hit ``GET /api/queue`` and assert the pending item has
     ``ready_now == False`` and ``depends_on == [running-id]``.
  5. Hit ``GET /`` and assert the rendered ``<article>`` for the pending
     item does NOT contain ``<span class="badge ghead"`` (the READY
     badge) inside its body, and DOES contain the dep badge.

Run:
    python3 queue-minisite/test_task_scope_deps.py
"""

from __future__ import annotations

import json
import os
import re
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


class TaskScopeDepsTest(unittest.TestCase):
    """Regression: task:<id> scope tokens MUST gate ready_now, same as
    depends_on does."""

    @classmethod
    def setUpClass(cls):
        cls.tmp = tempfile.mkdtemp(prefix="qmin-task-scope-deps-")
        cls.queue_path = Path(cls.tmp) / "queue.json"

        cls.env = dict(os.environ)
        cls.env["HOME"] = cls.tmp
        Path(cls.tmp, ".config/session").mkdir(parents=True, exist_ok=True)
        Path(cls.tmp, ".config/claude").mkdir(parents=True, exist_ok=True)
        Path(cls.tmp, "claude-events").mkdir(parents=True, exist_ok=True)

        cls.env["PINGME_SESSION_TASK"] = "0"
        cls.env["CLAUDE_EVENT_SESSION_TASK"] = "0"
        cls.env["PINGME_DISABLE"] = "1"

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
        self.appmod._cache.fetched_at = 0.0

    # ------------------------------------------------------------------
    # helpers
    # ------------------------------------------------------------------

    def _add(self, desc: str, scopes: list[str], *extra: str) -> dict:
        cmd = [
            sys.executable, str(SESSION_TASK), "queue", "add",
            desc, "--summary", desc, "--json",
        ]
        for s in scopes:
            cmd.extend(["--scope", s])
        cmd.extend(extra)
        r = subprocess.run(
            cmd, capture_output=True, text=True, env=self.env, timeout=15,
        )
        if r.returncode != 0:
            raise RuntimeError(f"add failed: {r.stderr}")
        return json.loads(r.stdout)

    def _register(self, qid: str) -> None:
        r = subprocess.run(
            [sys.executable, str(SESSION_TASK), "queue", "register", qid, "--json"],
            capture_output=True, text=True, env=self.env, timeout=15,
        )
        if r.returncode != 0:
            raise RuntimeError(f"register failed: {r.stderr}")

    # ------------------------------------------------------------------
    # tests
    # ------------------------------------------------------------------

    def test_compute_ready_now_blocks_on_task_scope_dep(self):
        """``_compute_ready_now`` must return False when the only dep is
        encoded as a ``task:<running-id>`` scope token."""
        running = self._add("running-blocker", ["repo:a"])
        self._register(running["id"])
        pending = self._add(
            "pending-dependent",
            ["repo:b", f"task:{running['id']}"],
            "--force-enqueue",
        )

        with open(self.queue_actual) as f:
            data = json.load(f)
        items = data["items"]
        pending_item = next(it for it in items if it["id"] == pending["id"])

        # Sanity: dep IS encoded as scope, NOT as depends_on.
        self.assertIn(f"task:{running['id']}", pending_item.get("scope") or [])
        self.assertFalse(pending_item.get("depends_on"))

        ready = self.appmod._compute_ready_now(items, pending_item)
        self.assertFalse(
            ready,
            "pending item with task:<running-id> scope MUST NOT be ready_now",
        )

    def test_api_queue_ready_now_false_for_task_scope_dep(self):
        """``GET /api/queue`` must report ``ready_now=False`` for a
        pending item gated by a ``task:`` scope token."""
        running = self._add("api-running-blocker", ["repo:a"])
        self._register(running["id"])
        pending = self._add(
            "api-pending-dependent",
            ["repo:b", f"task:{running['id']}"],
            "--force-enqueue",
        )

        # Force cache miss so the freshly-seeded queue is read.
        self.appmod._cache.fetched_at = 0.0
        r = self.client.get("/api/queue")
        self.assertEqual(r.status_code, 200, r.get_data(as_text=True))
        body = r.get_json()

        pending_items = body.get("pending") or []
        match = next(
            (it for it in pending_items if it.get("id") == pending["id"]),
            None,
        )
        self.assertIsNotNone(
            match,
            f"pending item {pending['id']} not in /api/queue pending list",
        )
        self.assertFalse(
            match["ready_now"],
            f"/api/queue reported ready_now=True for {pending['id']} "
            f"despite task:<running-id> scope; full item={match!r}",
        )
        self.assertIn(running["id"], match.get("depends_on") or [])

    def test_rendered_html_omits_ready_badge_for_task_scope_dep(self):
        """``GET /`` must NOT render the READY badge in the article body
        of a pending item gated by a ``task:`` scope token."""
        running = self._add("html-running-blocker", ["repo:a"])
        self._register(running["id"])
        pending = self._add(
            "html-pending-dependent",
            ["repo:b", f"task:{running['id']}"],
            "--force-enqueue",
        )

        self.appmod._cache.fetched_at = 0.0
        r = self.client.get("/")
        self.assertEqual(r.status_code, 200)
        html = r.get_data(as_text=True)

        # Extract the <article ...> ... </article> block for the pending
        # item. The pending id is unique so we can scope to its block.
        pattern = re.compile(
            rf'<article[^>]*id="queue-{re.escape(pending["id"])}"[^>]*>.*?</article>',
            re.DOTALL,
        )
        match = pattern.search(html)
        self.assertIsNotNone(
            match,
            f"<article> for {pending['id']} not found in rendered HTML",
        )
        block = match.group(0)

        # The READY badge: <span class="badge ghead" ...>ready</span>
        self.assertNotIn(
            'class="badge ghead"',
            block,
            f"READY badge rendered inside <article> of {pending['id']} "
            "despite task:<running-id> scope blocking readiness.\n"
            f"Article block:\n{block}",
        )
        # And the article element itself should NOT carry the `ready`
        # CSS class (the line 237 conditional).
        article_open_tag = block.split(">", 1)[0]
        self.assertNotRegex(
            article_open_tag,
            r'class="[^"]*\bready\b[^"]*"',
            f"<article> for {pending['id']} carries the `ready` CSS "
            f"class despite task:<running-id> scope.\n"
            f"Open tag: {article_open_tag}",
        )

        # Sanity: the dep badge IS rendered (so the user can see the chain).
        self.assertIn(f"&rarr; {running['id']}", block)


if __name__ == "__main__":
    unittest.main(verbosity=2)
