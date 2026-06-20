FROM python:3.13-slim-bookworm

# Security: upgrade libgnutls30 to deb12u7 (CVE-2026-33845, CVE-2026-42010).
RUN apt-get update \
    && apt-get install -y --only-upgrade libgnutls30 \
    && rm -rf /var/lib/apt/lists/*

COPY --from=ghcr.io/astral-sh/uv:latest /uv /usr/local/bin/uv
RUN uv pip install --system --no-cache prometheus_client

# COPY source is relative to the build context, which the monitoring compose
# stack sets to the REPO ROOT (build.context: ../../..) — so reference the
# exporter source by its full in-repo path, not a bare filename.
COPY exporters/claude-events-exporter/claude_events_exporter.py /app/
WORKDIR /app

EXPOSE 9103

CMD ["python3", "claude_events_exporter.py"]
