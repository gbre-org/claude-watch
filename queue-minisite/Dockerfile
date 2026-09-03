# queue-minisite — mobile-friendly view of the session-task work queue
# with a Stop button for running items. Single-file Flask app (app.py).
# Reads queue.json off a host bind mount; the Stop button shells out to
# a host-mounted copy of session-task to mutate it. Auth (if any) is
# expected to be handled upstream (e.g. oauth2-proxy / nginx
# auth_request) — this app trusts X-Auth-Request-Email for display
# only and does NOT enforce access control internally.

# Build context is the claude-watch repo root (NOT queue-minisite/) so
# this Dockerfile can COPY from sibling tools/. See the consumer
# docker-compose.yml's queue-minisite.build entry for the matching
# context/dockerfile pair.

FROM python:3.12-alpine

WORKDIR /app

# flask + gunicorn — no requests dep, file-only data source.
RUN pip install --no-cache-dir \
    flask==3.0.3 \
    gunicorn==23.0.0

COPY queue-minisite/app.py /app/app.py
# Shared agent-id / agent-state helper. Committed alongside the app so
# docker build is self-contained.
COPY queue-minisite/claude_agents.py /app/claude_agents.py
# Host CLIs used by the Stop button (POST /api/queue/stop) and the
# force-start button. Both are pure-Python stdlib scripts. Sourced
# from tools/ in the same repo (single source of truth — no vendored
# duplicates).
#
#   session-task — canonical source lives at
#       tools/session-task/session-task. Its containing DIRECTORY is
#       bind-mounted read-only at /app/session-task-src by docker-compose
#       (SESSION_TASK_BIN=/app/session-task-src/session-task) so the
#       container always runs the upstream version without rebuilding on
#       every PR. A directory (not single-file) mount is deliberate: a
#       single-file bind mount binds the source inode and goes STALE when
#       the host script is atomically replaced (edit / deploy / git
#       checkout), silently breaking the write endpoints. The Stop /
#       Abandon endpoints invoke `python3 $SESSION_TASK_BIN queue
#       abandon ...` and the force-start endpoint invokes `python3
#       $SESSION_TASK_BIN queue force-start`.
#   claude-event — emits queue-abandoned events into
#       $HOME/claude-events/ for the host's event watcher to pick up.
#   obligations — registers a hard-gate `force_started_unspawned`
#       obligation (wrapped `all_of [is_main_loop, ...]`, 60s dispatch
#       grace) in $HOME/.config/claude/obligations.json (bind-mounted
#       from the host) so the force-start path blocks the host's MAIN
#       loop -- never its already-running subagents -- until an Agent
#       has been dispatched for the promoted queue id.
COPY tools/claude-event/claude-event /usr/local/bin/claude-event
COPY tools/obligations/obligations /usr/local/bin/obligations
RUN chmod +x /usr/local/bin/claude-event /usr/local/bin/obligations
COPY queue-minisite/templates /app/templates
COPY queue-minisite/static /app/static

# Run as uid 1000 so the queue.json bind mount is read/writable
# without surprise perm gymnastics (assumes the host user owning the
# mounted files is uid 1000 — override via docker-compose `user:` if
# you map a different uid). The /queue-home dir (HOME inside the
# container) is created here so the bind mounts can land beneath it;
# we chown it to 1000 so the running user can write.
USER root
RUN mkdir -p /queue-home/.config/session /queue-home/claude-events \
    && chown -R 1000:1000 /queue-home
USER 1000:1000

EXPOSE 8000

# --timeout 0 disables gunicorn's 30s sync worker timeout — required so
# the live-log SSE stream (GET /api/queue/<id>/stream) can hold a
# connection open while tailing an agent transcript. We have our own
# in-handler idle/lifetime caps (SSE_TAIL_MAX_IDLE_SECONDS,
# SSE_TAIL_MAX_LIFETIME_SECONDS) that bound thread leaks. Threads bumped
# from 4 -> 8 to absorb a couple of long-lived SSE connections without
# starving the regular request path.
CMD ["gunicorn", "-b", "0.0.0.0:8000", "--workers", "2", "--threads", "8", "--timeout", "0", "--access-logfile", "-", "--error-logfile", "-", "app:app"]
