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
from app import _split_cr_lf_segments, _collapse_transient_runs  # noqa: E402


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


class TsRsyncRealWorldTests(unittest.TestCase):
    """Regression test for the exact pattern the production batch1
    stv-promote workload emitted that triggered q-2026-05-13-d57b.

    ``stv-promote`` pipes rsync's ``--info=progress2`` through ``ts``
    which prepends an ISO-8601 timestamp to each ``\\n``-terminated
    line. A SINGLE logical line therefore looks like::

        <TS>  \\r<prog1>\\r<prog2>\\n

    where each ``<progN>`` is a partial-bytes/percent update. PR #133's
    ``cr-test4`` synthetic only exercised the bare ``\\r%d%%`` shape;
    this case adds the ts-prefixed flavor so a future regression
    (e.g. someone special-cases the prefix or trims leading whitespace
    out of transient frames) is caught at unit-test time.
    """

    def test_ts_prefixed_rsync_logical_line(self) -> None:
        # One logical rsync line: TS prefix + 2 \r updates + final \n.
        # Mirrors a real stv-promote progress2 line for a 33KB file.
        buf = (
            "2026-05-13T01:23:59-0400  "  # ts prefix
            "\r          32,768   98%  238.81kB/s    0:00:00  "
            "\r          33,431  100%  243.64kB/s    0:00:00 (xfr#82, to-chk=154/249)"
            "\n"
        )
        segs, rem = _split_cr_lf_segments(buf, flush_remainder=True)
        self.assertEqual(rem, "")
        # 3 segments: 2 transient + 1 permanent. The TS-prefix segment
        # is itself transient — the next \r segment REPLACES it in the UI.
        self.assertEqual(len(segs), 3)
        self.assertTrue(segs[0][1])  # transient
        self.assertIn("2026-05-13T01:23:59-0400", segs[0][0])
        self.assertTrue(segs[1][1])  # transient
        self.assertIn("32,768", segs[1][0])
        self.assertFalse(segs[2][1])  # permanent — graduates
        self.assertIn("xfr#82", segs[2][0])
        self.assertIn("100%", segs[2][0])

    def test_ts_prefixed_rsync_many_files(self) -> None:
        # Three back-to-back ts+rsync logical lines (3 files) — the same
        # shape that produced 367KB of \r-rich output in q-2026-05-13-41b3.
        per_file = (
            "2026-05-13T01:00:00  "
            "\r 32%  1.2MB/s  0:00:01  "
            "\r 64%  2.4MB/s  0:00:00  "
            "\r100% (xfr#N, to-chk=0/1)\n"
        )
        buf = per_file.replace("xfr#N", "xfr#a") \
            + per_file.replace("xfr#N", "xfr#b") \
            + per_file.replace("xfr#N", "xfr#c")
        segs, rem = _split_cr_lf_segments(buf, flush_remainder=True)
        self.assertEqual(rem, "")
        # 3 files × 4 segments = 12 segments. Each block has the same
        # transient/permanent shape: T, T, T, F.
        self.assertEqual(len(segs), 12)
        for i, (text, transient) in enumerate(segs):
            block = i // 4
            within = i % 4
            self.assertEqual(
                transient, within != 3,
                f"segment {i} (block {block}/{within}) transient mismatch: "
                f"got transient={transient}, text={text!r}"
            )

    def test_cr_only_inside_logical_line(self) -> None:
        # If the producer buffer ends mid-line on a bare \r (rsync hasn't
        # written its terminating \n yet), the splitter must defer that \r
        # as remainder so the next read can fuse a possible \r\n or \n.
        buf = "2026-05-13T01:00:00  \r 32%\r"  # ends on bare \r
        segs, rem = _split_cr_lf_segments(buf)
        self.assertEqual(len(segs), 1)  # only the TS prefix emitted
        self.assertTrue(segs[0][1])
        self.assertIn("2026-05-13T01:00:00", segs[0][0])
        # The trailing " 32%\r" carried as remainder for the next read.
        self.assertEqual(rem, " 32%\r")


