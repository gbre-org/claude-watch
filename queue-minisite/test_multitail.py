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
  * the overlay frame, the toggle button (in BOTH renderers), the ``wrap`` /
    ``time`` / ``clear`` pills and the script tag are all actually served;
  * **the ended-pane retention contract across three files.** Finished panes
    are cleared after a configurable delay (``1m`` default, ``keep`` = the old
    keep-forever behaviour). The behaviour is client-side and driven under
    jsdom in ``static/multitail.test.js``; what is pinned here is what has to
    AGREE about it — the Jinja-rendered pill's default, the module's option
    table, the persistence key, and the stylesheet.

And the two CROSS-SIDE contracts the client cannot pin by itself:

  * **what the stream says when a log does not exist YET.** Eligibility and
    log-existence are different instants: a ``workload:`` row is eligible as
    soon as its scope is on the queue record, which is before the runner has
    created ``<label>.output``. The server answers with a ONE-SHOT in-stream
    error and closes, and ``static/multitail.js`` retries exactly the error
    kinds listed here. Rename one server-side and the front-end silently stops
    retrying — a pane that stays blank until the whole mode is toggled, which
    is the bug this pins shut.
  * **which sources carry a per-line timestamp.** The ``t`` toggle shows the
    log's OWN time and deliberately shows nothing for sources that have none,
    rather than substituting the browser's arrival time. That is only honest
    while the frame shapes below hold: agent JSONL records reach the client
    with their ``timestamp`` intact, and plain-text ``workload_line`` frames
    carry no time field at all.

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

    # -- display toggles (wrap / timestamps) ------------------------------

    def test_display_toggles_rendered(self):
        """The `wrap` + `time` pills and their key hints are actually served.

        They live INSIDE the ``data-no-morph`` overlay, so unlike the topbar
        multitail pill they are rendered once and survive the 5s tick — which
        is why there is no refresh.js parity assertion for them.
        """
        self._seed_mixed()
        html = self._html()
        self.assertIn('id="multitail-wrap"', html)
        self.assertIn('id="multitail-ts"', html)
        # Both start OFF: off is the behaviour that existed before they did.
        for btn_id in ("multitail-wrap", "multitail-ts"):
            m = re.search(r'<button[^>]*id="%s".*?>' % btn_id, html, re.S)
            self.assertIsNotNone(m, f"{btn_id} not rendered")
            self.assertIn('aria-pressed="false"', m.group(0))
        # The keys are discoverable without reading the source.
        self.assertIn("<kbd>w</kbd>", html)
        self.assertIn("<kbd>t</kbd>", html)
        # The pills sit inside the no-morph overlay, not the topbar.
        overlay = re.search(
            r'<section\s+id="multitail".*?</section>', html, re.S
        )
        self.assertIsNotNone(overlay)
        self.assertIn('id="multitail-wrap"', overlay.group(0))
        self.assertIn('id="multitail-ts"', overlay.group(0))

    def test_wrap_and_timestamp_styles_shipped(self):
        """The toggles are class-driven; without the CSS they do nothing.

        ``.mt-wrap`` is what actually switches ``white-space`` — and
        ``overflow-wrap: anywhere`` is load-bearing, because the long lines in
        these panes are paths / JSON / base64, i.e. single tokens that
        ``break-word`` alone leaves overflowing.
        """
        css = (HERE / "static" / "style.css").read_text()
        self.assertIn(".multitail.mt-wrap .mt-line", css)
        self.assertIn("pre-wrap", css)
        self.assertIn("overflow-wrap: anywhere", css)
        self.assertIn(".mt-line .mt-ts", css)
        self.assertIn(".mt-pane-nots", css)
        self.assertIn(".multitail-display", css)
        # The retention pill is a VALUE, so it never takes the aria-pressed
        # tint; it has its own rule (stable width + the `keep` outline).
        self.assertIn(".multitail-retain", css)
        self.assertIn('.multitail-retain[data-retention="keep"]', css)

    # -- ended-pane retention ---------------------------------------------
    #
    # Ended panes are cleared after a configurable delay, `keep` (forever)
    # being one of the choices. The behaviour itself is client-side and pinned
    # under jsdom in static/multitail.test.js; what belongs HERE — the suite
    # CI actually runs — is the contract between the three files that have to
    # agree about it: the Jinja-rendered pill, the module's option table, and
    # the stylesheet.

    def _retention_options(self) -> list[tuple[str, str]]:
        """[(key, ms-expression)] parsed out of the module's option table."""
        src = (HERE / "static" / "multitail.js").read_text()
        m = re.search(r"const RETENTION_OPTIONS = \[(.*?)\n  \];", src, re.S)
        self.assertIsNotNone(m, "RETENTION_OPTIONS not declared")
        return re.findall(r"\{\s*key:\s*'([^']+)',\s*ms:\s*([^,]+),", m.group(1))

    def test_retention_control_rendered(self):
        """The `clear` pill and its key hint are actually served.

        It lives inside the ``data-no-morph`` overlay next to `wrap` / `time`,
        so like them it is rendered once and survives the 5s tick — a control
        placed outside that subtree would vanish on the next merge.
        """
        self._seed_mixed()
        html = self._html()
        overlay = re.search(r'<section\s+id="multitail".*?</section>', html, re.S)
        self.assertIsNotNone(overlay)
        self.assertIn('id="multitail-retain"', overlay.group(0))
        btn = re.search(
            r'<button[^>]*id="multitail-retain".*?</button>', html, re.S
        )
        self.assertIsNotNone(btn, "retention pill not rendered")
        # A value, not a toggle: aria-pressed would claim a binary state that
        # does not exist. The accessible name spells the value out instead.
        self.assertNotIn("aria-pressed", btn.group(0))
        self.assertIn("aria-label=", btn.group(0))
        # Keyboard-reachable and discoverable without reading the source.
        self.assertIn("<kbd>c</kbd>", html)

    def test_rendered_retention_default_matches_the_module(self):
        """THE CROSS-FILE PIN: the served pill and the module agree.

        The pill's label/attribute are Jinja text; the default that actually
        governs behaviour is ``DEFAULT_RETENTION_KEY`` in multitail.js. Drift
        between them ships a header that lies about what the window will do
        (and the module only rewrites the label when a *stored* choice exists).
        """
        src = (HERE / "static" / "multitail.js").read_text()
        m = re.search(r"DEFAULT_RETENTION_KEY = '([^']+)'", src)
        self.assertIsNotNone(m, "DEFAULT_RETENTION_KEY not declared")
        default_key = m.group(1)
        # What was asked for, stated rather than implied by array position.
        self.assertEqual(default_key, "1m")
        keys = [k for k, _ in self._retention_options()]
        self.assertIn(default_key, keys)

        self._seed_mixed()
        btn = re.search(
            r'<button[^>]*id="multitail-retain".*?</button>',
            self._html(),
            re.S,
        )
        self.assertIsNotNone(btn)
        self.assertIn(f'data-retention="{default_key}"', btn.group(0))
        # ...and the visible label is that option's label, not a stale string.
        label = re.search(r"key: '%s'.*?label: '([^']+)'" % default_key, src, re.S)
        self.assertIsNotNone(label)
        self.assertIn(f">{label.group(1)}</button>", btn.group(0))

    def test_retention_options_are_a_small_set_including_keep_forever(self):
        """Four values, ascending, and one of them never clears.

        `keep` is the behaviour that existed before the delay did, kept as a
        real choice rather than deleted — ``ms: 0`` is what the sweep reads as
        "no deadline". Nothing below a minute is offered: a grace period
        shorter than that stops being a grace period, which is the whole point
        of not yanking a pane the instant it goes final.
        """
        opts = self._retention_options()
        self.assertEqual([k for k, _ in opts], ["1m", "5m", "15m", "keep"], opts)
        as_ms = {}
        for key, expr in opts:
            # Expressions are plain arithmetic on literals (`5 * 60 * 1000`).
            self.assertRegex(expr.strip(), r"^[\d\s*]+$", expr)
            as_ms[key] = eval(expr, {"__builtins__": {}}, {})  # noqa: S307
        self.assertEqual(as_ms["keep"], 0)
        timed = [as_ms[k] for k, _ in opts if k != "keep"]
        self.assertEqual(timed, sorted(timed), as_ms)
        self.assertTrue(all(ms >= 60_000 for ms in timed), as_ms)

    def test_retention_choice_is_persisted_per_viewer(self):
        """Stored in localStorage, guarded, under the site's key prefix.

        The MODE is deliberately not persisted; the retention choice is,
        because it is a policy about how much finished output survives rather
        than a projection of the current page. Storage throws in some privacy
        modes and comes back empty in others, so both accesses are wrapped and
        an unrecognised value resolves to the default.
        """
        src = (HERE / "static" / "multitail.js").read_text()
        self.assertIn("RETENTION_STORAGE_KEY = 'qsite_mt_retain'", src)
        reader = re.search(
            r"function readStoredRetention\(\) \{(.*?)\n  \}", src, re.S
        )
        self.assertIsNotNone(reader, "readStoredRetention not declared")
        self.assertIn("try {", reader.group(1))
        self.assertIn("catch", reader.group(1))
        # An unknown stored value must not become live state.
        self.assertIn("RETENTION_BY_KEY[v]", reader.group(1))
        writer = re.search(r"function storeRetention\(key\) \{(.*?)\n  \}", src, re.S)
        self.assertIsNotNone(writer, "storeRetention not declared")
        self.assertIn("try {", writer.group(1))
        # Same `qsite_` prefix as the density / header-collapse preferences, so
        # one origin's keys stay identifiable.
        self.assertIn("qsite_", src)

    def test_retention_rides_the_existing_reconcile_tick(self):
        """No second timer, and the sweep cannot disturb the stream pump.

        multitail.js has exactly two intervals: the 2s reconcile (which is also
        the stream-retry clock) and the 1s toggle-button re-sync. A per-pane
        setTimeout per clear would be a third clock to leak on close.
        """
        src = (HERE / "static" / "multitail.js").read_text()
        self.assertEqual(src.count("setInterval("), 2, "a new timer appeared")
        self.assertNotIn("setTimeout(sweep", src)
        self.assertIn("sweepEndedPanes(seen)", src)
        # An ended pane already released its slot in markEnded, so clearing one
        # returns nothing to the pump; the sweep must not be re-implementing
        # slot bookkeeping.
        self.assertIn("releaseSlot(pane)", src)

    def test_auto_clear_is_not_a_manual_dismissal(self):
        """A timer must never poison the qid the way the × does.

        ``dismissed`` is permanent for the life of the mode ("I closed that");
        an auto-clear uses the separate ``cleared`` set, which reconcile drops
        the first pass the row is not eligible — so a requeued job reusing the
        qid gets a fresh pane instead of being suppressed by a stale clear.
        """
        src = (HERE / "static" / "multitail.js").read_text()
        # The ONLY writer of `dismissed` is the per-pane close button.
        self.assertEqual(src.count("dismissed.add("), 1, "dismissed gained a writer")
        self.assertIn("cleared.add(pane.qid)", src)
        self.assertIn("cleared.delete(qid)", src)
        # ...and the release is driven by eligibility, not by a countdown.
        self.assertIn("if (!seen.has(qid)) cleared.delete(qid);", src)

    def test_multitail_js_wires_both_keys_and_keeps_chords_free(self):
        """`w` / `t` / `c` are bound, and modified chords are passed through.

        Ctrl+W must keep closing the tab and Ctrl/Cmd+C must keep copying the
        log text a reader just selected. The guard is the module's single early
        return on ctrl/meta/alt — assert it is still there alongside the
        bindings rather than trusting the comment.
        """
        src = (HERE / "static" / "multitail.js").read_text()
        self.assertIn("ev.key === 'w'", src)
        self.assertIn("ev.key === 't'", src)
        self.assertIn("ev.key === 'c'", src)
        self.assertIn("if (ev.ctrlKey || ev.metaKey || ev.altKey) return;", src)
        # No other key the site binds may be reused. This is the map keyboard.js
        # documents (j/k/Enter/Space/g/G//) plus live-log.js's in-modal keys.
        for taken in ("'j'", "'k'", "'g'", "'G'", "'/'"):
            self.assertNotIn(f"ev.key === {taken}", src, f"{taken} is already bound")

    def test_agent_records_reach_the_client_with_their_timestamp(self):
        """Agent JSONL: the record's OWN timestamp survives the parse.

        The `t` toggle renders exactly this field. The server hands the whole
        record through, so the assertion is that nothing strips it.
        """
        payload = self.appmod._parse_jsonl_line(
            json.dumps(
                {
                    "type": "assistant",
                    "timestamp": "2026-09-28T15:28:44.618Z",
                    "message": {"content": [{"type": "text", "text": "hi"}]},
                }
            )
        )
        self.assertEqual(payload["kind"], "assistant_text")
        self.assertEqual(payload["rec"]["timestamp"], "2026-09-28T15:28:44.618Z")

    def test_plain_text_frames_carry_no_timestamp(self):
        """Workload / hostjob tails have NO time field — hence `no ts`.

        The front-end shows nothing for these panes and says why. That is only
        honest while this holds: if a time field is ever added here, the
        client's "this source has no timestamps" claim goes stale and this
        test is the place that notices.
        """
        wl_dir = Path(self.tmp) / "ts-workloads"
        wl_dir.mkdir(parents=True, exist_ok=True)
        (wl_dir / "tsjob.output").write_text("one\ntwo\n")
        (wl_dir / "tsjob.exit").write_text("0\n")
        prev = self.appmod.WORKLOAD_LOG_DIR
        self.appmod.WORKLOAD_LOG_DIR = str(wl_dir)
        try:
            frames = [
                json.loads(line[len("data: ") :])
                for chunk in self.appmod._tail_workload_output("tsjob")
                for line in chunk.decode().splitlines()
                if line.startswith("data: ")
            ]
        finally:
            self.appmod.WORKLOAD_LOG_DIR = prev
        lines = [f for f in frames if f.get("kind") == "workload_line"]
        self.assertTrue(lines, "no workload_line frames produced")
        for frame in lines:
            self.assertEqual(
                set(frame) & {"timestamp", "ts", "time", "at"},
                set(),
                f"plain-text frame gained a time field: {frame}",
            )

    # -- "the log does not exist YET" -------------------------------------

    def _stream_frames(self, qid: str) -> list[dict]:
        """Read one stream's frames. Only safe for the terminating paths."""
        resp = self.client.get(f"/api/queue/{qid}/stream")
        self.assertEqual(resp.status_code, 200)
        out = []
        for line in resp.data.decode("utf-8", errors="replace").splitlines():
            if line.startswith("data: "):
                out.append(json.loads(line[len("data: ") :]))
        return out

    def test_missing_workload_log_is_a_retryable_error_frame(self):
        """A workload row eligible before its .output exists.

        This is the reported bug's origin: the row is eligible from the moment
        its scope lands, the runner has not created the file yet, and the
        stream says so with a ONE-SHOT error and closes. Note the ORDERING —
        ``stream-start`` is emitted BEFORE the open is attempted, so the
        failure arrives after a stream-start. Any client-side rule keyed on
        "have I seen a stream-start before" would therefore mis-classify the
        successful retry's first backfill as a replay.
        """
        _write_queue(
            self.queue_actual,
            [_item("q-nolog", "running", scope=["workload:not-created-yet"])],
        )
        self.appmod._cache.fetched_at = 0.0
        frames = self._stream_frames("q-nolog")
        kinds = [f.get("kind") for f in frames]
        self.assertIn("open-failed", kinds, frames)
        self.assertEqual(
            frames[0].get("kind"), "stream-start", "stream-start must come first"
        )
        err = next(f for f in frames if f.get("type") == "error")
        self.assertEqual(err["kind"], "open-failed")

    def test_missing_agent_transcript_is_a_retryable_error_frame(self):
        """An agent row eligible before its transcript's first write."""
        _write_queue(self.queue_actual, [_item("q-nojsonl", "running")])
        _seed_agent_state(self.agent_state, {"q-nojsonl": "bbbb111122223333"})
        self.appmod._cache.fetched_at = 0.0
        frames = self._stream_frames("q-nojsonl")
        self.assertEqual([f.get("kind") for f in frames], ["no-jsonl"], frames)

    def test_unowned_running_item_is_a_retryable_error_frame(self):
        """No active-agents row yet for a running item -> `no-agent`."""
        _write_queue(self.queue_actual, [_item("q-unowned", "running")])
        self.agent_state.write_text(
            json.dumps({"subagents": [], "workloads": [], "agents": []})
        )
        self.appmod._cache.fetched_at = 0.0
        frames = self._stream_frames("q-unowned")
        self.assertEqual([f.get("kind") for f in frames], ["no-agent"], frames)

    def test_client_retries_exactly_the_not_there_yet_error_kinds(self):
        """THE CROSS-SIDE PIN.

        The three tests above are the server's side: these are the kinds it
        emits when a log is merely not there yet. This one asserts the client's
        retryable set is exactly those (plus ``read-failed``, the transient
        read error on an existing log) — so renaming one server-side fails here
        instead of silently producing a pane that never streams.

        Also pinned: the client does NOT mark these terminal. That single line
        is what the bug was.
        """
        src = (HERE / "static" / "multitail.js").read_text()
        m = re.search(
            r"const RETRYABLE_ERROR_KINDS = \{(.*?)\};", src, re.S
        )
        self.assertIsNotNone(m, "RETRYABLE_ERROR_KINDS not declared")
        declared = set(re.findall(r"'([a-z-]+)':\s*true", m.group(1)))
        self.assertEqual(
            declared,
            {"no-agent", "no-jsonl", "open-failed", "read-failed"},
            f"retryable set drifted from the server's error kinds: {declared}",
        )
        # Every kind the server emits for a not-yet-existing log must be in it.
        app_src = (HERE / "app.py").read_text()
        for kind in ("no-agent", "no-jsonl", "open-failed", "read-failed"):
            self.assertIn(f'"kind": "{kind}"', app_src, f"{kind} no longer emitted")
        # The retry itself: a backoff with a ceiling, expired by the existing
        # reconcile tick rather than by a second timer.
        self.assertIn("STREAM_RETRY_BASE_MS", src)
        self.assertIn("STREAM_RETRY_MAX_MS", src)
        self.assertIn("scheduleStreamRetry", src)
        # Backfill suppression must key on data having been SHOWN, not on
        # having seen a stream-start — the workload/hostjob tails emit
        # stream-start before they open the file, so the stream-start rule
        # would swallow a successful retry's first content.
        self.assertIn("pane.sawData", src)
        self.assertNotIn("sawStreamStart", src)

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
