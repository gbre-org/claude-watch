#!/usr/bin/env python3
"""Tests for the FRONT-END BUILD VERSION signal (botchat #4947).

This dashboard is built to be left open. It refreshes itself by polling
``/api/queue`` every 5s and morphing the result into the live DOM, so a tab can
sit on one screen for hours — and it never reloads, which means **a deploy does
not reach it**. The HTML, the JS and the CSS an open tab runs are the ones it
fetched when it was opened.

That bit the multitail auto-close feature: it was merged, deployed, and its
served bytes verified on the server, and it was still reported as not working
half an hour later. It was not broken — it simply was not in the browser doing
the looking, whose tab predated the deploy and had made 447 API polls and zero
page loads since.

So every render stamps the build that produced it on ``<body
data-asset-version>``, the same value rides every ``/api/queue`` payload, and
``static/refresh.js`` compares them on each tick and unhides a banner when they
differ. The page tells the viewer it is stale instead of silently misbehaving,
and it does not reload by itself — that would discard open panes, scroll
position and any dialog in flight.

Pinned here (the server half, plus the cross-file contract):

  * ``_asset_version()`` is non-empty, stable while the files are, and CHANGES
    when a served asset does — a version that never moved would make the whole
    signal a permanent "all good";
  * the rendered page carries it on ``<body>`` and ``/api/queue`` reports the
    SAME value, because a signal whose two halves disagree cries wolf every
    tick;
  * the banner ships hidden, with a reload control;
  * the banner is NOT a ``data-no-morph`` element — multitail.js treats any
    visible one as a dialog that owns the keyboard, so marking it would
    silently kill the ``w`` / ``t`` / ``c`` keys whenever it appeared;
  * refresh.js actually reads both sides.

Run::

    python3 queue-minisite/test_build_version.py
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


def _write_queue(path: Path, items: list[dict]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with open(path, "w") as f:
        json.dump({"schema_version": 3, "items": items, "locked_scopes": {}}, f)


def _item(item_id: str, status: str) -> dict:
    return {
        "id": item_id,
        "summary": f"summary {item_id}",
        "description": "",
        "scope": [],
        "status": status,
        "priority": 5,
        "created_by": "main-loop",
        "created_at": "2026-09-29T00:00:00+00:00",
        "registered_at": "2026-09-29T00:00:00+00:00",
    }


class BuildVersionTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.tmp = tempfile.mkdtemp(prefix="qmin-build-version-")
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
        _write_queue(self.queue_actual, [_item("q-1", "running")])
        self.appmod._cache.fetched_at = 0.0

    def _html(self) -> str:
        return self.client.get("/").data.decode("utf-8", errors="replace")

    def test_version_is_non_empty_and_stable(self):
        """Same files in, same version out — twice in a row."""
        first = self.appmod._asset_version()
        self.assertTrue(first, "asset version must not be empty")
        self.assertEqual(first, self.appmod._asset_version())

    def test_version_changes_when_a_served_asset_changes(self):
        """The whole signal rests on this: touch a script, get a new version.

        Without it the comparison would be a constant and every stale tab would
        look current — the exact failure the banner exists to prevent.
        """
        target = HERE / "static" / "multitail.js"
        before = self.appmod._asset_version()
        st = target.stat()
        try:
            # ns precision throughout: the version hashes st_mtime_ns, and a
            # float round-trip would not restore the original value.
            os.utime(target, ns=(st.st_atime_ns, st.st_mtime_ns + 5_000_000_000))
            self.assertNotEqual(
                before,
                self.appmod._asset_version(),
                "a changed asset must produce a different build version",
            )
        finally:
            os.utime(target, ns=(st.st_atime_ns, st.st_mtime_ns))
        self.assertEqual(
            before,
            self.appmod._asset_version(),
            "restoring the mtime must restore the version",
        )

    def test_page_and_api_report_the_same_version(self):
        """The page stamps its build; the API reports the live one."""
        html = self._html()
        m = re.search(r'<body[^>]*data-asset-version="([^"]+)"', html)
        self.assertIsNotNone(m, "<body> must carry data-asset-version")
        api = json.loads(self.client.get("/api/queue").data)
        self.assertIn("asset_version", api)
        self.assertEqual(
            m.group(1),
            api["asset_version"],
            "page and API versions must agree or the banner cries wolf",
        )
        self.assertTrue(api["asset_version"])

    def test_banner_ships_hidden_with_a_reload_control(self):
        html = self._html()
        self.assertIn('id="stale-build"', html)
        self.assertIn('id="stale-build-reload"', html)
        banner = html[html.index('id="stale-build"') - 40:]
        banner = banner[: banner.index("</div>")]
        self.assertIn("hidden", banner, "the banner must start hidden")

    def test_banner_is_not_a_no_morph_dialog(self):
        """`data-no-morph` on the banner would disable multitail's keys.

        multitail.js treats ANY visible `[data-no-morph]` element as a dialog
        that owns the keyboard (so `m` cannot yank the window out from under a
        modal). The banner sits outside both merge roots and needs no exemption,
        so marking it would buy nothing and would silently kill `w` / `t` / `c`
        for as long as it was up.
        """
        html = self._html()
        start = html.index('id="stale-build"')
        end = html.index(">", start)
        self.assertNotIn("data-no-morph", html[start:end])

    def test_refresh_js_compares_both_sides(self):
        """The client half: refresh.js reads the page stamp AND the payload."""
        src = (HERE / "static" / "refresh.js").read_text()
        self.assertIn("data-asset-version", src)
        self.assertIn("asset_version", src)
        self.assertIn("stale-build", src)
        # It must offer a reload, not perform one on its own.
        self.assertIn("stale-build-reload", src)

    def test_stylesheet_puts_the_banner_above_the_multitail_overlay(self):
        """A banner the whole-window overlay covered would be useless.

        multitail is the mode a stale build hides in (its panes are the thing
        that stops behaving), so the banner's z-index has to beat the overlay's.
        """
        css = (HERE / "static" / "style.css").read_text()
        self.assertIn(".stale-build {", css)

        def z_of(selector: str) -> int:
            at = css.index(selector)
            block = css[at: css.index("}", at)]
            m = re.search(r"z-index:\s*(\d+)", block)
            assert m, f"no z-index in {selector} block"
            return int(m.group(1))

        self.assertGreater(z_of(".stale-build {"), z_of(".multitail {"))


if __name__ == "__main__":
    unittest.main(verbosity=2)
