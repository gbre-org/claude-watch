# clipboard-upload sidecar

A tiny aiohttp server that accepts PNG uploads from a browser and writes
them atomically to a shared volume, so a sibling Claude Code container's
xclip shim can read pasted screenshots without ever touching the host
clipboard daemon directly.

## Why

Claude Code reads clipboard images out-of-band via `xclip -selection
clipboard -t image/png -o`. When Claude Code runs inside a container
served over the browser (via ttyd in `examples/compose/`), there is no
host clipboard from the container's perspective — `xclip` has nothing
to read.

This sidecar closes the gap. A small browser-side handler reads the
clipboard via `navigator.clipboard.read()`, POSTs the PNG bytes here,
and this service drops the file onto a volume shared with the
claude-container. The container's xclip shim then serves that file when
Claude Code asks for clipboard contents.

End-to-end flow:

```
   user presses paste in ttyd terminal
                |
                v
   browser frontend handler             (separate change)
   navigator.clipboard.read()
                |
                v
   POST /clipboard-upload (this service)
                |
                v
   /host-clipboard/clipboard.png        (named volume, shared)
                |
                v
   xclip shim in claude-container       (separate change)
                |
                v
   Claude Code reads pasted image
```

## API

`POST /clipboard-upload`

Two accepted wire shapes:

| Content-Type | Body |
| ---- | ---- |
| `image/png` | raw PNG bytes |
| `application/json` | `{"png_base64": "<base64-encoded PNG>"}` |

Responses:

| Status | Meaning |
| ------ | ------- |
| `200`  | `{"ok": true, "bytes": N}` — file written |
| `400`  | malformed body / not a PNG / invalid base64 |
| `413`  | body larger than 10 MB |
| `415`  | unsupported `Content-Type` |
| `500`  | filesystem write failed |

Optional header: `X-Auth-Uid: <user>` — logged for audit. Not
authenticated by the service itself (see below).

Health check: `GET /healthz` -> `200 ok\n`.

## Auth model

**The service trusts whatever the upstream reverse proxy lets through.**
There is no in-process authentication. Deploy behind a real auth layer:

- nginx with `auth_basic` or `auth_request`
- oauth2-proxy
- Cloudflare Access
- A custom auth sidecar that proxies after validating a session cookie

Pass the authenticated identity in `X-Auth-Uid` so the upload log lines
attribute the write. Without an upstream proxy, the loopback bind in
`compose-snippet.yml` (`127.0.0.1:9701`) keeps the endpoint
unreachable from the network.

## Atomicity

The handler uses `tempfile.mkstemp(dir=dest.parent)` followed by
`os.replace`. Because the temp file and the destination are on the same
filesystem, `os.replace` is atomic on POSIX. A concurrent reader (the
xclip shim) sees either the previous file inode or the new one, never
a partial write.

The temp file is created with a `.clipboard.` prefix and a `.png.tmp`
suffix so even a crash mid-write leaves behind a clearly-labeled
artifact (cleanup is best-effort in the `finally` block).

## Wiring

`compose-snippet.yml` shows the drop-in. Two pieces:

1. The `clipboard-upload` service block — builds from this directory,
   publishes `127.0.0.1:9701:9701`, mounts the shared volume.
2. The named volume `host-clipboard:` at the bottom — Docker-managed
   by default; swap for a bind mount if you want to inspect the file
   from the host shell.

You ALSO need to add `host-clipboard:/host-clipboard` to the sibling
`claude-container` service's `volumes:` list so the xclip shim can
read the file. The snippet's inline comments call this out.

## Running locally for development

The service is a single Python file with one dependency:

```sh
uv venv --python 3.12
uv pip install -p .venv/bin/python aiohttp pytest pytest-aiohttp
.venv/bin/python app.py    # listens on :9701, writes to /host-clipboard/clipboard.png
```

Test against it:

```sh
# Raw PNG upload
curl -X POST \
     -H 'Content-Type: image/png' \
     --data-binary @screenshot.png \
     http://127.0.0.1:9701/clipboard-upload

# JSON base64 upload
b64=$(base64 -w0 screenshot.png)
curl -X POST \
     -H 'Content-Type: application/json' \
     -d "{\"png_base64\": \"$b64\"}" \
     http://127.0.0.1:9701/clipboard-upload
```

Override the destination path via `CLIPBOARD_DEST_PATH` and the port
via `PORT`.

## Tests

```sh
.venv/bin/python -m pytest tests/ -v
```

Coverage:

- Pure validation: PNG magic, size limits, JSON envelope, base64
  decoding, error mapping (400/413/415).
- Atomic write: round-trip, overwrite, missing parent dir creation,
  no leftover temp files, repeated writes against a stable
  destination.
- End-to-end aiohttp: status codes + filesystem effects for raw PNG,
  JSON base64, non-PNG rejection, oversize rejection, unsupported
  Content-Type rejection, `X-Auth-Uid` audit header.

## Limits

- 10 MB body cap (`MAX_BYTES` in `app.py`). Plenty for a 4K screenshot
  at typical PNG compression ratios. Bump if you need bigger.
- PNG only. Not because the file format matters to the shim — the
  shim just reads bytes — but because validating the magic catches
  client bugs (sending JPEG/HEIC/text) at upload time rather than
  later when Claude Code tries to read the file.

## Related queue items

- q-2026-05-20-dd54 — investigation that scoped this work
- q-2026-05-20-5f6e — browser-side paste handler (frontend, separate change)
- q-2026-05-20-a787 — bind-mount + xclip shim on the claude-container side
- q-2026-05-20-370e — this service