class StaticVersionUrlDefaultsTests(unittest.TestCase):
    """``app.url_defaults`` cache-buster injection for ``url_for('static', ...)``.

    A tab opened before a JS deploy keeps running the OLD code from
    in-memory state even after the container restarts; EventSource
    reconnects through the stale renderer. Pinning the static-asset URL
    to file mtime forces the browser to refetch on every deploy.

    q-2026-05-13-d57b — Andrew saw stacked rsync progress lines in a
    modal opened pre-PR-#133 even after the deploy. The hard-refresh
    workaround works but shouldn't be required.
    """

    def setUp(self) -> None:
        # Late import to keep the heavy flask import out of the
        # splitter-only path. Clear the per-process mtime cache so each
        # test sees a clean state.
        from app import app, _STATIC_MTIME_CACHE
        self.app = app
        _STATIC_MTIME_CACHE.clear()
        self.client = app.test_client()

    def test_url_for_static_includes_v_param(self) -> None:
        from flask import url_for
        with self.app.test_request_context("/"):
            url = url_for("static", filename="live-log.js")
        self.assertIn("?v=", url, f"expected ?v= in {url!r}")
        # Param value is a non-empty hex token.
        version = url.rsplit("?v=", 1)[1]
        self.assertTrue(version, "version param is empty")
        self.assertTrue(all(c in "0123456789abcdef" for c in version),
                        f"version {version!r} is not pure hex")

    def test_url_for_non_static_endpoint_unchanged(self) -> None:
        from flask import url_for
        with self.app.test_request_context("/"):
            url = url_for("index")
        self.assertNotIn("?v=", url, f"non-static URL should not carry ?v=: {url!r}")

    def test_url_for_missing_static_file_falls_through(self) -> None:
        from flask import url_for
        with self.app.test_request_context("/"):
            url = url_for("static", filename="this-file-does-not-exist.js")
        # Missing file -> empty version -> bare URL (no ?v= appended).
        self.assertNotIn("?v=", url, f"missing file should not get ?v=: {url!r}")

    def test_url_for_traversal_attempt_falls_through(self) -> None:
        from flask import url_for
        with self.app.test_request_context("/"):
            # Path-traversal target outside the static folder must not
            # leak file mtimes via the version param.
            url = url_for("static", filename="../app.py")
        self.assertNotIn("?v=", url)

    def test_index_html_renders_versioned_script_tag(self) -> None:
        # End-to-end: GET / and verify the rendered HTML carries ?v=
        # in the live-log.js script tag.
        resp = self.client.get("/")
        # The index handler tolerates a missing queue.json by rendering
        # an empty-queue page — the script tags still render.
        self.assertEqual(resp.status_code, 200)
        body = resp.get_data(as_text=True)
        self.assertIn("live-log.js?v=", body,
                      "expected versioned live-log.js script tag in /")


