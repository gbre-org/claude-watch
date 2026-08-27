FROM python:3.13-slim-bookworm

# Security: upgrade libgnutls30 (matches the sibling exporters' hardening).
RUN apt-get update \
    && apt-get install -y --only-upgrade libgnutls30 \
    && rm -rf /var/lib/apt/lists/*

COPY --from=ghcr.io/astral-sh/uv:latest /uv /usr/local/bin/uv
RUN uv pip install --system --no-cache prometheus_client

# COPY source is relative to the build context, which the monitoring compose
# stack sets to the REPO ROOT — so reference the exporter sources by their
# full in-repo path, not a bare filename. Both the library and the exporter
# are needed; the exporter imports agent_psi.
COPY exporters/agent-psi-exporter/agent_psi.py /app/
COPY exporters/agent-psi-exporter/agent_psi_exporter.py /app/
WORKDIR /app

# The exporter reads the Claude Code transcript store; bind-mount the host's
# ~/.claude/projects read-only at this path (CLAUDE_PROJECTS_DIR override).
ENV CLAUDE_PROJECTS_DIR=/claude-projects

EXPOSE 9104

CMD ["python3", "agent_psi_exporter.py"]
