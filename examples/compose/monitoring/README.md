# claude-watch monitoring stack (Prometheus + Alertmanager)

A self-contained `docker compose` monitoring plane that scrapes the
**claude-watch metrics surface** and evaluates a starter set of alert rules.

It is a **separate compose file** from the fresh-laptop dev stack in
`examples/compose/docker-compose.yml` so the two planes start / stop
independently — bring up monitoring without pulling in claude-container, or
vice-versa.

```bash
cd examples/compose/monitoring
cp .env.example .env        # optional — edit host-path overrides
docker compose up -d        # prometheus + alertmanager + both exporters
```

Then:

- Prometheus UI / targets / alerts: <http://localhost:9090>
- Alertmanager UI: <http://localhost:9093>
- (optional) Grafana + Solarized theme + claude-watch dashboard:
  ```bash
  docker compose --profile grafana up -d --build
  ```
  -> <http://localhost:3000> (admin / admin by default)

Tear down: `docker compose down` (add `-v` to drop the TSDB/Grafana volumes).

## Grafana — Solarized theme + claude-watch dashboard

The optional `grafana` profile brings up a Grafana instance with:

1. **Solarized dark/light theme** — `grafana/Dockerfile` patches Grafana's
   compiled CSS and JS bundles with Solarized color replacements at image-build
   time, and injects `grafana/solarized.css` to catch Emotion-generated runtime
   classes. The `--build` flag on first run builds the image; subsequent starts
   are instant (cached layer).

2. **claude-watch dashboard** — provisioned from
   `grafana/dashboards/claude-watch.json`. 27 panels covering:
   - Current status, heartbeat age, context tokens, Claude Code version
   - Watcher health (live vs enabled), agent/task/shell counts
   - Interruption rate by kind (thinking, context-warning, watcher-down, etc.)
   - Hybrid-hook cooperation ratio + reminder/fallback rates
   - Build info (commit + PR of the running daemon binary)
   - Config file sizes, restarts, alerts fired

   Most panels require the `node-exporter` profile (daemon textfile metrics).
   Queue and events panels work with just the core stack.

3. **Datasource** — auto-provisioned pointing at the `prometheus` service in
   this stack (UID `prometheus`, so the dashboard JSON's datasource refs resolve
   without manual setup).

To use stock Grafana without the Solarized theme, edit `docker-compose.yml`:
replace `build: { context: ./grafana, dockerfile: Dockerfile }` with
`image: grafana/grafana:11.2.0` and remove the `--build` flag.

## The claude-watch metrics surface — THREE sources

claude-watch does not expose a single `/metrics` endpoint. Metrics come from
three places, and this stack wires up all three:

| Source | Transport | Port | Metric prefix | Reads |
|---|---|---|---|---|
| `work-queue-exporter` (`exporters/work-queue-exporter/`) | HTTP `/metrics` | 9099 | `worktask_queue_*` | `queue.json` + `active-agents.json` |
| `claude-events-exporter` (`exporters/claude-events-exporter/`) | HTTP `/metrics` | 9103 | `claude_events_*` | `~/claude-events/` spool |
| `claude-watch` daemon (`claude-watch metrics`, `src/metrics.rs`) | **node-exporter textfile** | 9100 | `claude_watch_*`, `claude_code_*` | `~/.config/claude-watch/state.json` -> writes `.prom` |

The two Python exporters are HTTP scrape targets and are built + run by this
compose file (from the in-repo Dockerfiles). The daemon is different: it only
**writes a textfile** `.prom` (default
`/var/lib/node-exporter/textfile/claude_watch.prom`) — it has no HTTP server.
To scrape it, enable the optional `node-exporter` profile, which runs
node-exporter with just the textfile collector pointed at that dir:

```bash
docker compose --profile node-exporter up -d
```

and make sure your `claude-watch metrics` cron writes into `CW_TEXTFILE_DIR`
(see `.env.example`). Without that profile, the `node-exporter` scrape job
simply stays DOWN and only the queue/events metrics are collected.

### Exporter data sources

The exporters observe the live system's files via **read-only bind-mounts**
(host paths, overridable in `.env`): `queue.json`, `active-agents.json`, the
`claude-events` spool, and the workload / hostjob progress-heartbeat dirs.
Defaults match the standard Linux host layout; macOS / non-default layouts set
the `CW_*` overrides in `.env`.

If you already run the exporters elsewhere (e.g. on the host, or inside the
fresh-laptop stack's own network) rather than here, point Prometheus at them
by setting `CW_EXPORTER_HOST=host.docker.internal` and editing
`prometheus.yml`'s targets to the host-gateway address — the `prometheus` and
`alertmanager` services already declare `host.docker.internal:host-gateway`
(matching the sibling stack's pattern).

## Alert rules — DERIVED FROM THE DOCS, not pre-existing

**claude-watch ships no alert-rule files.** The README (§ *External alerting —
not a fourth tier*) states Prometheus / Alertmanager are explicitly **out of
scope** for the daemon; the rule names (`WorkQueueOrphaned`,
`WorkQueueStuckSoft`, `WorkQueueReadyStuck`, ...) appear in the repo only as
**prose** — described as "the out-of-tree Prometheus alert rules" that the
in-tree `claude-watch queue-check` subcommand mirrors (`src/config.rs`
`QueueCheckConfig`, `config.toml [queue_check]`).

`alerts.rules.yml` therefore **translates that documented intent** into
runnable PromQL against the metric names the exporters + daemon actually emit.
Each rule's comment cites its provenance. Treat thresholds as starting points:

- `WorkQueueOrphaned` — `has_live_owner{status="running"} == 0` (the exporter
  docstring requires the `{status="running"}` filter so *blocked* items, which
  have no live agent by design, don't fire).
- `WorkQueueStuckSoft` — long `running_elapsed` `unless on(id)` a fresh
  `progress_age` (excludes healthy long-running workloads), `for: 15m`
  (mirrors `config.toml stale_heartbeat_min = 15`).
- `WorkQueueReadyStuck` — `ready_age_seconds` over threshold.
- `AgentStateFileMissing` — `agent_state_last_modified == 0` (claude-watch
  stopped publishing `active-agents.json`).
- `ClaudeEventsBacklogStale` — oldest unconsumed event aging out (wedged main
  loop / dead `claude-event-watch`).
- `ClaudeWatchDown` / `ClaudeWatchersMissing` / `ClaudeMainLoopHeartbeatStale`
  — daemon textfile gauges (only meaningful with the `node-exporter` profile).

## Alertmanager -> back into claude-watch's native tiers

Per the README, external alerts should route **back into** one of
claude-watch's three native tiers (events / obligations / interruptions). The
idiomatic wiring is a webhook receiver that turns an alert into a
`claude-event` (dropping JSON into `~/claude-events/`, surfaced by
`claude-event-watch`). That bridge is operator-specific, so `alertmanager.yml`
ships a null default receiver with the webhook-bridge receiver documented +
commented as the integration point.
