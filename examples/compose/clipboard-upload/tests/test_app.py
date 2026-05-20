"""Tests for the clipboard-upload sidecar.

Two layers:

1. Pure-function tests against `validate_and_decode` + `atomic_write` —
   no aiohttp, no server, just byte-level behavior.
2. End-to-end aiohttp tests via `AioHTTPTestCase` that hit the real
   handler with the real router. These verify wire-shape (HTTP status,
   JSON body) and confirm the temp-file + atomic-rename actually
   produces a file on disk.

Run with `pytest tests/` from the clipboard-upload/ directory, OR
`pytest examples/compose/clipboard-upload/tests/` from the repo root.
"""

from __future__ import annotations

import asyncio
import base64
import json
import os
import sys
import tempfile
import unittest
from pathlib import Path

# Make `app` importable when pytest is launched from the repo root.
HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE.parent))

import app  # noqa: E402  (path mangled above)
from app import (  # noqa: E402
    PNG_MAGIC,
    MAX_BYTES,
    ValidationError,
    atomic_write,
    make_app,
    validate_and_decode,
)
from aiohttp.test_utils import AioHTTPTestCase  # noqa: E402


def make_png(extra: bytes = b"") -> bytes:
    """Minimal byte sequence that passes the magic-bytes check.

    The validator only checks the 8-byte signature, so we don't need a
    structurally-valid PNG — we just need the magic prefix.
    """
    return PNG_MAGIC + extra


# ---------------------------------------------------------------------
# Layer 1 — pure-function tests
# ---------------------------------------------------------------------


class ValidateAndDecodeTests(unittest.TestCase):
    def test_raw_png_accepted(self):
        body = make_png(b"hello")
        out = validate_and_decode("image/png", body)
        self.assertEqual(out, body)

    def test_content_type_with_charset_accepted(self):
        body = make_png(b"x")
        out = validate_and_decode("image/png; charset=binary", body)
        self.assertEqual(out, body)

    def test_json_base64_accepted(self):
        body = make_png(b"abc")
        payload = json.dumps({"png_base64": base64.b64encode(body).decode()})
        out = validate_and_decode(
            "application/json", payload.encode("utf-8")
        )
        self.assertEqual(out, body)

    def test_non_png_magic_rejected(self):
        with self.assertRaises(ValidationError) as cm:
            validate_and_decode("image/png", b"\x00\x00not-a-png")
        self.assertEqual(cm.exception.status, 400)
        self.assertIn("PNG", cm.exception.message)

    def test_oversize_raw_rejected(self):
        body = make_png(b"\x00" * (MAX_BYTES + 1))
        with self.assertRaises(ValidationError) as cm:
            validate_and_decode("image/png", body)
        self.assertEqual(cm.exception.status, 413)

    def test_oversize_base64_rejected(self):
        # Decoded payload must exceed MAX_BYTES.
        big = make_png(b"\x00" * (MAX_BYTES + 1))
        payload = json.dumps({"png_base64": base64.b64encode(big).decode()})
        with self.assertRaises(ValidationError) as cm:
            validate_and_decode("application/json", payload.encode("utf-8"))
        self.assertEqual(cm.exception.status, 413)

    def test_unsupported_content_type_rejected(self):
        with self.assertRaises(ValidationError) as cm:
            validate_and_decode("text/plain", b"hello")
        self.assertEqual(cm.exception.status, 415)

    def test_empty_content_type_rejected(self):
        with self.assertRaises(ValidationError) as cm:
            validate_and_decode("", b"hello")
        self.assertEqual(cm.exception.status, 415)

    def test_invalid_json_rejected(self):
        with self.assertRaises(ValidationError) as cm:
            validate_and_decode("application/json", b"{not json")
        self.assertEqual(cm.exception.status, 400)

    def test_json_missing_key_rejected(self):
        with self.assertRaises(ValidationError) as cm:
            validate_and_decode(
                "application/json", b'{"wrong_key": "abc"}'
            )
        self.assertEqual(cm.exception.status, 400)

    def test_json_non_string_value_rejected(self):
        with self.assertRaises(ValidationError) as cm:
            validate_and_decode(
                "application/json", b'{"png_base64": 123}'
            )
        self.assertEqual(cm.exception.status, 400)

    def test_json_invalid_base64_rejected(self):
        with self.assertRaises(ValidationError) as cm:
            validate_and_decode(
                "application/json",
                b'{"png_base64": "not valid base64!!!"}',
            )
        self.assertEqual(cm.exception.status, 400)


