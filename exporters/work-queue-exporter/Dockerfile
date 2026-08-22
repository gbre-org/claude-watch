FROM python:3.13-slim-bookworm

# Security: upgrade libgnutls30 to deb12u7 (CVE-2026-33845, CVE-2026-42010).
RUN apt-get update \
    && apt-get install -y --only-upgrade libgnutls30 \
    && rm -rf /var/lib/apt/lists/*

COPY --from=ghcr.io/astral-sh/uv:latest /uv /usr/local/bin/uv
RUN uv pip install --system --no-cache prometheus_client

# COPY sources are relative to the build context, which the monitoring
# compose stack sets to the REPO ROOT (build.context: ../../..) so it can
# also reach shared repo files — so the exporter sources must be referenced
# by their full in-repo path, not bare filenames.
COPY exporters/work-queue-exporter/work_queue_exporter.py /app/
# Shared agent-id / agent-state helper. Lives alongside the exporter
# in claude-watch/exporters/work-queue-exporter/ and is maintained here.
COPY exporters/work-queue-exporter/claude_agents.py /app/
WORKDIR /app

# --- build identity -------------------------------------------------------
#
# Baked into the `worktask_exporter_build_info` gauge the exporter emits, so
# a scrape can name the revision it is running instead of an operator
# inferring it from container create times.
#
# The commit must be handed IN: `.dockerignore` prunes `.git/` from the build
# context and this base image has no `git`, so nothing here can resolve HEAD
# on its own. That is the same constraint the Rust daemon's
# `claude_watch_build_info` lives under, and the same fix — the caller
# resolves the identity on the host, where git exists, and passes it as a
# build arg:
#
#   docker build --build-arg CW_BUILD_COMMIT="$(git rev-parse --short HEAD)" \
#                --build-arg CW_BUILD_VERSION=0.1.0 ...
#
# (`make work-queue-exporter-build` does exactly that; the out-of-tree
# monitoring compose stack should pass the same two args under `build.args`.)
#
# Both default to empty, and the exporter maps empty -> "unknown" / "0.0.0"
# rather than dropping the series, so an un-stamped image is visibly
# un-stamped instead of silently indistinguishable from an old one.
#
# Deliberately placed AFTER the COPYs: an ARG/ENV layer this late invalidates
# nothing above it, so a changed commit does not re-run the apt/uv layers.
ARG CW_BUILD_COMMIT=""
ARG CW_BUILD_VERSION=""
ENV WORKTASK_EXPORTER_COMMIT=${CW_BUILD_COMMIT} \
    WORKTASK_EXPORTER_VERSION=${CW_BUILD_VERSION} \
    WORKTASK_EXPORTER_SOURCE=image

EXPOSE 9099

CMD ["python3", "work_queue_exporter.py"]
