#!/usr/bin/env python3
"""Unit tests for ``_split_cr_lf_segments`` (CR/LF-aware line splitter).

The workload tail/replay paths split on BOTH ``\\r`` and ``\\n`` so
in-place progress updates (``rsync --info=progress2``, ``curl``, bare
``printf "\\r"`` loops) surface to the UI as transient segments that
the front-end replaces in place instead of stacking.

Run::

    python3 queue-minisite/test_cr_lf_split.py
"""

from __future__ import annotations

import sys
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))

# Late import — the parent app.py pulls in flask via app-level imports,
# so the import time isn't free but we only pay it once per test run.
from app import _split_cr_lf_segments  # noqa: E402


class SplitCrLfSegmentsTests(unittest.TestCase):
    """Pure-string splitter cases. No filesystem, no SSE wire shape."""

    def test_empty_buffer(self) -> None:
        segs, rem = _split_cr_lf_segments("")
        self.assertEqual(segs, [])
        self.assertEqual(rem, "")

    def test_single_lf_terminated_line(self) -> None:
        segs, rem = _split_cr_lf_segments("hello\n")
        self.assertEqual(segs, [("hello", False)])
        self.assertEqual(rem, "")

    def test_single_cr_terminated_line(self) -> None:
        segs, rem = _split_cr_lf_segments("20%\r")
        # Bare \r at end-of-buffer without flush — deferred as remainder
        # so a possible \n in the next chunk can fuse with it.
        self.assertEqual(segs, [])
        self.assertEqual(rem, "20%\r")

    def test_single_cr_terminated_line_flushed(self) -> None:
        segs, rem = _split_cr_lf_segments("20%\r", flush_remainder=True)
        self.assertEqual(segs, [("20%", True)])
        self.assertEqual(rem, "")

    def test_cr_followed_by_more_text_in_same_buffer(self) -> None:
        # "20%\r40%" — the \r is unambiguous (next char is not \n) so we
        # emit "20%" as transient and carry "40%" as remainder.
        segs, rem = _split_cr_lf_segments("20%\r40%")
        self.assertEqual(segs, [("20%", True)])
        self.assertEqual(rem, "40%")

    def test_crlf_collapses_to_single_newline(self) -> None:
        segs, rem = _split_cr_lf_segments("hello\r\nworld\n")
        self.assertEqual(segs, [("hello", False), ("world", False)])
        self.assertEqual(rem, "")

    def test_crlf_split_across_read_boundary(self) -> None:
        # First chunk ends in \r; second chunk starts with \n. Without
        # the remainder-carry trick the \r would be reported as a
        # transient terminator and the \n would later be treated as an
        # empty-line terminator. Verify the two-step protocol fuses
        # them into ONE \n-terminated segment.
        segs1, rem1 = _split_cr_lf_segments("hello\r")
        self.assertEqual(segs1, [])
        self.assertEqual(rem1, "hello\r")
        segs2, rem2 = _split_cr_lf_segments(rem1 + "\nworld\n")
        self.assertEqual(segs2, [("hello", False), ("world", False)])
        self.assertEqual(rem2, "")

    def test_only_lf_sequence(self) -> None:
        segs, rem = _split_cr_lf_segments("a\nb\nc\n")
        self.assertEqual(segs, [("a", False), ("b", False), ("c", False)])
        self.assertEqual(rem, "")

    def test_only_cr_sequence_progress_style(self) -> None:
        # Classic rsync: ".10%\r.20%\r.30%\r" — three transient frames,
        # no remainder when the final \r is followed by nothing in this
        # buffer and we DO flush at EOF.
        segs, rem = _split_cr_lf_segments(".10%\r.20%\r.30%\r", flush_remainder=True)
        self.assertEqual(segs, [(".10%", True), (".20%", True), (".30%", True)])
        self.assertEqual(rem, "")

    def test_mixed_cr_lf_sequence(self) -> None:
        # Progress updates terminated by \r, final state newline-flushed.
        # "20%\r40%\r60%\r80%\r100%\ndone\n"
        segs, rem = _split_cr_lf_segments("20%\r40%\r60%\r80%\r100%\ndone\n")
        self.assertEqual(
            segs,
            [
                ("20%", True),
                ("40%", True),
                ("60%", True),
                ("80%", True),
                ("100%", False),
                ("done", False),
            ],
        )
        self.assertEqual(rem, "")

    def test_unterminated_tail_without_flush_kept_as_remainder(self) -> None:
        segs, rem = _split_cr_lf_segments("a\nb")
        self.assertEqual(segs, [("a", False)])
        self.assertEqual(rem, "b")

    def test_unterminated_tail_flushed_as_permanent(self) -> None:
        # EOF / workload-exit case — the trailing unterminated chunk is
        # the producer's final byte sequence. We emit it as permanent
        # because there will be no further segments to replace it.
        segs, rem = _split_cr_lf_segments("a\nb", flush_remainder=True)
        self.assertEqual(segs, [("a", False), ("b", False)])
        self.assertEqual(rem, "")

    def test_bare_lf_inside_otherwise_cr_stream(self) -> None:
        # "20%\r\n40%\r" — the \r\n is the only newline; the trailing
        # \r is unambiguous because there's no buffer left after it,
        # so without flush it carries.
        segs, rem = _split_cr_lf_segments("20%\r\n40%\r")
        # The "\r\n" collapses; then "40%\r" is at end-of-buffer with
        # no following byte, so it's deferred.
        self.assertEqual(segs, [("20%", False)])
        self.assertEqual(rem, "40%\r")

    def test_empty_lines(self) -> None:
        segs, rem = _split_cr_lf_segments("a\n\nb\n")
        self.assertEqual(segs, [("a", False), ("", False), ("b", False)])
        self.assertEqual(rem, "")

    def test_consecutive_cr_no_intermediate_content(self) -> None:
        # Pathological but possible — two \r in a row with no payload
        # between them.
        segs, rem = _split_cr_lf_segments("\r\r", flush_remainder=True)
        self.assertEqual(segs, [("", True), ("", True)])
        self.assertEqual(rem, "")


