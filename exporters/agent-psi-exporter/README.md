# agent-psi-exporter

Pressure-stall (PSI) metrics for a Claude Code **agent fleet**, exported to
Prometheus.

Linux PSI answers "how much productive work is lost to waiting on a resource".
This applies the same idea to a fleet of Claude Code agents. At any instant an
agent's turn loop is blocked on exactly one of:

- **inference** — the model is generating the next turn (network RTT folded in;
  deliberately not split from inference)
- **tool** — a `tool_use` block is executing (Bash / Read / MCP / sub-agent),
  up to its matching `tool_result`
- **other**, split into **idle** (waiting on an event/debounce),
  **waiting_human** (blocked on a reply), and **overhead** (the loop's own
  between-turn bookkeeping)

Pressure is computed over **active** wall-time only = total − idle −
waiting_human. `overhead` counts as productive-self, not a stall.

## What it reads

Claude Code's transcript store, `~/.claude/projects/` by default
(`CLAUDE_PROJECTS_DIR`):

- `<slug>/<session>.jsonl` — a main-loop transcript
- `<slug>/<session>/subagents/agent-<id>.jsonl` — that session's sub-agents

Each JSONL line carries an ISO-8601 `timestamp`. `assistant` entries hold
`message.content` blocks (a `tool_use` block has `{id, name}`); `user` entries
hold `tool_result` blocks keyed by `tool_use_id` (and a top-level
`toolUseResult`). Turn intervals come from message timestamps; tool intervals
from `tool_use` → matching `tool_result`; gaps classify into
idle/waiting_human/overhead by what the loop did next. Only transcripts written
within the live window (`AGENT_PSI_LIVE_WINDOW_SECONDS`, default 900s) count.

