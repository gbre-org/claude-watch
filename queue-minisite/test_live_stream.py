#!/usr/bin/env python3
"""End-to-end smoke tests for the live-log ``/api/queue/<id>/stream`` endpoint.

Pipeline covered (in order):

  1. ``queue.json`` lists a running queue item.
  2. ``active-agents.json`` (claude-watch state) maps queue_id -> agent_id.
  3. ``<AGENTS_JSONL_ROOT>/<session>/subagents/agent-<id>.jsonl`` holds the
     transcript.
  4. ``GET /api/queue/<id>/stream`` resolves the chain and streams SSE
     frames (stream-start, backfill events, backfill-end).

This file fills a real coverage gap: the existing ``test_workload_archive.py``
suite covers ``/archive`` (post-mortem replay) and ``test_meta.py`` covers
``/meta`` (modal header), but until now there were ZERO tests for the live
``/stream`` path that actually drives the "live log" modal on the home page.
If any link in the chain (queue.json -> active-agents.json -> JSONL on disk)
breaks, the modal silently shows nothing — exactly the kind of regression a
smoke test should catch.

Run::

    python3 -m pytest queue-minisite/test_live_stream.py -v

Or as part of the full session-task / minisite suite::

    cd queue-minisite && uv run --python 3.11 --with pytest --with flask \
        --with python-dateutil python -m pytest -v
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
    """Write a single-agent active-agents.json that maps queue_id -> agent_id."""
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


def _seed_jsonl(jsonl_root: Path, session_uuid: str, agent_id: str,
                *, lines: list[dict], project_slug: str | None = None) -> Path:
    """Write a subagent JSONL at the canonical layout the app expects.

    ``project_slug``:
      * ``None`` (default) — one-level layout: writes at
        ``<jsonl_root>/<session_uuid>/subagents/agent-<id>.jsonl``.
        This mirrors gomorrah's production bind-mount which lands inside
        a single project slug.
      * ``str``           — two-level layout: writes at
        ``<jsonl_root>/<project_slug>/<session_uuid>/subagents/agent-<id>.jsonl``.
        This mirrors workbot's container bind-mount which lands at the
        ``~/.claude/projects`` parent dir (the public examples/compose
        default).
    """
    if project_slug:
        sess_dir = jsonl_root / project_slug / session_uuid / "subagents"
    else:
        sess_dir = jsonl_root / session_uuid / "subagents"
    sess_dir.mkdir(parents=True, exist_ok=True)
    path = sess_dir / f"agent-{agent_id}.jsonl"
    body = "\n".join(json.dumps(rec) for rec in lines) + "\n"
    path.write_text(body)
    return path


class LiveStreamEndpointTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.tmp = tempfile.mkdtemp(prefix="qmin-live-stream-")
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

        # Point app at the scratch state file / JSONL root rather than the
        # production /agents-state / /agents-jsonl paths. Without this the
        # tests would silently read host state and produce non-deterministic
        # results (a real footgun the workload-archive suite avoided by
        # never touching the agent state file).
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
        # Clear queue.json + active-agents.json + JSONL tree so each test
        # starts from a clean slate. Cache TTL = 5s; reset to force reread.
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
                payload = raw[len("data: "):]
                events.append(json.loads(payload))
        return events

    # ---------- happy path: live stream for a running agent ----------

    def test_stream_resolves_running_agent_and_emits_backfill(self):
        """End-to-end smoke: live /stream for a running queue item with a
        seeded active-agents.json + JSONL must yield stream-start, backfill
        events, and backfill-end (in that order). This is the exact
        invariant ``q-2026-05-15-991f`` (the queue item that opened this
        investigation) exercises in production — and the one Andrew's
        ``agent logs are not getting picked up in queue site`` DM was
        flagging.
        """
        item = _add(self.env, self.queue_actual,
                    "live stream smoke", ["repo:live-stream-test"])
        qid = item["id"]
        _register(self.env, qid)

        agent_id = "atest1234567890ab"
        session_uuid = "a1b2c3d4-e5f6-7890-abcd-ef1234567890"
        _seed_agent_state(self.agent_state, agent_id, qid, alive=True, age=1)
        _seed_jsonl(self.jsonl_root, session_uuid, agent_id, lines=[
            {
                "type": "user",
                "message": {"role": "user", "content": "Queue item: " + qid},
                "uuid": "u1",
                "sessionId": session_uuid,
                "agentId": agent_id,
            },
            {
                "type": "assistant",
                "message": {
                    "role": "assistant",
                    "content": [{"type": "text", "text": "hello"}],
                },
                "uuid": "a1",
                "sessionId": session_uuid,
                "agentId": agent_id,
            },
        ])
        self.appmod._cache.fetched_at = 0.0

        # Reduce idle-timeout to ~0.1s so the tail loop exits quickly after
        # backfill instead of holding the connection open for the default
        # 1800s. Each tail iteration sleeps SSE_TAIL_POLL_SECONDS (0.5s
        # default) — bring that down too so the test finishes inside a
        # single second of wall-clock.
        self.appmod.SSE_TAIL_MAX_IDLE_SECONDS = 0.1
        self.appmod.SSE_TAIL_POLL_SECONDS = 0.05
        self.appmod.SSE_TAIL_MAX_LIFETIME_SECONDS = 5.0

        r = self.client.get(f"/api/queue/{qid}/stream")
        self.assertEqual(r.status_code, 200, r.get_data(as_text=True))
        self.assertEqual(
            r.headers.get("Content-Type", "").split(";")[0],
            "text/event-stream",
        )
        # Belt-and-braces against nginx response buffering.
        self.assertEqual(r.headers.get("X-Accel-Buffering"), "no")

        events = self._read_sse(r.get_data())
        self.assertGreater(len(events), 0,
                           "stream returned NO events — pipeline broken")

        # First frame: stream-start meta with the resolved JSONL path.
        self.assertEqual(events[0]["type"], "meta")
        self.assertEqual(events[0]["kind"], "stream-start")
        self.assertIn(f"agent-{agent_id}.jsonl", events[0].get("path", ""),
                      events[0])

        # Backfill-begin frame, then per-line event frames, then
        # backfill-end. With 2 JSONL lines we expect 2 event frames.
        kinds = [e.get("kind") for e in events]
        self.assertIn("backfill-begin", kinds)
        self.assertIn("backfill-end", kinds)

        line_events = [
            e for e in events
            if e.get("type") == "event"
        ]
        self.assertEqual(
            len(line_events), 2,
            f"expected 2 transcript events, got {len(line_events)}: {kinds}",
        )
        # First line is a user record, second is assistant_text.
        self.assertEqual(line_events[0]["kind"], "user")
        self.assertEqual(line_events[1]["kind"], "assistant_text")

    # ---------- container shape: two-level (project-slug + session-uuid) ----------

    def test_stream_resolves_two_level_container_layout(self):
        """workbot / container shape: bind-mount lands at
        ``~/.claude/projects`` so the resolver sees
        ``<root>/<project-slug>/<session-uuid>/subagents/agent-<id>.jsonl``
        (one extra directory level above the gomorrah shape).

        The public ``examples/compose/docker-compose.yml`` ships with
        ``${HOME}/.claude/projects:/agents-jsonl:ro`` which is exactly
        this layout. Before PR #queue-minisite-container-jsonl-shape the
        resolver only handled the gomorrah-style mount (one level) and
        silently missed every agent JSONL on workbot — Andrew's "still
        not seeing agent logs in workbots queue site" DM
        (2026-05-15 17:36 ET) was this exact symptom.
        """
        item = _add(self.env, self.queue_actual,
                    "two-level layout", ["repo:two-level-test"])
        qid = item["id"]
        _register(self.env, qid)

        agent_id = "atwolevel12345678"
        session_uuid = "b1b2c3d4-e5f6-7890-abcd-ef1234567890"
        project_slug = "-home-hndrewaall-workspace"  # workbot-style slug
        _seed_agent_state(self.agent_state, agent_id, qid, alive=True, age=1)
        _seed_jsonl(
            self.jsonl_root, session_uuid, agent_id,
            project_slug=project_slug,
            lines=[
                {
                    "type": "user",
                    "message": {"role": "user",
                                "content": "Queue item: " + qid},
                    "uuid": "u1",
                    "sessionId": session_uuid,
                    "agentId": agent_id,
                },
                {
                    "type": "assistant",
                    "message": {
                        "role": "assistant",
                        "content": [{"type": "text", "text": "two-level"}],
                    },
                    "uuid": "a1",
                    "sessionId": session_uuid,
                    "agentId": agent_id,
                },
            ],
        )
        self.appmod._cache.fetched_at = 0.0

        # Shrink the tail loop so the test finishes promptly (same
        # rationale as the happy-path test above).
        self.appmod.SSE_TAIL_MAX_IDLE_SECONDS = 0.1
        self.appmod.SSE_TAIL_POLL_SECONDS = 0.05
        self.appmod.SSE_TAIL_MAX_LIFETIME_SECONDS = 5.0

        r = self.client.get(f"/api/queue/{qid}/stream")
        self.assertEqual(r.status_code, 200, r.get_data(as_text=True))
        events = self._read_sse(r.get_data())
        self.assertGreater(
            len(events), 0,
            "two-level resolver returned NO events — workbot regression",
        )
        # stream-start meta must surface the JSONL path (with project slug).
        self.assertEqual(events[0]["type"], "meta")
        self.assertEqual(events[0]["kind"], "stream-start")
        self.assertIn(
            project_slug, events[0].get("path", ""),
            f"stream-start path missing project slug: {events[0]!r}",
        )
        self.assertIn(
            f"agent-{agent_id}.jsonl", events[0].get("path", ""),
            events[0],
        )

        # 2 transcript events backfilled.
        line_events = [e for e in events if e.get("type") == "event"]
        self.assertEqual(
            len(line_events), 2,
            f"expected 2 transcript events, got {len(line_events)}",
        )
        self.assertEqual(line_events[0]["kind"], "user")
        self.assertEqual(line_events[1]["kind"], "assistant_text")

    def test_find_agent_jsonl_prefers_one_level_when_both_present(self):
        """Mixed layout (one-level dir AND two-level slug both contain
        a JSONL for the same agent_id): the one-level resolver runs
        first to preserve gomorrah's fast path. The two-level fallback
        only walks when the one-level probe finds nothing.

        We don't promise a stable picker across both shapes — only that
        a one-level hit short-circuits the two-level walk. The test
        asserts that property directly by checking the resolved path
        lives at the one-level depth.
        """
        agent_id = "amixedlayout1234"
        sess_a = "c1c2c3c4-c5c6-7890-abcd-ef1234567890"
        sess_b = "d1d2d3d4-d5d6-7890-abcd-ef1234567890"
        _seed_jsonl(
            self.jsonl_root, sess_a, agent_id,
            lines=[{"type": "user",
                    "message": {"role": "user", "content": "one-level"}}],
        )
        _seed_jsonl(
            self.jsonl_root, sess_b, agent_id,
            project_slug="-some-project",
            lines=[{"type": "user",
                    "message": {"role": "user", "content": "two-level"}}],
        )

        resolved = self.appmod._find_agent_jsonl(agent_id)
        self.assertIsNotNone(resolved)
        # Must be the one-level hit, not the two-level fallback.
        self.assertIn(f"/{sess_a}/", str(resolved))
        self.assertNotIn("-some-project", str(resolved))

    # ---------- failure: no active-agents record ----------

    def test_stream_emits_no_agent_error_when_state_missing_for_qid(self):
        """When the queue item exists but no agent is registered in
        active-agents.json, /stream must emit a one-shot
        ``error:no-agent`` SSE frame (not an HTTP 5xx). The front-end's
        polling mode depends on this exact shape to retry until the
        agent appears.
        """
        item = _add(self.env, self.queue_actual,
                    "no-agent stream", ["repo:no-agent-test"])
        qid = item["id"]
        _register(self.env, qid)
        # Intentionally seed an EMPTY agent state so the lookup misses.
        _seed_agent_state.__wrapped__ if False else None  # no-op alias
        self.agent_state.write_text(json.dumps(
            {"subagents": [], "workloads": [], "agents": []}
        ))
        self.appmod._cache.fetched_at = 0.0

        r = self.client.get(f"/api/queue/{qid}/stream")
        self.assertEqual(r.status_code, 200)
        events = self._read_sse(r.get_data())
        self.assertEqual(len(events), 1, events)
        self.assertEqual(events[0]["type"], "error")
        self.assertEqual(events[0]["kind"], "no-agent")
        self.assertEqual(events[0]["queue_id"], qid)

    # ---------- failure: agent state has the qid but JSONL is missing ----------

    def test_stream_emits_no_jsonl_error_when_transcript_missing(self):
        """active-agents.json points at an agent_id whose JSONL file
        doesn't actually exist on disk (e.g. host-token bind-mount
        misconfigured, transcript GC'd between active-agents write and
        the SSE open). /stream must emit ``error:no-jsonl`` rather than
        500. This is the canonical signal Andrew's DM was flagging if
        the bind-mount path drifted.
        """
        item = _add(self.env, self.queue_actual,
                    "no-jsonl stream", ["repo:no-jsonl-test"])
        qid = item["id"]
        _register(self.env, qid)

        # Seed agent state but NO matching JSONL on disk.
        agent_id = "amissingjsonl1234"
        _seed_agent_state(self.agent_state, agent_id, qid, alive=True, age=1)
        self.appmod._cache.fetched_at = 0.0

        r = self.client.get(f"/api/queue/{qid}/stream")
        self.assertEqual(r.status_code, 200)
        events = self._read_sse(r.get_data())
        self.assertEqual(len(events), 1, events)
        self.assertEqual(events[0]["type"], "error")
        self.assertEqual(events[0]["kind"], "no-jsonl")
        self.assertEqual(events[0]["agent_id"], agent_id)

    # ---------- format guards ----------

    def test_stream_400_on_bad_qid_format(self):
        """Malformed queue id (not matching the q-XXXX regex) -> 400."""
        r = self.client.get("/api/queue/not-a-real-id/stream")
        self.assertEqual(r.status_code, 400)

    # ---------- meta endpoint surfaces owner block for running agent ----------

    def test_meta_exposes_owner_block_for_live_agent(self):
        """``/meta`` must surface owner.agent_id + owner.mode='agent' +
        owner.alive=True for a running item with a live agent. This is
        the data the live-log modal renders in the Metadata section
        next to the live transcript — if the owner block is missing the
        modal title says ``owner unknown`` and the operator can't tell
        which agent the transcript belongs to.
        """
        item = _add(self.env, self.queue_actual,
                    "meta owner check", ["repo:meta-owner-test"])
        qid = item["id"]
        _register(self.env, qid)

        agent_id = "ameta1234567890ab"
        _seed_agent_state(self.agent_state, agent_id, qid, alive=True, age=1)
        # No JSONL needed — meta endpoint only consults queue.json + state.
        self.appmod._cache.fetched_at = 0.0

        r = self.client.get(f"/api/queue/{qid}/meta")
        self.assertEqual(r.status_code, 200, r.get_data(as_text=True))
        body = r.get_json()
        self.assertTrue(body.get("ok"))
        owner = body.get("owner")
        self.assertIsNotNone(owner, f"meta returned NO owner block: {body}")
        self.assertEqual(owner.get("mode"), "agent")
        self.assertTrue(owner.get("alive"))
        self.assertEqual(owner.get("agent_id"), agent_id)


if __name__ == "__main__":
    unittest.main(verbosity=2)
