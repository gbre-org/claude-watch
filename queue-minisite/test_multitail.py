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

  * **the per-pane FOOTER BAR contract across three files.** Each pane shows
    the model / tool calls / context / output / age / last tool for its item,
    read off the rendered ROW rather than re-derived — so the strings come from
    ONE formatter (app.py ``_shape_agent_stat``) and a footer can never disagree
    with the row cell it came from. That only works while BOTH row renderers
    emit those values as data attributes: an attribute present in the Jinja
    template but missing from ``static/refresh.js`` disappears after one 5s
    tick, and the footer would go blank a few seconds after the mode opened.

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
    rather than substituting the browser's arrival time. Agent JSONL records
    reach the client with their ``timestamp`` intact. A plain-text line carries
    a time only when its PRODUCER stamped it: workload wrappers pipe their
    payload through ``claude-watch workload stamp``, so each line starts with
    ``date -Iseconds``, and the server splits that prefix off into the frame's
    ``source_ts`` (``_plain_line_event``). A line with no stamp — every log
    written before stamping existed, and every hostjob log — keeps its text
    verbatim and gets no time field at all, which is what the pane's ``no ts``
    marker is about. Both shapes are pinned below: strip the prefix off the
    wrong thing and a log line loses its leading word; fail to strip it and the
    time renders twice.

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


# -- tiny CSS reader ------------------------------------------------------
#
# Some of what this suite pins is geometry that lives only in the stylesheet,
# and `assertIn("float: right", css)` would pass on a rule for something else
# entirely. These two helpers cut the sheet into (selectors, declarations)
# pairs so an assertion can name the RULE it means. Comments go first, because
# this stylesheet carries long ones and they contain braces-free prose that
# would otherwise read as selectors. Nested at-rules are skipped rather than
# parsed: the regex cannot cross a brace, so an `@media` header never matches
# and the rules inside it are returned on their own, which is what a caller
# asking for a plain selector wants anyway.


def _css_rules(css: str) -> list[tuple[list[str], str]]:
    body = re.sub(r"/\*.*?\*/", "", css, flags=re.S)
    out = []
    for m in re.finditer(r"([^{}]+)\{([^{}]*)\}", body):
        sels = [s.strip() for s in m.group(1).split(",") if s.strip()]
        out.append((sels, m.group(2)))
    return out


def _decls(rules: list[tuple[list[str], str]], selector: str) -> str | None:
    """Every declaration written for `selector`, in source order."""
    found = [d for sels, d in rules if selector in sels]
    return "\n".join(found) if found else None


