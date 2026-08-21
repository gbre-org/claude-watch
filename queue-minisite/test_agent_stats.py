#!/usr/bin/env python3
"""Tests for the live per-agent activity counters (botchat #2967).

A host cron writes an atomic JSON snapshot of per-agent tool-call / token
counters (``agent-stats.json``). The minisite JOINS ``agents[].queue_id``
onto the RUNNING rows and renders:

* per running row, in the item HEAD (so compact density keeps it): a cell
  with ``11 calls · 82K tok`` (comfortable) / ``11·82Kt`` (compact);
* in the header: ``N agents · C calls · K tok`` (+ main-loop context).

Rules pinned here:

* rows join by queue id; a running row with no live agent renders NO cell;
* a STALE snapshot (> ``QUEUE_MINISITE_AGENT_STATS_STALE_SECONDS``) renders
  blank cells + an explicit "agents n/a" pill — never a frozen number;
* an ABSENT file hides the feature entirely (no pill, no cell);
* an EMPTY ``QUEUE_MINISITE_AGENT_STATS_FILE`` switches it off;
* the 5s refresh.js renderer mirrors the Jinja markup (class parity);
* ``/api/agent-stats`` exposes the normalised view.

Run::

    python3 queue-minisite/test_agent_stats.py
"""

from __future__ import annotations

import json
import os
import shutil
import sys
import tempfile
import time
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent


