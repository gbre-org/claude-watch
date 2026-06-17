#!/usr/bin/env python3
"""End-to-end tests for the nested-subagent tree + per-subagent log streams.

Covers the feature that surfaces each running main-loop agent's child
SUBAGENTS as a nested/expandable tree with per-subagent live log streams:

  1. ``/api/queue/<qid>/meta`` (and the home payload via ``_shape``) attaches a
     ``subagents`` list for a RUNNING item: it resolves the owner agent's
     parent session and enumerates ALL ``agent-*.jsonl`` siblings in that
     session's ``subagents/`` dir.
  2. ``GET /api/subagent/<id>/stream`` tails a subagent transcript directly
     (same SSE framing as ``/api/queue/<id>/stream``) — stream-start meta,
     backfill event frames, backfill-end.
  3. ``GET /api/subagent/<id>/meta`` returns cheap metadata (parent_session_id,
     label, age) — 400 on bad id format, 404 on a well-formed-but-missing id.
  4. Path-traversal / format guards on both endpoints.

Companion to ``test_live_stream.py`` (whose temp-HOME + sys.modules-reset +
``app.test_client()`` + ``data:`` SSE-parse style this file mirrors).

Run::

    python3 -m pytest queue-minisite/test_subagent_stream.py -v
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


def _resolve_session_task() -> Path:
    """Resolve the session-task CLI robustly.

    The sibling tests hard-code ``HERE.parent.parent / "claude-watch" /
    "tools" / "session-task" / "session-task"``, which MISSES when this
    file lives in a worktree dir named something other than
    ``claude-watch`` (e.g. ``claude-watch-minisite-nested``). Resolve via
    the SESSION_TASK_BIN env var first, else walk up from HERE looking for
    ``tools/session-task/session-task`` (the worktree root has it one level
    above the queue-minisite dir).
    """
    env_bin = os.environ.get("SESSION_TASK_BIN")
    if env_bin:
        return Path(env_bin)
    # HERE is the queue-minisite dir; HERE.parent is the worktree root.
    direct = HERE.parent / "tools" / "session-task" / "session-task"
    if direct.is_file():
        return direct
    # Walk further up just in case the layout nests deeper.
    cur = HERE
    for _ in range(6):
        cand = cur / "tools" / "session-task" / "session-task"
        if cand.is_file():
            return cand
        cur = cur.parent
    return direct  # best-effort default (matches HERE.parent shape)


SESSION_TASK = _resolve_session_task()


def _add(env: dict, desc: str, scopes: list[str]) -> dict:
    """Add a queue item via the canonical session-task CLI."""
    cmd = [sys.executable, str(SESSION_TASK), "queue", "add", desc,
           "--summary", desc, "--json"]
    for s in scopes:
        cmd.extend(["--scope", s])
    r = subprocess.run(cmd, capture_output=True, text=True, env=env, timeout=15)
    if r.returncode != 0:
        raise RuntimeError(f"add failed: {r.stderr}")
    return json.loads(r.stdout)


def _register(env: dict, qid: str) -> None:
    """Flip a queue item from pending -> running via session-task register."""
    cmd = [sys.executable, str(SESSION_TASK), "queue", "register", qid, "--json"]
    r = subprocess.run(cmd, capture_output=True, text=True, env=env, timeout=15)
    if r.returncode != 0:
        raise RuntimeError(f"register failed: {r.stderr}")


def _seed_agent_state(state_path: Path, agent_id: str, queue_id: str,
                      *, alive: bool = True, age: int = 1) -> None:
    """Write a single-agent active-agents.json mapping queue_id -> agent_id."""
    state_path.parent.mkdir(parents=True, exist_ok=True)
    state = {
        "subagents": [],
        "workloads": [],
        "agents": [
            {
                "agent_id": agent_id,
                "queue_id": queue_id,
                "alive": alive,
                "jsonl_age_seconds": age,
            }
        ],
    }
    state_path.write_text(json.dumps(state))


def _seed_subagent_jsonl(jsonl_root: Path, session_uuid: str, agent_id: str,
                         *, project_slug: str | None = None,
                         lines: list[dict] | None = None) -> Path:
    """Write a subagent transcript at the canonical on-disk layout.

    Mirrors ``test_live_stream._seed_jsonl``: one-level layout when
    ``project_slug`` is None, else two-level (project-slug + session) layout.
    Each record carries ``sessionId`` + ``agentId`` so the resolver chain
    (``_session_id_for_subagent`` -> ``_list_session_subagents``) works.
    """
    if project_slug:
        sess_dir = jsonl_root / project_slug / session_uuid / "subagents"
    else:
        sess_dir = jsonl_root / session_uuid / "subagents"
    sess_dir.mkdir(parents=True, exist_ok=True)
    if lines is None:
        lines = [
            {
                "type": "user",
                "sessionId": session_uuid,
                "agentId": agent_id,
                "isSidechain": True,
                "uuid": "u1",
                "message": {"role": "user", "content": "do the thing"},
            },
            {
                "type": "assistant",
                "sessionId": session_uuid,
                "agentId": agent_id,
                "uuid": "a1",
                "message": {
                    "role": "assistant",
                    "content": [{"type": "text", "text": "working on it"}],
                },
            },
        ]
    path = sess_dir / f"agent-{agent_id}.jsonl"
    body = "\n".join(json.dumps(rec) for rec in lines) + "\n"
    path.write_text(body)
    return path


class SubagentTreeTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.tmp = tempfile.mkdtemp(prefix="qmin-subagent-")
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
        cls.agent_state = Path(cls.tmp) / "active-agents.json"
        cls.jsonl_root = Path(cls.tmp) / "agents-jsonl"

        os.environ["QUEUE_JSON"] = str(cls.queue_actual)
        os.environ["AGENT_STATE_JSON"] = str(cls.agent_state)
        os.environ["AGENTS_JSONL_ROOT"] = str(cls.jsonl_root)
        os.environ["QUEUE_LOG_ARCHIVE_DIR"] = str(cls.archive_dir)
        os.environ["WORKLOAD_LOG_DIR"] = str(Path(cls.tmp) / "no-workloads")
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
        if self.agent_state.exists():
            self.agent_state.unlink()
        if self.jsonl_root.exists():
            shutil.rmtree(self.jsonl_root)
        self.appmod._cache.fetched_at = 0.0

    # ---------- helpers ----------

    def _read_sse(self, body_bytes: bytes) -> list[dict]:
        """Parse SSE 'data: {...}' lines from the response body into JSON."""
        events = []
        for raw in body_bytes.decode("utf-8", errors="replace").splitlines():
            if raw.startswith("data: "):
                events.append(json.loads(raw[len("data: "):]))
        return events

    def _fast_tail(self):
        """Shrink the SSE tail loop so streaming tests finish in <1s."""
        self.appmod.SSE_TAIL_MAX_IDLE_SECONDS = 0.1
        self.appmod.SSE_TAIL_POLL_SECONDS = 0.05
        self.appmod.SSE_TAIL_MAX_LIFETIME_SECONDS = 5.0

    # ---------- linkage / listing via /api/queue/<qid>/meta ----------

    def test_meta_surfaces_subagents_for_running_owner(self):
        """A running item whose owner agent resolves to a session dir with
        multiple ``agent-*.jsonl`` siblings must surface ALL of them in the
        ``subagents`` list on /meta. The owner agent is itself one of the
        siblings (it has a transcript in the same session subagents/ dir).
        """
        item = _add(self.env, "nested subagent tree", ["repo:subagent-test"])
        qid = item["id"]
        _register(self.env, qid)

        session_uuid = "a11e41aa-d23c-6fca-c000-0000000000aa"
        owner_id = "a11e41aad23c6fcac"
        sibling_id = "b22f52bbe34d7adbd"
        _seed_agent_state(self.agent_state, owner_id, qid, alive=True, age=5)
        # Two subagent transcripts in the SAME session subagents/ dir.
        _seed_subagent_jsonl(self.jsonl_root, session_uuid, owner_id)
        _seed_subagent_jsonl(self.jsonl_root, session_uuid, sibling_id)
        self.appmod._cache.fetched_at = 0.0

        r = self.client.get(f"/api/queue/{qid}/meta")
        self.assertEqual(r.status_code, 200, r.get_data(as_text=True))
        body = r.get_json()
        self.assertTrue(body.get("ok"))

        subs = body.get("subagents")
        self.assertIsInstance(subs, list, f"no subagents list: {body}")
        ids = {s["subagent_id"] for s in subs}
        self.assertIn(owner_id, ids, f"owner not in subagents: {subs}")
        self.assertIn(sibling_id, ids, f"sibling not in subagents: {subs}")
        # Each record carries the documented key set.
        for s in subs:
            self.assertIn("subagent_id", s)
            self.assertIn("label", s)
            self.assertIn("age_seconds", s)
            self.assertIn("age", s)
        # The owner record's label is derived from the first user-message text.
        owner_rec = next(s for s in subs if s["subagent_id"] == owner_id)
        self.assertEqual(owner_rec["label"], "do the thing")

    def test_meta_subagents_empty_when_no_owner_agent(self):
        """A running item with NO agent record (owner unknown) -> empty
        subagents list (the resolver short-circuits with no owner_agent_id)."""
        item = _add(self.env, "no owner", ["repo:subagent-noowner"])
        qid = item["id"]
        _register(self.env, qid)
        # Empty agent state -> owner.mode == 'unknown', no agent_id.
        self.agent_state.write_text(json.dumps(
            {"subagents": [], "workloads": [], "agents": []}))
        self.appmod._cache.fetched_at = 0.0

        r = self.client.get(f"/api/queue/{qid}/meta")
        self.assertEqual(r.status_code, 200)
        self.assertEqual(r.get_json().get("subagents"), [])

    def test_meta_subagents_two_level_container_layout(self):
        """The session-dir enumeration must also resolve the two-level
        (project-slug + session) container mount shape, matching
        ``_find_agent_jsonl`` / ``_list_session_subagents``.
        """
        item = _add(self.env, "two-level subs", ["repo:subagent-twolevel"])
        qid = item["id"]
        _register(self.env, qid)

        session_uuid = "c33f63cc-f45e-8a0b-d000-0000000000cc"
        owner_id = "c33f63ccf45e8a0bd"
        sibling_id = "d44a74dda56f9b1ce"
        project_slug = "-home-hndrewaall-workspace"
        _seed_agent_state(self.agent_state, owner_id, qid, alive=True, age=2)
        _seed_subagent_jsonl(self.jsonl_root, session_uuid, owner_id,
                             project_slug=project_slug)
        _seed_subagent_jsonl(self.jsonl_root, session_uuid, sibling_id,
                             project_slug=project_slug)
        self.appmod._cache.fetched_at = 0.0

        r = self.client.get(f"/api/queue/{qid}/meta")
        self.assertEqual(r.status_code, 200, r.get_data(as_text=True))
        ids = {s["subagent_id"] for s in r.get_json().get("subagents", [])}
        self.assertIn(owner_id, ids)
        self.assertIn(sibling_id, ids)

    def _seed_marked_subagent(self, jsonl_root, session_uuid, agent_id, qid):
        """Seed a subagent transcript whose first user message carries the
        ``Queue item: q-XXXX`` spawn marker (so its record gets a queue_id)."""
        lines = [
            {
                "type": "user",
                "sessionId": session_uuid,
                "agentId": agent_id,
                "isSidechain": True,
                "uuid": "u1",
                "message": {
                    "role": "user",
                    "content": f"Queue item: {qid}\nDo the scoped task.",
                },
            },
        ]
        return _seed_subagent_jsonl(
            jsonl_root, session_uuid, agent_id, lines=lines)

    def test_meta_subagents_filtered_to_owning_item(self):
        """When session siblings carry DIFFERENT ``Queue item:`` markers, a
        running item must surface ONLY the subagents bound to ITS q-id -- not
        every subagent in the owner's parent main-loop session.
        """
        # Two distinct items, both registered running, sharing one session.
        item_a = _add(self.env, "scoped item A", ["repo:subagent-filter-a"])
        qid_a = item_a["id"]
        _register(self.env, qid_a)
        item_b = _add(self.env, "scoped item B", ["repo:subagent-filter-b"])
        qid_b = item_b["id"]
        _register(self.env, qid_b)

        session_uuid = "e55a85ee-f56a-9b1c-e000-0000000000ee"
        # Owner of item A; its own subagent carries qid_a's marker.
        owner_a = "e55a85eef56a9b1ce"
        # A sibling subagent in the SAME session, bound to item B.
        sibling_b = "f66b96ffa67b0c2df"
        # The owner agent for item A.
        _seed_agent_state(self.agent_state, owner_a, qid_a, alive=True, age=5)
        self._seed_marked_subagent(self.jsonl_root, session_uuid, owner_a, qid_a)
        self._seed_marked_subagent(
            self.jsonl_root, session_uuid, sibling_b, qid_b)
        self.appmod._cache.fetched_at = 0.0

        r = self.client.get(f"/api/queue/{qid_a}/meta")
        self.assertEqual(r.status_code, 200, r.get_data(as_text=True))
        subs = r.get_json().get("subagents")
        self.assertIsInstance(subs, list, f"no subagents list: {subs}")
        ids = {s["subagent_id"] for s in subs}
        # Only item A's own subagent -- the item-B sibling is filtered out.
        self.assertEqual(ids, {owner_a},
                         f"expected only owner_a, got {subs}")
        self.assertEqual(subs[0].get("queue_id"), qid_a)

    def test_meta_subagents_unfiltered_when_no_markers(self):
        """Back-compat fallback: when NO sibling carries a ``Queue item:``
        marker (older transcripts), keep the prior unfiltered behavior rather
        than render a silently-empty tree.
        """
        item = _add(self.env, "no-marker fallback", ["repo:subagent-nomarker"])
        qid = item["id"]
        _register(self.env, qid)

        session_uuid = "a77c97aa-b78c-0d2e-f000-0000000000aa"
        owner_id = "a77c97aab78c0d2ef"
        sibling_id = "b88d08bbc89d1e3fa"
        _seed_agent_state(self.agent_state, owner_id, qid, alive=True, age=3)
        # Default fixtures have first-user text "do the thing" -- no marker.
        _seed_subagent_jsonl(self.jsonl_root, session_uuid, owner_id)
        _seed_subagent_jsonl(self.jsonl_root, session_uuid, sibling_id)
        self.appmod._cache.fetched_at = 0.0

        r = self.client.get(f"/api/queue/{qid}/meta")
        self.assertEqual(r.status_code, 200, r.get_data(as_text=True))
        ids = {s["subagent_id"] for s in r.get_json().get("subagents", [])}
        # No markers -> fallback keeps both (unfiltered).
        self.assertEqual(ids, {owner_id, sibling_id},
                         f"fallback should keep all, got {ids}")

    # ---------- GET /api/subagent/<id>/stream ----------

    def test_subagent_stream_emits_backfill(self):
        """/api/subagent/<id>/stream tails the transcript directly:
        stream-start meta (with the resolved path) + per-line event frames
        + backfill-end, same framing as /api/queue/<id>/stream.
        """
        session_uuid = "e55a85ee-a67a-9c2b-e000-0000000000ee"
        agent_id = "e55a85eea67a9c2be"
        _seed_subagent_jsonl(self.jsonl_root, session_uuid, agent_id)
        self._fast_tail()

        r = self.client.get(f"/api/subagent/{agent_id}/stream")
        self.assertEqual(r.status_code, 200, r.get_data(as_text=True))
        self.assertEqual(
            r.headers.get("Content-Type", "").split(";")[0],
            "text/event-stream",
        )
        self.assertEqual(r.headers.get("X-Accel-Buffering"), "no")

        events = self._read_sse(r.get_data())
        self.assertGreater(len(events), 0, "stream returned NO events")
        self.assertEqual(events[0]["type"], "meta")
        self.assertEqual(events[0]["kind"], "stream-start")
        self.assertIn(f"agent-{agent_id}.jsonl", events[0].get("path", ""),
                      events[0])
        kinds = [e.get("kind") for e in events]
        self.assertIn("backfill-begin", kinds)
        self.assertIn("backfill-end", kinds)
        line_events = [e for e in events if e.get("type") == "event"]
        self.assertEqual(len(line_events), 2,
                         f"expected 2 transcript events: {kinds}")
        self.assertEqual(line_events[0]["kind"], "user")
        self.assertEqual(line_events[1]["kind"], "assistant_text")

    def test_subagent_stream_400_on_bad_id(self):
        """Malformed subagent id (path-traversal / illegal chars) -> 400,
        not a 500 or a path escape."""
        r = self.client.get("/api/subagent/not!valid/stream")
        self.assertEqual(r.status_code, 400)

    def test_subagent_stream_no_jsonl_for_missing_wellformed_id(self):
        """A well-formed-but-nonexistent id emits a one-shot error:no-jsonl
        SSE frame (status 200), NOT a path escape / 500."""
        self._fast_tail()
        r = self.client.get("/api/subagent/deadbeefdeadbeef/stream")
        self.assertEqual(r.status_code, 200)
        events = self._read_sse(r.get_data())
        self.assertEqual(len(events), 1, events)
        self.assertEqual(events[0]["type"], "error")
        self.assertEqual(events[0]["kind"], "no-jsonl")
        self.assertEqual(events[0]["subagent_id"], "deadbeefdeadbeef")

    # ---------- GET /api/subagent/<id>/meta ----------

    def test_subagent_meta_happy_path(self):
        """/api/subagent/<id>/meta -> 200 with parent_session_id resolved from
        the transcript's first record + a label."""
        session_uuid = "f66b96ff-b78b-ad3c-f000-0000000000ff"
        agent_id = "f66b96ffb78bad3cf"
        _seed_subagent_jsonl(self.jsonl_root, session_uuid, agent_id)

        r = self.client.get(f"/api/subagent/{agent_id}/meta")
        self.assertEqual(r.status_code, 200, r.get_data(as_text=True))
        body = r.get_json()
        self.assertTrue(body.get("ok"))
        self.assertEqual(body["subagent_id"], agent_id)
        self.assertEqual(body["parent_session_id"], session_uuid)
        self.assertEqual(body["label"], "do the thing")
        self.assertIn("age", body)
        self.assertIn("age_seconds", body)

    def test_subagent_meta_400_on_bad_id(self):
        r = self.client.get("/api/subagent/bad!id/meta")
        self.assertEqual(r.status_code, 400)

    def test_subagent_meta_404_on_missing_wellformed_id(self):
        r = self.client.get("/api/subagent/deadbeefdeadbeef/meta")
        self.assertEqual(r.status_code, 404)
        self.assertFalse(r.get_json().get("ok"))


if __name__ == "__main__":
    unittest.main(verbosity=2)
