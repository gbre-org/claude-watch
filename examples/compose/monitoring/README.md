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

   **Claude Token Usage dashboard** — provisioned alongside it from
   `grafana/dashboards/claude-tokens.json` (uid `claude-tokens`). A per-day
   stacked bar chart of token usage over the last 7 days
   (`increase(claude_code_tokens_total[1d])`) plus a month-to-date total
   stat (`sum(claude_code_tokens_month_to_date)`) that resets on the 1st.
   Requires the `node-exporter` profile (daemon textfile metrics).

   **LiteLLM Token Spend dashboard** — provisioned from
   `grafana/dashboards/litellm-spend.json` (uid `litellm-spend`). Shows the
   operator's LiteLLM-gateway **dollar** spend and their team's aggregate
   spend: headline tiles (my monthly spend, monthly-budget-used gauge, team
   MTD spend, my key's lifetime spend, team-budget-used gauge, team member
   count, scrape health + freshness) plus two time-series (my monthly spend
   over time; team + key spend over time). Requires the `litellm-spend`
   profile (see below) or the host LaunchAgent feeding `litellm_*` metrics
   into Prometheus.

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
| `litellm-spend-exporter` (`exporters/litellm-spend-exporter/`) — OPTIONAL, `litellm-spend` profile | HTTP `/metrics` | 9104 | `litellm_*_spend_dollars`, `litellm_team_*` | LiteLLM gateway `/user/info`, `/key/info`, `/team/info` (EXTERNAL, needs a key) |

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

## LiteLLM token-spend exporter (`litellm-spend` profile / host LaunchAgent)

`exporters/litellm-spend-exporter/` polls the SF **LiteLLM gateway**
(`eng-ai-model-gateway`, the same gateway Claude Code authenticates against)
and exposes token spend in **dollars**:

| Metric | Source field |
|---|---|
| `litellm_user_spend_dollars{user}` | `/user/info` `user_info.spend` (current **month**) |
| `litellm_user_max_budget_dollars{user}` / `litellm_user_budget_reset_timestamp_seconds{user}` | monthly budget + reset |
| `litellm_key_spend_dollars{key_name,key_hash}` | `/key/info` `info.spend` (**lifetime**) |
| `litellm_team_spend_dollars{team,team_id}` | `/team/info` `team_info.spend` (team aggregate) |
| `litellm_team_max_budget_dollars` / `litellm_team_budget_reset_timestamp_seconds` / `litellm_team_members` | team budget + roster size |
| `litellm_spend_scrape_success` / `_duration_seconds` / `_last_scrape_timestamp_seconds` | scrape health |

**Auth / role caveat.** A gateway key with role `internal_user_viewer` can
read ITS OWN `/user/info`, `/key/info`, and its team's `/team/info`
(aggregate), but **cannot** read another user's `/user/info` (403) nor the
admin `/global/spend` routes — so a **per-member** breakdown is not available
at this role; the **team aggregate** is. Note also which key you feed it:
some keys can read `/user/info` (the team key), while others (e.g. the one
`devbar auth claude` returns) only satisfy `/key/info` — with those, only
`litellm_key_spend_dollars` populates. Timescale: `/user/info` spend is the
current **calendar month**; `/key/info` spend is **lifetime** — don't
cross-check one against the other.

The exporter caches upstream reads for `SCRAPE_TTL_SECONDS` (default 60) so
repeated Prometheus scrapes don't blow the gateway rpm limit. Offline unit
tests (recorded fixtures, no network) live in
`test_litellm_spend_exporter.py`.

### Run it in this stack (Docker, opt-in profile)

```bash
CW_LITELLM_API_KEY=sk-... docker compose --profile litellm-spend up -d
```

It's gated behind its own profile (like `grafana` / `node-exporter`) because
it talks to an EXTERNAL gateway and needs both a key and the corp CA. The
service mounts `CW_CA_BUNDLE` (default the Homebrew CA bundle) as the exporter
container's `SSL_CERT_FILE` so it trusts the gateway's corp-signed cert. See
`.env.example` for `CW_LITELLM_*` / `CW_CA_BUNDLE`. Never commit a real key.

### Run it host-native (macOS LaunchAgent)

`exporters/litellm-spend-exporter/launchagent/` ships a `KeepAlive`
LaunchAgent (`com.claude-watch.litellm-spend-exporter`, serving
`:9104/metrics`):

```bash
cd exporters/litellm-spend-exporter/launchagent
./install.sh          # builds a venv, renders the plist, bootstraps the agent
```

Drop a `/user/info`-capable gateway key at
`~/.config/claude-watch/litellm-spend.key` for user + team metrics (otherwise
only `litellm_key_spend_dollars` populates via the `devbar auth claude`
fallback). To scrape a host LaunchAgent from a Prometheus running in this
stack, set `CW_EXPORTER_HOST=host.docker.internal` and point the
`litellm-spend-exporter` job's target at `host.docker.internal:9104`.

**Dismantle:**

```bash
cd exporters/litellm-spend-exporter/launchagent && ./uninstall.sh
# equivalently:
launchctl bootout gui/$(id -u)/com.claude-watch.litellm-spend-exporter
rm -f ~/Library/LaunchAgents/com.claude-watch.litellm-spend-exporter.plist
rm -rf ~/.local/share/claude-watch/litellm-spend-venv   # optional: drop the venv
```

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
