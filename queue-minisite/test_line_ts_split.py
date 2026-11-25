#!/usr/bin/env python3
"""``_split_line_ts``: exactly one source timestamp per plain-text log line.

Run::

    python3 queue-minisite/test_line_ts_split.py
"""

from __future__ import annotations

import sys
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))

from app import _plain_line_event, _split_line_ts  # noqa: E402


class SplitLineTsTests(unittest.TestCase):
    def test_workload_prefix(self) -> None:
        self.assertEqual(
            _split_line_ts("2026-11-25T13:13:44-05:00 make deploy"),
            ("2026-11-25T13:13:44-05:00", "make deploy"),
        )

    def test_ci_z_prefix_with_long_fraction(self) -> None:
        self.assertEqual(
            _split_line_ts("2026-11-25T17:41:36.4583039Z ##[group]Run"),
            ("2026-11-25T17:41:36.4583039Z", "##[group]Run"),
        )

    def test_unprefixed_line_untouched(self) -> None:
        self.assertEqual(_split_line_ts("   continuation"), ("", "   continuation"))

    def test_stacked_prefixes_collapse_outermost_wins(self) -> None:
        ts, body = _split_line_ts(
            "2026-11-25T13:13:45-05:00 2026-11-25T18:13:45.1Z step ok"
        )
        self.assertEqual((ts, body), ("2026-11-25T13:13:45-05:00", "step ok"))

    def test_bare_date_is_prose(self) -> None:
        self.assertEqual(
            _split_line_ts("2026-05-01 promoted 3 shows"),
            ("", "2026-05-01 promoted 3 shows"),
        )

    def test_stamp_only_line_kept_as_text(self) -> None:
        line = "2026-11-25T13:13:44-05:00 "
        self.assertEqual(_split_line_ts(line), ("", line))

    def test_inner_stamp_with_no_body_is_kept(self) -> None:
        ts, body = _split_line_ts("2026-11-25T13:13:44-05:00 2026-11-25T18:13:44Z ")
        self.assertEqual(ts, "2026-11-25T13:13:44-05:00")
        self.assertEqual(body, "2026-11-25T18:13:44Z ")

    def test_frame_has_source_ts_only_when_stamped(self) -> None:
        ev = _plain_line_event("2026-11-25T17:41:36Z hi", False)
        self.assertEqual((ev["source_ts"], ev["text"]), ("2026-11-25T17:41:36Z", "hi"))
        self.assertNotIn("source_ts", _plain_line_event("hi", False))


if __name__ == "__main__":
    unittest.main()
