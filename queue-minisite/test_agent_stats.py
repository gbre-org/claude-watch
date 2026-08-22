#!/usr/bin/env python3
"""Tests for the live per-agent activity counters (botchat #2967 / #3066).

A host cron writes an atomic JSON snapshot of per-agent tool-call / token
counters (``agent-stats.json``). The minisite JOINS ``agents[].queue_id``
onto the RUNNING rows and renders:

* per running row, in the item HEAD (so compact density keeps it): a cell
  with ``11 calls · 82K tok`` (comfortable) / ``11·82Kt`` (compact);
* in the header: the TOP half-row (right-aligned, #3090) is ONE outlined rounded pill —
  ``● N agents · C calls · K tok`` — botchat's topbar agent-bar look
  (botchat #3066): live dot, info-blue while ≥1 agent is live (``.active``),
  muted when none (``.idle``), dashed + ``n/a`` numerals when stale
  (``.stale``); it is a <button> that opens a per-agent popover
  (``#agent-bar-pop``, painted client-side by static/agent-bar.js from the
  same ``agent_stats`` payload: ``rows`` / ``main`` / freshness). The API
  still carries the long ``label`` (``N agents · C calls · K tok``).

Rules pinned here:

* rows join by queue id; a running row with no live agent renders NO cell;
* a STALE snapshot (> ``QUEUE_MINISITE_AGENT_STATS_STALE_SECONDS``) renders
  blank cells + an explicit ``n/a`` pill — never a frozen number;
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
import re
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
        cls.css = (HERE / "static" / "style.css").read_text(encoding="utf-8")
        cls.js = (HERE / "static" / "refresh.js").read_text(encoding="utf-8")
        cls.bar_js = (HERE / "static" / "agent-bar.js").read_text(encoding="utf-8")
        cls.tpl = (HERE / "templates" / "index.html").read_text(encoding="utf-8")

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

    def _meta(self, html: str) -> str:
        # #topbar-meta up to the popover shell that follows it (the JSON seed
        # after that carries the API strings — `label` etc. — and is NOT
        # rendered header text).
        start = html.index('id="topbar-meta"')
        end = html.index('<div id="agent-bar-pop"', start)
        return html[start:end]

    def _css_block(self, selector: str) -> str:
        m = re.search(re.escape(selector) + r"\s*\{([^}]*)\}", self.css)
        self.assertIsNotNone(m, selector)
        return m.group(1)

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

    def test_fmt_dur(self):
        """Popover durations mirror botchat's fmtSecs: 48s / 5m12s / 12m / 1h5m."""
        f = self.appmod._fmt_dur
        self.assertEqual(f(0), "0s")
        self.assertEqual(f(48.4), "48s")
        self.assertEqual(f(59.6), "60s")
        self.assertEqual(f(312), "5m12s")
        self.assertEqual(f(599), "9m59s")
        self.assertEqual(f(600), "10m")
        self.assertEqual(f(754), "12m")
        self.assertEqual(f(3900), "1h5m")
        self.assertEqual(f(7200), "2h0m")
        self.assertEqual(f(None), "?")
        self.assertEqual(f("x"), "?")
        self.assertEqual(f(-1), "?")

    def test_iso_to_epoch(self):
        f = self.appmod._iso_to_epoch
        self.assertAlmostEqual(f("1970-01-01T00:01:00Z"), 60.0)
        self.assertAlmostEqual(f("1970-01-01T00:01:00.500Z"), 60.5)
        self.assertAlmostEqual(f("1970-01-01T00:01:00+00:00"), 60.0)
        self.assertIsNone(f(""))
        self.assertIsNone(f(None))
        self.assertIsNone(f("not a date"))

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
        # Header agent-bar payload: the pill numerals + the long form for the
        # API + everything the popover prints.
        hdr = j["agent_stats"]
        self.assertIsNotNone(hdr)
        self.assertFalse(hdr["stale"])
        self.assertEqual(hdr["label"], "1 agents · 11 calls · 82K tok")
        self.assertEqual(hdr["main_label"], "main 546K")
        self.assertIn("main loop context 546K tokens", hdr["title"])
        self.assertEqual(hdr["agents"], 1)
        self.assertEqual(hdr["tool_calls"], 11)
        self.assertEqual(hdr["context_tokens"], 82040)
        self.assertEqual(hdr["output_tokens"], 3209)
        self.assertEqual(
            (hdr["agents_text"], hdr["calls_text"], hdr["tok_text"], hdr["out_text"]),
            ("1", "11", "82K", "3.2K"),
        )
        self.assertEqual(
            hdr["main"],
            {"context_tokens": 546266, "text": "546K", "age_seconds": 38.8, "age_text": "39s"},
        )
        self.assertEqual(hdr["host"], "testhost")
        self.assertEqual(hdr["age_text"], "0s")
        self.assertNotIn("pills", hdr)  # the per-part pills are gone
        # Popover rows: one per agent, description / type / qid / last tool /
        # the formatted numerals / age since spawn / last-write age.
        self.assertEqual(len(hdr["rows"]), 1)
        row = hdr["rows"][0]
        self.assertEqual(row["agent_id"], "a1")
        self.assertEqual(row["queue_id"], "q-2026-08-21-aaaa")
        self.assertEqual(row["description"], "agent a1")
        self.assertEqual(row["agent_type"], "general-purpose")
        self.assertEqual(row["last_tool"], "Bash")
        self.assertEqual((row["calls_text"], row["ctx_text"], row["out_text"]), ("11", "82K", "3.2K"))
        self.assertRegex(row["age_text"], r"^\d+h\d+m$")  # started_at is fixed in the past
        self.assertEqual(row["last_write_text"], "0s")
        self.assertFalse(row["finished"])
        # Projected: no per-row labels/titles the running cell already carries.
        self.assertNotIn("full_label", row)
        self.assertNotIn("title", row)

    def test_fresh_snapshot_renders_cell_and_agent_bar_in_html(self):
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
        # Header: the top half-row (right-aligned) is ONE outlined pill — a <button> with
        # the live dot, three numerals + long/short units, in the ACTIVE
        # state (≥1 live agent) — botchat's agent-bar look (#3066).
        meta = self._meta(html)
        self.assertIn('<div class="count-row count-row-agents">', meta)
        self.assertIn(
            '<button type="button" id="agent-bar" class="agent-bar active" aria-haspopup="dialog" '
            'aria-expanded="false" aria-controls="agent-bar-pop" title="',
            meta,
        )
        self.assertIn('<span class="agent-bar-dot" aria-hidden="true"></span>', meta)
        self.assertIn('<span class="agent-bar-num agent-bar-agents">1</span>', meta)
        self.assertIn('<span class="agent-bar-num agent-bar-calls">11</span>', meta)
        self.assertIn('<span class="agent-bar-num agent-bar-tok">82K</span>', meta)
        for unit in (" agents", " calls", " tok"):
            self.assertIn(f'<span class="agent-bar-unit agent-bar-unit-long">{unit}</span>', meta)
        for unit in ("a", "c", "t"):
            self.assertIn(f'<span class="agent-bar-unit agent-bar-unit-short">{unit}</span>', meta)
        self.assertEqual(meta.count('<span class="agent-bar-sep" aria-hidden="true">·</span>'), 2)
        self.assertIn("click for the per-agent breakdown", meta)
        # Exactly one pill, and the OLD per-part pills are gone.
        self.assertEqual(meta.count('id="agent-bar"'), 1)
        self.assertNotIn("count-agent-stats", html)
        self.assertNotIn(" agt", meta)
        self.assertNotIn("main 546K", meta)
        # The long form is NOT in the header (API `label` only).
        self.assertNotIn("1 agents · 11 calls · 82K tok", meta)
        # Popover shell + JSON seed (agent-bar.js paints from it on first
        # paint): outside #topbar-meta, inside the sticky header.
        hdr_start = html.index('<header class="topbar">')
        hdr_end = html.index("</header>", hdr_start)
        header = html[hdr_start:hdr_end]
        meta_end = header.index('id="topbar-meta"')
        pop_at = header.index('<div id="agent-bar-pop" class="agent-bar-pop" role="dialog" aria-label="Live agent activity" hidden></div>')
        seed_at = header.index('<script type="application/json" id="agent-bar-data">')
        self.assertLess(meta_end, pop_at)
        self.assertLess(pop_at, seed_at)
        seed_json = header[seed_at + len('<script type="application/json" id="agent-bar-data">'):header.index("</script>", seed_at)]
        seed = json.loads(seed_json)
        self.assertEqual(seed["agents_text"], "1")
        self.assertEqual(seed["rows"][0]["description"], "agent a1")
        self.assertEqual(seed["main"]["text"], "546K")
        # agent-bar.js is loaded by the page (after refresh.js).
        self.assertIn("filename='agent-bar.js'", self.tpl)
        self.assertLess(self.tpl.index("filename='refresh.js'"), self.tpl.index("filename='agent-bar.js'"))

    def test_json_seed_is_html_safe(self):
        """Descriptions come from agent prompts: a `</script>` in one must not
        break out of the seed (Flask's tojson escapes <, >, &, ')."""
        self._write_stats(_snapshot([_agent("a1", "q-2026-08-21-aaaa", description="x</script><b>y</b>&'z")]))
        html = self._html()
        start = html.index('id="agent-bar-data">') + len('id="agent-bar-data">')
        seed_json = html[start:html.index("</script>", start)]
        self.assertNotIn("</script>", seed_json)
        self.assertNotIn("<b>", seed_json)
        self.assertEqual(json.loads(seed_json)["rows"][0]["description"], "x</script><b>y</b>&'z")

    def test_idle_snapshot_renders_idle_pill_with_zero_numerals(self):
        """0 live agents is a real state (not stale): muted `.idle` pill,
        numerals 0 / 0 / 0, no rows, main loop still reported."""
        self._write_stats(_snapshot([]))
        j = self._api()
        hdr = j["agent_stats"]
        self.assertFalse(hdr["stale"])
        self.assertEqual(hdr["agents"], 0)
        self.assertEqual((hdr["agents_text"], hdr["calls_text"], hdr["tok_text"]), ("0", "0", "0"))
        self.assertEqual(hdr["rows"], [])
        self.assertEqual(hdr["main"]["text"], "546K")
        meta = self._meta(self._html())
        self.assertIn('class="agent-bar idle"', meta)
        self.assertIn('<span class="agent-bar-num agent-bar-agents">0</span>', meta)

    def test_liveness_badge_is_a_live_pill(self):
        """The old 10px liveness dot is now a small outlined `live` pill
        (botchat's connection-chip look); same element + classes."""
        html = self._html()
        self.assertIn('<span class="dot dot-ok" title="live — refreshes every 5s">live</span>', html)
        self.assertIn("${errorTxt ? 'error' : 'live'}</span>", self.js)
        self.assertIn("'dot-err' : 'dot-ok'", self.js)
        dot_css = self._css_block(".dot")
        self.assertIn("border-radius: 999px", dot_css)
        self.assertIn("border: 1px solid", dot_css)
        self.assertNotIn("width: 10px", dot_css)
        self.assertIn(".dot-ok  { color: var(--ok); border-color: var(--ok); }", self.css)
        self.assertIn(".dot-err { color: var(--critical); border-color: var(--critical); }", self.css)

    def test_header_is_two_stacked_rows_agent_bar_over_status(self):
        """botchat #2983 + #3066 + #3090: the header count pills are TWO
        stacked half-height rows inside one .count-stack — the agent-bar pill
        on TOP, right-aligned within the stack (#3090: "make the agent line
        the top one, and make it right aligned"), the status pills below,
        left-aligned as before — and both rows are hard-nowrap (flex-wrap +
        white-space + overflow/ellipsis) so the header never grows a third
        line. The pill is styled like botchat's topbar badge."""
        self._write_stats(_snapshot([_agent("a1", "q-2026-08-21-aaaa")]))
        html = self._html()
        meta = self._meta(html)
        stack = meta.index('<div class="count-stack" id="count-stack">')
        status = meta.index('<div class="count-row count-row-status">')
        agents = meta.index('<div class="count-row count-row-agents">')
        self.assertLess(stack, agents)
        self.assertLess(agents, status)
        # The agent-bar lives in the TOP row, the status pills in the BOTTOM row.
        self.assertLess(agents, meta.index('id="agent-bar"'))
        self.assertLess(meta.index('id="agent-bar"'), status)
        self.assertLess(status, meta.index("count-running"))
        self.assertLess(status, meta.index("count-pending"))
        # Exactly two rows, one stack; the controls stay OUTSIDE the stack.
        self.assertEqual(meta.count('class="count-row '), 2)
        self.assertEqual(meta.count('id="count-stack"'), 1)
        stack_end = meta.index("density-control")
        self.assertLess(meta.index("count-pending"), stack_end)
        # CSS: reduced size + no-wrap on both rows (one .count-row rule
        # covers both; the pill rule halves font/padding and ellipsises).
        stack_css = self._css_block(".count-stack")
        self.assertIn("flex-direction: column", stack_css)
        self.assertIn("min-width: 0", stack_css)
        # The stack stays flex-start (status row keeps its left edge); ONLY
        # the agent row is pushed to the stack's right edge.
        self.assertIn("align-items: flex-start", stack_css)
        agents_row_css = self._css_block(".count-row-agents")
        self.assertIn("align-self: flex-end", agents_row_css)
        self.assertIn("justify-content: flex-end", agents_row_css)
        self.assertNotIn("align-self", self._css_block(".count-row"))
        row_css = self._css_block(".count-row")
        self.assertIn("flex-wrap: nowrap", row_css)
        self.assertIn("white-space: nowrap", row_css)
        self.assertIn("overflow: hidden", row_css)
        pill_css = self._css_block(".count-row .count")
        self.assertIn("white-space: nowrap", pill_css)
        self.assertIn("text-overflow: ellipsis", pill_css)
        self.assertIn("overflow: hidden", pill_css)
        m = re.search(r"font-size:\s*([0-9.]+)rem", pill_css)
        self.assertIsNotNone(m, pill_css)
        self.assertLess(float(m.group(1)), 0.75)
        self.assertIn("padding: 1px 7px", pill_css)
        self.assertIn(".count-row .count { font-size: 0.62rem; padding: 1px 5px; }", self.css)
        self.assertIn("html.density-compact .count-row .count { font-size: 0.64rem;", self.css)
        # The agent-bar pill: botchat's chip geometry/colours, half-row sized.
        bar_css = self._css_block(".agent-bar")
        self.assertIn("border-radius: 999px", bar_css)
        self.assertIn("border: 1px solid var(--line)", bar_css)
        self.assertIn("font-variant-numeric: tabular-nums", bar_css)
        self.assertIn("cursor: pointer", bar_css)
        self.assertIn("white-space: nowrap", bar_css)
        m = re.search(r"font-size:\s*([0-9.]+)rem", bar_css)
        self.assertIsNotNone(m, bar_css)
        self.assertLess(float(m.group(1)), 0.75)
        self.assertIn("--info:      #268bd2", self.css)  # the blue token (light + dark)
        self.assertEqual(self.css.count("--info:"), 2)
        self.assertIn(".agent-bar.active { color: var(--info); border-color: var(--info); }", self.css)
        self.assertIn(".agent-bar.active .agent-bar-dot { background: var(--info); animation: agent-bar-pulse", self.css)
        self.assertIn(".agent-bar.stale { border-style: dashed;", self.css)
        self.assertIn(".agent-bar.stale .agent-bar-dot { background: var(--pending); }", self.css)
        self.assertIn("@keyframes agent-bar-pulse", self.css)
        self.assertIn(".agent-bar .agent-bar-unit-short { display: none; }", self.css)
        # Phone: the long units collapse to a/c/t (same as botchat's chip).
        self.assertIn("  .agent-bar .agent-bar-unit-long { display: none; }", self.css)
        self.assertIn("  .agent-bar .agent-bar-unit-short { display: inline;", self.css)
        # Popover: absolute in the sticky topbar, its right edge anchored to
        # the pill's via the --abp-right custom property that agent-bar.js
        # measures (12px fallback = the old fixed top-right inset); still
        # edge-to-edge on phones (that rule sets right/left outright, so the
        # measured anchor is ignored there).
        pop_css = self._css_block(".agent-bar-pop")
        self.assertIn("position: absolute", pop_css)
        self.assertIn("top: 100%", pop_css)
        self.assertIn("right: var(--abp-right, 12px)", pop_css)
        self.assertNotIn("left:", pop_css)
        self.assertIn(".agent-bar-pop[hidden] { display: none; }", self.css)
        self.assertIn("  .agent-bar-pop { right: 6px; left: 6px; min-width: 0; max-width: none; }", self.css)
        bar = self.bar_js
        self.assertIn("function position()", bar)
        self.assertIn("setProperty('--abp-right'", bar)
        self.assertIn("removeProperty('--abp-right')", bar)
        self.assertIn("addEventListener('resize'", bar)
        # Positioned on open, on a repaint while open, and exposed for tests.
        self.assertRegex(bar, r"pop\.hidden = false;\s*syncBar\(\);\s*position\(\);")
        self.assertIn("if (!pop.hidden) { paint(); position(); }", bar)
        self.assertIn("window.__qsiteAgentBar = { update, open, close, paint, position, isOpen", bar)
        # Reduced motion stops the pulse.
        self.assertIn(".agent-bar.active .agent-bar-dot { animation: none; }", self.css)
        # Nothing hides the stack / rows / pill in compact or collapsed.
        hidden = re.findall(
            r"html\.(?:density-compact|header-collapsed)[^{]*\.(?:count-(?:stack|row)|agent-bar)[^{]*\{[^}]*display:\s*none",
            self.css,
        )
        self.assertEqual(hidden, [], hidden)

    def test_compact_css_swaps_full_for_short_never_hides(self):
        """The compact-density rule shows the short label and hides the full
        one; nothing hides `.agent-stats` itself in compact."""
        css = self.css
        self.assertIn("html.density-compact .agent-stats-full { display: none; }", css)
        self.assertIn("html.density-compact .agent-stats-short { display: inline; }", css)
        hidden = re.findall(r"html\.density-compact[^{]*\.agent-stats\s*\{[^}]*display:\s*none", css)
        self.assertEqual(hidden, [], hidden)
        self.assertIn(".agent-stats-short { display: none; }", css)

    def test_refresh_js_mirrors_template_markup(self):
        """The 5s SPA rebuild must render the same cell + pill classes as the
        Jinja first paint, or morphdom drops them on the first tick."""
        js, tpl = self.js, self.tpl
        for token in (
            'class="agent-stats"',
            'class="agent-stats-full"',
            'class="agent-stats-short"',
            'class="count-stack" id="count-stack"',
            'class="count-row count-row-status"',
            "count-row count-row-agents",
            'id="agent-bar"',
            'aria-haspopup="dialog"',
            'aria-controls="agent-bar-pop"',
            'class="agent-bar-dot" aria-hidden="true"',
            'class="agent-bar-num agent-bar-agents"',
            'class="agent-bar-num agent-bar-calls"',
            'class="agent-bar-num agent-bar-tok"',
            'class="agent-bar-unit agent-bar-unit-long"',
            'class="agent-bar-unit agent-bar-unit-short"',
            'class="agent-bar-sep" aria-hidden="true"',
            "click for the per-agent breakdown",
            "agent_stats",
        ):
            self.assertIn(token, js, token)
            self.assertIn(token, tpl, token)
        # The JS prints the server-supplied strings verbatim (one formatter).
        self.assertIn("agentStats.full_label", js)
        self.assertIn("agentStats.short_label", js)
        self.assertIn("agentStatsPill.agents_text", js)
        self.assertIn("agentStatsPill.calls_text", js)
        self.assertIn("agentStatsPill.tok_text", js)
        # State classes mirror the template's stale / active / idle choice, and
        # the rebuilt pill mirrors the LIVE popover's open state.
        self.assertIn("stale ? 'stale' : (nAgents > 0 ? 'active' : 'idle')", js)
        self.assertIn("getElementById('agent-bar-pop')", js)
        self.assertIn("popOpen ? ' open' : ''", js)
        # The old per-part pills are gone from both renderers.
        self.assertNotIn("count-agent-stats", js)
        self.assertNotIn("count-agent-stats", tpl)
        self.assertNotIn("agentStatsPill.pills", js)
        # The merge hands the popover the fresh payload every tick.
        self.assertIn("__qsiteAgentBar.update(state.agent_stats || null)", js)
        # agent-bar.js: paints from the seed + update(), textContent only.
        bar = self.bar_js
        self.assertIn("getElementById('agent-bar-data')", bar)
        self.assertIn("getElementById('agent-bar-pop')", bar)
        self.assertIn("window.__qsiteAgentBar", bar)
        self.assertNotIn("innerHTML", bar)
        self.assertNotIn("insertAdjacentHTML", bar)

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
        # Every numeral is withheld; the popover gets no rows and no main.
        self.assertEqual((hdr["agents_text"], hdr["calls_text"], hdr["tok_text"], hdr["out_text"]), ("n/a",) * 4)
        self.assertIsNone(hdr["agents"])
        self.assertEqual(hdr["rows"], [])
        self.assertIsNone(hdr["main"])
        self.assertRegex(hdr["age_text"], r"^5m\d+s$")
        self.assertNotIn("pills", hdr)
        html = self._html()
        self.assertNotIn("82K", html)
        self.assertNotIn("11 calls", html)
        self.assertNotIn('class="agent-stats"', html)
        meta = self._meta(html)
        # The pill stays (dashed, amber dot via .stale) and reads n/a ×3; the
        # row carries `stale` too.
        self.assertIn('<div class="count-row count-row-agents stale">', meta)
        self.assertIn('class="agent-bar stale"', meta)
        self.assertIn('<span class="agent-bar-num agent-bar-agents">n/a</span>', meta)
        self.assertIn('<span class="agent-bar-num agent-bar-calls">n/a</span>', meta)
        self.assertIn('<span class="agent-bar-num agent-bar-tok">n/a</span>', meta)
        self.assertIn("counters withheld rather than frozen", meta)

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
        self.assertNotIn("count-row-agents", html)
        self.assertNotIn('id="agent-bar"', html)
        self.assertNotIn('class="agent-stats"', html)
        # The popover shell stays (hidden) so a snapshot that appears after
        # load has somewhere to render; the seed is an explicit null.
        self.assertIn('id="agent-bar-pop"', html)
        self.assertIn('<script type="application/json" id="agent-bar-data">null</script>', html)
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
        # by_agent_id keeps every agent — and so do the popover rows, in
        # producer order (the popover is per AGENT, not per queue item).
        self.assertEqual(set(view["by_agent_id"]), {"a-old", "a-live", "a-live2"})
        self.assertEqual([r["agent_id"] for r in view["totals"]["rows"]], ["a-old", "a-live", "a-live2"])
        self.assertTrue(view["totals"]["rows"][0]["finished"])

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
        self.assertEqual(j["totals"]["agents_text"], "1")
        self.assertEqual(j["totals"]["host"], "testhost")
        self.assertEqual(len(j["totals"]["rows"]), 1)
        self.assertEqual(j["main"]["context_tokens"], 546266)
        self.assertIn("q-2026-08-21-aaaa", j["by_queue_id"])
        self.assertIn("a1", j["by_agent_id"])
        self.assertEqual(j["stale_after_seconds"], 60.0)


