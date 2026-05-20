# clipboard-upload sidecar — accepts PNG uploads from the browser and
# materializes them on a shared volume so the sibling claude-container's
# xclip shim can read them.
#
# Tiny single-file aiohttp server. We use uv to install aiohttp into the
# system site-packages directly (no venv) — the container's only Python
# user is this script, so the indirection a venv would add is wasted
# layer + slower cold-start.
#
# Build context is THIS directory (not the repo root), so the Dockerfile
# can stay self-contained and `docker build .` works from the
# clipboard-upload/ folder for direct local iteration. The
# compose-snippet.yml wires it accordingly.

FROM python:3.12-slim

# Minimal runtime — no apt packages needed; aiohttp wheels include the
# C extensions for linux x86_64 + arm64.

# Install uv from the official static binary image. Multi-stage `COPY
# --from` keeps the runtime layer slim (uv itself isn't needed at
# runtime once the wheels are installed).
COPY --from=ghcr.io/astral-sh/uv:0.5.4 /uv /usr/local/bin/uv

# Install aiohttp into system site-packages. `--system` skips venv
# creation and `--no-cache` avoids leaving wheel caches in the final
# layer.
RUN uv pip install --system --no-cache aiohttp==3.10.11

# Non-root by default. uid 65532 is the standard "nonroot" id used by
# distroless / chainguard images; matches no special host user but is
# fine because the only filesystem write target is the shared
# /host-clipboard/ volume which the operator must chmod for write.
# Operators who need a different uid can override with `user:` in
# compose (see compose-snippet.yml comments).
RUN useradd --system --uid 65532 --no-create-home --shell /usr/sbin/nologin clipboard
USER 65532:65532

WORKDIR /app
COPY app.py /app/app.py

EXPOSE 9701

# Container healthcheck — hits the in-process /healthz endpoint. Uses
# stdlib urllib so we don't need to install curl. 5s timeout because
# aiohttp's accept loop is single-threaded; if it's blocked we want to
# notice immediately.
HEALTHCHECK --interval=30s --timeout=5s --start-period=5s --retries=3 \
    CMD python -c "import urllib.request,sys; \
sys.exit(0 if urllib.request.urlopen('http://127.0.0.1:9701/healthz', timeout=3).status == 200 else 1)"

ENTRYPOINT ["python", "-u", "/app/app.py"]
