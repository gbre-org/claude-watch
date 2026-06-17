# queue-minisite

Mobile-friendly Flask UI for the `session-task` work queue that
`claude-watch` ships. Renders the queue from `queue.json`, surfaces
running/pending/blocked items, and exposes Stop / Abandon / Force-start
buttons that mutate the queue via a host-mounted copy of
`session-task`.

Designed to sit BEHIND an upstream auth proxy (oauth2-proxy, nginx
`auth_request`, or similar). The app itself does NOT enforce access
control — it trusts the `X-Auth-Request-Email` header for display only.
Do not expose it to the public internet without a gate.

## Layout

| Path | Purpose |
|------|---------|
| `app.py` | Single-file Flask app (read endpoints + Stop/Abandon/Force-start writers + SSE live-log stream). |
| `claude_agents.py` | Shared helpers for parsing `claude-watch active-agents` JSON state (agent\_id, queue-id join, dedup). |
| `templates/index.html` | Solarized-themed queue view. |
| `static/` | JS modules (`refresh.js`, `live-log.js`, `keyboard.js`, etc.), CSS, icons. |
| `claude-event` | Vendored event-emitter CLI used by `session-task` lifecycle hooks. |
| `obligations` | Vendored obligations-gate CLI used by the force-start endpoint. |
| `Dockerfile` | Build (python:3.12-alpine + gunicorn). |
| `test_*.py` | End-to-end tests (run in-process against a tempdir-rooted queue.json). |

## Run standalone

```bash
cd queue-minisite
docker build -t queue-minisite .
docker run --rm -p 8000:8000 \
  -e QUEUE_JSON=/queue-home/.config/session/queue.json \
  -e AGENT_STATE_JSON=/agents-state/active-agents.json \
  -e QUEUE_SITE_TITLE="my queue" \
  -e QUEUE_SITE_LOGO_DEFAULT=1 \
  -v "$HOME/.config/session:/queue-home/.config/session:rw" \
  -v "$HOME/claude-events:/queue-home/claude-events:rw" \
  -v "/var/lib/claude-watch:/agents-state:ro" \
  -v "$HOME/.claude/projects:/agents-jsonl:ro" \
  -v "$PWD/../tools/session-task/session-task:/app/session-task:ro" \
  queue-minisite
```

Then open `http://localhost:8000/`.

## Branding

The minisite ships a generic `claude-watch` build with the bundled eye-glyph
logo at `static/claude-watch-logo.png`. The page title defaults to `queue`
and no header logo is rendered unless one of the following is set.

To swap in a private brand without forking, set the `QUEUE_SITE_*` env
vars below — typically by mounting an `env_file` on the container so the
brand identity lives outside the public image.

| Var | Default | Purpose |
|-----|---------|---------|
| `QUEUE_SITE_TITLE` | `queue` | `<title>` + header label. |
| `QUEUE_SITE_LOGO_URL` | (empty) | Header logo URL (absolute or under `/static/`). Empty = no logo unless `QUEUE_SITE_LOGO_DEFAULT=1`. |
| `QUEUE_SITE_LOGO_DEFAULT` | (unset) | Set to `1`/`true` to render the bundled `static/claude-watch-logo.png` when `QUEUE_SITE_LOGO_URL` is empty. |
| `QUEUE_SITE_BRAND` | (empty) | Footer brand string. Empty = no footer. |
| `QUEUE_SITE_FAVICON_URL` | (empty) | Favicon override. Empty falls back to the bundled generic favicons. |

## Environment

| Var | Default | Purpose |
|-----|---------|---------|
| `QUEUE_JSON` | `/queue/queue.json` | Path to `session-task` queue.json inside the container. |
| `AGENT_STATE_JSON` | `/agents-state/active-agents.json` | `claude-watch active-agents` JSON. |
| `AGENTS_JSONL_ROOT` | `/agents-jsonl` | Root of `~/.claude/projects/`; SSE live-log tails subagent transcripts here. |
| `QUEUE_LOG_ARCHIVE_DIR` | (unset) | Persistent archive dir for spawning-subagent transcripts. |
| `WORKLOAD_LOG_DIR` | `/workloads` | Workload `.output` archive dir, tailed by SSE for `workload:<label>` queue items. |
| `HOSTJOB_LOG_DIR` | `/hostjobs` | Hostjob log dir, tailed by SSE for `hostjob:<label>` queue items. NOTE per-label-dir layout: the tail target is `<HOSTJOB_LOG_DIR>/<label>/log` (not a flat `<label>.output`). |
| `CACHE_TTL_SECONDS` | `5` | Server-side cache TTL for the queue read. |
| `SSE_TAIL_MAX_IDLE_SECONDS` | `30` | Idle cap on SSE live-log streams. |
| `SSE_TAIL_MAX_LIFETIME_SECONDS` | `3600` | Lifetime cap on SSE live-log streams. |
| `SSE_TAIL_BACKFILL_LINES` | `200` | Historical-context backfill cap when a client first connects. |
| `PINGME_SESSION_TASK` | `0` | Set to `1` to suppress pingme chatter from `session-task` lifecycle. |
| `CLAUDE_EVENT_SESSION_TASK` | `0` | Set to `1` to suppress claude-event chatter from `session-task` lifecycle. |

## Tests

```bash
cd queue-minisite
python3 -m venv .venv
.venv/bin/pip install flask gunicorn
python3 test_meta.py
python3 test_depend.py
python3 test_force_start.py
python3 test_workload_archive.py
```

Tests spawn the Flask app in-process against a tempdir-rooted queue.json
and a vendored `session-task` (auto-located under `../tools/session-task/`).