class AgentStatsDefaultPathTest(unittest.TestCase):
    """With no QUEUE_MINISITE_AGENT_STATS_FILE the path is the SIBLING of
    AGENT_STATE_JSON (active-agents.json) — the producer's own default lands
    the snapshot in the claude-watch state dir, so sharing that one mount is
    enough. Runs in its own class so it can import ``app`` under a different
    env; AgentStatsTest re-imports with its explicit path afterwards."""

    def test_default_is_sibling_of_agent_state_json(self):
        saved = {k: os.environ.get(k) for k in ("QUEUE_MINISITE_AGENT_STATS_FILE", "AGENT_STATE_JSON")}
        tmp = tempfile.mkdtemp(prefix="qmin-agent-stats-default-")
        try:
            os.environ.pop("QUEUE_MINISITE_AGENT_STATS_FILE", None)
            os.environ["AGENT_STATE_JSON"] = str(Path(tmp) / "state" / "active-agents.json")
            os.environ.setdefault("QUEUE_JSON", str(Path(tmp) / "queue.json"))
            sys.path.insert(0, str(HERE))
            for mod in list(sys.modules):
                if mod in ("app", "claude_agents"):
                    del sys.modules[mod]
            import app as appmod  # noqa: E402

            self.assertEqual(appmod.AGENT_STATS_PATH, str(Path(tmp) / "state" / "agent-stats.json"))
            for mod in ("app", "claude_agents"):
                sys.modules.pop(mod, None)
        finally:
            for k, v in saved.items():
                if v is None:
                    os.environ.pop(k, None)
                else:
                    os.environ[k] = v
            shutil.rmtree(tmp, ignore_errors=True)


if __name__ == "__main__":
    unittest.main(verbosity=2)
