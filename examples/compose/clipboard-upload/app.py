"""clipboard-upload sidecar — accepts PNG uploads from the browser and
writes them atomically to /host-clipboard/clipboard.png so the sibling
claude-container's xclip shim can pick them up.

Context: Claude Code reads clipboard images out-of-band via xclip. In a
browser-served terminal (ttyd) there is no host clipboard from the
container's perspective. A small frontend handler (q-2026-05-20-5f6e)
reads the browser clipboard via `navigator.clipboard.read()` and POSTs
the PNG blob to this endpoint, which atomically materializes the file
on the shared volume (q-2026-05-20-a787) that the claude-container's
xclip shim then reads.

Auth model: this service trusts whatever the upstream reverse proxy
lets through. Deploy behind nginx / Cloudflared / oauth2-proxy that
enforces auth BEFORE forwarding. Optional `X-Auth-Uid` header is logged
for audit.

Wire shape:
  POST /clipboard-upload
    Content-Type: image/png        body: raw PNG bytes
    Content-Type: application/json body: {"png_base64": "<base64-PNG>"}

  -> 200 {"ok": true, "bytes": N}        on success
  -> 400 {"ok": false, "error": "..."}   on malformed input
  -> 413 {"ok": false, "error": "..."}   when body > MAX_BYTES
  -> 415 {"ok": false, "error": "..."}   on unsupported Content-Type

Health:
  GET /healthz -> 200 "ok\n"
"""

from __future__ import annotations

import base64
import binascii
import json
import logging
import os
import tempfile
from pathlib import Path
from typing import Tuple

from aiohttp import web

# PNG magic per RFC 2083 §3.1.
PNG_MAGIC = b"\x89PNG\r\n\x1a\n"

# Hard cap on accepted body size. 10 MB is more than enough for a
# pasted screenshot at typical 4K resolutions and keeps a single
# misbehaving client from filling the shared volume.
MAX_BYTES = 10 * 1024 * 1024

# Default destination on the shared volume. Override with
# CLIPBOARD_DEST_PATH (handy for tests).
DEFAULT_DEST = "/host-clipboard/clipboard.png"

log = logging.getLogger("clipboard-upload")


class ValidationError(Exception):
    """Raised by validate_and_decode when the request body is unusable.

    The first arg is the HTTP status code to return; the second is the
    user-facing message body for the JSON `error` field.
    """

    def __init__(self, status: int, message: str) -> None:
        super().__init__(message)
        self.status = status
        self.message = message


def validate_and_decode(content_type: str, raw_body: bytes) -> bytes:
    """Pure function: return verified PNG bytes or raise ValidationError.

    Handles the two accepted wire shapes (raw image/png and JSON
    {png_base64}), enforces MAX_BYTES on the DECODED payload, and
    verifies the PNG magic. Factored out so the test suite can exercise
    the validation logic without spinning up the aiohttp server.
    """
    ct = (content_type or "").split(";", 1)[0].strip().lower()

    if ct == "image/png":
        payload = raw_body
    elif ct in ("application/json", "application/json; charset=utf-8"):
        # Already lowercased + split-on-semicolon above, so the second
        # branch above is dead — kept for symmetry / readability.
        try:
            obj = json.loads(raw_body.decode("utf-8"))
        except (UnicodeDecodeError, json.JSONDecodeError) as e:
            raise ValidationError(400, f"invalid JSON body: {e}") from e
        if not isinstance(obj, dict) or "png_base64" not in obj:
            raise ValidationError(400, "JSON body must be {\"png_base64\": \"...\"}")
        b64 = obj["png_base64"]
        if not isinstance(b64, str):
            raise ValidationError(400, "png_base64 must be a string")
        try:
            payload = base64.b64decode(b64, validate=True)
        except (binascii.Error, ValueError) as e:
            raise ValidationError(400, f"png_base64 not valid base64: {e}") from e
    else:
        raise ValidationError(
            415,
            "Content-Type must be image/png or application/json",
        )

    if len(payload) > MAX_BYTES:
        raise ValidationError(
            413, f"payload {len(payload)} bytes exceeds {MAX_BYTES}"
        )
    if not payload.startswith(PNG_MAGIC):
        raise ValidationError(400, "payload is not a PNG (magic bytes mismatch)")

    return payload


