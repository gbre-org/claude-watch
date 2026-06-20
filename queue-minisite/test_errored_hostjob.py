#!/usr/bin/env python3
"""Tests for errored-hostjob recovery in queue-minisite.

A `hostjob` (andrew-sf-tools) whose host worker exits NON-ZERO is flipped
to queue status `abandoned` with `abandon_reason = "hostjob exit <N>"` by
the reaper's `finalize_queue`. That conflates a FAILED hostjob with an
operator CANCEL and — because the `abandoned` bucket is time-sorted and
capped at `RECENT_ABANDONED_LIMIT` — can hide the failure entirely.

The minisite can't change the upstream `queue abandon` call (it lives in
the separate, read-only andrew-sf-tools repo), so it recovers the
distinction at render time:

  * `_shape` sets `is_errored_hostjob` + `hostjob_exit_code` for an
    abandoned item whose reason matches `hostjob exit <non-zero>` AND that
    carries a `hostjob:<label>` scope.
  * `_build_state` pins errored hostjobs to the TOP of the abandoned
    bucket so the recent-cap can never hide them.

Run::

    python3 -m pytest queue-minisite/test_errored_hostjob.py -v
"""

from __future__ import annotations

import os
import sys
import unittest
from datetime import datetime, timezone
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
for _mod in list(sys.modules):
    if _mod in ("app", "claude_agents"):
        del sys.modules[_mod]
import app as appmod  # noqa: E402


def _shape(item):
    now = datetime.now(timezone.utc)
    # items=[item] so ready_now / dep resolution degrade gracefully; the
    # errored-hostjob logic does not depend on the graph.
    return appmod._shape(item, now, {}, items=[item], bindings={})


def _abandoned_hostjob(qid, reason, *, label="job", abandoned_at):
    return {
        "id": qid,
        "summary": "a hostjob",
        "scope": [f"hostjob:{label}"],
        "status": "abandoned",
        "abandon_reason": reason,
        "abandoned_at": abandoned_at,
        "created_at": "2026-06-19T00:00:00+00:00",
    }


class ShapeErroredHostjobTest(unittest.TestCase):
    def test_nonzero_hostjob_exit_marks_errored(self):
        s = _shape(_abandoned_hostjob(
            "q-err-1", "hostjob exit 22", abandoned_at="2026-06-19T01:00:00+00:00"))
        self.assertTrue(s["is_errored_hostjob"])
        self.assertEqual(s["hostjob_exit_code"], "22")
        # Still status abandoned (we don't fabricate a new queue status).
        self.assertEqual(s["status"], "abandoned")
        self.assertEqual(s["hostjob_label"], "job")

    def test_zero_hostjob_exit_is_not_errored(self):
        # The reaper never produces this (rc==0 => done), but be defensive.
        s = _shape(_abandoned_hostjob(
            "q-ok", "hostjob exit 0", abandoned_at="2026-06-19T01:00:00+00:00"))
        self.assertFalse(s["is_errored_hostjob"])
        self.assertEqual(s["hostjob_exit_code"], "")

    def test_operator_cancel_of_hostjob_is_not_errored(self):
        # A manual abandon of a running hostjob has a non-matching reason —
        # it's a genuine cancel, must stay plain `abandoned`.
        s = _shape(_abandoned_hostjob(
            "q-cancel", "operator cancelled it",
            abandoned_at="2026-06-19T01:00:00+00:00"))
        self.assertFalse(s["is_errored_hostjob"])
        self.assertEqual(s["hostjob_exit_code"], "")

    def test_nonhostjob_with_exitlike_reason_is_not_errored(self):
        # An abandoned NON-hostjob item that happens to carry a matching
        # reason must NOT be mislabeled — gate on the hostjob scope too.
        item = {
            "id": "q-nh",
            "summary": "not a hostjob",
            "scope": ["repo:test"],
            "status": "abandoned",
            "abandon_reason": "hostjob exit 1",
            "abandoned_at": "2026-06-19T01:00:00+00:00",
            "created_at": "2026-06-19T00:00:00+00:00",
        }
        s = _shape(item)
        self.assertFalse(s["is_errored_hostjob"])

    def test_running_hostjob_is_not_errored(self):
        item = {
            "id": "q-run",
            "summary": "running hostjob",
            "scope": ["hostjob:job"],
            "status": "running",
            "abandon_reason": "",
            "created_at": "2026-06-19T00:00:00+00:00",
        }
        s = _shape(item)
        self.assertFalse(s["is_errored_hostjob"])


class BuildStateOrderingTest(unittest.TestCase):
    def _render(self, items):
        # _render_payload reads the queue via _cached_queue(); inject our
        # fixture items and bypass the cache.
        orig = appmod._cached_queue
        appmod._cached_queue = lambda: ({"items": items}, None)
        try:
            # _render_payload reads request.headers, so it needs a request
            # context.
            with appmod.app.test_request_context("/"):
                return appmod._render_payload()
        finally:
            appmod._cached_queue = orig

    def test_errored_hostjob_pinned_to_top_of_abandoned(self):
        # An OLD errored hostjob must sort ahead of NEWER genuine cancels so
        # the RECENT_ABANDONED_LIMIT cap can never hide the failure.
        items = [
            _abandoned_hostjob(
                "q-old-err", "hostjob exit 7", label="oldjob",
                abandoned_at="2026-06-18T00:00:00+00:00"),
            {
                "id": "q-new-cancel",
                "summary": "newer cancel",
                "scope": ["repo:test"],
                "status": "abandoned",
                "abandon_reason": "gave up",
                "abandoned_at": "2026-06-19T12:00:00+00:00",
                "created_at": "2026-06-19T00:00:00+00:00",
            },
        ]
        state = self._render(items)
        ab = state["abandoned_recent"]
        self.assertEqual(ab[0]["id"], "q-old-err")
        self.assertTrue(ab[0]["is_errored_hostjob"])
        self.assertEqual(ab[1]["id"], "q-new-cancel")

    def test_errored_hostjob_survives_recent_cap(self):
        # Bury one errored hostjob under MORE than RECENT_ABANDONED_LIMIT
        # newer cancels; it must still appear in abandoned_recent.
        cap = appmod.RECENT_ABANDONED_LIMIT
        items = [
            _abandoned_hostjob(
                "q-buried-err", "hostjob exit 13", label="buried",
                abandoned_at="2026-06-01T00:00:00+00:00"),
        ]
        for i in range(cap + 5):
            items.append({
                "id": f"q-cancel-{i:03d}",
                "summary": "cancel",
                "scope": ["repo:test"],
                "status": "abandoned",
                "abandon_reason": "gave up",
                # All newer than the buried errored job.
                "abandoned_at": f"2026-06-19T{(i % 24):02d}:00:00+00:00",
                "created_at": "2026-06-19T00:00:00+00:00",
            })
        state = self._render(items)
        ab_ids = [a["id"] for a in state["abandoned_recent"]]
        self.assertEqual(len(state["abandoned_recent"]), cap)
        self.assertIn("q-buried-err", ab_ids)
        self.assertEqual(ab_ids[0], "q-buried-err")


if __name__ == "__main__":
    unittest.main()
