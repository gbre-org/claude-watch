#!/usr/bin/env python3
"""Tests for the BLOCKED-item detail modal (botchat #2413, 2026-08-19).

Andrew: "clicking on blocked items still opens a modal w metadata (esp block
reason). this isnt visible in collapsed mode so would be nice to be able to
click in and see without switching modes"

Before this change the detail modal (``#log-modal``) was wired for RUNNING
(live / workload / hostjob), for DONE / ABANDONED items carrying an archived
transcript, and for subagent nodes -- but a BLOCKED card had no
``.log-clickable`` class and no ``data-log-mode`` at all, so it was inert.
The block reason existed only as a ``<p class="description">`` on the card,
and ``html.density-compact .item > .description`` elides that paragraph
wholesale, so operators running compact density could not read the reason
without leaving compact.

The fix reuses the existing modal rather than building a second one: blocked
rows open it in a new ``meta`` mode -- metadata only, no SSE stream. There IS
nothing to tail (``session-task queue block`` parks the item without a live
agent and never stamps ``log_archive_path``), so a stream pane would only
show a connection error.

Load-bearing details these tests pin:

* ``/api/queue/<id>/meta`` must carry ``block_reason`` -- the modal is fed
  entirely by that endpoint, so a field missing there is a blank section no
  matter how the front-end is wired.
* The reason must arrive **verbatim**. Real reasons run to several hundred
  words; truncating server-side would defeat the whole request.
* The SPA renderer in ``static/refresh.js`` must mirror the template. morphdom
  replaces the server's first paint within 5s, so an affordance that exists
  only in ``templates/index.html`` disappears on the first tick -- exactly the
  failure that once ate the entire BLOCKED section (q-2026-05-20-db66).
* Blocked items are not second-class: scope / group / timestamps /
  dependencies must be in the payload too.
* The card-level compact-density elision stays as it is. The request was to
  read the reason WITHOUT switching modes, so nothing here may flip the
  viewer's density or section-fold state.

Run::

    python3 queue-minisite/test_blocked_detail_modal.py
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

# A realistic reason at the length these actually reach. Andrew's run to "a
# few hundred words"; anything that clamps or ellipsizes fails the request.
LONG_REASON = (
    "Parked pending Andrew's greenlight on the force-push to main. The branch "
    "protection rule on this repo refuses a non-fast-forward update, and the "
    "rewrite this task needs is by definition non-fast-forward, so the work "
    "cannot proceed without either a per-event protection toggle or an "
    "explicit instruction to take a different approach. "
) * 6


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
    }
    d.update(extra)
    return d


class BlockedDetailModalTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.tmp = tempfile.mkdtemp(prefix="qmin-blocked-modal-")
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
        items = [
            _item(
                "q-blk1",
                "blocked",
                block_reason=LONG_REASON,
                blocked_at="2026-06-02T09:30:00+00:00",
                registered_at="2026-06-01T00:10:00+00:00",
                description="the original task prompt",
                scope=["repo:claude-watch"],
                group_id="g-alpha",
                depends_on=["q-dep1"],
            ),
            _item("q-blk2", "blocked"),  # parked with no reason recorded
            _item("q-dep1", "pending"),
            _item("q-run1", "running", registered_at="2026-06-01T00:01:00+00:00"),
        ]
        self.queue_actual.parent.mkdir(parents=True, exist_ok=True)
        with open(self.queue_actual, "w") as f:
            json.dump(
                {"schema_version": 3, "items": items, "locked_scopes": {}}, f
            )
        self.appmod._cache.fetched_at = 0.0

    # ---------- helpers ----------

    def _html(self) -> str:
        return self.client.get("/").data.decode("utf-8", errors="replace")

    def _card(self, html: str, item_id: str) -> str:
        start = html.find(f'data-queue-id="{item_id}"')
        self.assertNotEqual(start, -1, f"no card for {item_id}")
        start = html.rfind("<article", 0, start)
        end = html.find("</article>", start)
        return html[start:end]

    def _meta(self, item_id: str) -> dict:
        resp = self.client.get(f"/api/queue/{item_id}/meta")
        self.assertEqual(resp.status_code, 200, resp.data[:400])
        return json.loads(resp.data)

    # ---------- the card is clickable ----------

    def test_blocked_card_is_log_clickable(self):
        """The whole point: a blocked card opens the detail modal."""
        card = self._card(self._html(), "q-blk1")
        self.assertIn("log-clickable", card)
        self.assertIn('data-log-mode="meta"', card)

    def test_blocked_card_is_keyboard_reachable(self):
        """Clickable-by-mouse only would be a regression on every other row.

        Every other clickable card carries tabindex + role=button; keyboard.js
        activates the focused row on Enter and early-returns unless the row
        has .log-clickable.
        """
        card = self._card(self._html(), "q-blk1")
        self.assertIn('tabindex="0"', card)
        self.assertIn('role="button"', card)

    def test_blocked_card_without_reason_still_opens(self):
        """A reason-less blocked item still has scope/group/dep metadata.

        The modal is the detail view for the item, not a viewer for one
        field, so the affordance must not be conditional on block_reason.
        """
        card = self._card(self._html(), "q-blk2")
        self.assertIn("log-clickable", card)
        self.assertIn('data-log-mode="meta"', card)

    # ---------- the meta endpoint feeds it ----------

    def test_meta_carries_block_reason(self):
        meta = self._meta("q-blk1")
        self.assertTrue(meta["ok"])
        self.assertIn("block_reason", meta)
        self.assertEqual(meta["block_reason"], LONG_REASON)

    def test_meta_block_reason_is_not_truncated(self):
        """Several-hundred-word reasons arrive whole."""
        meta = self._meta("q-blk1")
        self.assertGreater(len(LONG_REASON), 1000, "fixture is too short to pin this")
        self.assertEqual(len(meta["block_reason"]), len(LONG_REASON))
        self.assertNotIn("…", meta["block_reason"])

    def test_meta_carries_blocked_at(self):
        """`blocked_at` is a distinct anchor from created/started."""
        meta = self._meta("q-blk1")
        self.assertEqual(meta["blocked_at"], "2026-06-02T09:30:00+00:00")
        self.assertNotEqual(meta["blocked_at"], meta["created_at"])

    def test_meta_block_reason_empty_for_other_statuses(self):
        """A running item gets an empty string, not a missing key.

        The front-end hides the section on falsy; a missing key would work
        today but silently break if the check ever tightened.
        """
        meta = self._meta("q-run1")
        self.assertIn("block_reason", meta)
        self.assertEqual(meta["block_reason"], "")

    def test_blocked_items_are_not_second_class(self):
        """Scope / group / deps / dependents / timestamps come through too.

        Andrew's ask was for "metadata (esp block reason)" -- the block reason
        is the headline, not the whole payload.
        """
        meta = self._meta("q-blk1")
        self.assertEqual(meta["status"], "blocked")
        self.assertEqual(meta["scope"], ["repo:claude-watch"])
        self.assertEqual(meta["group_id"], "g-alpha")
        self.assertEqual(meta["depends_on"], ["q-dep1"])
        self.assertTrue(meta["created_at"])
        self.assertEqual(meta["created_by"], "main-loop")
        self.assertEqual(meta["summary"], "summary q-blk1")

        dep_meta = self._meta("q-dep1")
        self.assertIn(
            "q-blk1",
            [d if isinstance(d, str) else d.get("id") for d in dep_meta["dependents"]],
        )

    # ---------- the modal scaffolding exists ----------

    def test_modal_has_a_scrolling_block_reason_section(self):
        """The reason renders into a pane that scrolls rather than clips."""
        html = self._html()
        self.assertIn('id="log-modal-blocker"', html)
        self.assertIn('id="log-modal-blocker-body"', html)
        # The body reuses .prompt-body, which is capped + scrollable.
        m = re.search(r'<pre class="([^"]*)" id="log-modal-blocker-body">', html)
        self.assertIsNotNone(m, "block-reason body is not a <pre>")
        self.assertIn("prompt-body", m.group(1))

        css = (HERE / "static" / "style.css").read_text()
        rule = re.search(r"\.log-modal-blocker \.prompt-body \{(.*?)\}", css, re.S)
        self.assertIsNotNone(rule, "no CSS sizing the block-reason body")
        body = rule.group(1)
        self.assertIn("max-height", body)
        self.assertIn("overflow", body)

    def test_details_only_mode_hides_the_stream(self):
        """No transcript exists for a blocked item -- don't show an empty pane.

        With the flex:1 stream gone the panel becomes the scroll container,
        which is what keeps a very long reason inside the 90vh cap.
        """
        css = (HERE / "static" / "style.css").read_text()
        self.assertRegex(css, r'\.log-modal\[data-mode="meta"\][^{]*\.log-stream')
        panel = re.search(
            r'\.log-modal\[data-mode="meta"\] \.log-modal-panel \{(.*?)\}',
            css,
            re.S,
        )
        self.assertIsNotNone(panel, "details-only panel does not scroll")
        self.assertIn("overflow-y: auto", panel.group(1))

    # ---------- the front-end wiring ----------

    def test_live_log_accepts_meta_mode(self):
        """`meta` must survive the mode whitelist in open()."""
        js = (HERE / "static" / "live-log.js").read_text()
        m = re.search(
            r"mode = \(row\.getAttribute\('data-log-mode'\).*?;\s*\n(.*?)\) mode = 'live';",
            js,
            re.S,
        )
        self.assertIsNotNone(m, "could not locate the mode whitelist")
        self.assertIn("'meta'", m.group(1))

    def test_meta_mode_opens_no_event_source(self):
        """A stream connection would just render a no-agent error."""
        js = (HERE / "static" / "live-log.js").read_text()
        m = re.search(r"if \(mode !== 'meta'\) \{\s*\n\s*connectEventSource\(", js)
        self.assertIsNotNone(m, "meta mode still opens an EventSource")

    def test_live_log_renders_the_block_reason(self):
        """applyMetaSummary must feed the disclosure from meta.block_reason."""
        js = (HERE / "static" / "live-log.js").read_text()
        self.assertIn("applyBlockReason(meta.block_reason)", js)
        # Rendered via textContent, so reason prose containing angle brackets
        # is never interpreted as markup.
        fn = re.search(r"function applyBlockReason\(reason\) \{(.*?)\n  \}", js, re.S)
        self.assertIsNotNone(fn, "applyBlockReason is missing")
        self.assertIn("textContent", fn.group(1))
        self.assertNotIn("innerHTML", fn.group(1))

    def test_renderer_and_template_agree(self):
        """refresh.js repaints the card every 5s -- it must match.

        A blocked section rendered without the affordance is exactly how the
        whole BLOCKED section once vanished five seconds after page load
        (q-2026-05-20-db66).
        """
        js = (HERE / "static" / "refresh.js").read_text()
        fn = re.search(r"function renderBlockedItem\(it\) \{(.*?)\n  \}", js, re.S)
        self.assertIsNotNone(fn, "renderBlockedItem is missing")
        markup = fn.group(1)
        self.assertIn("log-clickable", markup)
        self.assertIn('data-log-mode="meta"', markup)
        self.assertIn('tabindex="0"', markup)
        self.assertIn('role="button"', markup)

    # ---------- scope guards ----------

    def test_card_reason_paragraph_unchanged(self):
        """The on-card paragraph stays exactly as it was.

        The modal is an ADDITION. Operators in comfortable density read the
        reason straight off the card and must keep doing so.
        """
        card = self._card(self._html(), "q-blk1")
        self.assertIn("<strong>blocker:</strong>", card)

    def test_compact_density_card_rule_untouched(self):
        """Andrew asked to see the reason WITHOUT switching modes.

        The fix is the modal, not un-hiding the paragraph -- so the compact
        elision must still be in force. If a later change makes the card
        paragraph visible in compact, that is a different (and possibly
        unwanted) product decision and should fail here first.
        """
        css = (HERE / "static" / "style.css").read_text()
        self.assertIn("html.density-compact .item > .description", css)

    def test_opening_the_modal_does_not_touch_density_or_folds(self):
        """The viewer's collapsed/expanded state survives an open.

        Nothing in the modal path may write the density or section-fold
        localStorage keys, or toggle their classes -- that would switch the
        very mode Andrew asked not to have to switch.
        """
        js = (HERE / "static" / "live-log.js").read_text()
        for token in ("density-compact", "qsite_density", "qsite_header_collapsed"):
            self.assertNotIn(token, js, f"live-log.js manipulates {token}")

    def test_other_statuses_keep_their_modes(self):
        """Scope guard: running rows still open their live stream."""
        card = self._card(self._html(), "q-run1")
        self.assertIn("log-clickable", card)
        self.assertIn('data-log-mode="live"', card)


if __name__ == "__main__":
    unittest.main(verbosity=2)