def _multitail_phone_block(css: str) -> str:
    """The 560px block that belongs to MULTITAIL, not the site-wide one.

    The stylesheet carries two `@media (max-width: 560px)` blocks and the
    site-wide one comes first, so a plain `index()` slice starts hundreds of
    lines above the multitail rules and picks up the desktop values — which
    is how a phone assertion passes while reading a desktop number.
    """
    block = css[css.rindex("@media (max-width: 560px)") :]
    assert ".mt-pane-titlebar" in block, "multitail's phone block moved"
    return block


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

    def test_timestamps_cost_the_body_no_horizontal_width(self):
        """The stamp is on the right, and it never indents the body.

        It used to live in a left gutter, i.e. a full-height flex column. On a
        one-row entry that was a slice of width; on a wrapped or verbose entry
        it was much worse, because a per-entry column indents EVERY
        continuation row and a pretty-printed JSON payload then wrapped into
        the right-hand fraction of the pane.

        Two rules carry the fix and each one is asserted here, because either
        alone silently restores the gutter:

          one-row mode   the cell keeps its flex column but takes `order`, so
                         the body (the only flexible item) starts at x=0 and
                         the stamp is pushed to the pane's right edge.
          multi-row      the row stops being a flex container at all and the
                         stamp becomes a right FLOAT, which shortens exactly
                         one line box. Anything that is not a float here -- a
                         narrower column, a flex cell, an absolutely
                         positioned box -- either keeps costing the body width
                         on every row or prints over its first one.
        """
        css = (HERE / "static" / "style.css").read_text()
        rules = _css_rules(css)

        cell = _decls(rules, ".mt-line .mt-ts")
        self.assertIsNotNone(cell, ".mt-line .mt-ts rule not found")
        self.assertRegex(
            cell, r"\border\s*:",
            "the one-row timestamp cell must be re-ordered past the body, "
            "or it is a left gutter again")

        for mode in ("wrap", "verbose"):
            row = _decls(rules, ".multitail.mt-%s .mt-line" % mode)
            self.assertIsNotNone(row, "no .mt-%s row rule" % mode)
            self.assertRegex(
                row, r"display\s*:\s*block",
                "a flex row gives the %s-mode timestamp a full-height column, "
                "which indents every continuation row of the body" % mode)
            stamp = _decls(rules, ".multitail.mt-%s .mt-line .mt-ts" % mode)
            self.assertIsNotNone(stamp, "no .mt-%s timestamp rule" % mode)
            self.assertRegex(
                stamp, r"float\s*:\s*right",
                "the %s-mode timestamp must float right so it shortens the "
                "first line box only" % mode)

    # -- pane title (the task summary) -------------------------------------
    #
    # A pane's header is `badge · queue id · TASK TITLE · status · close`. The
    # title is the only thing on screen that answers "which of these tails is
    # the promote job" — the queue id does not, and in this whole-window mode
    # the cards that carry the title are not visible. It went missing on
    # phones (botchat: "why aren't task titles visible in multitail anymore"),
    # and nothing caught it, because it disappeared through a STYLESHEET rule
    # rather than through the renderer. Hence two guards: the data has to be on
    # the row, and no rule may hide the element the renderer puts it in.

    def test_eligible_row_carries_the_task_title(self):
        """The pane title's SOURCE: `data-queue-summary` on the tailable row.

        multitail.js reads the title straight off the row (``rowInfo``), so an
        eligible row without this attribute is a pane with no title no matter
        what the stylesheet says.
        """
        self._seed_mixed()
        html = self._html()
        rows = [
            m.group(0)
            for m in re.finditer(r"<article\b[^>]*>", html)
            if "data-live-log-mode" in m.group(0)
        ]
        self.assertTrue(rows, "no multitail-eligible row rendered")
        for row in rows:
            m = re.search(r'data-queue-summary="([^"]*)"', row)
            self.assertIsNotNone(m, f"eligible row has no title attribute: {row}")
            self.assertNotEqual(m.group(1).strip(), "", f"empty title on: {row}")

    def test_refresh_js_keeps_the_title_on_the_rebuilt_row(self):
        """The 5s tick rebuilds the row, so it has to re-emit the title too.

        An attribute present only in the Jinja paint survives the first paint
        and then vanishes — the standing failure mode in this file.
        """
        src = (HERE / "static" / "refresh.js").read_text()
        self.assertIn("data-queue-summary", src)

    def test_no_rule_hides_the_pane_title(self):
        """THE REGRESSION GUARD: nothing may `display: none` the pane title.

        The phone breakpoint used to carry ``.mt-pane-summary { display: none;
        }``, which is why a stack of panes on a phone was identified by queue
        id alone. The title now wraps onto its own line at that width instead
        (``order`` + a 100% flex basis, so the status and close button are not
        pushed onto a third line). This asserts the absence, because the
        element is rendered either way and only CSS decides whether a human
        can see it.

        The reordered element is the TITLEBAR, not the title: since the
        metrics moved onto this line they have to wrap with it, or the phone
        gets a title on one row and its own numbers on another.
        """
        css = (HERE / "static" / "style.css").read_text()
        # The declaration itself, in any spacing.
        self.assertIsNone(
            re.search(r"\.mt-pane-summary[^{}]*\{[^}]*display:\s*none", css),
            ".mt-pane-summary is hidden by a rule — the pane title is invisible",
        )
        self.assertIsNone(
            re.search(r"\.mt-pane-titlebar[^{}]*\{[^}]*display:\s*none", css),
            ".mt-pane-titlebar is hidden by a rule — the pane title goes with it",
        )
        # And the wrap treatment that replaced it is actually shipped.
        self.assertIn(".mt-pane-head { flex-wrap: wrap; }", css)
        self.assertIsNotNone(
            re.search(r"\.mt-pane-titlebar\s*\{[^}]*order:\s*1", css),
            "the title line must be ordered last so it wraps alone onto line 2",
        )

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
        # The try/catch lives in the SHARED accessors every persisted setting
        # goes through (the display toggles were added to that same pair rather
        # than growing a second mechanism), so that is where it is asserted.
        for name in ("readStored", "writeStored"):
            fn = re.search(
                r"function " + name + r"\([^)]*\) \{(.*?)\n  \}", src, re.S
            )
            self.assertIsNotNone(fn, f"{name} not declared")
            self.assertIn("try {", fn.group(1), name)
            self.assertIn("catch", fn.group(1), name)
        reader = re.search(
            r"function readStoredRetention\(\) \{(.*?)\n  \}", src, re.S
        )
        self.assertIsNotNone(reader, "readStoredRetention not declared")
        self.assertIn("readStored(", reader.group(1))
        # An unknown stored value must not become live state.
        self.assertIn("RETENTION_BY_KEY[v]", reader.group(1))
        writer = re.search(r"function storeRetention\(key\) \{(.*?)\n  \}", src, re.S)
        self.assertIsNotNone(writer, "storeRetention not declared")
        self.assertIn("writeStored(", writer.group(1))
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

    # -- per-pane metrics (botchat #4935, moved to the title line by #4997) --

    #: What the footer prints, as (data attribute, ``_shape_agent_stat`` key).
    #: The attribute carries the SERVER-FORMATTED string; the footer never
    #: re-derives a number.
    FOOTER_ATTRS = (
        ("data-calls-text", "calls_text"),
        ("data-ctx-text", "ctx_text"),
        ("data-out-text", "out_text"),
        ("data-last-tool", "last_tool"),
        ("data-age-text", "age_text"),
    )

    def test_shaper_provides_every_string_the_footer_prints(self):
        """One formatter. The footer reads these; it computes nothing."""
        shaped = self.appmod._shape_agent_stat(
            {
                "agent_id": "agent-x",
                "queue_id": "q-1",
                "tool_calls": 41,
                "context_tokens": 118000,
                "output_tokens": 9100,
                "last_tool": "Bash",
                "started_at": "2026-09-29T02:00:00+00:00",
                "age_seconds": 12.0,
            }
        )
        for _attr, key in self.FOOTER_ATTRS:
            self.assertIn(key, shaped, f"_shape_agent_stat lost {key}")
            self.assertIsInstance(shaped[key], str)
        self.assertEqual(shaped["calls_text"], "41")
        self.assertEqual(shaped["ctx_text"], "118K")

    def test_both_row_renderers_emit_the_footer_attributes(self):
        """Jinja AND refresh.js — or the footer blanks after one 5s tick.

        The running-card subtree is rebuilt from ``static/refresh.js`` every
        tick, so an attribute that exists only in the template survives exactly
        until the first refresh. That is the failure this pins shut: a footer
        that is populated on load and empty five seconds later.
        """
        tpl = (HERE / "templates" / "index.html").read_text()
        js = (HERE / "static" / "refresh.js").read_text()
        # (The end-to-end "a real running row ships them" assertion lives in
        # test_agent_stats.py, which owns the snapshot fixture that cell needs.)
        for attr, key in self.FOOTER_ATTRS:
            self.assertIn(attr, tpl, f"template does not emit {attr}")
            self.assertIn(attr, js, f"refresh.js does not emit {attr}")
            self.assertIn(f"agent_stats.{key}", tpl, f"template does not read {key}")
            self.assertIn(f"agentStats.{key}", js, f"refresh.js does not read {key}")

    def test_footer_omits_unknown_values_rather_than_guessing(self):
        """`?` / `–` are the formatter's "not known" markers, not values.

        A footer that confidently printed a wrong context size would be worse
        than one with fewer cells, so the module drops those cells — the same
        rule the model chip already follows by rendering nothing when no model
        is attributable.
        """
        src = (HERE / "static" / "multitail.js").read_text()
        self.assertIn("function metaValue(", src)
        # The three markers the server can produce for "no value".
        self.assertIn("'?'", src)
        self.assertIn("'–'", src)
        self.assertIn("bar.hidden = cells === 0;", src)

    def test_footer_reads_the_model_off_the_row_and_pins_nothing(self):
        """The model comes from the row's chip — never a hardcoded id.

        An alias like `opus` tracks whichever model is newest, so a pinned id
        would go stale silently and claim the wrong model ran.
        """
        src = (HERE / "static" / "multitail.js").read_text()
        self.assertIn(".model-tag", src)
        for pinned in ("claude-opus", "claude-sonnet", "claude-haiku"):
            self.assertNotIn(pinned, src, f"multitail.js pins a model id: {pinned}")

    def test_metrics_styles_shipped(self):
        """The cells need CSS, and `hidden` has to beat `display: flex`."""
        css = (HERE / "static" / "style.css").read_text()
        for cls in (
            ".mt-pane-meta",
            ".mt-meta-model",
            ".mt-meta-cell",
            ".mt-meta-tool",
            ".mt-meta-label",
        ):
            self.assertIn(cls, css, f"missing metrics style {cls}")
        self.assertIn(".mt-pane-meta[hidden] { display: none; }", css)
        # Phones drop the two narrowest-value cells rather than crowding the
        # title they now share a line with.
        phone_block = _multitail_phone_block(css)
        self.assertIn(".mt-meta-out", phone_block)
        self.assertIn(".mt-meta-tool", phone_block)

    def test_metrics_sit_on_the_title_line_and_give_the_row_back(self):
        """botchat #4997: title left, metrics right, ONE row for both.

        The metrics were a footer strip under the stream — a whole row of
        chrome per pane, which with five or six panes open is five or six rows
        the logs do not get. Three things have to hold together, and each is
        asserted separately because any one of them alone leaves the change
        half-made:

          renderer   the metrics element is built inside the pane HEADER, not
                     appended to the pane after the stream.
          geometry   inside the title line the TITLE is the only flexible
                     item, which is what pushes the metrics against the far
                     edge — the same mechanism the timestamp cell uses, rather
                     than a third way of aligning something right. It is also
                     the half that GIVES: the title ellipsises, while a
                     shrinking metrics group would clip a number in half and
                     print `12` where the agent is 12 minutes old. The
                     wrapper clips, so a pane too narrow even for the title's
                     floor cannot spill into the status and close button.
          budget     the pane's legibility floor has to come back DOWN by the
                     strip's height, or the reclaimed row is spent on nothing.
        """
        src = (HERE / "static" / "multitail.js").read_text()
        # Renderer: the metrics go into the header's title line.
        self.assertIn("titlebar.appendChild(meta);", src)
        self.assertIn("head.appendChild(titlebar);", src)
        self.assertNotRegex(
            src, r"wrap\.appendChild\(\s*(meta|foot)\s*\)",
            "the metrics are appended to the pane again — that is the footer "
            "row this change removed")
        self.assertNotIn("el('footer', 'mt-pane-meta')", src)

        css = (HERE / "static" / "style.css").read_text()
        rules = _css_rules(css)

        title = _decls(rules, ".mt-pane-summary")
        self.assertIsNotNone(title, ".mt-pane-summary rule not found")
        self.assertRegex(
            title, r"flex\s*:\s*1\s+1\s",
            "the title must be the flexible item, or nothing pushes the "
            "metrics right")

        self.assertRegex(
            title, r"min-width\s*:\s*[1-9]",
            "the title needs a floor, or the metrics can squeeze it to nothing")
        self.assertRegex(
            title, r"text-overflow\s*:\s*ellipsis",
            "the title is the half that gives, so it has to say it was cut")

        meta = _decls(rules, ".mt-pane-meta")
        self.assertIsNotNone(meta, ".mt-pane-meta rule not found")
        self.assertRegex(
            meta, r"flex\s*:\s*0\s+0\s",
            "the metrics must NOT shrink: a clipped number reads as a "
            "different number, while a clipped title reads as a clipped title")

        bar = _decls(rules, ".mt-pane-titlebar")
        self.assertIsNotNone(bar, ".mt-pane-titlebar rule not found")
        self.assertRegex(bar, r"display\s*:\s*flex", "the title line is a flex row")
        self.assertRegex(
            bar, r"min-width\s*:\s*0",
            "without min-width:0 the title cannot ellipsise inside the header")
        self.assertRegex(
            bar, r"overflow\s*:\s*hidden",
            "a pane too narrow for the title's floor plus the metrics must "
            "clip inside the title line, not spill onto the close button")

        # Budget: 132px / 108px were the values before the footer strip was
        # added; it cost 14px at both breakpoints and the strip is now gone.
        desktop = int(re.search(r"--mt-pane-min:\s*(\d+)px", css).group(1))
        self.assertEqual(
            desktop, 132,
            "--mt-pane-min must drop back by the strip's height, or the "
            "reclaimed row is not handed to the window budget")
        phone_block = _multitail_phone_block(css)
        phone = int(re.search(r"--mt-pane-min:\s*(\d+)px", phone_block).group(1))
        self.assertEqual(
            phone, 122,
            "the phone floor keeps the title line (+14px over the 108px base) "
            "and drops the strip's")

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

    def _workload_line_frames(self, label: str, body: str) -> list[dict]:
        """Every ``workload_line`` frame a tail of ``body`` produces."""
        wl_dir = Path(self.tmp) / f"ts-workloads-{label}"
        wl_dir.mkdir(parents=True, exist_ok=True)
        (wl_dir / f"{label}.output").write_text(body)
        (wl_dir / f"{label}.exit").write_text("0\n")
        prev = self.appmod.WORKLOAD_LOG_DIR
        self.appmod.WORKLOAD_LOG_DIR = str(wl_dir)
        try:
            frames = [
                json.loads(line[len("data: ") :])
                for chunk in self.appmod._tail_workload_output(label)
                for line in chunk.decode().splitlines()
                if line.startswith("data: ")
            ]
        finally:
            self.appmod.WORKLOAD_LOG_DIR = prev
        return [f for f in frames if f.get("kind") == "workload_line"]

    def test_unstamped_plain_text_frames_carry_no_time(self):
        """A log written before stamping existed renders exactly as before.

        No time field of any name, and the text untouched. This is the shape
        the pane's `no ts` marker is telling the truth about, and it is also
        every hostjob log — nothing on the host side stamps those.
        """
        lines = self._workload_line_frames("tsjob", "one\ntwo\n")
        self.assertTrue(lines, "no workload_line frames produced")
        for frame in lines:
            self.assertEqual(
                set(frame) & {"timestamp", "ts", "time", "at", "source_ts"},
                set(),
                f"unstamped frame gained a time field: {frame}",
            )
        self.assertEqual([f["text"] for f in lines], ["one", "two"])

    def test_stamped_plain_text_frames_split_the_prefix_into_source_ts(self):
        """A stamped line's time reaches the client as ``source_ts``.

        ``claude-watch workload stamp`` writes ``date -Iseconds`` and one space
        in front of every line. The server splits it off ONCE, here, so the
        panes, the single-item modal and the archived-output view all show the
        time in their timestamp column rather than each re-parsing the text —
        and so the time is not rendered twice (column AND line body).
        """
        lines = self._workload_line_frames(
            "tsjob2",
            "2026-09-28T22:53:35-04:00 promoting Gundam\n"
            "2026-09-28T22:53:36-04:00 done\n",
        )
        self.assertEqual([f["text"] for f in lines], ["promoting Gundam", "done"])
        self.assertEqual(
            [f.get("source_ts") for f in lines],
            ["2026-09-28T22:53:35-04:00", "2026-09-28T22:53:36-04:00"],
        )

    def test_the_stamp_parser_leaves_prose_alone(self):
        """Narrow on purpose: a line that merely BEGINS with a date is text.

        ``2026-05-01 promoted 3 shows`` is a sentence, not a stamped line, and
        eating its first word would silently corrupt the log. Only a full
        RFC3339 datetime with the ``T`` separator and an explicit offset (what
        the stamper writes) counts.
        """
        split = self.appmod._split_line_ts
        self.assertEqual(split("2026-05-01 promoted 3 shows"), ("", "2026-05-01 promoted 3 shows"))
        self.assertEqual(split("2026-05-01T10:00:00 no offset"), ("", "2026-05-01T10:00:00 no offset"))
        self.assertEqual(split("rsync 2026-05-01T10:00:00Z mid-line"), ("", "rsync 2026-05-01T10:00:00Z mid-line"))
        self.assertEqual(split(""), ("", ""))
        # A whole line that is nothing BUT a stamp has no body to attach it to,
        # so it stays text — a row with a time and no content says nothing.
        self.assertEqual(
            split("2026-05-01T10:00:00Z"), ("", "2026-05-01T10:00:00Z")
        )
        # The shapes that DO count: `Z`, `+HH:MM`, `+HHMM`, and sub-seconds.
        for stamp in (
            "2026-05-01T10:00:00Z",
            "2026-05-01T10:00:00-04:00",
            "2026-05-01T10:00:00+0200",
            "2026-05-01T10:00:00.512-04:00",
        ):
            self.assertEqual(
                split(f"{stamp} body"), (stamp, "body"), f"not recognised: {stamp}"
            )

    def test_both_log_views_read_source_ts(self):
        """The panes AND the single-item modal render the parsed stamp.

        Two surfaces show plain-text logs. A stamp that only one of them knew
        about would make the same line look timed in one place and untimed in
        the other.
        """
        mt = (HERE / "static" / "multitail.js").read_text()
        ll = (HERE / "static" / "live-log.js").read_text()
        self.assertIn("payload.source_ts", mt)
        self.assertIn("source_ts", ll)
        # Neither may fall back to arrival time for a source with no stamp.
        self.assertNotIn("Date.now()", mt.split("function sourceTs")[1][:400])

    # -- ANSI / SGR rendering in log lines ---------------------------------
    #
    # Terminal colour sequences in tailed output used to render as literal
    # `[32m` text. static/ansi.js converts them; the conversion itself is
    # pinned by static/ansi.test.js (plain node, run by `make
    # test-minisite-ansi`). What belongs HERE is the wiring CI would otherwise
    # never check: the module is SERVED, it loads before the two views that
    # call it, both views actually call it, and the palette it emits classes
    # for exists in BOTH themes.

    def test_ansi_module_is_served_and_loaded_first(self):
        self._seed_mixed()
        html = self._html()
        self.assertIn("ansi.js", html)
        resp = self.client.get("/static/ansi.js")
        self.assertEqual(resp.status_code, 200)
        # `defer` scripts run in document order, and both log views call
        # window.AnsiText while rendering a line, so ansi.js has to be declared
        # ahead of them or the first paint takes the fallback path.
        pos_ansi = html.index("ansi.js")
        self.assertLess(pos_ansi, html.index("live-log.js"))
        self.assertLess(pos_ansi, html.index("multitail.js"))

    def test_both_log_views_render_through_the_converter(self):
        """One converter, both views — not two half-implementations."""
        for name in ("live-log.js", "multitail.js"):
            src = (HERE / "static" / name).read_text()
            self.assertIn("window.AnsiText", src, name)
        # The multitail pane path must use the DOM-node renderer, because that
        # module's standing invariant is that it never assigns innerHTML.
        mt = (HERE / "static" / "multitail.js").read_text()
        self.assertIn("toFragment", mt)
        self.assertNotRegex(mt, r"\.innerHTML\s*=")

    def test_ansi_palette_is_defined_for_both_themes(self):
        """A terminal palette picked for a dark background is unreadable light.

        So ansi.js emits CLASSES, never raw ANSI RGB, and every base colour is
        defined twice — once in `:root` and once under the dark
        `prefers-color-scheme` block. A colour defined only once is a colour
        that is wrong in one of the two themes.
        """
        css = (HERE / "static" / "style.css").read_text()
        dark_blocks = re.findall(
            r"@media \(prefers-color-scheme: dark\)\s*\{(.*?)\n\}", css, re.S
        )
        dark = "\n".join(dark_blocks)
        for colour in (
            "black", "red", "green", "yellow", "blue", "magenta", "cyan", "white",
        ):
            for var in (f"--ansi-{colour}", f"--ansi-bright-{colour}"):
                self.assertIn(f"{var}:", css, var)
                self.assertIn(f"{var}:", dark, f"{var} has no dark-theme value")
                # And a class that consumes it, or the variable is decoration.
                self.assertIn(f"var({var})", css, f"{var} is never used")
        # Indexed-cube / truecolor values have no table: the module computes a
        # light and a dark variant per colour and the stylesheet picks one.
        self.assertIn("--ansi-rgb-pick", css)
        self.assertIn(".ansi-fg-rgb", css)
        # Attributes, not just colours.
        for cls in (".ansi-bold", ".ansi-dim", ".ansi-italic",
                    ".ansi-underline", ".ansi-strike"):
            self.assertIn(cls, css, cls)

    # -- verbose mode + persisted settings ---------------------------------

    def test_verbose_control_rendered(self):
        """The `all` pill and its key hint are served, defaulting to OFF.

        Off is the behaviour that existed before the toggle, and the pill lives
        inside the ``data-no-morph`` overlay like `wrap` / `time` / `clear`, so
        it is rendered once and survives the 5s tick.
        """
        self._seed_mixed()
        html = self._html()
        overlay = re.search(r'<section\s+id="multitail".*?</section>', html, re.S)
        self.assertIsNotNone(overlay)
        self.assertIn('id="multitail-verbose"', overlay.group(0))
        btn = re.search(r'<button[^>]*id="multitail-verbose".*?>', html, re.S)
        self.assertIsNotNone(btn, "verbose pill not rendered")
        self.assertIn('aria-pressed="false"', btn.group(0))
        # Discoverable without reading the source.
        self.assertIn("<kbd>v</kbd>", html)

    def test_verbose_has_a_ceiling(self):
        """Verbose stops eliding; it does not remove the bounds.

        Four live streams at full tilt is the shape of this mode, so the module
        must still cap a single line AND the text one pane retains — a line
        budget alone stops bounding memory once one line can be ten times its
        normal size.
        """
        src = (HERE / "static" / "multitail.js").read_text()
        for name in ("MAX_LINE_CHARS_VERBOSE", "MAX_PANE_CHARS"):
            m = re.search(rf"const {name} = (\d+);", src)
            self.assertIsNotNone(m, f"{name} not declared")
            self.assertGreater(int(m.group(1)), 0)
        verbose_cap = int(re.search(r"const MAX_LINE_CHARS_VERBOSE = (\d+);", src).group(1))
        wrapped_cap = int(re.search(r"const MAX_LINE_CHARS_WRAPPED = (\d+);", src).group(1))
        self.assertGreater(verbose_cap, wrapped_cap)
        # A ceiling low enough to still be a ceiling.
        self.assertLessEqual(verbose_cap, 20000)
        # The stylesheet half: verbose renders multi-line bodies, so it has to
        # wrap and honour newlines whether or not `w` is on.
        css = (HERE / "static" / "style.css").read_text()
        self.assertIn(".multitail.mt-verbose .mt-line", css)

    def test_every_header_setting_is_persisted_under_its_own_key(self):
        """Wrap / time / verbose persist the way retention already did.

        One key each, all four distinct, all read through the same guarded
        accessor — a second storage mechanism for the same kind of setting is
        how two of them end up disagreeing about what "unavailable" means.
        """
        src = (HERE / "static" / "multitail.js").read_text()
        keys = {}
        for name in ("RETENTION", "WRAP", "TS", "VERBOSE"):
            m = re.search(rf"const {name}_STORAGE_KEY = '([^']+)';", src)
            self.assertIsNotNone(m, f"{name}_STORAGE_KEY not declared")
            keys[name] = m.group(1)
        self.assertEqual(len(set(keys.values())), 4, keys)
        # The retention key is LOAD-BEARING for viewers who already have a
        # stored choice; renaming it silently resets everyone.
        self.assertEqual(keys["RETENTION"], "qsite_mt_retain")
        # Reads go through the guarded accessors, not a bare getItem.
        self.assertIn("function readStored(", src)
        self.assertIn("function readStoredFlag(", src)
        self.assertEqual(src.count("window.localStorage.getItem"), 1)
        self.assertEqual(src.count("window.localStorage.setItem"), 1)

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
