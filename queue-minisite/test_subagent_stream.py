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
        # Authoritative agent_id -> queue_id bindings (post-tool-agent-arm-hook
        # output). The minisite reads this to build the REAL subagent tree.
        cls.bindings_path = Path(cls.tmp) / ".config/claude/agent-queue-bindings.json"

        os.environ["QUEUE_JSON"] = str(cls.queue_actual)
        os.environ["AGENT_STATE_JSON"] = str(cls.agent_state)
        os.environ["AGENTS_JSONL_ROOT"] = str(cls.jsonl_root)
        os.environ["QUEUE_LOG_ARCHIVE_DIR"] = str(cls.archive_dir)
        os.environ["WORKLOAD_LOG_DIR"] = str(Path(cls.tmp) / "no-workloads")
        os.environ["SESSION_TASK_BIN"] = str(SESSION_TASK)
        os.environ["AGENT_QUEUE_BINDINGS_JSON"] = str(cls.bindings_path)

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
        if self.bindings_path.exists():
            self.bindings_path.unlink()
        self.appmod._cache.fetched_at = 0.0

    def _seed_bindings(self, mapping: dict) -> None:
        """Write the arm-hook bindings file: {agent_id: queue_id}.

        Mirrors post-tool-agent-arm-hook's on-disk shape
        ``{"bindings": {aid: {"queue_id": qid, "registered_at": <int>}}}``
        so app.py _load_agent_queue_bindings parses it identically.
        """
        self.bindings_path.parent.mkdir(parents=True, exist_ok=True)
        bindings = {
            aid: {"queue_id": qid, "registered_at": 1700000000}
            for aid, qid in mapping.items()
        }
        self.bindings_path.write_text(json.dumps({"bindings": bindings}))

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
        multiple ``agent-*.jsonl`` siblings surfaces the siblings in the
        ``subagents`` list on /meta. The OWNER agent itself is DROPPED (it IS
        the item, not a child of it — the self-nesting fix). With no bindings
        and no markers this exercises the fallback path, which still drops
        the owner.
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
        self.assertNotIn(owner_id, ids,
                         f"owner must NOT self-nest: {subs}")
        self.assertIn(sibling_id, ids, f"sibling not in subagents: {subs}")
        # Each record carries the documented key set.
        for s in subs:
            self.assertIn("subagent_id", s)
            self.assertIn("label", s)
            self.assertIn("age_seconds", s)
            self.assertIn("age", s)
        # The sibling record's label is derived from the first user message.
        sib_rec = next(s for s in subs if s["subagent_id"] == sibling_id)
        self.assertEqual(sib_rec["label"], "do the thing")

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
        # Owner dropped (self-nest fix); the sibling resolves through the
        # two-level (project-slug + session) mount shape.
        self.assertNotIn(owner_id, ids)
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
        """Marker fallback (no bindings file): when session siblings carry
        DIFFERENT ``Queue item:`` markers, a running item surfaces ONLY the
        subagents attributed to ITS q-id -- and the OWNER agent is dropped
        (it is the item itself). Here owner_a carries qid_a's marker (so it's
        this item's own dispatch -> dropped) and the only other sibling is
        attributed to item B -> excluded. Result: empty tree for A.
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
        # Owner is self -> dropped; item-B sibling attributed elsewhere ->
        # excluded. A's tree is empty (no genuine child work under A).
        self.assertEqual(ids, set(),
                         f"expected empty tree for A, got {subs}")

    def test_meta_subagents_unfiltered_when_no_markers(self):
        """Back-compat fallback: when NO sibling carries a ``Queue item:``
        marker AND there is no bindings file (pre-arm-hook transcripts), keep
        the session siblings rather than render a silently-empty tree -- but
        STILL drop the owner so the self-nesting bug can't resurface even in
        fallback mode.
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
        # No attribution -> fallback keeps siblings, but the owner is dropped.
        self.assertEqual(ids, {sibling_id},
                         f"fallback keeps siblings minus owner, got {ids}")

    # ---------- authoritative-bindings tree (the FULL fix) ----------

    def test_bindings_drop_self_nested_owner(self):
        """With authoritative bindings, an item's OWN owner agent must NOT
        appear as a child of itself. The prior tree parsed the spawn marker
        and nested the owner under the item — the self-nesting bug. The
        owner is dropped; with only the owner present the tree is empty.
        """
        item = _add(self.env, "self-nest guard", ["repo:subagent-selfnest"])
        qid = item["id"]
        _register(self.env, qid)

        session_uuid = "aa11bb22-cc33-dd44-ee55-ff6600000001"
        owner_id = "aaa111bbb222ccc33"
        _seed_agent_state(self.agent_state, owner_id, qid, alive=True, age=5)
        # Owner transcript carries this item's own marker (as the main loop
        # seeds it) — the prior code would have nested it under the item.
        self._seed_marked_subagent(self.jsonl_root, session_uuid, owner_id, qid)
        # Authoritative binding: owner agent -> this item.
        self._seed_bindings({owner_id: qid})
        self.appmod._cache.fetched_at = 0.0

        r = self.client.get(f"/api/queue/{qid}/meta")
        self.assertEqual(r.status_code, 200, r.get_data(as_text=True))
        subs = r.get_json().get("subagents")
        self.assertIsInstance(subs, list)
        ids = {s["subagent_id"] for s in subs}
        self.assertNotIn(owner_id, ids,
                         f"owner must NOT be nested under itself: {subs}")
        self.assertEqual(subs, [], f"only-owner session -> empty tree: {subs}")

    def test_bindings_parent_attribution(self):
        """Bindings are authoritative: a subagent bound (via the bindings
        file) to a DIFFERENT queue id must NOT be attributed to this item,
        even if its transcript marker would (mis)match this item's id.
        """
        item_a = _add(self.env, "attrib item A", ["repo:subagent-attrib-a"])
        qid_a = item_a["id"]
        _register(self.env, qid_a)
        item_b = _add(self.env, "attrib item B", ["repo:subagent-attrib-b"])
        qid_b = item_b["id"]
        _register(self.env, qid_b)

        session_uuid = "bb22cc33-dd44-ee55-ff66-110000000002"
        owner_a = "bbb222ccc333ddd44"
        # A sibling whose TRANSCRIPT marker says qid_a (stale/misleading)
        # but whose AUTHORITATIVE binding says qid_b. It must be attributed
        # to B (i.e. excluded from A's tree).
        sibling = "ccc333ddd444eee55"
        _seed_agent_state(self.agent_state, owner_a, qid_a, alive=True, age=5)
        self._seed_marked_subagent(self.jsonl_root, session_uuid, owner_a, qid_a)
        # sibling's transcript marker claims qid_a...
        self._seed_marked_subagent(self.jsonl_root, session_uuid, sibling, qid_a)
        # ...but the authoritative binding attributes it to qid_b.
        self._seed_bindings({owner_a: qid_a, sibling: qid_b})
        self.appmod._cache.fetched_at = 0.0

        r = self.client.get(f"/api/queue/{qid_a}/meta")
        self.assertEqual(r.status_code, 200, r.get_data(as_text=True))
        ids = {s["subagent_id"] for s in r.get_json().get("subagents", [])}
        # Owner dropped (self), sibling attributed to B by binding -> A's
        # tree is empty. The sibling is NOT mis-pulled into A.
        self.assertNotIn(sibling, ids,
                         "binding -> B sibling must not appear under A")
        self.assertNotIn(owner_a, ids, "owner is self, must be dropped")

    def test_bindings_retry_siblings_collapsed_as_attempts(self):
        """Multiple agents bound to the SAME queue id are retry attempts of
        that item, NOT distinct children. The live owner is dropped; the
        earlier attempt(s) surface as kind='attempt' nodes (attempt N), in
        chronological order — never as separate child work.
        """
        item = _add(self.env, "retry attempts", ["repo:subagent-retry"])
        qid = item["id"]
        _register(self.env, qid)

        session_uuid = "cc33dd44-ee55-ff66-1122-330000000003"
        owner_id = "ddd444eee555fff66"      # current live dispatch
        retry_old = "eee555fff666aaa77"     # earlier abandoned attempt
        # owner is the live agent per active-agents.
        _seed_agent_state(self.agent_state, owner_id, qid, alive=True, age=2)
        # Make retry_old OLDER (larger age) than owner so it's attempt 1.
        self._seed_marked_subagent(self.jsonl_root, session_uuid, owner_id, qid)
        retry_path = self._seed_marked_subagent(
            self.jsonl_root, session_uuid, retry_old, qid)
        # Backdate the retry transcript so its mtime is clearly older.
        old_t = retry_path.stat().st_mtime - 600
        os.utime(retry_path, (old_t, old_t))
        # Both bound to the SAME item id.
        self._seed_bindings({owner_id: qid, retry_old: qid})
        self.appmod._cache.fetched_at = 0.0

        r = self.client.get(f"/api/queue/{qid}/meta")
        self.assertEqual(r.status_code, 200, r.get_data(as_text=True))
        subs = r.get_json().get("subagents")
        ids = {s["subagent_id"] for s in subs}
        # Owner dropped; only the earlier attempt remains, labeled attempt 1.
        self.assertNotIn(owner_id, ids, "live owner must be dropped")
        self.assertEqual(ids, {retry_old},
                         f"only the prior attempt remains: {subs}")
        attempt = next(s for s in subs if s["subagent_id"] == retry_old)
        self.assertEqual(attempt.get("kind"), "attempt",
                         f"retry sibling must be kind=attempt: {attempt}")
        self.assertEqual(attempt.get("attempt"), 1,
                         f"oldest retry is attempt 1: {attempt}")
        # And it carries an empty children list (uniform recursive shape).
        self.assertEqual(attempt.get("children"), [])

    def _seed_parent_with_spawn(self, jsonl_root, session_uuid, agent_id, qid,
                                child_agent_id):
        """Seed a transcript carrying the item marker AND a recorded
        Agent-tool spawn of ``child_agent_id``.

        The spawn shows up as an ``Agent`` ``tool_use`` block plus its matching
        ``tool_result`` whose text echoes ``agentId: <child>`` — exactly the
        ``run_in_background`` launch record Claude Code writes. This is the
        signal ``_session_subagent_parent_map`` uses to reconstruct the real
        spawn hierarchy.
        """
        tuid = f"toolu_{child_agent_id[:8]}"
        lines = [
            {
                "type": "user",
                "sessionId": session_uuid,
                "agentId": agent_id,
                "isSidechain": True,
                "uuid": "u1",
                "message": {
                    "role": "user",
                    "content": f"Queue item: {qid}\nParent task.",
                },
            },
            {
                "type": "assistant",
                "sessionId": session_uuid,
                "agentId": agent_id,
                "uuid": "a1",
                "message": {
                    "role": "assistant",
                    "content": [
                        {
                            "type": "tool_use",
                            "id": tuid,
                            "name": "Agent",
                            "input": {"prompt": f"Queue item: {qid}\nChild."},
                        }
                    ],
                },
            },
            {
                "type": "user",
                "sessionId": session_uuid,
                "agentId": agent_id,
                "uuid": "u2",
                "message": {
                    "role": "user",
                    "content": [
                        {
                            "type": "tool_result",
                            "tool_use_id": tuid,
                            "content": [
                                {
                                    "type": "text",
                                    "text": (
                                        "Async agent launched successfully.\n"
                                        f"agentId: {child_agent_id} (internal "
                                        "ID - do not mention to user.)"
                                    ),
                                }
                            ],
                        }
                    ],
                },
            },
        ]
        return _seed_subagent_jsonl(
            jsonl_root, session_uuid, agent_id, lines=lines)

    def test_nested_child_not_mislabeled_as_attempt(self):
        """REGRESSION (q-2026-06-19-7aa6): a nested child spawned BY the owner
        agent — which inherits the owner's ``Queue item: q-XXXX`` line and so
        binds to the SAME item — must render as a nested ``kind='child'``
        under the owner's tree, NOT as a false peer ``kind='attempt'`` retry.
        """
        item = _add(self.env, "nested child", ["repo:subagent-nested"])
        qid = item["id"]
        _register(self.env, qid)

        session_uuid = "dd44ee55-ff66-1122-3344-550000000004"
        owner_id = "a63d61fcdccb43cb6"   # the main-loop dispatch (owner)
        child_id = "aaa255509254c3c81"   # nested child the owner spawned
        _seed_agent_state(self.agent_state, owner_id, qid, alive=True, age=2)
        # Owner transcript records the Agent-tool spawn of child_id.
        self._seed_parent_with_spawn(
            self.jsonl_root, session_uuid, owner_id, qid, child_id)
        # Child transcript carries the inherited item marker.
        self._seed_marked_subagent(self.jsonl_root, session_uuid, child_id, qid)
        # BOTH bound to the SAME item id (the child inherited the line).
        self._seed_bindings({owner_id: qid, child_id: qid})
        self.appmod._cache.fetched_at = 0.0

        r = self.client.get(f"/api/queue/{qid}/meta")
        self.assertEqual(r.status_code, 200, r.get_data(as_text=True))
        subs = r.get_json().get("subagents")
        self.assertIsInstance(subs, list)
        # Owner dropped; the child surfaces (parent == dropped owner) at top
        # level of the owner's tree — but as a CHILD, never an attempt.
        ids = {s["subagent_id"] for s in subs}
        self.assertEqual(ids, {child_id},
                         f"only the nested child should surface: {subs}")
        child = next(s for s in subs if s["subagent_id"] == child_id)
        self.assertEqual(child.get("kind"), "child",
                         f"nested child must be kind=child, not attempt: {child}")
        self.assertNotIn("attempt", child,
                         f"child must carry no attempt ordinal: {child}")
        # No node anywhere is mislabeled an attempt.
        self.assertFalse(any(s.get("kind") == "attempt" for s in subs),
                         f"a parent->child pair must yield ZERO attempts: {subs}")

    def test_grandchild_nests_under_child(self):
        """A 3-level chain owner -> child -> grandchild (all bound to the same
        item via inherited markers) renders as real nesting: the grandchild
        hangs UNDER the child, and neither is an attempt.
        """
        item = _add(self.env, "grandchild chain", ["repo:subagent-gchild"])
        qid = item["id"]
        _register(self.env, qid)

        session_uuid = "ee55ff66-1122-3344-5566-770000000005"
        owner_id = "1111aaaa2222bbbb3"
        child_id = "4444cccc5555dddd6"
        grandchild_id = "7777eeee8888ffff9"
        _seed_agent_state(self.agent_state, owner_id, qid, alive=True, age=2)
        # owner spawns child; child spawns grandchild.
        self._seed_parent_with_spawn(
            self.jsonl_root, session_uuid, owner_id, qid, child_id)
        self._seed_parent_with_spawn(
            self.jsonl_root, session_uuid, child_id, qid, grandchild_id)
        self._seed_marked_subagent(
            self.jsonl_root, session_uuid, grandchild_id, qid)
        self._seed_bindings(
            {owner_id: qid, child_id: qid, grandchild_id: qid})
        self.appmod._cache.fetched_at = 0.0

        r = self.client.get(f"/api/queue/{qid}/meta")
        self.assertEqual(r.status_code, 200, r.get_data(as_text=True))
        subs = r.get_json().get("subagents")
        # Owner dropped -> child is the single top-level node; grandchild
        # nests under it. Zero attempts.
        self.assertEqual(len(subs), 1, f"child should be the lone root: {subs}")
        top = subs[0]
        self.assertEqual(top["subagent_id"], child_id)
        self.assertEqual(top.get("kind"), "child")
        self.assertEqual(len(top["children"]), 1,
                         f"grandchild must nest under child: {top}")
        self.assertEqual(top["children"][0]["subagent_id"], grandchild_id)
        self.assertEqual(top["children"][0].get("kind"), "child")
        self.assertFalse(any(s.get("kind") == "attempt" for s in subs))

    def test_deep_tree_descendant_bound_to_other_item_still_nests(self):
        """REGRESSION (depth-1-only bug): a deeply-nested descendant that
        binds to a DIFFERENT queue item than the owner must STILL nest under
        its real parent — it must not be severed and dropped.

        Real-world shape that broke (session 227ccc1f, item q-...-ade0):

            owner (item A)
              └─ child (item A)            # inherited A's marker
                   ├─ grandchild (item A)  # inherited A's marker
                   └─ grandchild (item B)  # re-seeded a FRESH queue line

        The prior tree filtered children to SAME-item agents only, so the
        item-B grandchild was pruned out of A's tree entirely (and its own
        item-B card showed nothing useful) — the tree rendered only the
        contiguous same-item sub-chain. The full-spawn-graph walk renders
        ALL transitive descendants of the owner regardless of binding, so
        BOTH grandchildren nest under the child, to full depth.
        """
        item_a = _add(self.env, "deep cross-item", ["repo:subagent-deepx"])
        qid_a = item_a["id"]
        item_b = _add(self.env, "child reseed", ["repo:subagent-deepx-b"])
        qid_b = item_b["id"]
        _register(self.env, qid_a)

        session_uuid = "11223344-5566-7788-99aa-bb0000000007"
        owner_id = "owner000aaaa1111a"
        child_id = "child111bbbb2222b"
        gc_same = "gcsame22cccc3333c"   # grandchild bound to item A
        gc_other = "gcother3dddd4444d"  # grandchild bound to item B (re-seed)

        _seed_agent_state(self.agent_state, owner_id, qid_a, alive=True, age=2)
        # owner -> child (both item A); child -> two grandchildren.
        self._seed_parent_with_spawn(
            self.jsonl_root, session_uuid, owner_id, qid_a, child_id)
        # child spawns gc_same (still item A) AND gc_other (re-seeded item B).
        # _seed_parent_with_spawn overwrites the child transcript, so seed the
        # child with BOTH launch records via a single transcript below.
        self._seed_parent_with_two_spawns(
            self.jsonl_root, session_uuid, child_id, qid_a, gc_same, gc_other)
        # Grandchild transcripts carry their respective inherited markers.
        self._seed_marked_subagent(self.jsonl_root, session_uuid, gc_same, qid_a)
        self._seed_marked_subagent(self.jsonl_root, session_uuid, gc_other, qid_b)
        # Authoritative bindings: gc_other binds to item B, the rest to A.
        self._seed_bindings({
            owner_id: qid_a, child_id: qid_a,
            gc_same: qid_a, gc_other: qid_b,
        })
        self.appmod._cache.fetched_at = 0.0

        r = self.client.get(f"/api/queue/{qid_a}/meta")
        self.assertEqual(r.status_code, 200, r.get_data(as_text=True))
        subs = r.get_json().get("subagents")
        # Owner dropped -> child is the lone top-level node.
        self.assertEqual(len(subs), 1, f"child should be lone root: {subs}")
        top = subs[0]
        self.assertEqual(top["subagent_id"], child_id)
        self.assertEqual(top.get("kind"), "child")
        # BOTH grandchildren nest under the child — including the item-B one.
        gc_ids = {c["subagent_id"] for c in top["children"]}
        self.assertEqual(
            gc_ids, {gc_same, gc_other},
            f"both grandchildren must nest under child (the item-B one was "
            f"the dropped node in the depth-1 bug): {top}")
        for c in top["children"]:
            self.assertEqual(c.get("kind"), "child",
                             f"grandchild must be a child node: {c}")
        # The item-B grandchild keeps its OWN owning-item attribution for
        # display even while nested under an item-A parent.
        gc_other_node = next(
            c for c in top["children"] if c["subagent_id"] == gc_other)
        self.assertEqual(gc_other_node.get("queue_id"), qid_b,
                         f"item-B grandchild keeps its own queue_id: {gc_other_node}")
        # Zero attempts anywhere (every node is a genuine spawn).
        def _has_attempt(nodes):
            return any(n.get("kind") == "attempt" or _has_attempt(
                n.get("children", [])) for n in nodes)
        self.assertFalse(_has_attempt(subs),
                         f"a pure spawn tree must yield ZERO attempts: {subs}")

    def _seed_parent_with_two_spawns(self, jsonl_root, session_uuid, agent_id,
                                     qid, child_a, child_b):
        """Like _seed_parent_with_spawn but records TWO Agent-tool launches in
        one transcript (one agent that spawned two children)."""
        def _spawn_pair(child, n):
            tuid = f"toolu_{child[:8]}{n}"
            return [
                {
                    "type": "assistant",
                    "sessionId": session_uuid,
                    "agentId": agent_id,
                    "uuid": f"a{n}",
                    "message": {
                        "role": "assistant",
                        "content": [{
                            "type": "tool_use", "id": tuid, "name": "Agent",
                            "input": {"prompt": f"Queue item: {qid}\nChild."},
                        }],
                    },
                },
                {
                    "type": "user",
                    "sessionId": session_uuid,
                    "agentId": agent_id,
                    "uuid": f"u{n}r",
                    "message": {
                        "role": "user",
                        "content": [{
                            "type": "tool_result", "tool_use_id": tuid,
                            "content": [{
                                "type": "text",
                                "text": (
                                    "Async agent launched successfully.\n"
                                    f"agentId: {child} (internal ID - do not "
                                    "mention to user.)"),
                            }],
                        }],
                    },
                },
            ]
        lines = [{
            "type": "user", "sessionId": session_uuid, "agentId": agent_id,
            "isSidechain": True, "uuid": "u1",
            "message": {"role": "user",
                        "content": f"Queue item: {qid}\nParent task."},
        }]
        lines += _spawn_pair(child_a, 1)
        lines += _spawn_pair(child_b, 2)
        return _seed_subagent_jsonl(
            jsonl_root, session_uuid, agent_id, lines=lines)

    def test_genuine_retry_and_nested_child_coexist(self):
        """A genuine main-loop retry (NOT spawned by any session agent) stays
        ``kind='attempt'`` while a nested child of the owner stays
        ``kind='child'`` — the two are partitioned correctly when both are
        bound to the same item.
        """
        item = _add(self.env, "retry plus child", ["repo:subagent-mix"])
        qid = item["id"]
        _register(self.env, qid)

        session_uuid = "ff661122-3344-5566-7788-990000000006"
        owner_id = "aaaa1111bbbb2222c"   # live owner dispatch
        retry_old = "dddd3333eeee4444f"  # earlier main-loop attempt (no parent)
        child_id = "5555aaaa6666bbbb7"   # owner's nested child
        _seed_agent_state(self.agent_state, owner_id, qid, alive=True, age=2)
        # owner spawns child_id (nested).
        self._seed_parent_with_spawn(
            self.jsonl_root, session_uuid, owner_id, qid, child_id)
        self._seed_marked_subagent(self.jsonl_root, session_uuid, child_id, qid)
        # retry_old is a plain transcript with the marker but NO spawn record
        # pointing at it -> main-loop attempt. Backdate so it's attempt 1.
        retry_path = self._seed_marked_subagent(
            self.jsonl_root, session_uuid, retry_old, qid)
        old_t = retry_path.stat().st_mtime - 600
        os.utime(retry_path, (old_t, old_t))
        self._seed_bindings(
            {owner_id: qid, retry_old: qid, child_id: qid})
        self.appmod._cache.fetched_at = 0.0

        r = self.client.get(f"/api/queue/{qid}/meta")
        self.assertEqual(r.status_code, 200, r.get_data(as_text=True))
        subs = r.get_json().get("subagents")
        by_id = {s["subagent_id"]: s for s in subs}
        self.assertIn(retry_old, by_id, f"retry must surface: {subs}")
        self.assertIn(child_id, by_id, f"nested child must surface: {subs}")
        self.assertEqual(by_id[retry_old].get("kind"), "attempt",
                         f"main-loop retry must stay attempt: {subs}")
        self.assertEqual(by_id[retry_old].get("attempt"), 1)
        self.assertEqual(by_id[child_id].get("kind"), "child",
                         f"owner's nested child must stay child: {subs}")

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
