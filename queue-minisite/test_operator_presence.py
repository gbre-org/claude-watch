#!/usr/bin/env python3
"""Tests for the operator-presence idle stopwatch on queue-minisite.

The header "operator present" pill reads the presence carrier file's mtime
(the SAME carrier the Rust daemon reads for its ``claude_operator_present*``
gauges) and turns ``now - mtime`` into a client-side idle stopwatch. These
tests cover the server side: carrier resolution, the idle computation, the
present/idle/away state machine, the stopwatch formatter, and the guarantee
that the stopwatch never resets across the present->away transition.

``_operator_presence`` takes an explicit ``now_epoch``, so the idle values
here are fully deterministic regardless of wall-clock time.

Run::

    python3 queue-minisite/test_operator_presence.py
"""

from __future__ import annotations

import json
import os
import shutil
import sys
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent


def _load_app(carrier_path: Path | None, tmp: Path, **env):
    """Import app.py with the presence carrier pointed at ``carrier_path``.

    ``env`` overrides (e.g. CW_PRESENCE_MAX_AGE) are applied before import
    because the presence knobs are read at module load. All other required
    env knobs are pointed at scratch paths that need not exist.
    """
    if carrier_path is not None:
        os.environ["CW_PRESENCE_FILE"] = str(carrier_path)
    else:
        os.environ.pop("CW_PRESENCE_FILE", None)
    for k, v in env.items():
        os.environ[k] = str(v)

    os.environ["QUEUE_JSON"] = str(tmp / "queue.json")
    os.environ.setdefault("AGENT_STATE_JSON", str(tmp / "no-agents.json"))
    os.environ.setdefault("AGENTS_JSONL_ROOT", str(tmp / "no-jsonl"))
    os.environ.setdefault("QUEUE_LOG_ARCHIVE_DIR", str(tmp / "no-archive"))
    os.environ.setdefault("WORKLOAD_LOG_DIR", str(tmp / "no-workloads"))

    sys.path.insert(0, str(HERE))
    for mod in list(sys.modules):
        if mod in ("app", "claude_agents"):
            del sys.modules[mod]
    import app as appmod  # noqa: E402

    return appmod


def _stamp(path: Path, mtime: float) -> None:
    """Create/refresh ``path`` and set its mtime to ``mtime`` (epoch secs)."""
    path.write_text("", encoding="utf-8")
    os.utime(path, (mtime, mtime))


