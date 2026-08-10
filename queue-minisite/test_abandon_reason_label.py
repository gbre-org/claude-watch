#!/usr/bin/env python3
"""Tests for the ABANDONMENT-REASON label (botchat #939, 2026-08-10).

Andrew: "make abandonment reason visible in q site".

The reason was already being emitted -- ``session-task queue abandon <id>
--reason "..."`` stores ``abandon_reason`` in queue.json and the abandoned
card rendered it. But it rendered as a BARE ``<p class="description">``: the
exact same unlabelled italic paragraph an item uses for its own task
description. Nothing on the card said "this sentence is WHY the item died",
so the reason read as leftover task blurb and was effectively invisible.

Note the asymmetry that gave it away: the BLOCKED card's sibling field
renders as ``<strong>blocker:</strong> ...``. ``abandon_reason`` was the only
reason-class field on the site with no label at all.

Two further visibility gaps are pinned here:

* **compact density** elided ``.description`` wholesale, so the reason
  vanished entirely for operators running compact. The task description may
  fairly be elided there (it is duplicated in the Prompt disclosure); the
  abandonment reason may NOT, because it is the ONLY record of why the item
  died -- it appears nowhere else on the card.
* **long reasons** must render in full. Real reasons run to ~1000 chars of
  multi-sentence prose, so nothing may clamp or ellipsize them.

Run::

    python3 queue-minisite/test_abandon_reason_label.py
"""

from __future__ import annotations

import html as html_mod
import json
import os
import re
import shutil
import sys
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent

# A realistic reason: multi-sentence, ~1000 chars. Modelled on the real
# q-2026-08-10-1d9b abandonment.
LONG_REASON = (
    "ANSWERED by q-2026-08-10-89f0 before it could spawn. That agent already "
    "performed the headless render Andrew asked for (983-word BBC article, "
    "clean 200, no DataDome) plus a live get_text A/B, proving the failure is "
    "Instapaper's server-side extractor, not entitlement. Running this "
    "separately would burn additional renders against a sensitive profile for "
    "information already in hand."
)


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


class AbandonReasonLabelTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.tmp = tempfile.mkdtemp(prefix="qmin-abandon-reason-")
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
            _item(
                "q-aba1",
                "abandoned",
                abandon_reason=LONG_REASON,
                description="the original task prompt",
            ),
            _item("q-aba2", "abandoned"),  # no reason recorded
            _item("q-blk1", "blocked", block_reason="waiting on a human"),
        ]
        self.queue_actual.parent.mkdir(parents=True, exist_ok=True)
        with open(self.queue_actual, "w") as f:
            json.dump(
                {"schema_version": 3, "items": items, "locked_scopes": {}}, f
            )
        self.appmod._cache.fetched_at = 0.0

    def _html(self) -> str:
        return self.client.get("/").data.decode("utf-8", errors="replace")

    def _card(self, html: str, item_id: str) -> str:
        """Return the <article> block for one queue item."""
        start = html.find(f'data-queue-id="{item_id}"')
        self.assertNotEqual(start, -1, f"no card for {item_id}")
        start = html.rfind("<article", 0, start)
        end = html.find("</article>", start)
        return html[start:end]

    # ---------- the label ----------

    def test_abandon_reason_is_labelled(self):
        """The reason paragraph says it is a reason.

        Without a label the sentence is indistinguishable from the item's own
        task description -- which is exactly the bug Andrew reported.
        """
        card = self._card(self._html(), "q-aba1")
        self.assertIn("abandon-reason", card)
        m = re.search(
            r'<p class="[^"]*abandon-reason[^"]*">(.*?)</p>', card, re.S
        )
        self.assertIsNotNone(m, "no .abandon-reason paragraph on the card")
        para = m.group(1)
        self.assertIn("<strong>reason:</strong>", para)

    def test_reason_text_renders_in_full(self):
        """Long multi-sentence reasons are not truncated or ellipsized."""
        # Jinja escapes the prose (apostrophes -> &#39;), so compare on the
        # unescaped card rather than the raw markup.
        card = html_mod.unescape(self._card(self._html(), "q-aba1"))
        self.assertIn(LONG_REASON, card)
        self.assertNotIn("…", card)

    def test_no_reason_means_no_empty_label(self):
        """An abandoned item with no recorded reason shows no stray label."""
        card = self._card(self._html(), "q-aba2")
        self.assertNotIn("abandon-reason", card)
        self.assertNotIn("<strong>reason:</strong>", card)

    def test_blocker_label_untouched(self):
        """Scope guard: the BLOCKED card's own label is unchanged."""
        card = self._card(self._html(), "q-blk1")
        self.assertIn("<strong>blocker:</strong>", card)
        self.assertIn("waiting on a human", card)

    def test_reason_is_distinct_from_task_description(self):
        """The reason and the task prompt stay separately identifiable."""
        card = self._card(self._html(), "q-aba1")
        # The task prompt lives in the Prompt disclosure, not in the
        # .abandon-reason paragraph.
        m = re.search(
            r'<p class="[^"]*abandon-reason[^"]*">(.*?)</p>', card, re.S
        )
        self.assertIsNotNone(m)
        self.assertNotIn("the original task prompt", m.group(1))
        self.assertIn("the original task prompt", card)

    # ---------- the SPA renderer must agree ----------

    def test_renderer_and_template_agree(self):
        """refresh.js re-renders the card every 5s -- it must match.

        morphdom replaces the server's first paint with the JS renderer's
        markup, so a label that exists only in the template disappears within
        five seconds of page load.
        """
        js = (HERE / "static" / "refresh.js").read_text()
        m = re.search(r"it\.abandon_reason.*?\n.*?reasonHtml\s*=\s*`(.*?)`", js, re.S)
        self.assertIsNotNone(m, "could not locate the abandon-reason renderer")
        markup = m.group(1)
        self.assertIn("abandon-reason", markup)
        self.assertIn("<strong>reason:</strong>", markup)

    # ---------- compact density ----------

    def test_reason_survives_compact_density(self):
        """Compact elides descriptions, but never the abandonment reason.

        The task description is safe to elide (the Prompt disclosure still
        carries it). The reason is not: the card is its only home.
        """
        css = (HERE / "static" / "style.css").read_text()
        # There must be a compact-density rule that re-shows the reason, and
        # it must come after the blanket .description hide so it wins.
        hide = css.find("html.density-compact .item > .description")
        self.assertNotEqual(hide, -1, "blanket compact hide rule vanished")
        show = css.find("abandon-reason", hide)
        self.assertNotEqual(
            show, -1, "compact density still hides the abandonment reason"
        )
        rule_end = css.find("}", show)
        self.assertIn("display", css[show:rule_end])


if __name__ == "__main__":
    unittest.main(verbosity=2)