def atomic_write(dest: Path, payload: bytes) -> int:
    """Write `payload` to a NamedTemporaryFile in dest's parent dir,
    then `os.replace` it into place.

    os.replace is atomic on POSIX when source and destination are on
    the same filesystem, which we guarantee by creating the temp file
    in dest.parent. Returns bytes written.

    A reader (xclip shim) that opens `dest` either sees the OLD inode
    or the NEW inode in full — never a partial write.
    """
    dest.parent.mkdir(parents=True, exist_ok=True)
    # delete=False because we hand the path off to os.replace; the
    # NamedTemporaryFile context-manager would otherwise unlink it on
    # exit. If os.replace fails, the finally-clause cleans up.
    fd, tmp_path = tempfile.mkstemp(
        prefix=".clipboard.", suffix=".png.tmp", dir=str(dest.parent)
    )
    try:
        with os.fdopen(fd, "wb") as f:
            f.write(payload)
            f.flush()
            os.fsync(f.fileno())
        os.replace(tmp_path, dest)
        tmp_path = None  # ownership transferred
    finally:
        if tmp_path is not None and os.path.exists(tmp_path):
            try:
                os.unlink(tmp_path)
            except OSError:
                pass
    return len(payload)


async def handle_upload(request: web.Request) -> web.Response:
    """POST /clipboard-upload — validate + atomic-write."""
    # Short-circuit oversize bodies BEFORE buffering them all into
    # memory. Content-Length is advisory but if the client supplied
    # one larger than MAX_BYTES we can reject immediately.
    cl = request.content_length
    if cl is not None and cl > MAX_BYTES:
        return _err(413, f"Content-Length {cl} exceeds {MAX_BYTES}")

    raw = await request.read()
    if cl is None and len(raw) > MAX_BYTES:
        return _err(413, f"payload {len(raw)} bytes exceeds {MAX_BYTES}")

    try:
        payload = validate_and_decode(
            request.headers.get("Content-Type", ""), raw
        )
    except ValidationError as e:
        return _err(e.status, e.message)

    dest = Path(request.app["dest_path"])
    try:
        n = atomic_write(dest, payload)
    except OSError as e:
        log.exception("atomic_write failed: %s", e)
        return _err(500, f"write failed: {e}")

    uid = request.headers.get("X-Auth-Uid", "-")
    log.info("upload ok uid=%s bytes=%d dest=%s", uid, n, dest)
    return web.json_response({"ok": True, "bytes": n})


async def handle_health(_request: web.Request) -> web.Response:
    return web.Response(text="ok\n")


def _err(status: int, message: str) -> web.Response:
    return web.json_response({"ok": False, "error": message}, status=status)


def make_app(dest_path: str = DEFAULT_DEST) -> web.Application:
    app = web.Application(client_max_size=MAX_BYTES + 1024)
    app["dest_path"] = dest_path
    app.router.add_post("/clipboard-upload", handle_upload)
    app.router.add_get("/healthz", handle_health)
    return app


def main() -> None:
    logging.basicConfig(
        level=os.environ.get("LOG_LEVEL", "INFO").upper(),
        format="%(asctime)s %(levelname)s %(name)s %(message)s",
    )
    dest = os.environ.get("CLIPBOARD_DEST_PATH", DEFAULT_DEST)
    port = int(os.environ.get("PORT", "9701"))
    log.info("clipboard-upload starting on :%d dest=%s", port, dest)
    web.run_app(make_app(dest), host="0.0.0.0", port=port, print=None)


if __name__ == "__main__":
    main()