def _write_queue(path: Path, items: list[dict]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with open(path, "w") as f:
        json.dump({"schema_version": 3, "items": items, "locked_scopes": {}}, f)


def _item(item_id: str, status: str = "running") -> dict:
    return {
        "id": item_id,
        "summary": f"summary {item_id}",
        "description": "",
        "scope": [],
        "status": status,
        "priority": 5,
        "created_by": "main-loop",
        "created_at": "2026-06-01T00:00:00+00:00",
        "registered_at": "2026-06-01T00:00:00+00:00",
    }


def _agent(aid: str, qid: str, **over) -> dict:
    rec = {
        "agent_id": aid,
        "session_id": "sess-1",
        "description": f"agent {aid}",
        "agent_type": "general-purpose",
        "queue_id": qid,
        "tool_calls": 11,
        "context_tokens": 82040,
        "output_tokens": 3209,
        "last_tool": "Bash",
        "started_at": "2026-08-21T21:57:49.957Z",
        "last_write_at": "2026-08-21T21:58:49.280Z",
        "age_seconds": 0.3,
        "finished": False,
    }
    rec.update(over)
    return rec


def _snapshot(agents: list[dict], generated_at: float | None = None) -> dict:
    tot_calls = sum(a.get("tool_calls") or 0 for a in agents)
    tot_ctx = sum(a.get("context_tokens") or 0 for a in agents)
    tot_out = sum(a.get("output_tokens") or 0 for a in agents)
    return {
        "version": 1,
        "host": "testhost",
        "generated_at": time.time() if generated_at is None else generated_at,
        "live_window_seconds": 900.0,
        "main": {
            "session_id": "sess-1",
            "context_tokens": 546266,
            "last_write_at": "2026-08-21T21:58:10.666Z",
            "age_seconds": 38.8,
        },
        "agents": agents,
        "totals": {
            "agents": len(agents),
            "tool_calls": tot_calls,
            "context_tokens": tot_ctx,
            "output_tokens": tot_out,
        },
    }


class AgentStatsTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.tmp = tempfile.mkdtemp(prefix="qmin-agent-stats-")
        cls.queue_actual = Path(cls.tmp) / ".config/session/queue.json"
        cls.stats_path = Path(cls.tmp) / "botchat" / "agent-stats.json"
        cls.stats_path.parent.mkdir(parents=True, exist_ok=True)
        os.environ["QUEUE_JSON"] = str(cls.queue_actual)
        os.environ["AGENT_STATE_JSON"] = str(Path(cls.tmp) / "no-agents.json")
        os.environ["AGENTS_JSONL_ROOT"] = str(Path(cls.tmp) / "no-jsonl")
        os.environ["QUEUE_LOG_ARCHIVE_DIR"] = str(Path(cls.tmp) / "no-archive")
        os.environ["WORKLOAD_LOG_DIR"] = str(Path(cls.tmp) / "no-workloads")
        os.environ["AGENT_QUEUE_BINDINGS_JSON"] = str(Path(cls.tmp) / "no-bindings.json")
        os.environ["QUEUE_MINISITE_AGENT_STATS_FILE"] = str(cls.stats_path)
        os.environ["QUEUE_MINISITE_AGENT_STATS_STALE_SECONDS"] = "60"

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

    # -- helpers ----------------------------------------------------------
    def setUp(self):
        self.appmod._cache.fetched_at = 0.0
        self.appmod.AGENT_STATS_PATH = str(self.stats_path)
        self._reset_stats_cache()
        if self.stats_path.exists():
            self.stats_path.unlink()
        _write_queue(
            self.queue_actual,
            [_item("q-2026-08-21-aaaa"), _item("q-2026-08-21-bbbb"), _item("q-p1", "pending")],
        )

    def _reset_stats_cache(self):
        c = self.appmod._agent_stats_cache
        c.data = None
        c.mtime = -1.0
        c.size = -1
        self.appmod._cache.fetched_at = 0.0

    def _write_stats(self, snap: dict, mtime: float | None = None) -> None:
        tmp = self.stats_path.with_suffix(".tmp")
        with open(tmp, "w") as f:
            json.dump(snap, f)
        os.replace(tmp, self.stats_path)
        if mtime is not None:
            os.utime(self.stats_path, (mtime, mtime))
        self._reset_stats_cache()

    def _api(self) -> dict:
        r = self.client.get("/api/queue")
        self.assertEqual(r.status_code, 200)
        return r.get_json()

    def _html(self) -> str:
        return self.client.get("/").data.decode("utf-8", errors="replace")

    def _article(self, html: str, qid: str) -> str:
        start = html.index(f'id="queue-{qid}"')
        end = html.index("</article>", start)
        return html[start:end]

    # -- formatting ---------------------------------------------------------
    def test_fmt_count(self):
        f = self.appmod._fmt_count
        self.assertEqual(f(0), "0")
        self.assertEqual(f(820), "820")
        self.assertEqual(f(999), "999")
        self.assertEqual(f(1000), "1K")
        self.assertEqual(f(8204), "8.2K")
        self.assertEqual(f(82040), "82K")
        self.assertEqual(f(546266), "546K")
        self.assertEqual(f(1_234_567), "1.2M")
        self.assertEqual(f(12_345_678), "12M")
        self.assertEqual(f(None), "?")
        self.assertEqual(f("junk"), "?")
        self.assertEqual(f(-5), "?")

    # -- fresh snapshot: rows joined + header totals -----------------------
    def test_fresh_snapshot_joins_running_rows_by_queue_id(self):
        self._write_stats(_snapshot([_agent("a1", "q-2026-08-21-aaaa")]))
        j = self._api()
        by_id = {it["id"]: it for it in j["running"]}
        a = by_id["q-2026-08-21-aaaa"]["agent_stats"]
        self.assertIsNotNone(a)
        self.assertEqual(a["tool_calls"], 11)
        self.assertEqual(a["context_tokens"], 82040)
        self.assertEqual(a["output_tokens"], 3209)
        self.assertEqual(a["last_tool"], "Bash")
        self.assertEqual(a["full_label"], "11 calls · 82K tok")
        self.assertEqual(a["short_label"], "11·82Kt")
        self.assertIn("3.2K output tokens", a["title"])
        self.assertIn("last tool Bash", a["title"])
        # The other running row has no live agent -> no cell.
        self.assertIsNone(by_id["q-2026-08-21-bbbb"]["agent_stats"])
        # Pending rows never carry the key.
        pend = {it["id"]: it for it in j["pending"]}
        self.assertNotIn("agent_stats", pend["q-p1"])
        # Header pill.
        hdr = j["agent_stats"]
        self.assertIsNotNone(hdr)
        self.assertFalse(hdr["stale"])
        self.assertEqual(hdr["label"], "1 agents · 11 calls · 82K tok")
        self.assertEqual(hdr["main_label"], "main 546K")
        self.assertIn("main loop context 546K tokens", hdr["title"])

    def test_fresh_snapshot_renders_cell_and_pill_in_html(self):
        self._write_stats(_snapshot([_agent("a1", "q-2026-08-21-aaaa")]))
        html = self._html()
        art = self._article(html, "q-2026-08-21-aaaa")
        # The cell sits in the item head, BEFORE the stop button, so it is
        # part of the one line compact density keeps.
        self.assertIn('class="agent-stats"', art)
        head_end = art.index("</header>")
        self.assertLess(art.index('class="agent-stats"'), head_end)
        self.assertIn('<span class="agent-stats-full">11 calls · 82K tok</span>', art)
        self.assertIn('<span class="agent-stats-short">11·82Kt</span>', art)
        self.assertIn('data-tool-calls="11"', art)
        self.assertIn('data-context-tokens="82040"', art)
        # Title carries the hover detail (output tokens, last tool, age).
        self.assertIn("3.2K output tokens", art)
        self.assertIn("last tool Bash", art)
        # The unmatched running row renders NO cell.
        art_b = self._article(html, "q-2026-08-21-bbbb")
        self.assertNotIn("agent-stats", art_b)
        # Header pill with totals + main context.
        self.assertIn('class="count count-agent-stats"', html)
        self.assertIn("1 agents · 11 calls · 82K tok", html)
        self.assertIn('<span class="agent-stats-main">· main 546K</span>', html)

    def test_compact_css_swaps_full_for_short_never_hides(self):
        """The compact-density rule shows the short label and hides the full
        one; nothing hides `.agent-stats` itself in compact."""
        css = (HERE / "static" / "style.css").read_text(encoding="utf-8")
        self.assertIn("html.density-compact .agent-stats-full { display: none; }", css)
        self.assertIn("html.density-compact .agent-stats-short { display: inline; }", css)
        # Guard: no rule hides the whole cell in compact.
        import re

        hidden = re.findall(r"html\.density-compact[^{]*\.agent-stats\s*\{[^}]*display:\s*none", css)
        self.assertEqual(hidden, [], hidden)
        # And the base hides the short form (comfortable shows the full one).
        self.assertIn(".agent-stats-short { display: none; }", css)

    def test_refresh_js_mirrors_template_markup(self):
        """The 5s SPA rebuild must render the same cell + pill classes as the
        Jinja first paint, or morphdom drops them on the first tick."""
        js = (HERE / "static" / "refresh.js").read_text(encoding="utf-8")
        tpl = (HERE / "templates" / "index.html").read_text(encoding="utf-8")
        for token in (
            'class="agent-stats"',
            'class="agent-stats-full"',
            'class="agent-stats-short"',
            "count-agent-stats",
            'class="agent-stats-main"',
            "agent_stats",
        ):
            self.assertIn(token, js, token)
            self.assertIn(token, tpl, token)
        # The JS prints the server-supplied labels verbatim (one formatter).
        self.assertIn("agentStats.full_label", js)
        self.assertIn("agentStats.short_label", js)
        self.assertIn("agentStatsPill.label", js)

    # -- staleness ----------------------------------------------------------
    def test_stale_snapshot_blanks_cells_and_shows_na_pill(self):
        old = time.time() - 300
        self._write_stats(
            _snapshot([_agent("a1", "q-2026-08-21-aaaa")], generated_at=old), mtime=old
        )
        j = self._api()
        by_id = {it["id"]: it for it in j["running"]}
        self.assertIsNone(by_id["q-2026-08-21-aaaa"]["agent_stats"])
        hdr = j["agent_stats"]
        self.assertIsNotNone(hdr)
        self.assertTrue(hdr["stale"])
        self.assertEqual(hdr["label"], "agents n/a")
        self.assertIn("stale", hdr["title"])
        html = self._html()
        self.assertNotIn("82K", html)
        self.assertNotIn("11 calls", html)
        self.assertNotIn('class="agent-stats"', html)
        self.assertIn("agents n/a", html)
        self.assertIn("count-agent-stats stale", html)

    def test_stale_by_generated_at_even_if_mtime_fresh(self):
        """generated_at is authoritative: a fresh mtime on an old snapshot
        (e.g. a copy / touch) must not resurrect frozen numbers."""
        old = time.time() - 300
        self._write_stats(_snapshot([_agent("a1", "q-2026-08-21-aaaa")], generated_at=old))
        view = self.appmod._load_agent_stats()
        self.assertTrue(view["stale"])
        self.assertEqual(view["by_queue_id"], {})
        self.assertIsNone(view["totals"])

    def test_stale_by_mtime_even_if_generated_at_missing(self):
        old = time.time() - 300
        snap = _snapshot([_agent("a1", "q-2026-08-21-aaaa")])
        snap.pop("generated_at")
        self._write_stats(snap, mtime=old)
        view = self.appmod._load_agent_stats()
        self.assertTrue(view["stale"])

    def test_staleness_is_not_cached(self):
        """A producer that stops writing flips the cached parse to stale
        without any on-disk change."""
        self._write_stats(_snapshot([_agent("a1", "q-2026-08-21-aaaa")]))
        fresh = self.appmod._load_agent_stats()
        self.assertFalse(fresh["stale"])
        later = self.appmod._load_agent_stats(now_ts=time.time() + 120)
        self.assertTrue(later["stale"])
        self.assertEqual(later["by_queue_id"], {})

    # -- absent / disabled ----------------------------------------------------
    def test_absent_file_hides_feature(self):
        # setUp already unlinked the file.
        j = self._api()
        self.assertIsNone(j["agent_stats"])
        for it in j["running"]:
            self.assertIsNone(it["agent_stats"])
        html = self._html()
        self.assertNotIn("count-agent-stats", html)
        self.assertNotIn('class="agent-stats"', html)
        view = self.appmod._load_agent_stats()
        self.assertTrue(view["enabled"])
        self.assertFalse(view["available"])

    def test_unparseable_file_hides_feature(self):
        self.stats_path.write_text("{not json", encoding="utf-8")
        self._reset_stats_cache()
        j = self._api()
        self.assertIsNone(j["agent_stats"])

    def test_empty_env_disables_feature(self):
        self._write_stats(_snapshot([_agent("a1", "q-2026-08-21-aaaa")]))
        self.appmod.AGENT_STATS_PATH = ""
        self._reset_stats_cache()
        j = self._api()
        self.assertIsNone(j["agent_stats"])
        by_id = {it["id"]: it for it in j["running"]}
        self.assertIsNone(by_id["q-2026-08-21-aaaa"]["agent_stats"])
        view = self.appmod._load_agent_stats()
        self.assertFalse(view["enabled"])
        self.assertFalse(view["available"])

    # -- dedup + cache ----------------------------------------------------------
    def test_dedup_prefers_unfinished_then_most_recent(self):
        snap = _snapshot(
            [
                _agent("a-old", "q-2026-08-21-aaaa", tool_calls=40, finished=True, age_seconds=500),
                _agent("a-live", "q-2026-08-21-aaaa", tool_calls=7, finished=False, age_seconds=30),
                _agent("a-live2", "q-2026-08-21-aaaa", tool_calls=9, finished=False, age_seconds=2),
            ]
        )
        self._write_stats(snap)
        view = self.appmod._load_agent_stats()
        self.assertEqual(view["by_queue_id"]["q-2026-08-21-aaaa"]["agent_id"], "a-live2")
        self.assertEqual(view["by_queue_id"]["q-2026-08-21-aaaa"]["tool_calls"], 9)
        # by_agent_id keeps every agent.
        self.assertEqual(set(view["by_agent_id"]), {"a-old", "a-live", "a-live2"})

    def test_parse_cached_on_mtime_size(self):
        self._write_stats(_snapshot([_agent("a1", "q-2026-08-21-aaaa")]))
        self.appmod._load_agent_stats()
        cached = self.appmod._agent_stats_cache.data
        self.assertIsNotNone(cached)
        self.appmod._load_agent_stats()
        self.assertIs(self.appmod._agent_stats_cache.data, cached)
        # A rewrite (new mtime) is picked up.
        time.sleep(0.02)
        self._write_stats(_snapshot([_agent("a1", "q-2026-08-21-aaaa", tool_calls=12)]))
        view = self.appmod._load_agent_stats()
        self.assertEqual(view["by_queue_id"]["q-2026-08-21-aaaa"]["tool_calls"], 12)

    # -- endpoint ----------------------------------------------------------------
    def test_api_agent_stats_endpoint_shape(self):
        self._write_stats(_snapshot([_agent("a1", "q-2026-08-21-aaaa")]))
        r = self.client.get("/api/agent-stats")
        self.assertEqual(r.status_code, 200)
        j = r.get_json()
        for key in (
            "enabled", "available", "stale", "age_seconds", "generated_at",
            "by_queue_id", "by_agent_id", "totals", "main", "path",
            "stale_after_seconds",
        ):
            self.assertIn(key, j, key)
        self.assertTrue(j["available"])
        self.assertFalse(j["stale"])
        self.assertEqual(j["totals"]["agents"], 1)
        self.assertEqual(j["totals"]["tool_calls"], 11)
        self.assertEqual(j["totals"]["context_tokens"], 82040)
        self.assertEqual(j["totals"]["main_context_tokens"], 546266)
        self.assertEqual(j["main"]["context_tokens"], 546266)
        self.assertIn("q-2026-08-21-aaaa", j["by_queue_id"])
        self.assertIn("a1", j["by_agent_id"])
        self.assertEqual(j["stale_after_seconds"], 60.0)


if __name__ == "__main__":
    unittest.main(verbosity=2)