class CollapseTransientRunsTests(unittest.TestCase):
    """Backfill-path transient collapser.

    The live tail keeps every ``\\r``-terminated frame so the front-end
    can render in-place rewrite animation. The BACKFILL path collapses
    consecutive transient runs to the LAST frame before applying the
    line-budget trim, so a long rsync (1000s of progress frames) doesn't
    starve the actual ``\\n``-terminated context out of the modal.

    Regression target: q-2026-05-13-65b0 — modal opened on a live rsync
    showed only the latest progress frame in the backfill; the
    stv-promote header + prior shows' completion lines had been
    crowded out by transient frames in the 200-segment budget.
    """

    def test_empty_input(self) -> None:
        self.assertEqual(_collapse_transient_runs([]), [])

    def test_no_transients_unchanged(self) -> None:
        segs = [("a", False), ("b", False), ("c", False)]
        self.assertEqual(_collapse_transient_runs(segs), segs)

    def test_single_transient_preserved(self) -> None:
        segs = [("a", False), ("p", True), ("b", False)]
        self.assertEqual(_collapse_transient_runs(segs), segs)

    def test_transient_run_before_permanent_keeps_last(self) -> None:
        # T T T F  ->  T F  (only the last transient is kept; the F
        # ultimately graduates so the user sees "final transient state"
        # then "permanent line").
        segs = [
            ("10%", True),
            ("20%", True),
            ("30%", True),
            ("done", False),
        ]
        self.assertEqual(
            _collapse_transient_runs(segs),
            [("30%", True), ("done", False)],
        )

    def test_trailing_transient_run_at_eof(self) -> None:
        # T T T (no graduating F) -> single T (the latest mid-flight
        # progress frame).
        segs = [("10%", True), ("20%", True), ("30%", True)]
        self.assertEqual(_collapse_transient_runs(segs), [("30%", True)])

    def test_multiple_transient_runs(self) -> None:
        # T T F T T F T -> T F T F T
        segs = [
            ("a1", True),
            ("a2", True),
            ("A", False),
            ("b1", True),
            ("b2", True),
            ("B", False),
            ("c1", True),
        ]
        self.assertEqual(
            _collapse_transient_runs(segs),
            [
                ("a2", True),
                ("A", False),
                ("b2", True),
                ("B", False),
                ("c1", True),
            ],
        )

    def test_docstring_example_under_trim(self) -> None:
        # End-to-end example from the q-2026-05-13-65b0 spec:
        #   \r\r\rfinal\r\nA\r\r\rfinal2\r\nB\n
        # The splitter sees \r\n pairs and collapses them, so "final\r\n"
        # graduates "final" as PERMANENT (not transient). Pre-split this
        # is 9 segments: ['', '', '', 'final', 'A', '', '', 'final2', 'B'].
        # transients are the bare-\r-terminated empty heads of each
        # progress run; the "final"/"final2" lines graduate via the
        # collapsed \r\n.
        buf = "\r\r\rfinal\r\nA\r\r\rfinal2\r\nB\n"
        segs, rem = _split_cr_lf_segments(buf, flush_remainder=True)
        self.assertEqual(rem, "")
        # Pre-collapse: 5 transients (3 \r-prefix + 1 between A and
        # final2 + 1 between A and final2's \r-prefix) interleaved with
        # 4 permanents = 9. The transients are empty because the actual
        # payload lives in the \r\n-terminated row that follows.
        self.assertEqual(len(segs), 9)
        collapsed = _collapse_transient_runs(segs)
        # Post-collapse: each leading transient run reduces to one empty
        # transient, then the \n-terminated permanent line.
        # Run 1 (\r\r\r): "" "" "" -> ""      (T)
        # final (was followed by \r\n)        (F)
        # A (\n-terminated)                   (F)
        # Run 2 (\r\r\r): "" "" "" -> ""      (T)
        # final2 (was followed by \r\n)       (F)
        # B (\n-terminated)                   (F)
        self.assertEqual(
            collapsed,
            [
                ("", True),
                ("final", False),
                ("", True),
                ("final2", False),
                ("B", False),
            ],
        )
        # Sanity: under a trim limit of 10 the user sees all 5 rows —
        # not 9 stacked rows, and not just one "final" row.
        self.assertLessEqual(len(collapsed), 10)

    def test_real_world_rsync_backfill_shape(self) -> None:
        # 100 progress frames per file × 5 files, each terminated by
        # a final \n line. Pre-collapse: 100×5 + 5 = 505 segments.
        # The 200-line budget would catch ~2 full progress runs and the
        # actual context (the 5 \n lines) would be invisible. After
        # collapse: 5 transients + 5 permanents = 10 segments, all of
        # which fit comfortably under the 200-line budget.
        parts: list[str] = []
        for f in range(5):
            for pct in range(0, 100):
                parts.append(f"prog-{f}-{pct}%\r")
            parts.append(f"file{f} done\n")
        buf = "".join(parts)
        segs, _ = _split_cr_lf_segments(buf, flush_remainder=True)
        # 100 transients + 1 permanent per file × 5 files = 505 segs.
        self.assertEqual(len(segs), 505)
        collapsed = _collapse_transient_runs(segs)
        # 1 transient + 1 permanent per file × 5 files = 10 segs.
        self.assertEqual(len(collapsed), 10)
        # Verify the last transient of each run + permanent line.
        for f in range(5):
            self.assertEqual(collapsed[2 * f], (f"prog-{f}-99%", True))
            self.assertEqual(collapsed[2 * f + 1], (f"file{f} done", False))


if __name__ == "__main__":  # pragma: no cover
    unittest.main(verbosity=2)
