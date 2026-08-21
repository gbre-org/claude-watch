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

## Status sections (and why none may be silently dropped)

Items are bucketed into sections from a single table, `STATUS_SECTION` in
`app.py`. Section order is RUNNING → WEDGED → QUARANTINED → PENDING →
BLOCKED → OTHER → DONE → ABANDONED.

`WEDGED` and `QUARANTINED` sit directly under RUNNING because they are
in-flight items that **still hold their scope**: a pending peer in the same
scope cannot start until one of them ends.

* **wedged** — was running, the owning agent is stuck. Card shows the wedge
  reason and the two ways out (`queue unwedge`, `queue abandon`).
* **quarantined** — `queue abandon` was called on a scope-owning item without
  positive evidence the process is gone, so the scope stays locked and the
  item waits on a human. Card shows the quarantine reason, the fact that the
  scope is still held, and the three exits in descending order of evidence
  (`queue done`, `queue resurrect`, `queue release --reason ...`).

The exits are shown as copyable commands rather than one-click buttons on
purpose: each is an assertion about whether a process is still alive, and that
judgement is exactly what the quarantine state exists to stop the system from
making on an inference.

**OTHER is the structural guarantee.** Bucketing previously used a hardcoded
if/elif chain with no `else`, so any status it didn't name was dropped —
no row, no count, no log line. `wedged` and `quarantined` were both invisible
that way. Anything whose status has no declared section (including a missing
or null status) now lands in OTHER, which renders the raw status verbatim and
logs a one-time warning, so a status added to `session-task` tomorrow shows up
immediately instead of disappearing. Giving it a first-class section means
adding it to `STATUS_SECTION` plus a section in `templates/index.html` and
`static/refresh.js` — an upgrade, never a prerequisite for visibility.

Every section must exist in **both** renderers. The 5s morphdom refresh
rebuilds `#queue-root` from `static/refresh.js`, so a section present only in
the Jinja template flashes on first paint and vanishes on the first tick.
`test_status_sections.py` and `test_foldable_sections.py` pin that parity.

## Done view (archive union)

The **Done** section does NOT source solely from the `done` items still
resident in `queue.json`. It UNIONs those live items with the persistent
append-only completed-tasks archive (`completed-tasks.jsonl` — the record
`session-task` writes on every queue done/abandon), deduped by queue id
(the live `queue.json` entry wins over its archive echo). This makes the
view **reset-proof**: a `queue.json` corruption/reset wipes the live done
tail, but the historical record survives in the archive and keeps
rendering. Only DONE rows are pulled from the archive (abandon / merge /
block / … lifecycle rows are dropped). The rendered card list is capped at
`RECENT_DONE_LIMIT` (newest first); the section header's `N / M` count
reports `M` as the full union total. The archive path defaults to a
sibling of `QUEUE_JSON` (`COMPLETED_TASKS_JSONL` overrides it) and is
parsed once per file change (cached on mtime/size), so the growing archive
adds no per-request cost.

## Agent activity counters (tool calls + tokens)

Every RUNNING row carries a live cell in its item head — `11 calls · 82K tok`
in comfortable density, `11·82Kt` in compact (the head is the one line
compact never elides, so the counters stay visible there too; hover for
output tokens / last tool / last-write age) — and the header shows the
session totals `N agents · C calls · K tok · main 546K` (main = the main
loop's own context tokens).

The minisite does NOT fold transcripts itself. It reads a small JSON
snapshot that a host-side cron rewrites atomically about once a minute
(`QUEUE_MINISITE_AGENT_STATS_FILE`; shape: `{generated_at, main:{context_tokens,…},
agents:[{agent_id, queue_id, tool_calls, context_tokens, output_tokens,
last_tool, age_seconds, finished,…}], totals:{agents, tool_calls,
context_tokens, output_tokens}}`) and JOINS `agents[].queue_id` onto the
running rows. The parse is cached on the file's mtime/size and rides along
in the existing `/api/queue` 5s poll (no second timer); `/api/agent-stats`
exposes the normalised view (join maps, staleness verdict, totals) for
debugging.

Degradation rules, in order:

* empty env var → feature off (no read, no pill, no cell);
* file missing / unreadable / not JSON → hidden (same as off);
* snapshot older than `QUEUE_MINISITE_AGENT_STATS_STALE_SECONDS` (60s) →
  **stale**: every cell is blank and the pill reads `agents n/a` — a frozen
  number is worse than none, so staleness is re-derived on every request
  even when the file has not changed;
* a running row with no live agent for its queue id → no cell.

**Mount the snapshot's DIRECTORY, not the file.** The producer replaces the
file atomically (tmp + rename); a single-file bind mount pins the original
inode and goes stale on the first rewrite — the same trap the
`session-task` mount documents. Either mirror the host path
(`/var/apps/botchat:/var/apps/botchat:ro`, default env works) or mount the
dir elsewhere and point the env var at the file inside it.

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
| `QUEUE_MINISITE_AGENT_STATS_FILE` | `/var/apps/botchat/agent-stats.json` | Per-agent activity snapshot (tool calls + tokens) joined onto running rows + summed in the header — see "Agent activity counters" below. Empty = feature off. |
| `QUEUE_MINISITE_AGENT_STATS_STALE_SECONDS` | `60` | Snapshot older than this (by `generated_at` or file mtime) renders as stale: blank cells + an `agents n/a` pill, never a frozen number. |
| `PINGME_SESSION_TASK` | `0` | Set to `1` to suppress pingme chatter from `session-task` lifecycle. |
| `CLAUDE_EVENT_SESSION_TASK` | `0` | Set to `1` to suppress claude-event chatter from `session-task` lifecycle. |

## Tests

Run the whole suite the way CI does — from the repo root, one
interpreter per file, flask supplied by `uv`:

```bash
make test-queue-minisite
```

A single file, without the make wrapper:

```bash
cd queue-minisite
python3 -m venv .venv
.venv/bin/pip install flask gunicorn
.venv/bin/python test_depend.py
```

Tests spawn the Flask app in-process against a tempdir-rooted queue.json
and a vendored `session-task` (auto-located under `../tools/session-task/`;
override with `SESSION_TASK_BIN`). Each file gets its own process because
they rewrite `os.environ` and reload the `app` module at class setup.

These suites run in CI (`Queue-minisite Python tests` job) and gate
merges to `main`.
