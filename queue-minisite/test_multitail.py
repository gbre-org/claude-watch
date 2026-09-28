#!/usr/bin/env python3
"""Tests for the queue-minisite MULTITAIL mode.

Multitail is a whole-window mode that stacks one live tail per RUNNING queue
item **that actually has a log**, toggled by a header button and by the ``m``
key, with per-pane manual dismissal.

The panes themselves are built client-side (``static/multitail.js``). What the
server owns — and what this suite pins — is the single question the mode turns
on:

  **WHICH rows have a log to tail right now?**

``app.py`` answers it once, as ``live_log_mode`` on every shaped row:

  ``"hostjob"``   tail the hostjob's log file
  ``"workload"``  tail the workload's ``.output`` file
  ``"live"``      tail the owning agent's transcript JSONL
  ``""``          nothing to tail

That field is deliberately NARROWER than the ``data-log-mode`` every running
card carries. Every running card is clickable (a ``starting`` item opens the
single-item modal in a polling state and waits for its agent's first write),
but an item with nothing to read must NOT get a pane: an empty pane costs a
slice of the viewport and one of the browser's scarce concurrent connections
to say nothing.

Also pinned here:

  * the attribute only appears on ELIGIBLE rows in the rendered HTML;
  * ``static/refresh.js`` emits the same attribute — that subtree is rebuilt
    every 5s tick, so an attribute present only in the Jinja template would
    vanish after one tick and the mode would quietly lose its panes;
  * the overlay frame, the toggle button (in BOTH renderers) and the script
    tag are all actually served.

Run::

    python3 queue-minisite/test_multitail.py
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


def _item(
    item_id: str,
    status: str,
    *,
    scope: list[str] | None = None,
    registered_at: str = "2026-06-01T00:00:00+00:00",
) -> dict:
    return {
        "id": item_id,
        "summary": f"summary {item_id}",
        "description": "",
        "scope": scope or [],
        "status": status,
        "priority": 5,
        "created_by": "main-loop",
        "created_at": "2026-06-01T00:00:00+00:00",
        "registered_at": registered_at,
        "completed_at": "2026-06-01T00:05:00+00:00",
        "abandoned_at": "2026-06-01T00:05:00+00:00",
    }


def _seed_agent_state(state_path: Path, mapping: dict[str, str]) -> None:
    """active-agents.json mapping queue_id -> agent_id (all alive)."""
    state_path.parent.mkdir(parents=True, exist_ok=True)
    state = {
        "subagents": [],
        "workloads": [],
        "agents": [
            {
                "agent_id": agent_id,
                "queue_id": queue_id,
                "alive": True,
                "jsonl_age_seconds": 1,
            }
            for queue_id, agent_id in mapping.items()
        ],
    }
    state_path.write_text(json.dumps(state))


class MultitailTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.tmp = tempfile.mkdtemp(prefix="qmin-multitail-")
        cls.queue_actual = Path(cls.tmp) / ".config/session/queue.json"
        cls.agent_state = Path(cls.tmp) / "active-agents.json"
        os.environ["QUEUE_JSON"] = str(cls.queue_actual)
        os.environ["AGENT_STATE_JSON"] = str(cls.agent_state)
        os.environ["AGENTS_JSONL_ROOT"] = str(Path(cls.tmp) / "no-jsonl")
        os.environ["QUEUE_LOG_ARCHIVE_DIR"] = str(Path(cls.tmp) / "no-archive")
        os.environ["WORKLOAD_LOG_DIR"] = str(Path(cls.tmp) / "no-workloads")
        os.environ["HOSTJOB_LOG_DIR"] = str(Path(cls.tmp) / "no-hostjobs")
        # Agent-stats snapshot off — irrelevant here and keeps the payload small.
        os.environ["QUEUE_MINISITE_AGENT_STATS_FILE"] = ""

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

    # -- fixtures ---------------------------------------------------------

    def _seed_mixed(self) -> None:
        """One row of every interesting shape.

        ``q-agent``    running, owned by a live agent          -> "live"
        ``q-work``     running, scope workload:<label>          -> "workload"
        ``q-host``     running, scope hostjob:<label>           -> "hostjob"
        ``q-starting`` running, NO agent record, just registered-> ""
        ``q-pending``  pending                                  -> ""
        ``q-done``     done                                     -> ""
        """
        from datetime import datetime, timezone

        now_iso = datetime.now(timezone.utc).isoformat()
        _write_queue(
            self.queue_actual,
            [
                _item("q-agent", "running"),
                _item("q-work", "running", scope=["workload:my-label"]),
                _item("q-host", "running", scope=["hostjob:my-job"]),
                # Registered just now and absent from active-agents.json: this
                # is the STARTING window — clickable card, but no transcript
                # exists yet, so nothing to tail.
                _item("q-starting", "running", registered_at=now_iso),
                _item("q-pending", "pending"),
                _item("q-done", "done"),
            ],
        )
        _seed_agent_state(self.agent_state, {"q-agent": "aaaa111122223333"})
        self.appmod._cache.fetched_at = 0.0

    def _api(self) -> dict:
        return json.loads(self.client.get("/api/queue").data)

    def _html(self) -> str:
        return self.client.get("/").data.decode("utf-8", errors="replace")

    def _modes(self) -> dict[str, str]:
        state = self._api()
        out: dict[str, str] = {}
        for key in (
            "running",
            "pending",
            "blocked",
            "wedged",
            "quarantined",
            "done_recent",
            "abandoned_recent",
            "other",
        ):
            for row in state.get(key) or []:
                out[row["id"]] = row.get("live_log_mode", "<missing>")
        return out

    # -- eligibility ------------------------------------------------------

    def test_live_log_mode_per_row_shape(self):
        """Each row reports exactly the tail its stream endpoint would serve."""
        self._seed_mixed()
        modes = self._modes()
        self.assertEqual(modes.get("q-agent"), "live")
        self.assertEqual(modes.get("q-work"), "workload")
        self.assertEqual(modes.get("q-host"), "hostjob")

    def test_starting_item_has_no_log(self):
        """A registered item whose agent has not written yet is NOT eligible.

        This is the "no empty panes" requirement. The card is still clickable
        (``data-log-mode="live"``, modal polls for the first event) — but there
        is nothing to tail, so it gets no pane.
        """
        self._seed_mixed()
        modes = self._modes()
        self.assertEqual(modes.get("q-starting"), "")
        # Sanity: it really is in the starting window, i.e. we tested the
        # interesting case and not just "some unowned running row".
        running = {r["id"]: r for r in self._api()["running"]}
        self.assertTrue(running["q-starting"]["is_starting"])

    def test_non_running_rows_are_never_eligible(self):
        """Pending / done rows have no live tail, whatever else they carry."""
        self._seed_mixed()
        modes = self._modes()
        self.assertEqual(modes.get("q-pending"), "")
        self.assertEqual(modes.get("q-done"), "")

    def test_field_is_always_present(self):
        """Every row carries the key, so the front-end never sees undefined."""
        self._seed_mixed()
        for qid, mode in self._modes().items():
            self.assertNotEqual(mode, "<missing>", f"{qid} has no live_log_mode")

    def test_hostjob_wins_over_workload(self):
        """Precedence matches the stream endpoint's own dispatch order.

        ``/api/queue/<qid>/stream`` checks hostjob BEFORE workload, so a row
        carrying both scopes must not advertise a tail the server would not
        actually serve.
        """
        _write_queue(
            self.queue_actual,
            [_item("q-both", "running", scope=["workload:w", "hostjob:h"])],
        )
        self.appmod._cache.fetched_at = 0.0
        self.assertEqual(self._modes().get("q-both"), "hostjob")

    def test_orphaned_agent_item_is_still_eligible(self):
        """An owner record means a transcript exists — even if it died.

        What a dead agent wrote is still this item's log, and usually exactly
        what the operator opened multitail to read.
        """
        _write_queue(self.queue_actual, [_item("q-orphan", "running")])
        self.agent_state.write_text(
            json.dumps(
                {
                    "subagents": [],
                    "workloads": [],
                    "agents": [
                        {
                            "agent_id": "dead111122223333",
                            "queue_id": "q-orphan",
                            "alive": False,
                            "jsonl_age_seconds": 9000,
                        }
                    ],
                }
            )
        )
        self.appmod._cache.fetched_at = 0.0
        row = {r["id"]: r for r in self._api()["running"]}["q-orphan"]
        self.assertIs(row["owner"]["alive"], False)
        self.assertEqual(row["live_log_mode"], "live")

    # -- rendered HTML ----------------------------------------------------

    def test_eligible_rows_carry_the_attribute(self):
        """data-live-log-mode lands on the eligible rows and nowhere else."""
        self._seed_mixed()
        html = self._html()
        found = dict(
            re.findall(
                r'data-queue-id="(q-[^"]+)"(?:(?!</article>).)*?'
                r'data-live-log-mode="([a-z]+)"',
                html,
                re.S,
            )
        )
        self.assertEqual(found.get("q-agent"), "live")
        self.assertEqual(found.get("q-work"), "workload")
        self.assertEqual(found.get("q-host"), "hostjob")
        self.assertNotIn("q-starting", found)
        self.assertNotIn("q-pending", found)

    def test_ineligible_row_still_clickable(self):
        """Eligibility must not have narrowed the existing click affordance."""
        self._seed_mixed()
        html = self._html()
        card = re.search(
            r'<article[^>]*data-queue-id="q-starting".*?</article>', html, re.S
        )
        self.assertIsNotNone(card, "starting card not rendered")
        self.assertIn("log-clickable", card.group(0))
        self.assertIn('data-log-mode="live"', card.group(0))
        self.assertNotIn("data-live-log-mode", card.group(0))

    def test_overlay_frame_and_toggle_rendered(self):
        """The mode's chrome is actually served on first paint."""
        self._seed_mixed()
        html = self._html()
        self.assertIn('id="multitail"', html)
        self.assertIn('id="multitail-panes"', html)
        self.assertIn('id="multitail-empty"', html)
        self.assertIn('id="multitail-exit"', html)
        self.assertIn('id="multitail-toggle"', html)
        # Toggle starts unpressed; the button is the mode's aria owner.
        self.assertIn('aria-controls="multitail"', html)
        # The overlay must start hidden — it takes the whole window.
        overlay = re.search(r"<section\s+id=\"multitail\".*?>", html, re.S)
        self.assertIsNotNone(overlay)
        self.assertIn("hidden", overlay.group(0))
        # data-no-morph keeps the 5s merge (and its EventSource-destroying
        # re-render) out of the pane subtree.
        self.assertIn("data-no-morph", overlay.group(0))

    def test_module_is_loaded_and_served(self):
        self._seed_mixed()
        self.assertIn("multitail.js", self._html())
        resp = self.client.get("/static/multitail.js")
        self.assertEqual(resp.status_code, 200)

    # -- renderer parity --------------------------------------------------
    #
    # #topbar-meta and #queue-root are both rebuilt from static/refresh.js on
    # every 5s tick. Anything emitted only by the Jinja template survives the
    # first paint and then disappears — the failure mode this repo has hit
    # more than once (a dropped section, a dropped source filter). These two
    # greps are the cheap standing guard.

    def test_refresh_js_mirrors_the_row_attribute(self):
        src = (HERE / "static" / "refresh.js").read_text()
        self.assertIn("live_log_mode", src)
        self.assertIn("data-live-log-mode", src)
        self.assertIn("liveLogAttr", src)

    def test_refresh_js_mirrors_the_toggle_button(self):
        src = (HERE / "static" / "refresh.js").read_text()
        self.assertIn('id="multitail-toggle"', src)
        self.assertIn("multitail-control", src)

    def test_multitail_js_caps_concurrent_streams(self):
        """The connection cap is load-bearing, not decoration.

        A browser allows only ~6 concurrent HTTP/1.1 connections per origin.
        Uncapped, enough panes starve the site's own 5s /api/queue poll and the
        page looks frozen. Pin that a cap exists and leaves headroom.
        """
        src = (HERE / "static" / "multitail.js").read_text()
        m = re.search(r"MAX_LIVE_STREAMS\s*=\s*(\d+)", src)
        self.assertIsNotNone(m, "MAX_LIVE_STREAMS not declared")
        self.assertLessEqual(int(m.group(1)), 4)
        # Panes are selected by the server-derived attribute, never by
        # re-deriving eligibility in the front-end.
        self.assertIn("data-live-log-mode", src)


if __name__ == "__main__":
    unittest.main(verbosity=2)
