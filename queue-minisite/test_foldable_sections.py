#!/usr/bin/env python3
"""Tests for the foldable status sections (botchat #843, 2026-08-05).

Andrew: "can you make the sections foldable". Every status section is now a
native ``<details class="queue-section" data-section-key="...">`` whose
``<summary>`` wraps the existing ``<h2 class="section-title">`` (spec-valid:
``<summary>`` accepts one heading element), so the section keeps its heading
semantics and — critically — keeps its item count visible while collapsed.

These tests pin the SERVER-SIDE FIRST PAINT. The same markup is emitted a
second time by ``static/refresh.js`` (``buildQueueDOM``) for the 5s morphdom
re-render, and pinned a third time by ``static/refresh.test.js`` (which also
proves the fold survives a merge). All three must agree: if the template
drifts, morphdom re-renders the section from the JS renderer's markup and the
template's version is discarded within 5 seconds.

Defaults pinned here: RUNNING + PENDING open (actionable work), BLOCKED /
DONE / ABANDONED collapsed (parked + terminal noise). Operator toggles are
persisted client-side in localStorage under ``section:<key>``.

Run::

    python3 queue-minisite/test_foldable_sections.py
"""

from __future__ import annotations

import json
import os
import re
import shutil
import sys
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent

# key -> default-open?
SECTION_DEFAULTS = {
    "running": True,
    "pending": True,
    "blocked": False,
    "done": False,
    "abandoned": False,
}


def _item(item_id: str, status: str, **extra) -> dict:
    d = {
        "id": item_id,
        "summary": f"summary {item_id}",
        "description": "",
        "scope": [],
        "status": status,
        "priority": 5,
        "created_by": "main-loop",
        "created_at": "2026-06-01T00:00:00+00:00",
        "registered_at": "2026-06-01T00:00:00+00:00",
        "completed_at": "2026-06-01T00:05:00+00:00",
        "abandoned_at": "2026-06-01T00:05:00+00:00",
    }
    d.update(extra)
    return d


class FoldableSectionsTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.tmp = tempfile.mkdtemp(prefix="qmin-foldable-")
        cls.queue_actual = Path(cls.tmp) / ".config/session/queue.json"
        os.environ["QUEUE_JSON"] = str(cls.queue_actual)
        os.environ["AGENT_STATE_JSON"] = str(Path(cls.tmp) / "no-agents.json")
        os.environ["AGENTS_JSONL_ROOT"] = str(Path(cls.tmp) / "no-jsonl")
        os.environ["QUEUE_LOG_ARCHIVE_DIR"] = str(Path(cls.tmp) / "no-archive")
        os.environ["WORKLOAD_LOG_DIR"] = str(Path(cls.tmp) / "no-workloads")

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
        self.appmod._cache.fetched_at = 0.0
        items = [
            _item("q-run1", "running"),
            _item("q-pen1", "pending"),
            _item("q-blk1", "blocked", block_reason="waiting on a human"),
            _item("q-don1", "done"),
            _item("q-aba1", "abandoned"),
        ]
        self.queue_actual.parent.mkdir(parents=True, exist_ok=True)
        with open(self.queue_actual, "w") as f:
            json.dump(
                {"schema_version": 3, "items": items, "locked_scopes": {}}, f
            )
        self.appmod._cache.fetched_at = 0.0

    def _html(self) -> str:
        return self.client.get("/").data.decode("utf-8", errors="replace")

    def _open_tag(self, html: str, key: str) -> str:
        m = re.search(rf"<details id=\"section-{key}\"[^>]*>", html)
        self.assertIsNotNone(m, f"no <details> for section-{key}")
        return m.group(0)

    def test_every_section_is_a_details_disclosure(self):
        """All five status sections render as .queue-section <details>."""
        html = self._html()
        for key in SECTION_DEFAULTS:
            tag = self._open_tag(html, key)
            self.assertIn('class="queue-section"', tag)
            self.assertIn(f'data-section-key="{key}"', tag)
        # No status section may remain a plain <section> wrapper.
        self.assertNotIn('<section id="section-', html)

    def test_default_open_states(self):
        """Actionable sections open; parked + terminal ones collapsed."""
        html = self._html()
        for key, want_open in SECTION_DEFAULTS.items():
            tag = self._open_tag(html, key)
            has_open = re.search(r"\bopen\b", tag) is not None
            self.assertEqual(
                has_open,
                want_open,
                f"section-{key}: expected default open={want_open}, tag={tag}",
            )

    def test_count_visible_in_collapsed_summary(self):
        """Folding hides detail, never information: the count is in the
        <summary>, so a collapsed section still says how much it holds."""
        html = self._html()
        for key in SECTION_DEFAULTS:
            start = html.index(f'<details id="section-{key}"')
            summary_end = html.index("</summary>", start)
            summary = html[start:summary_end]
            self.assertIn('class="section-summary"', summary)
            self.assertIn('class="section-count"', summary)
            # The heading survives inside the summary (a11y: heading nav).
            self.assertIn('<h2 class="section-title"', summary)
            self.assertRegex(
                summary, r'<span class="section-count">\s*\d+', f"section-{key}"
            )

    def test_items_live_inside_their_section(self):
        """Each card is nested in its section, so folding actually hides it."""
        html = self._html()
        order = ["running", "pending", "blocked", "done", "abandoned"]
        bounds = [html.index(f'<details id="section-{k}"') for k in order]
        expected = {
            "running": "q-run1",
            "pending": "q-pen1",
            "blocked": "q-blk1",
            "done": "q-don1",
            "abandoned": "q-aba1",
        }
        for i, key in enumerate(order):
            start = bounds[i]
            end = bounds[i + 1] if i + 1 < len(bounds) else len(html)
            self.assertIn(expected[key], html[start:end], f"section-{key}")

    def test_section_order_unchanged(self):
        """RUNNING → PENDING → BLOCKED → DONE → ABANDONED (botchat #833)."""
        html = self._html()
        found = re.findall(r'<details id="section-([a-z]+)"', html)
        self.assertEqual(
            found,
            ["running", "pending", "blocked", "done", "abandoned"],
        )

    def test_renderer_and_template_agree_on_defaults(self):
        """refresh.js' SPA renderer must use the SAME defaults as the
        template. If it drifts, the 5s morphdom tick silently re-renders
        every section with the JS defaults and the template's version is
        discarded — the exact three-place trap this feature has to avoid."""
        js = (HERE / "static" / "refresh.js").read_text()
        m = re.search(
            r"const SECTION_DEFAULT_OPEN = \{(.*?)\};", js, re.S
        )
        self.assertIsNotNone(m, "SECTION_DEFAULT_OPEN missing from refresh.js")
        body = m.group(1)
        for key, want_open in SECTION_DEFAULTS.items():
            mm = re.search(rf"{key}:\s*(true|false)", body)
            self.assertIsNotNone(mm, f"{key} missing from SECTION_DEFAULT_OPEN")
            self.assertEqual(
                mm.group(1) == "true",
                want_open,
                f"refresh.js default for {key} disagrees with the template",
            )


if __name__ == "__main__":
    unittest.main(verbosity=2)
