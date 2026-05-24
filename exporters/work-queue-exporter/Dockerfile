FROM python:3.13-slim-bookworm

# Security: upgrade libgnutls30 to deb12u7 (CVE-2026-33845, CVE-2026-42010).
RUN apt-get update \
    && apt-get install -y --only-upgrade libgnutls30 \
    && rm -rf /var/lib/apt/lists/*

COPY --from=ghcr.io/astral-sh/uv:latest /uv /usr/local/bin/uv
RUN uv pip install --system --no-cache prometheus_client

COPY work_queue_exporter.py /app/
# Shared agent-id / agent-state helper. Lives alongside the exporter
# in claude-watch/exporters/work-queue-exporter/. Originally vendored
# from claude-config/lib/claude_agents.py; now maintained here.
COPY claude_agents.py /app/
WORKDIR /app

EXPOSE 9099

CMD ["python3", "work_queue_exporter.py"]
