#!/usr/bin/env python3
"""Tests for status bucketing: no queue row may render invisibly.

The render pass used to bucket items with a hardcoded if/elif chain that had
no ``else``. Any status the chain didn't name was dropped on the floor — no
row, no count, no placeholder, no log line. Two real statuses were being eaten
that way:

  * ``wedged``      — a running item whose owning agent is stuck.
  * ``quarantined`` — ``queue abandon`` on a scope-owning item WITHOUT
                      evidence the process is gone.

Both still HOLD THEIR SCOPE. Hiding them is worse than hiding an idle row: the
operator sees the work vanish while the next spawn on that scope keeps getting
refused, and the UI never explains why.

The load-bearing test here is ``test_unknown_status_lands_in_fallback``. Adding
two hardcoded sections fixes two instances; it does not fix the bug, which is
that an undeclared status disappears silently. That test pins the general
property — a status this code has never heard of still renders — so the next
status added to the queue cannot vanish the way these two did.

Run::

    python3 queue-minisite/test_status_sections.py
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
REPO_ROOT = HERE.parent


def _item(item_id: str, status: str, **extra) -> dict:
    d = {
        "id": item_id,
        "summary": f"summary {item_id}",
        "description": "",
        "scope": [],
        "status": status,
        "priority": 5,
        "created_by": "main-loop",
        "created_at": "2026-08-01T00:00:00+00:00",
    }
    d.update(extra)
    return d


class StatusSectionsTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.tmp = tempfile.mkdtemp(prefix="qmin-status-sections-")
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
        # `_note_unknown_status` warns once per process; clear the memo so each
        # test starts from the same state.
        self.appmod._warned_unknown_statuses.clear()

    def _write(self, items: list[dict]) -> None:
        self.queue_actual.parent.mkdir(parents=True, exist_ok=True)
        with open(self.queue_actual, "w") as f:
            json.dump(
                {"schema_version": 3, "items": items, "locked_scopes": {}}, f
            )
        self.appmod._cache.fetched_at = 0.0

    def _payload(self) -> dict:
        return self.client.get("/api/queue").get_json()

    def _section(self, key: str) -> list[str]:
        return [it["id"] for it in self._payload().get(key, [])]

    def _html(self) -> str:
        resp = self.client.get("/")
        self.assertEqual(resp.status_code, 200)
        return resp.get_data(as_text=True)

    # -- the two statuses that were already invisible ----------------------

    def test_wedged_item_renders(self):
        """A wedged row appears in the payload AND in the rendered page."""
        self._write(
            [
                _item(
                    "q-wedge",
                    "wedged",
                    wedged_at="2026-08-10T12:00:00+00:00",
                    wedged_reason="context_limit",
                )
            ]
        )
        self.assertEqual(self._section("wedged"), ["q-wedge"])
        self.assertEqual(self._payload()["totals"]["wedged"], 1)

        html = self._html()
        self.assertIn('id="section-wedged"', html)
        self.assertIn("q-wedge", html)
        # The reason is the only record of WHY it's stuck.
        self.assertIn("context_limit", html)

    def test_quarantined_item_renders(self):
        """A quarantined row appears, with its reason and origin status."""
        self._write(
            [
                _item(
                    "q-quar",
                    "quarantined",
                    quarantined_at="2026-08-11T09:00:00+00:00",
                    quarantine_reason="no output for 40m, presumed dead",
                    quarantined_from="running",
                    scope=["repo:example"],
                )
            ]
        )
        self.assertEqual(self._section("quarantined"), ["q-quar"])
        self.assertEqual(self._payload()["totals"]["quarantined"], 1)

        html = self._html()
        self.assertIn('id="section-quarantined"', html)
        self.assertIn("q-quar", html)
        self.assertIn("no output for 40m, presumed dead", html)
        self.assertIn("from running", html)
        # The consequence an operator most needs: the scope is NOT free.
        self.assertIn("scope still held", html)

    def test_quarantined_card_shows_all_three_exits(self):
        """The three ways out are on the card, not just the state.

        A quarantined item is waiting on a human decision. Rendering the state
        without the exits leaves the operator knowing something is stuck but
        not what to do about it.
        """
        self._write(
            [
                _item(
                    "q-quar",
                    "quarantined",
                    quarantined_at="2026-08-11T09:00:00+00:00",
                    quarantine_reason="presumed dead",
                )
            ]
        )
        html = self._html()
        for cmd in (
            "session-task queue done q-quar",
            "session-task queue resurrect q-quar",
            "session-task queue release q-quar --reason",
        ):
            self.assertIn(cmd, html, f"missing quarantine exit: {cmd}")

    def test_wedged_card_shows_its_exits(self):
        self._write(
            [
                _item(
                    "q-wedge",
                    "wedged",
                    wedged_at="2026-08-10T12:00:00+00:00",
                    wedged_reason="heartbeat-stale",
                )
            ]
        )
        html = self._html()
        self.assertIn("session-task queue unwedge q-wedge", html)
        self.assertIn("session-task queue abandon q-wedge --reason", html)

    def test_scope_holding_ages_anchor_on_their_own_stamp(self):
        """Wedged/quarantined age from `wedged_at` / `quarantined_at`.

        Falling back to `created_at` would report the age of the TASK rather
        than the age of the wedge/quarantine, which is the number that decides
        whether an operator should act.
        """
        self._write(
            [
                _item(
                    "q-wedge",
                    "wedged",
                    created_at="2026-01-01T00:00:00+00:00",
                    wedged_at="2026-08-10T12:00:00+00:00",
                ),
                _item(
                    "q-quar",
                    "quarantined",
                    created_at="2026-01-01T00:00:00+00:00",
                    quarantined_at="2026-08-11T09:00:00+00:00",
                ),
            ]
        )
        payload = self._payload()
        self.assertEqual(payload["wedged"][0]["age_label"], "wedged")
        self.assertEqual(
            payload["wedged"][0]["wedged_at_iso"], "2026-08-10T12:00:00+00:00"
        )
        self.assertEqual(payload["quarantined"][0]["age_label"], "quarantined")
        self.assertEqual(
            payload["quarantined"][0]["quarantined_at_iso"],
            "2026-08-11T09:00:00+00:00",
        )

    def test_scope_holding_sections_sort_newest_first(self):
        self._write(
            [
                _item("q-w-old", "wedged", wedged_at="2026-08-01T00:00:00+00:00"),
                _item("q-w-new", "wedged", wedged_at="2026-08-09T00:00:00+00:00"),
                _item(
                    "q-q-old",
                    "quarantined",
                    quarantined_at="2026-08-02T00:00:00+00:00",
                ),
                _item(
                    "q-q-new",
                    "quarantined",
                    quarantined_at="2026-08-08T00:00:00+00:00",
                ),
            ]
        )
        self.assertEqual(self._section("wedged"), ["q-w-new", "q-w-old"])
        self.assertEqual(self._section("quarantined"), ["q-q-new", "q-q-old"])

    # -- the actual bug: an undeclared status must not vanish ---------------

    def test_unknown_status_lands_in_fallback(self):
        """A made-up status still renders — in `other`, not nowhere.

        This is the regression guard for the whole class. Without it the fix
        covers two instances of the bug and leaves the next status to
        disappear exactly the way these did.
        """
        self._write([_item("q-future", "hibernating")])

        payload = self._payload()
        self.assertEqual([it["id"] for it in payload["other"]], ["q-future"])
        self.assertEqual(payload["totals"]["other"], 1)
        self.assertEqual(payload["unknown_statuses"], ["hibernating"])
        # It must NOT have been quietly filed under a known section.
        for key in (
            "running",
            "wedged",
            "quarantined",
            "pending",
            "blocked",
            "done_recent",
            "abandoned_recent",
        ):
            self.assertEqual(
                [it["id"] for it in payload[key]],
                [],
                f"unknown-status item leaked into {key}",
            )

        html = self._html()
        self.assertIn('id="section-other"', html)
        self.assertIn("q-future", html)
        # The raw status is shown verbatim — the operator needs the name to
        # look it up, and this UI has nothing else to say about it.
        self.assertIn("hibernating", html)

    def test_no_item_is_ever_dropped(self):
        """Every row in queue.json lands in exactly one rendered bucket."""
        items = [
            _item("q-run", "running", registered_at="2026-08-05T00:00:00+00:00"),
            _item("q-pen", "pending"),
            _item("q-blk", "blocked", blocked_at="2026-08-05T00:00:00+00:00"),
            _item("q-wed", "wedged", wedged_at="2026-08-05T00:00:00+00:00"),
            _item("q-qua", "quarantined", quarantined_at="2026-08-05T00:00:00+00:00"),
            _item("q-don", "done", completed_at="2026-08-05T00:00:00+00:00"),
            _item("q-aba", "abandoned", abandoned_at="2026-08-05T00:00:00+00:00"),
            _item("q-new1", "some-future-state"),
            _item("q-new2", "another-one"),
        ]
        self._write(items)
        payload = self._payload()

        seen: list[str] = []
        for key in (
            "running",
            "wedged",
            "quarantined",
            "pending",
            "blocked",
            "other",
            "done_recent",
            "abandoned_recent",
        ):
            seen.extend(it["id"] for it in payload[key])

        self.assertEqual(sorted(seen), sorted(it["id"] for it in items))
        self.assertEqual(len(seen), len(set(seen)), "an item was double-bucketed")
        self.assertEqual(
            payload["unknown_statuses"], ["another-one", "some-future-state"]
        )

        html = self._html()
        for it in items:
            self.assertIn(it["id"], html, f"{it['id']} missing from the page")

    def test_missing_or_null_status_is_not_dropped(self):
        """A corrupt row (no status / null status) renders too.

        `status` missing or null used to compare unequal to every branch and
        disappear — the most likely shape of a genuinely broken row, and the
        one an operator most needs to see.
        """
        self._write(
            [
                {
                    "id": "q-nostatus",
                    "summary": "row with no status",
                    "created_at": "2026-08-01T00:00:00+00:00",
                },
                _item("q-nullstatus", None),
            ]
        )
        payload = self._payload()
        self.assertEqual(
            sorted(it["id"] for it in payload["other"]),
            ["q-nostatus", "q-nullstatus"],
        )
        self.assertEqual(payload["unknown_statuses"], ["unknown"])
        html = self._html()
        self.assertIn("q-nostatus", html)
        self.assertIn("q-nullstatus", html)

    def test_fallback_keys_present_when_empty(self):
        """`other` / `unknown_statuses` always exist, so callers can tell
        "nothing unrecognised" from "key missing"."""
        self._write([_item("q-pen", "pending")])
        payload = self._payload()
        self.assertEqual(payload["other"], [])
        self.assertEqual(payload["unknown_statuses"], [])
        self.assertEqual(payload["totals"]["other"], 0)
        # An empty fallback section renders no wrapper at all (same
        # omit-when-empty convention as BLOCKED), so it adds no noise.
        self.assertNotIn('id="section-other"', self._html())

    def test_unknown_status_is_logged_once(self):
        """The row is visible AND the drift is reported to whoever maintains
        this file — but not once per 5s refresh tick."""
        self._write([_item("q-future", "hibernating")])
        with self.assertLogs(self.appmod.app.logger, level="WARNING") as cm:
            self._payload()
        self.assertTrue(
            any("hibernating" in line for line in cm.output), cm.output
        )
        # Second render: same status, no new warning (the memo suppresses it).
        self.appmod._cache.fetched_at = 0.0
        self.appmod.app.logger.warning("sentinel")
        with self.assertLogs(self.appmod.app.logger, level="WARNING") as cm2:
            self.appmod.app.logger.warning("sentinel")
            self._payload()
        self.assertFalse(
            any("hibernating" in line for line in cm2.output), cm2.output
        )

    # -- existing sections unregressed -------------------------------------

    def test_known_sections_unchanged(self):
        """Scope guard: the sections operators already read still bucket and
        sort exactly as before."""
        self._write(
            [
                _item("q-run-new", "running", registered_at="2026-08-05T20:00:00+00:00"),
                _item("q-run-old", "running", registered_at="2026-08-01T00:00:00+00:00"),
                _item("q-pen-lo", "pending", priority=9),
                _item("q-pen-hi", "pending", priority=1),
                _item(
                    "q-blk-old",
                    "blocked",
                    created_at="2026-07-01T00:00:00+00:00",
                    blocked_at="2026-07-01T00:00:00+00:00",
                ),
                _item(
                    "q-blk-new",
                    "blocked",
                    created_at="2026-08-01T00:00:00+00:00",
                    blocked_at="2026-08-01T00:00:00+00:00",
                ),
                _item("q-done-old", "done", completed_at="2026-08-01T00:00:00+00:00"),
                _item("q-done-new", "done", completed_at="2026-08-05T00:00:00+00:00"),
                _item("q-aba-old", "abandoned", abandoned_at="2026-08-01T00:00:00+00:00"),
                _item("q-aba-new", "abandoned", abandoned_at="2026-08-05T00:00:00+00:00"),
            ]
        )
        self.assertEqual(self._section("running"), ["q-run-old", "q-run-new"])
        self.assertEqual(self._section("pending"), ["q-pen-hi", "q-pen-lo"])
        self.assertEqual(self._section("blocked"), ["q-blk-new", "q-blk-old"])
        self.assertEqual(self._section("done_recent"), ["q-done-new", "q-done-old"])
        self.assertEqual(
            self._section("abandoned_recent"), ["q-aba-new", "q-aba-old"]
        )
        totals = self._payload()["totals"]
        self.assertEqual(totals["running"], 2)
        self.assertEqual(totals["pending"], 2)
        self.assertEqual(totals["blocked"], 2)
        self.assertEqual(totals["done"], 2)
        self.assertEqual(totals["abandoned"], 2)
        self.assertEqual(totals["other"], 0)


class RendererParityTest(unittest.TestCase):
    """The 5s SPA refresh rebuilds #queue-root from scratch in refresh.js, so
    a section that exists only in the Jinja template is discarded by the first
    morphdom merge — the exact failure that once made BLOCKED flash and vanish
    (q-2026-05-20-db66). These are cheap structural guards, not a substitute
    for the jsdom suite: they only assert that both renderers KNOW about every
    declared section key.
    """

    def setUp(self):
        sys.path.insert(0, str(HERE))
        import app as appmod  # noqa: E402

        self.appmod = appmod
        self.template = (HERE / "templates" / "index.html").read_text()
        self.refresh = (HERE / "static" / "refresh.js").read_text()

    def test_every_section_key_is_rendered_by_both_paths(self):
        for key in self.appmod.SECTION_KEYS:
            self.assertIn(
                f'id="section-{key}"',
                self.template,
                f"templates/index.html has no section for '{key}'",
            )
            self.assertIn(
                f"sectionHead('{key}'",
                self.refresh,
                f"static/refresh.js has no section renderer for '{key}'",
            )

    def test_refresh_js_declares_fold_default_for_every_section(self):
        block = re.search(
            r"const SECTION_DEFAULT_OPEN = \{(.*?)\};", self.refresh, re.S
        )
        self.assertIsNotNone(block, "SECTION_DEFAULT_OPEN not found")
        declared = set(re.findall(r"^\s*(\w+):", block.group(1), re.M))
        self.assertEqual(
            set(self.appmod.SECTION_KEYS) - declared,
            set(),
            "SECTION_DEFAULT_OPEN is missing a declared section key",
        )

    def test_copy_cmd_script_is_loaded(self):
        """The exit-command copy handler must actually be on the page."""
        self.assertIn("copy-cmd.js", self.template)
        self.assertTrue((HERE / "static" / "copy-cmd.js").is_file())


if __name__ == "__main__":
    unittest.main(verbosity=2)