The classifier is a **deliberately rough first cut** — refined against the live
Grafana series, not on paper. See the `agent_psi.py` module docstring for the
exact rules (including the max-gap idle cap and the trailing open interval that
captures each agent's *current* state).

### API retry back-off counts as stall

A client sitting in retry back-off ("Waiting for API response · will retry in
1m 14s · check your network") writes nothing to its transcript, so wall-time
that is pure API stall used to read as a healthy in-flight turn — and, past the
300s dormancy cap, as `idle`. Two rules fix that:

- a gap that **ends at an API-error entry** (`isApiErrorMessage`, e.g. "API
  Error: 529 Overloaded") is inference for its whole length, exempt from the
  dormancy cap, and always `stalled` — zero tokens landed. Real overload
  episodes run ~220-240s per failed request, right against that cap;
- a **trailing in-flight turn** silent for more than
  `AGENT_PSI_API_STALL_TAIL_SECONDS` (default 120) is `stalled` at its true
  length. That threshold is the hysteresis keeping normal turns and one-shot
  retry blips out of the series: across 8.5k real inference gaps, p99 was 29s
  and the longest was 68s.

Both stop at `AGENT_PSI_API_STALL_MAX_SECONDS` (default 900, the live window),
past which a silent transcript reads as dormant/killed rather than stalled.

## What it emits (`/metrics`, default port 9104)

Headline — fleet & per-session-subtree pressure over 10s / 60s / 300s sliding
windows:

| metric | labels | meaning |
|---|---|---|
| `agent_psi_inference_some` | `scope`, `window`, `model` | fraction of the window ≥1 agent in scope was blocked on inference |
| `agent_psi_inference_full` | `scope`, `window`, `model` | fraction ALL active agents were blocked on inference at once — the money metric (API/rate-limit bound) |
| `agent_psi_inference_stalled_some` / `agent_psi_inference_stalled_full` | `scope`, `window`, `model` | the **stalled** subset of the above: inference gaps whose output-token throughput fell below the stall floor (429 back-off / network / TTFT / queueing, not generation), gaps that ended in an API error, and in-flight turns that have been silent past the API-stall threshold (a client parked in retry back-off — see below). `stalled_full` near 1 = the fleet is rate-limit bound, disentangled from "everyone generating hard" |
| `agent_psi_tool_some` / `agent_psi_tool_full` | `scope`, `window`, `model` | same, for tool |
| `agent_psi_scope_agents` | `scope`, `model` | live agents contributing to each (scope, model) line |
| `agent_psi_live_agents` | — | **sub-agents actually still running** this scrape (main loop excluded) — transcript not ended in a completed final turn, so a finished agent drops immediately while a mid-tool-wait agent stays counted |

`scope` is `fleet` (**sub-agents only** — the main loop is excluded), `main`
(the main loop / dispatcher on its own — its profile is idle-heavy and unlike a
worker, so it is reported side-by-side rather than blended in), or
`session:<8-char id>` (a main loop + its live sub-agents — the subtree).
`window` is `10` / `60` / `300`. `model` is `all` for the cross-model aggregate,
or a model family (`opus` / `sonnet` / `haiku` / `fable` / …) for the per-model
breakdown emitted on the `fleet` scope — so a single model's rate-limiting reads
off `agent_psi_inference_full{scope="fleet",model="opus"}`.

Byproduct — per-agent duty-cycle (a serial agent's some==full):

| metric | labels | meaning |
|---|---|---|
| `agent_duty_ratio` | `agent_id`, `category` | share of active time for `inference`/`tool`/`overhead` |
| `agent_duty_seconds` | `agent_id`, `category` | raw seconds per category (all five) |

`agent_id` is the sub-agent id or `main_loop:<8-char session id>`.

Housekeeping: `agent_psi_scrape_errors_total`,
`agent_psi_exporter_build_info{commit,version,source}`.

## Run

Host:

```
PORT=9104 CLAUDE_PROJECTS_DIR=~/.claude/projects \
  uv run --python 3.11 --with prometheus_client python3 agent_psi_exporter.py
curl -s localhost:9104/metrics | grep agent_psi_inference_full
```

Container: build with the repo root as context
(`docker build -f exporters/agent-psi-exporter/Dockerfile .`) and bind-mount
`~/.claude/projects` read-only at `/claude-projects`.

Tunables (env): `PORT`, `CLAUDE_PROJECTS_DIR`, `AGENT_PSI_MAX_GAP_SECONDS`
(default 300), `AGENT_PSI_LIVE_WINDOW_SECONDS` (default 900), `AGENT_PSI_WINDOWS`
(default `10,60,300`), `AGENT_PSI_STALLED_TOKENS_PER_SEC` (default 8 — inference
throughput below this reads as stall, not generation), `AGENT_PSI_MIN_STALL_GAP_SECONDS`
(default 5 — gaps shorter than this are never judged stalled),
`AGENT_PSI_API_STALL_TAIL_SECONDS` (default 120 — an in-flight turn silent this
long reads as an API stall), `AGENT_PSI_API_STALL_MAX_SECONDS` (default 900 —
ceiling on silent time attributable to an API stall).

## Scrape target

Point Prometheus at `agent-psi-exporter:9104` (or `localhost:9104` on the host).
The scrape config is intentionally **not** wired here — it lives in the
operator's monitoring stack. A Grafana dashboard for this exporter's metrics
ships in-repo at `monitoring/dashboards/agent-psi.json` (uid `agent-psi`); see
`monitoring/dashboards/README.md`.

## Test

```
python3 test_agent_psi_exporter.py   # exits 0/1; uv supplies prometheus_client in CI
```

## Deferred (phase 2/3)

- True exponential-decay windows (phase 1 uses fixed sliding windows).
- Nested subtree pressure for agent-spawns-agent trees (phase 1 scopes the
  subtree to a session).
- Tighter `other` split (idle vs waiting_human vs overhead) and tool-kind
  labels (compute-tool vs wait-tool), refined from the live series.
- Finer **intra-turn** stall detection. An in-flight turn is now flagged once it
  has been silent past the API-stall threshold, but that is still a blunt
  duration test: time-to-first-token, inter-delta gaps and explicit 429
  `Retry-After` values need instrumentation at the SDK / HTTP streaming layer
  (timestamp SSE deltas per request, capture 429 events), which the
  turn-granular transcript does not carry.