class AtomicWriteTests(unittest.TestCase):
    def test_writes_file_and_returns_bytes(self):
        with tempfile.TemporaryDirectory() as d:
            dest = Path(d) / "clipboard.png"
            body = make_png(b"hello world")
            n = atomic_write(dest, body)
            self.assertEqual(n, len(body))
            self.assertEqual(dest.read_bytes(), body)

    def test_overwrites_existing(self):
        with tempfile.TemporaryDirectory() as d:
            dest = Path(d) / "clipboard.png"
            dest.write_bytes(b"OLD")
            new = make_png(b"NEW")
            atomic_write(dest, new)
            self.assertEqual(dest.read_bytes(), new)

    def test_creates_missing_parent(self):
        with tempfile.TemporaryDirectory() as d:
            dest = Path(d) / "nested" / "deep" / "clipboard.png"
            body = make_png(b"x")
            atomic_write(dest, body)
            self.assertTrue(dest.exists())

    def test_no_partial_tempfiles_left_after_success(self):
        with tempfile.TemporaryDirectory() as d:
            dest = Path(d) / "clipboard.png"
            atomic_write(dest, make_png(b"x"))
            leftovers = [
                p for p in Path(d).iterdir() if p.name.startswith(".clipboard.")
            ]
            self.assertEqual(leftovers, [], f"tempfiles left behind: {leftovers}")

    def test_concurrent_writes_serialize_via_rename(self):
        """Two concurrent atomic_writes must both finish with the
        destination containing one of the two complete payloads — never
        a torn write. We can't easily race threads deterministically,
        but we CAN verify the basic contract by running 20 sequential
        writes and asserting that each leaves a valid, complete file.
        """
        with tempfile.TemporaryDirectory() as d:
            dest = Path(d) / "clipboard.png"
            for i in range(20):
                body = make_png(f"iteration-{i}".encode())
                atomic_write(dest, body)
                self.assertEqual(dest.read_bytes(), body)
                # No leftover tempfiles between iterations.
                tmps = [
                    p
                    for p in Path(d).iterdir()
                    if p.name.startswith(".clipboard.")
                ]
                self.assertEqual(tmps, [])


# ---------------------------------------------------------------------
# Layer 2 — end-to-end aiohttp tests
# ---------------------------------------------------------------------


class HandlerTests(AioHTTPTestCase):
    async def get_application(self):
        self.tmpdir = tempfile.mkdtemp()
        self.dest = os.path.join(self.tmpdir, "clipboard.png")
        return make_app(dest_path=self.dest)

    async def tearDownAsync(self):
        # AioHTTPTestCase doesn't guarantee filesystem cleanup; do it
        # ourselves so /tmp doesn't pile up under repeated runs.
        import shutil

        shutil.rmtree(self.tmpdir, ignore_errors=True)
        await super().tearDownAsync()

    async def test_healthz(self):
        resp = await self.client.get("/healthz")
        self.assertEqual(resp.status, 200)
        text = await resp.text()
        self.assertIn("ok", text)

    async def test_post_raw_png_writes_file(self):
        body = make_png(b"raw-png-payload")
        resp = await self.client.post(
            "/clipboard-upload",
            data=body,
            headers={"Content-Type": "image/png"},
        )
        self.assertEqual(resp.status, 200)
        js = await resp.json()
        self.assertTrue(js["ok"])
        self.assertEqual(js["bytes"], len(body))
        with open(self.dest, "rb") as f:
            self.assertEqual(f.read(), body)

    async def test_post_json_base64_writes_file(self):
        body = make_png(b"base64-payload")
        envelope = {"png_base64": base64.b64encode(body).decode()}
        resp = await self.client.post(
            "/clipboard-upload",
            json=envelope,
        )
        self.assertEqual(resp.status, 200)
        with open(self.dest, "rb") as f:
            self.assertEqual(f.read(), body)

    async def test_post_non_png_rejected(self):
        resp = await self.client.post(
            "/clipboard-upload",
            data=b"definitely not a PNG",
            headers={"Content-Type": "image/png"},
        )
        self.assertEqual(resp.status, 400)
        js = await resp.json()
        self.assertFalse(js["ok"])
        self.assertFalse(os.path.exists(self.dest))

    async def test_post_oversize_rejected(self):
        body = make_png(b"\x00" * (MAX_BYTES + 1))
        resp = await self.client.post(
            "/clipboard-upload",
            data=body,
            headers={"Content-Type": "image/png"},
        )
        # aiohttp's client_max_size also returns 413, so either route
        # is acceptable here.
        self.assertEqual(resp.status, 413)
        self.assertFalse(os.path.exists(self.dest))

    async def test_post_unsupported_content_type_rejected(self):
        resp = await self.client.post(
            "/clipboard-upload",
            data=b"hello",
            headers={"Content-Type": "text/plain"},
        )
        self.assertEqual(resp.status, 415)
        self.assertFalse(os.path.exists(self.dest))

    async def test_audit_header_logged(self):
        # Just make sure passing X-Auth-Uid doesn't break the path —
        # we don't assert on log output (would tie the test to the
        # logging config).
        body = make_png(b"auditme")
        resp = await self.client.post(
            "/clipboard-upload",
            data=body,
            headers={
                "Content-Type": "image/png",
                "X-Auth-Uid": "alice@example.com",
            },
        )
        self.assertEqual(resp.status, 200)


if __name__ == "__main__":
    unittest.main()
