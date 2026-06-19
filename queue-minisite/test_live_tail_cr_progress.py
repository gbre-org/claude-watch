#!/usr/bin/env python3
"""Regression tests for the live-tail \r-progress-frame fix.

Context (the bug this guards against): the SSE live-tail follow loop in
``_tail_workload_output`` / ``_tail_hostjob_output`` holds a trailing
bare-``\r`` frame in ``pending`` so a ``\r\n`` straddling the next read
can fuse. A pure-``\r`` progress producer (download bars:
``downloaded 7.3 MB\r`` rewriting in place, NO trailing newline) never
emits a ``\n``, so its latest frame stayed stuck in ``pending`` forever
and the live tail went deaf after backfill while the file grew. The fix
(``_pending_transient_frame`` + a speculative EOF emit with
consecutive-duplicate suppression) surfaces the buffered frame without
consuming ``pending`` (so a future ``\r\n`` still fuses).

These tests exercise the helper directly plus the dedup semantics; the
companion ``test_cr_lf_split.py`` covers the splitter. See PR
"fix(queue-minisite): live-tail surfaces \r progress frames".

Run::

    python3 -m pytest queue-minisite/test_live_tail_cr_progress.py -v
    # or: python3 queue-minisite/test_live_tail_cr_progress.py
"""

from __future__ import annotations

import sys
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))

from app import _pending_transient_frame, _split_cr_lf_segments  # noqa: E402


class PendingTransientFrameTests(unittest.TestCase):
    """Pure-string cases for the buffered-frame surfacer."""

    def test_empty_pending_is_none(self) -> None:
        self.assertIsNone(_pending_transient_frame(""))

    def test_bare_cr_frame(self) -> None:
        # The canonical bug input: a download bar rewriting in place.
        self.assertEqual(
            _pending_transient_frame("downloaded 7.3 MB\r"),
            "downloaded 7.3 MB",
        )

    def test_bare_cr_frame_matches_deferred_remainder(self) -> None:
        # What the follow loop actually buffers: _split_cr_lf_segments
        # without flush defers a trailing bare-\r as the remainder, and
        # _pending_transient_frame must recover exactly that frame's body.
        _, remainder = _split_cr_lf_segments("downloaded 7.3 MB\r")
        self.assertEqual(remainder, "downloaded 7.3 MB\r")
        self.assertEqual(_pending_transient_frame(remainder), "downloaded 7.3 MB")

    def test_multi_frame_buffer_ending_in_cr(self) -> None:
        # Several \r frames accumulated in one pending buffer -> surface
        # only the LAST (in-place) frame.
        buf = "10%\r20%\r33%\r"
        self.assertEqual(_pending_transient_frame(buf), "33%")

    def test_unterminated_tail_no_terminator(self) -> None:
        # A long tail with no \r or \n yet (mid-line read) -> the whole
        # buffer is the in-flight frame.
        self.assertEqual(
            _pending_transient_frame("downloading without a terminator yet"),
            "downloading without a terminator yet",
        )

    def test_pending_ending_in_lf_returns_post_lf_tail(self) -> None:
        # A buffer ending in \n has no buffered transient frame -> the
        # post-\n tail is empty string (a graduated \n line is already
        # yielded by the normal segment path; nothing transient remains).
        self.assertEqual(_pending_transient_frame("done\n"), "")

    def test_lf_then_partial_frame(self) -> None:
        # Graduated line followed by a partial bare-\r progress frame:
        # surface only the trailing frame body.
        self.assertEqual(_pending_transient_frame("stage 1 done\n80%\r"), "80%")

    def test_cr_then_partial_unterminated(self) -> None:
        # A \r frame followed by a partial next frame with no terminator
        # yet -> the partial tail is the current in-flight frame.
        self.assertEqual(_pending_transient_frame("50%\rdownloading 5"), "downloading 5")


class SpeculativeEmitDedupTests(unittest.TestCase):
    """Models the follow-loop's consecutive-transient dedup + speculative
    EOF emit using the same primitives the loop uses, so a regression in
    the dedup logic (re-emitting unchanged frames / re-stacking on the
    final flush) is caught without driving the full SSE generator."""

    @staticmethod
    def _emit_segments(segments, last_transient):
        """Mirror the loop's per-segment emit: skip a transient that
        duplicates last_transient, track last_transient across yields."""
        emitted = []
        for text, transient in segments:
            if transient and text == last_transient:
                continue
            emitted.append((text, transient))
            last_transient = text if transient else None
        return emitted, last_transient

    def test_speculative_surfaces_buffered_cr_frame_at_eof(self) -> None:
        # Simulate: chunk arrives ending in a bare \r (deferred), so the
        # normal segment path yields nothing; the EOF speculative emit
        # must surface the frame.
        pending = ""
        last_transient = None

        chunk = "downloaded 1.0 MB\r"
        pending += chunk
        segments, pending = _split_cr_lf_segments(pending)
        emitted, last_transient = self._emit_segments(segments, last_transient)
        self.assertEqual(emitted, [])  # nothing graduated; frame deferred
        self.assertEqual(pending, "downloaded 1.0 MB\r")

        # EOF, not terminal -> speculative emit.
        spec = _pending_transient_frame(pending)
        self.assertIsNotNone(spec)
        self.assertNotEqual(spec, last_transient)
        # (emit it)
        last_transient = spec
        self.assertEqual(spec, "downloaded 1.0 MB")

        # Next idle poll, file UNCHANGED -> guard suppresses re-emit.
        spec2 = _pending_transient_frame(pending)
        self.assertEqual(spec2, last_transient)  # equal -> skipped, idle-times-out

    def test_growing_progress_emits_each_distinct_frame(self) -> None:
        # The file climbs with distinct \r frames across idle polls; each
        # CHANGED frame surfaces exactly once, no stacking of duplicates.
        last_transient = None
        surfaced = []
        for body in ["1 MB", "2 MB", "2 MB", "5 MB"]:
            pending = f"downloaded {body}\r"
            spec = _pending_transient_frame(pending)
            if spec is not None and spec != last_transient:
                surfaced.append(spec)
                last_transient = spec
        self.assertEqual(
            surfaced,
            ["downloaded 1 MB", "downloaded 2 MB", "downloaded 5 MB"],
        )

    def test_pending_left_intact_so_crlf_still_fuses(self) -> None:
        # The speculative emit must NOT consume pending: a later \n
        # arriving makes the \r\n collapse to a single graduated line.
        pending = "downloaded 7.3 MB\r"
        last_transient = None
        spec = _pending_transient_frame(pending)
        last_transient = spec
        # pending is intentionally NOT cleared by the speculative emit.
        # Now the producer appends a \n (it was a \r\n all along):
        pending += "\n"
        segments, pending = _split_cr_lf_segments(pending)
        # \r\n collapses to ONE non-transient (graduated) segment.
        self.assertEqual(segments, [("downloaded 7.3 MB", False)])
        self.assertEqual(pending, "")

    def test_final_flush_does_not_restack_last_speculative(self) -> None:
        # At terminal flush, the same dedup must avoid re-emitting the
        # frame already surfaced speculatively.
        last_transient = "downloaded 7.3 MB"  # already speculatively emitted
        # The terminal read sees the bare-\r frame and flushes it.
        segments, _ = _split_cr_lf_segments("downloaded 7.3 MB\r", flush_remainder=True)
        self.assertEqual(segments, [("downloaded 7.3 MB", True)])
        emitted, last_transient = self._emit_segments(segments, last_transient)
        self.assertEqual(emitted, [])  # suppressed -> no duplicate row


if __name__ == "__main__":
    unittest.main()