class WorkloadOutputCrFlavorTests(unittest.TestCase):
    """Integration sanity for the file-open semantics.

    The tail/replay paths MUST open workload output files with
    ``newline=""`` so Python's universal-newlines translation doesn't
    silently rewrite ``\\r`` and ``\\r\\n`` to ``\\n`` during read().
    Without that flag the splitter never sees a ``\\r`` byte and every
    rsync-style progress frame would be emitted as ``transient=False``,
    defeating the whole point of the feature.
    """

    def test_universal_newlines_default_strips_cr(self) -> None:
        # Sanity baseline: prove the default open() mode IS lossy. If
        # this ever changes upstream we want to know.
        import tempfile
        with tempfile.NamedTemporaryFile("wb", delete=False) as tf:
            tf.write(b"a\r10\r20\r30\n")
            path = tf.name
        try:
            with open(path, "r", encoding="utf-8", errors="replace") as f:
                self.assertNotIn("\r", f.read())
        finally:
            import os
            os.unlink(path)

    def test_newline_empty_preserves_cr(self) -> None:
        import tempfile
        with tempfile.NamedTemporaryFile("wb", delete=False) as tf:
            tf.write(b"a\r10\r20\r30\n")
            path = tf.name
        try:
            with open(path, "r", encoding="utf-8", errors="replace", newline="") as f:
                data = f.read()
            self.assertEqual(data.count("\r"), 3)
            segs, _ = _split_cr_lf_segments(data, flush_remainder=True)
            self.assertEqual(
                segs,
                [("a", True), ("10", True), ("20", True), ("30", False)],
            )
        finally:
            import os
            os.unlink(path)


if __name__ == "__main__":  # pragma: no cover
    unittest.main(verbosity=2)
