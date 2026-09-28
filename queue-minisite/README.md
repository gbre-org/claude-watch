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
session totals as ONE outlined rounded pill in the TOP half-row, `● N agents
· C calls · K tok`, right-aligned within the stack so its right edge lines up
with the last status pill's; the status pills (running / blocked / pending)
sit in the row below, left-aligned. The pill is the botchat topbar agent-bar look: a live dot,
info-blue while at least one agent is live (`.active`), muted when none
(`.idle`), dashed with an amber dot and `n/a` numerals when the snapshot is
stale (`.stale`); under 480px the units collapse to `a` / `c` / `t`.

The agent COUNT is always "live right now". The other two numerals are the
live sums only while something IS running: with no live agent those sums are
structurally 0, so instead of `0 agents · 0 calls · 0 tok` — which reads as a
broken sensor rather than an idle minute — the pill shows the last window's
tool calls and the MAIN loop's context, tagged `main`
(`0 agents · 37 calls · main 157K tok`). The server picks that
(`pill_calls_text` / `pill_tok_text` / `pill_tok_pre`); `calls_text` /
`tok_text` stay the live sums for the popover and API consumers.

Click (pin) or hover (peek) the pill for the per-agent popover — `N live
agents — C calls · K ctx · O out`, a `last 15m` line covering every agent seen
in the live window (finished ones included, so a returned agent's work does not
vanish), one row per live agent (description; type · queue id
· last tool; calls / ctx / out / age since spawn) and a footer with the main
loop's own context tokens, the snapshot age and the host (`static/agent-bar.js`,
painted from the same `/api/queue` payload: `agent_stats.rows` /
`agent_stats.main`; the template embeds the first paint as a JSON seed). The
popover's right edge is anchored to the pill's (measured into the `--abp-right`
CSS property on open / repaint / resize, clamped inside the header on both
sides); under 480px it pins edge-to-edge instead.
The two rows are half-size and hard-nowrap at every width, so the header is
always exactly two rows; `agent_stats.label` still carries the long form
(`N agents · C calls · K tok`) for API consumers. The liveness dot next to
the controls is a matching small `live` / `error` pill.

The minisite does NOT fold transcripts itself. It reads a small JSON
snapshot that the host-side producer in this repo —
[`tools/cw-agent-stats/cw-agent-stats`](../tools/cw-agent-stats/README.md),
run from cron (Linux) or the launchd plist beside it (macOS) — rewrites
atomically every few seconds
(`QUEUE_MINISITE_AGENT_STATS_FILE`; shape: `{generated_at, main:{context_tokens,…},
agents:[{agent_id, queue_id, tool_calls, context_tokens, output_tokens,
last_tool, age_seconds, finished,…}], totals:{agents, agents_spawned,
tool_calls, context_tokens, output_tokens, window_tool_calls,
window_context_tokens, window_output_tokens}}` — the bare totals are
live-only, the `window_*` ones cover the whole live window including agents
that already returned) and JOINS `agents[].queue_id` onto the
running rows. The parse is cached on the file's mtime/size and rides along
in the existing `/api/queue` 5s poll (no second timer); `/api/agent-stats`
exposes the normalised view (join maps, staleness verdict, totals) for
debugging.

Degradation rules, in order:

* empty env var → feature off (no read, no pill, no cell);
* file missing / unreadable / not JSON → hidden (same as off);
* snapshot older than `QUEUE_MINISITE_AGENT_STATS_STALE_SECONDS` (60s) →
  **stale**: every cell is blank and the pill's numerals read `n/a` (dashed
  `.stale` pill, popover shows no rows) — a frozen number is worse than
  none, so staleness is re-derived on every request even when the file has
  not changed;
* a running row with no live agent for its queue id → no cell.