class PresenceCarrierTest(unittest.TestCase):
    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp(prefix="qmin-presence-"))

    def tearDown(self):
        shutil.rmtree(self.tmp, ignore_errors=True)
        for k in ("CW_PRESENCE_FILE", "CW_PRESENCE_MAX_AGE",
                  "OPERATOR_IDLE_STOPWATCH_THRESHOLD"):
            os.environ.pop(k, None)

    # ---- carrier resolution -------------------------------------------

    def test_absent_carrier_yields_none(self):
        """No carrier file -> presence payload is None (pill doesn't render)."""
        missing = self.tmp / "does-not-exist"
        self.assertFalse(missing.exists())
        appmod = _load_app(missing, self.tmp)
        self.assertIsNone(appmod._presence_carrier_mtime())
        self.assertIsNone(appmod._operator_presence(1_000_000.0))

    def test_mtime_is_read_from_carrier(self):
        carrier = self.tmp / "operator-present"
        _stamp(carrier, 1_000_000.0)
        appmod = _load_app(carrier, self.tmp)
        self.assertAlmostEqual(appmod._presence_carrier_mtime(), 1_000_000.0, places=0)

    # ---- state machine ------------------------------------------------

    def test_present_below_threshold(self):
        """idle < threshold (10s default) -> 'present', present flag True."""
        carrier = self.tmp / "operator-present"
        _stamp(carrier, 1_000_000.0)
        appmod = _load_app(carrier, self.tmp)
        p = appmod._operator_presence(1_000_000.0 + 3)
        self.assertEqual(p["state"], "present")
        self.assertTrue(p["present"])
        self.assertAlmostEqual(p["idle_seconds"], 3, places=0)

    def test_idle_between_threshold_and_maxage(self):
        """threshold <= idle <= max_age -> 'idle', present flag still True."""
        carrier = self.tmp / "operator-present"
        _stamp(carrier, 1_000_000.0)
        appmod = _load_app(carrier, self.tmp)
        p = appmod._operator_presence(1_000_000.0 + 60)
        self.assertEqual(p["state"], "idle")
        self.assertTrue(p["present"])
        self.assertEqual(p["stopwatch"], "1:00")

    def test_away_past_maxage(self):
        """idle > max_age (420s default) -> 'away', present flag False."""
        carrier = self.tmp / "operator-present"
        _stamp(carrier, 1_000_000.0)
        appmod = _load_app(carrier, self.tmp)
        p = appmod._operator_presence(1_000_000.0 + 500)
        self.assertEqual(p["state"], "away")
        self.assertFalse(p["present"])
        self.assertEqual(p["stopwatch"], "8:20")

    def test_negative_idle_clamped(self):
        """A carrier mtime in the future (clock skew) clamps idle to 0."""
        carrier = self.tmp / "operator-present"
        _stamp(carrier, 1_000_000.0)
        appmod = _load_app(carrier, self.tmp)
        p = appmod._operator_presence(1_000_000.0 - 30)
        self.assertEqual(p["idle_seconds"], 0.0)
        self.assertEqual(p["state"], "present")

    # ---- continuity across present->away ------------------------------

    def test_stopwatch_does_not_reset_across_away_transition(self):
        """The stopwatch counts continuously across the present->away flip.

        With the carrier frozen (operator gone), idle grows monotonically and
        the state walks present -> idle -> away WITHOUT the elapsed time ever
        being reset or hidden — the core requirement.
        """
        carrier = self.tmp / "operator-present"
        _stamp(carrier, 1_000_000.0)
        appmod = _load_app(carrier, self.tmp)

        # Just before the max_age boundary: still 'idle'/present.
        before = appmod._operator_presence(1_000_000.0 + 419)
        # Just after: flips to 'away'/absent.
        after = appmod._operator_presence(1_000_000.0 + 421)

        self.assertEqual(before["state"], "idle")
        self.assertTrue(before["present"])
        self.assertEqual(after["state"], "away")
        self.assertFalse(after["present"])

        # Elapsed idle only ever grows — no reset at the boundary.
        self.assertGreater(after["idle_seconds"], before["idle_seconds"])
        self.assertEqual(before["stopwatch"], "6:59")
        self.assertEqual(after["stopwatch"], "7:01")

    # ---- config overrides ---------------------------------------------

    def test_custom_threshold_and_maxage(self):
        carrier = self.tmp / "operator-present"
        _stamp(carrier, 1_000_000.0)
        appmod = _load_app(
            carrier, self.tmp,
            OPERATOR_IDLE_STOPWATCH_THRESHOLD=5,
            CW_PRESENCE_MAX_AGE=100,
        )
        # idle 7 > threshold 5 -> idle state.
        self.assertEqual(appmod._operator_presence(1_000_000.0 + 7)["state"], "idle")
        # idle 150 > max_age 100 -> away.
        self.assertEqual(appmod._operator_presence(1_000_000.0 + 150)["state"], "away")

    # ---- stopwatch formatter ------------------------------------------

    def test_format_idle_stopwatch(self):
        appmod = _load_app(None, self.tmp)
        f = appmod._format_idle_stopwatch
        self.assertEqual(f(0), "0:00")
        self.assertEqual(f(5), "0:05")
        self.assertEqual(f(59), "0:59")
        self.assertEqual(f(60), "1:00")
        self.assertEqual(f(75), "1:15")
        self.assertEqual(f(600), "10:00")
        self.assertEqual(f(3599), "59:59")
        self.assertEqual(f(3600), "1:00:00")
        self.assertEqual(f(3661), "1:01:01")
        self.assertEqual(f(-4), "0:00")  # clamped

    # ---- payload wiring -----------------------------------------------

    def test_render_payload_includes_presence(self):
        """/api/queue surfaces operator_presence when a carrier exists."""
        carrier = self.tmp / "operator-present"
        _stamp(carrier, 1_000_000.0)
        (self.tmp / "queue.json").write_text(
            json.dumps({"items": [], "locked_scopes": {}}), encoding="utf-8"
        )
        appmod = _load_app(carrier, self.tmp)
        payload = appmod.app.test_client().get("/api/queue").get_json()
        self.assertIn("operator_presence", payload)
        p = payload["operator_presence"]
        self.assertIsInstance(p, dict)
        for key in ("present_ts", "idle_seconds", "present", "state",
                    "stopwatch", "threshold", "max_age", "server_now"):
            self.assertIn(key, p)

    def test_render_payload_presence_none_without_carrier(self):
        """No carrier -> operator_presence key present but None."""
        (self.tmp / "queue.json").write_text(
            json.dumps({"items": [], "locked_scopes": {}}), encoding="utf-8"
        )
        appmod = _load_app(self.tmp / "does-not-exist", self.tmp)
        payload = appmod.app.test_client().get("/api/queue").get_json()
        self.assertIn("operator_presence", payload)
        self.assertIsNone(payload["operator_presence"])

    def test_index_renders_pill_html(self):
        """The `/` server paint includes the pill markup when present."""
        carrier = self.tmp / "operator-present"
        _stamp(carrier, 1_000_000.0)
        (self.tmp / "queue.json").write_text(
            json.dumps({"items": [], "locked_scopes": {}}), encoding="utf-8"
        )
        appmod = _load_app(carrier, self.tmp)
        html = appmod.app.test_client().get("/").get_data(as_text=True)
        self.assertIn('id="operator-presence"', html)
        self.assertIn("presence-stopwatch", html)
        # The untouched liveness dot is still present alongside it.
        self.assertIn('title="live"', html)


if __name__ == "__main__":
    unittest.main()
