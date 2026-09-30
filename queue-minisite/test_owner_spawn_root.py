#!/usr/bin/env python3
"""Regression tests: the owner of a queue item is never its own subagent.

THE BUG THIS PINS (reported against both the dashboard cards and the
multitail view): when a running agent spawns SEVERAL subagents, the main
agent eventually re-appears as one of its own children — two rows, two log
streams, one agent — and the "N subagents" count includes it.

WHY IT HAPPENS. claude-watch's ``active-agents`` state maps agents to queue
items by parsing the ``Queue item: q-XXXX`` marker out of each transcript's
first user message. A subagent spawned by an agent inherits that line in its
own spawn prompt, so PARENT AND CHILDREN ALL CARRY THE SAME QUEUE ID and all
get a record under it. The consumer-side dedup (``agents_by_queue_id``) then
picks a single winner by liveness + transcript freshness — a race the parent
LOSES whenever it is parked inside a long tool call (or simply waiting on its
children) at the moment the state snapshot is taken. The item's "owner" flips
to one of its own descendants, and ``_build_subagent_tree`` — which correctly
drops the OWNER from the tree — then drops the child and re-emits the real
owner as a co-bound "peer" subagent. Hence the duplicate.

It starts MID-RUN because at spawn time the parent is the only agent carrying
the marker; the contest only exists once children are running.

THE FIX these tests pin: among agents sharing a queue id, one that is a
SPAWN DESCENDANT of another (evidence: the parent transcript's Agent/Task
launch record, the same ``agentId: <child>`` marker the subagent tree already
reconstructs) can never be that item's owner. Liveness/freshness only break
ties between candidates with no spawn edge between them.

Run::

    python3 queue-minisite/test_owner_spawn_root.py
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
    env_bin = os.environ.get("SESSION_TASK_BIN")
    if env_bin:
        return Path(env_bin)
    cur = HERE
    for _ in range(6):
        cand = cur / "tools" / "session-task" / "session-task"
        if cand.is_file():
            return cand
        cur = cur.parent
    return HERE.parent / "tools" / "session-task" / "session-task"


SESSION_TASK = _resolve_session_task()


def _add(env: dict, desc: str, scopes: list[str]) -> dict:
    cmd = [sys.executable, str(SESSION_TASK), "queue", "add", desc,
           "--summary", desc, "--json"]
    for s in scopes:
        cmd.extend(["--scope", s])
    r = subprocess.run(cmd, capture_output=True, text=True, env=env, timeout=15)
    if r.returncode != 0:
        raise RuntimeError(f"add failed: {r.stderr}")
    return json.loads(r.stdout)


def _register(env: dict, qid: str) -> None:
    cmd = [sys.executable, str(SESSION_TASK), "queue", "register", qid, "--json"]
    r = subprocess.run(cmd, capture_output=True, text=True, env=env, timeout=15)
    if r.returncode != 0:
        raise RuntimeError(f"register failed: {r.stderr}")


class OwnerSpawnRootTest(unittest.TestCase):
    """The real failing shape: one owner agent + two of its own subagents,
    all three carrying the SAME ``Queue item:`` marker, with the parent's
    record stale (parked in a tool call) and a child's record fresh.
    """

    @classmethod
    def setUpClass(cls):
        cls.tmp = tempfile.mkdtemp(prefix="qmin-ownerroot-")
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
        cls.agent_state = Path(cls.tmp) / "active-agents.json"
        cls.jsonl_root = Path(cls.tmp) / "agents-jsonl"
        cls.bindings_path = (
            Path(cls.tmp) / ".config/claude/agent-queue-bindings.json"
        )

        os.environ["QUEUE_JSON"] = str(cls.queue_actual)
        os.environ["AGENT_STATE_JSON"] = str(cls.agent_state)
        os.environ["AGENTS_JSONL_ROOT"] = str(cls.jsonl_root)
        os.environ["QUEUE_LOG_ARCHIVE_DIR"] = str(Path(cls.tmp) / "queue-logs")
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

    # ---------- fixture builders ----------

    def _session_dir(self, session_uuid: str) -> Path:
        d = self.jsonl_root / session_uuid / "subagents"
        d.mkdir(parents=True, exist_ok=True)
        return d

    def _write_transcript(self, session_uuid: str, agent_id: str,
                          qid: str, *, spawns: list[str] | None = None,
                          body: str = "working") -> Path:
        """Write ``agent-<id>.jsonl`` carrying the queue marker, and — for a
        parent — the Agent tool_use/tool_result pair whose result text names
        each spawned child (``agentId: <child>``). That pair is the on-disk
        evidence of a spawn edge; it is what the tree already reads.
        """
        lines: list[dict] = [
            {
                "type": "user",
                "sessionId": session_uuid,
                "agentId": agent_id,
                "isSidechain": True,
                "uuid": "u1",
                "message": {
                    "role": "user",
                    "content": f"Queue item: {qid}\nTASK: {body}",
                },
            },
        ]
        for n, child in enumerate(spawns or []):
            tool_use_id = f"toolu_{agent_id}_{n}"
            lines.append({
                "type": "assistant",
                "sessionId": session_uuid,
                "agentId": agent_id,
                "uuid": f"a{n}",
                "message": {
                    "role": "assistant",
                    "content": [{
                        "type": "tool_use",
                        "id": tool_use_id,
                        "name": "Agent",
                        "input": {"prompt": f"Queue item: {qid}\nTASK: sub"},
                    }],
                },
            })
            lines.append({
                "type": "user",
                "sessionId": session_uuid,
                "agentId": agent_id,
                "uuid": f"r{n}",
                "message": {
                    "role": "user",
                    "content": [{
                        "type": "tool_result",
                        "tool_use_id": tool_use_id,
                        "content": (
                            f"Agent started. agentId: {child} "
                            "(internal ID - do not mention to user)"
                        ),
                    }],
                },
            })
        lines.append({
            "type": "assistant",
            "sessionId": session_uuid,
            "agentId": agent_id,
            "uuid": "z1",
            "message": {
                "role": "assistant",
                "content": [{"type": "text", "text": body}],
            },
        })
        path = self._session_dir(session_uuid) / f"agent-{agent_id}.jsonl"
        path.write_text("\n".join(json.dumps(r) for r in lines) + "\n")
        return path

    def _seed_state(self, records: list[dict]) -> None:
        self.agent_state.write_text(json.dumps(
            {"subagents": [], "workloads": [], "agents": records}))

    def _seed_bindings(self, mapping: dict) -> None:
        self.bindings_path.parent.mkdir(parents=True, exist_ok=True)
        self.bindings_path.write_text(json.dumps({"bindings": {
            aid: {"queue_id": qid, "registered_at": 1700000000}
            for aid, qid in mapping.items()
        }}))

    def _fixture(self, scope: str, *, bindings: bool = True):
        """Owner P + children C1, C2 — all three co-bound to one queue id,
        P's record STALE (parked in a tool call), C1's record FRESH.
        """
        item = _add(self.env, "multi-subagent run", [scope])
        qid = item["id"]
        _register(self.env, qid)
        session = "c33690f0-fd8a-49c3-bd8c-000000000001"
        owner = "a3e17f46a3230aaaa"
        child_a = "ad95ba4981d10bbbb"
        child_b = "ab11cc22dd330cccc"
        self._write_transcript(session, owner, qid,
                               spawns=[child_a, child_b],
                               body="GPU capacity - real constraint")
        self._write_transcript(session, child_a, qid, body="LoRA adapter")
        self._write_transcript(session, child_b, qid, body="dataset fields")
        # The state snapshot that loses the race: the parent has been quiet
        # inside a tool call; a child wrote a second ago.
        self._seed_state([
            {"agent_id": owner, "queue_id": qid, "alive": False,
             "jsonl_age_seconds": 300, "in_flight_tool_use": True},
            {"agent_id": child_a, "queue_id": qid, "alive": True,
             "jsonl_age_seconds": 1},
            {"agent_id": child_b, "queue_id": qid, "alive": True,
             "jsonl_age_seconds": 40},
        ])
        if bindings:
            self._seed_bindings({owner: qid, child_a: qid, child_b: qid})
        self.appmod._cache.fetched_at = 0.0
        return qid, owner, child_a, child_b

    # ---------- the regression ----------

    def test_owner_is_the_spawn_root_not_its_freshest_child(self):
        qid, owner, child_a, child_b = self._fixture("repo:ownerroot-1")

        r = self.client.get(f"/api/queue/{qid}/meta")
        self.assertEqual(r.status_code, 200, r.get_data(as_text=True))
        body = r.get_json()
        self.assertEqual(
            body["owner"]["agent_id"], owner,
            "the item's owner is the agent the main loop spawned, not the "
            f"child that happened to write last: {body['owner']}",
        )

    def test_owner_never_appears_in_its_own_subagent_tree(self):
        qid, owner, child_a, child_b = self._fixture("repo:ownerroot-2")

        body = self.client.get(f"/api/queue/{qid}/meta").get_json()
        subs = body["subagents"]
        ids = {s["subagent_id"] for s in subs}
        self.assertNotIn(
            owner, ids,
            "the owner agent must never render as one of its own subagents "
            f"(that is the duplicate row/stream): {subs}",
        )
        self.assertEqual(
            ids, {child_a, child_b},
            f"both real children, and only those, belong in the tree: {subs}",
        )
        # And they are genuine spawn children, not neutral co-bound peers.
        self.assertEqual(
            {s["kind"] for s in subs}, {"child"},
            f"children of the owner are kind=child: {subs}",
        )
        self.assertEqual(
            len(subs), 2,
            "the subagent count the UI renders counts real children only",
        )

    def test_stream_resolves_the_same_owner_as_the_card(self):
        """The log stream and the card must resolve ONE owner: if they
        disagree the 'agent' pane and a 'subagent' pane tail the same file.
        """
        qid, owner, _child_a, _child_b = self._fixture("repo:ownerroot-3")

        queue_data, _err = self.appmod._read_queue()
        item = next(it for it in queue_data["items"] if it["id"] == qid)
        agent_by_qid = self.appmod._load_agent_state()
        aid, _source = self.appmod._resolve_stream_agent_id(
            qid, item, agent_by_qid)
        self.assertEqual(aid, owner,
                         "the stream tails the owner agent's transcript")

    def test_marker_only_attribution_still_re_roots(self):
        """No arm-hook bindings file (the long-lived legacy path): the
        transcript ``Queue item:`` markers alone must still re-root."""
        qid, owner, child_a, child_b = self._fixture(
            "repo:ownerroot-4", bindings=False)

        body = self.client.get(f"/api/queue/{qid}/meta").get_json()
        self.assertEqual(body["owner"]["agent_id"], owner)
        ids = {s["subagent_id"] for s in body["subagents"]}
        self.assertNotIn(owner, ids)
        self.assertEqual(ids, {child_a, child_b})

    def test_spawning_agent_is_never_rendered_as_a_subagent(self):
        """Belt and braces: when the spawning agent has NO active-agents
        record at all, owner resolution cannot re-root to it — and it must
        STILL be kept out of the tree, because a recorded spawn edge says it
        is the owner's parent, not its child.
        """
        item = _add(self.env, "parent aged out", ["repo:ownerroot-6"])
        qid = item["id"]
        _register(self.env, qid)
        session = "c33690f0-fd8a-49c3-bd8c-000000000003"
        owner = "a3e17f46a3230aaaa"
        child_a = "ad95ba4981d10bbbb"
        child_b = "ab11cc22dd330cccc"
        self._write_transcript(session, owner, qid,
                               spawns=[child_a, child_b], body="parent")
        self._write_transcript(session, child_a, qid, body="child a")
        self._write_transcript(session, child_b, qid, body="child b")
        # Only the children are in the state snapshot.
        self._seed_state([
            {"agent_id": child_a, "queue_id": qid, "alive": True,
             "jsonl_age_seconds": 1},
            {"agent_id": child_b, "queue_id": qid, "alive": True,
             "jsonl_age_seconds": 40},
        ])
        self._seed_bindings({owner: qid, child_a: qid, child_b: qid})
        self.appmod._cache.fetched_at = 0.0

        body = self.client.get(f"/api/queue/{qid}/meta").get_json()
        ids = {s["subagent_id"] for s in body["subagents"]}
        self.assertNotIn(
            owner, ids,
            f"the spawning agent is not a subagent of the item: {ids}")

    def test_unrelated_co_bound_agents_still_break_ties_on_liveness(self):
        """Two agents under one queue id with NO spawn edge between them
        (a genuine re-dispatch) keep the existing live>stale, freshest-wins
        rule — the fix must not change that."""
        item = _add(self.env, "re-dispatch", ["repo:ownerroot-5"])
        qid = item["id"]
        _register(self.env, qid)
        session = "c33690f0-fd8a-49c3-bd8c-000000000002"
        first = "aa11111111111aaaa"
        second = "bb22222222222bbbb"
        self._write_transcript(session, first, qid, body="attempt one")
        self._write_transcript(session, second, qid, body="attempt two")
        self._seed_state([
            {"agent_id": first, "queue_id": qid, "alive": False,
             "jsonl_age_seconds": 900},
            {"agent_id": second, "queue_id": qid, "alive": True,
             "jsonl_age_seconds": 2},
        ])
        self._seed_bindings({first: qid, second: qid})
        self.appmod._cache.fetched_at = 0.0

        body = self.client.get(f"/api/queue/{qid}/meta").get_json()
        self.assertEqual(body["owner"]["agent_id"], second)
        subs = body["subagents"]
        self.assertEqual({s["subagent_id"] for s in subs}, {first})
        self.assertEqual({s["kind"] for s in subs}, {"peer"})


if __name__ == "__main__":
    unittest.main(verbosity=2)