**Mount the snapshot's DIRECTORY, not the file.** The producer replaces the
file atomically (tmp + rename); a single-file bind mount pins the original
inode and goes stale on the first rewrite — the same trap the
`session-task` mount documents. The producer's default `--out` is
`<claude-watch state dir>/agent-stats.json` (`$CLAUDE_WATCH_STATE_DIR`, else
`/var/lib/claude-watch` — beside the daemon's `active-agents.json`), and the
minisite's default path is the SIBLING of `AGENT_STATE_JSON`, so when the
compose stack bind-mounts that state dir at `/agents-state` (`CW_STATE_PATH`)
both sides already agree (`/agents-state/agent-stats.json`) with no env var.
Otherwise mount the producer's output dir elsewhere and point the env var at
the file inside it (the `CLAUDE_HOST_AGENT_STATS_DIR` pattern in
`examples/compose/docker-compose.yml`).

## Model tag (which model ran the item)

Every list entry — running, pending, blocked, wedged, quarantined, done,
abandoned, and the unrecognised-status fallback — carries a short chip in
its item head naming the model that ran the work (`opus`, `sonnet`, …),
with the raw transcript id (`claude-opus-5`) on hover. Like the agent
activity cell it lives in the HEAD, so compact density keeps it (one notch
smaller, never hidden) — that is the whole point: the compact row showed
queue id, priority, token count, age and creator, and no model anywhere.

The model is not carried in `queue.json` nor in the active-agents state, so
it is read from the transcript — the ARCHIVED one for finished items, the
owner's LIVE one for running items — from `message.model` on the first
non-synthetic assistant record, exactly as the detail modal's `model` row
already does (one resolver, so the row and the modal can never disagree).
A `model` string stamped on the queue record itself wins over both, which
lets tooling record the model explicitly for an item whose transcript has
since been rotated away.

`/api/queue` carries it per row as `model` (raw id) + `model_label` (family
shorthand, or the raw id verbatim when the family is unrecognised — no
invented labels). Both are `""` when no model is attributable: workload and
hostjob items ran no model at all, pending items have not run yet, and an
agent item whose transcript is gone has no truthful answer. All three
render as ABSENT — no chip, no "unknown" placeholder. Resolution is
memoised on each transcript's (mtime, size), so the archived transcripts
behind the done section are scanned once rather than on every 5s poll.

## Multitail mode (all running tails at once)

A whole-window mode that stacks one live tail per RUNNING item, so one glance
answers "what is everything doing" instead of opening the single-item log
modal N times in a row.

* **Toggle**: the `multitail` pill in the header, or the **`m`** key. `m`,
  `Esc` or the **exit** button leave it. `m` is inert while you are typing in
  a field and while another dialog (the log modal, the stop/abandon confirm)
  owns the keyboard.
* **Dismiss one pane**: the `×` in that pane's header. It does not leave the
  mode, and the pane does not come back while the mode stays open.
* Nothing is persisted. A full-window takeover that survived a reload would
  be a surprise rather than a convenience.

**Which items get a pane — and why some do not.** Exactly the rows the server
marks with a non-empty `live_log_mode` (`hostjob` / `workload` / `live`),
surfaced to both renderers as `data-live-log-mode`. That is deliberately
NARROWER than the `data-log-mode` every running card carries: every running
card is clickable, because clicking a `starting` item opens the modal in a
polling state and waits for its agent's first write — but an item with nothing
to read yet must not get a pane, since an empty pane spends a slice of the
viewport and one of the browser's scarce connections to say nothing. An item
that acquires a log joins on the next reconcile pass (2s). Precedence matches
the stream endpoint's own dispatch (hostjob → workload → agent), so a pane can
never advertise a tail the server would not serve.

**How many panes stay legible.** Panes flex-share the viewport evenly, but only
down to `--mt-pane-min` (132px — a header plus roughly ten monospace lines;
108px under 560px wide). Below that a tail shows a line or two and stops being
information, so past that point the stack **scrolls** instead of shrinking
further. On a laptop viewport that is an even split up to about five or six
tails and a scrolling stack beyond.

**Why only four stream at a time.** Each pane tails via `EventSource`, i.e. a
long-lived HTTP connection, and a browser allows only about six concurrent
connections per origin on HTTP/1.1. Six open tails and the site's own 5s
`/api/queue` poll can no longer get a connection: the page freezes in a way
that looks exactly like a server fault. So `MAX_LIVE_STREAMS` (4) panes hold a
connection at a time; the rest are built, visible, and labelled "waiting for a
stream slot", and are promoted the moment an earlier pane is closed or its job
ends. Every eligible item still gets a pane — what is rationed is the socket,
not the row.

**Panes render one line per event**, not the modal's full pretty-printed
transcript: the modal is where a single item gets read properly, and a rich
renderer inside a 132px pane shows one tool call. Agent transcript records
collapse to `▸ Bash <command>` / `← <first line of result>` / `· <text>`;
plain-text workload and hostjob logs pass through verbatim. Lines are clipped
rather than wrapped, and each pane retains the last 400.

**A job that finishes while the mode is open keeps its pane.** Removing it
would delete the output the operator was reading at the exact moment it became
final. The pane keeps its content, its header flips to `ended` (with the exit
code when the stream reported one), and its stream slot returns to the pool.
The same is true for an item that leaves the running section entirely. Closing
it is the operator's call.

The mode reads eligibility off the rendered rows rather than re-fetching
`/api/queue`, so it is consistent with what the page is showing by
construction and adds no second poller — the existing 5s tick is what moves
the attribute. `data-no-morph` on the overlay keeps that merge away from the
panes, whose live connections and scroll positions a re-render would destroy.

`test_multitail.py` pins the server-side eligibility rules, the rendered
attributes, and (by grep, since it is the CI-gating suite) the fact that
`refresh.js` mirrors both the attribute and the toggle.
`static/multitail.test.js` drives the module itself under jsdom — pane
construction, the connection cap and slot promotion, manual close, the
toggles, the terminal-event handling and the compact formatter — plus the same
parity checks against the real `refresh.js` builders.

## Layout

| Path | Purpose |
|------|---------|
| `app.py` | Single-file Flask app (read endpoints + Stop/Abandon/Force-start writers + SSE live-log stream). |
| `claude_agents.py` | Shared helpers for parsing `claude-watch active-agents` JSON state (agent\_id, queue-id join, dedup). |
| `templates/index.html` | Solarized-themed queue view. |
| `static/` | JS modules (`refresh.js`, `live-log.js`, `multitail.js`, `keyboard.js`, etc.), CSS, icons. |
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
| `QUEUE_SITE_FAVICON_URL` | (empty) | Favicon override. Empty falls back to the bundled generic favicons in `static/branding/`. |

### Overriding the whole icon set: `static/branding/`

The favicon set and `logo.svg` live in **`static/branding/`**, not `static/`
itself, so a deploy can replace all of them by mounting ONE read-only folder
over `static/branding` — shadowing nothing else. Two constraints make that the
only safe shape:

- Mounting the files individually pins each host inode, so a later atomic
  rewrite on the host (write-temp-then-rename, which is how most tooling
  updates a file) never becomes visible inside the container.
- Mounting `static/` itself would shadow the app's own frontend —
  `refresh.js`, `live-log.js`, `style.css`, the vendored morphdom — straight
  out of the image.

The files in `static/branding/` are the bundled generic defaults; a mount
layers over them, and `QUEUE_SITE_LOGO_URL` should then point at
`/static/branding/<file>`. `static/claude-watch-logo.png` stays outside that
folder on purpose: it is the app's own default logo, not a brand slot.

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
| `QUEUE_MINISITE_AGENT_STATS_FILE` | sibling of `AGENT_STATE_JSON` (`/agents-state/agent-stats.json`) | Per-agent activity snapshot (tool calls + tokens) written by `tools/cw-agent-stats/cw-agent-stats`, joined onto running rows + summed in the header — see "Agent activity counters" below. Empty = feature off. |
| `QUEUE_MINISITE_AGENT_STATS_STALE_SECONDS` | `60` | Snapshot older than this (by `generated_at` or file mtime) renders as stale: blank cells + `n/a` numerals on the header pill, never a frozen number. |
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
