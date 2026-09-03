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
waiting_human. `overhead` counts as productive-self, not a stall — but it still
gets its own some/full pair, because inference + tool + overhead partition
active time, so a panel can show tool use *and* overhead and account for every
active second.

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

### A long overload storm is NOT fully visible here — read the daemon gauge

Those two rules cover a stall of up to `AGENT_PSI_API_STALL_MAX_SECONDS`. A
**longer** overload storm falls off this exporter entirely, and it does so
silently, in two compounding steps:

1. past `AGENT_PSI_API_STALL_MAX_SECONDS` the stall attribution stops, so the
   silence reverts to reading as dormant rather than stalled;
2. past `AGENT_PSI_LIVE_WINDOW_SECONDS` (also 900 by default) the transcript's
   mtime leaves the live window, so the session is dropped from the fleet
   before it is aggregated at all — pressure over zero agents is not "high",
   it is absent.

So a main loop parked in `529 Overloaded · Retrying in 12s · attempt 7/10` for
17 minutes ends up indistinguishable from an idle one on these panels — the
failure mode observed 2026-09-03. That is not a tuning bug to widen the
windows out of: the cause is structural. **A client in retry back-off writes
nothing to its transcript**, so the only evidence of the storm is the retry
banner the client paints on its terminal, and a transcript reader cannot see
that by construction. Raising the windows just trades a false negative for a
long-silence-reads-as-stall false positive on genuinely dead sessions.

The daemon reads the terminal pane, so it CAN see the banner, and exports the
state directly (`src/metrics.rs`, via the `claude-watch metrics` textfile):

| metric | meaning |
|---|---|
| `claude_watch_api_retry_active` | 1 while a 529/overloaded/5xx retry banner was seen within `claude_watch_api_retry_stale_after_secs`. **The alertable state** |
| `claude_watch_api_retry_episode_seconds` | how long the current episode has run — the severity axis, since a 12s blip and a 17-minute storm are both `active=1` |
| `claude_watch_api_retry_consecutive_cycles` | detection cycles in the current episode |
| `claude_watch_api_retry_last_seen_timestamp_seconds` | epoch of the most recent detection, for age-based queries |
| `claude_watch_api_retry_episodes_total` | storm COUNT (one per episode, not per cycle) — frequency, independent of duration |
| `claude_watch_api_retry_stale_after_secs` | the freshness window itself; read it rather than hardcoding a `for:` duration |

Treat these as the authority on "is the API overloaded right now" and this
exporter's `agent_psi_inference_stalled_*` as the authority on "how much of
the fleet's active time is going to waiting" — they answer different
questions and neither substitutes for the other. Note in particular that
`claude_watch_api_retry_suppressions_total` (which predates the gauges above)
answers NEITHER: it counts cycles where the daemon suppressed an interrupt,
so it stops advancing once suppression gives up mid-storm at
`[api_retry] max_stuck_secs`.

## What it emits (`/metrics`, default port 9104)

Headline — fleet & per-session-subtree pressure over 10s / 60s / 300s sliding
windows:

| metric | labels | meaning |
|---|---|---|
| `agent_psi_inference_some` | `scope`, `window`, `model` | fraction of the window ≥1 agent in scope was blocked on inference |
| `agent_psi_inference_full` | `scope`, `window`, `model` | fraction ALL active agents were blocked on inference at once — the money metric (API/rate-limit bound) |
| `agent_psi_inference_stalled_some` / `agent_psi_inference_stalled_full` | `scope`, `window`, `model` | the **stalled** subset of the above: inference gaps whose output-token throughput fell below the stall floor (429 back-off / network / TTFT / queueing, not generation), gaps that ended in an API error, and in-flight turns that have been silent past the API-stall threshold (a client parked in retry back-off — see below). `stalled_full` near 1 = the fleet is rate-limit bound, disentangled from "everyone generating hard" |
| `agent_psi_tool_some` / `agent_psi_tool_full` | `scope`, `window`, `model` | same, for tool |
| `agent_psi_overhead_some` / `agent_psi_overhead_full` | `scope`, `window`, `model` | same, for overhead — the loop's own between-turn bookkeeping. Not a stall, but the remainder of active time: with inference, tool and overhead all emitted a fleet panel accounts for every active second. Scope-level counterpart of `agent_duty_ratio{category="overhead"}` |
| `agent_psi_mean_agents` | `scope`, `window`, `model`, `state` | **mean NUMBER OF AGENTS** in `state` over the window (agent-seconds ÷ window). The occupancy series — see [Pressure vs occupancy](#pressure-vs-occupancy-full-is-not-how-busy-is-the-fleet) |
| `agent_psi_api_errors` | `scope`, `window`, `model`, `kind` | API-error transcript entries in the window, per model and cause — see [Per-model upstream impact](#per-model-upstream-impact) |
| `agent_psi_stale_agents` | `scope`, `model` | agents whose CURRENT state is **unobservable** (silent past `AGENT_PSI_STALE_AFTER_SECONDS` while last seen working) |
| `agent_psi_scope_agents` | `scope`, `model` | live agents contributing to each (scope, model) line |
| `agent_psi_live_agents` | — | **sub-agents actually still running** this scrape (main loop excluded) — transcript not ended in a completed final turn, so a finished agent drops immediately while a mid-tool-wait agent stays counted |

`scope` is `fleet` (**sub-agents only** — the main loop is excluded), `main`
(the main loop / dispatcher on its own — its profile is idle-heavy and unlike a
worker, so it is reported side-by-side rather than blended in), or
`session:<8-char id>` (a main loop + its live sub-agents — the subtree).
`window` is `10` / `60` / `300`. `model` is `all` for the cross-model aggregate,
or a model family (`opus` / `sonnet` / `haiku` / `fable` / …) for the per-model
breakdown, emitted on the `fleet` scope AND on `main` (the dispatcher usually
runs a different model from the workers, and "which models did upstream hit" is
unanswerable if the main loop's model only ever lands in `model="all"`) — so a
single model's rate-limiting reads off
`agent_psi_inference_full{scope="fleet",model="opus"}`.

### Pressure vs occupancy: `full` is not "how busy is the fleet"

`some`/`full` are PSI questions and both normalize over the **active** agents,
which makes `full` a *unanimity* signal. It is the right metric for "is the
whole scope blocked on one thing at once" and the wrong one for fleet
composition, in two opposite ways — both measured on this fleet:

* **one** active worker, tool-bound → `tool_full` = 1.0. Arithmetically correct,
  but it renders as a saturated fleet when the honest statement is "one agent,
  in a tool".
* **four** active workers in four different states → every `*_full` collapses
  toward 0 even though the fleet is 100% busy, because no category holds all of
  them. A stack of `*_full` series is therefore anti-correlated with busyness
  exactly at peak concurrency.

`agent_psi_mean_agents` is the occupancy counterpart and has **no denominator**:
one busy agent reads `1.0`, four read `4.0`, and summing over `state` gives the
mean number of agents present. `state` is one of `inference` (productive),
`inference_stalled`, `tool`, `overhead`, `idle`, `waiting_human`,
`unobservable` — mutually exclusive, so they stack directly with no
`clamp_min` subtraction. `idle` and `waiting_human` are first-class on purpose:
a quiet fleet must read as quiet, and the main loop is idle **by design** (it
parks on event holds between dispatches), so a busy-only view of it has no
honest denominator.

Use `*_full` for "everyone is stuck on the same thing"; use `mean_agents` for
"what is the fleet doing".

### `unobservable`: the third state

Every state is inferred from transcript writes, so a host that **goes away**
mid-run (a laptop lid closing, a suspended VM, a killed client) freezes the
transcript with the last observed state still "busy". Read literally that is
tool time forever, until the file-mtime live window drops the agent and it
silently vanishes — neither "perpetually busy" nor "quietly idle" is true.

So a trailing **active** interval is split at `AGENT_PSI_STALE_AFTER_SECONDS`
of silence: the first part keeps its category, and the remainder keeps the
*same* category with `observed=False`. Splitting rather than re-categorizing is
deliberate — every category-based metric (`agent_duty_seconds`, all the
some/full series) sees identical coverage, so **nothing about the existing
metrics moves** — while `agent_psi_mean_agents` reports the remainder as
`state="unobservable"` and `agent_psi_stale_agents` counts the agents in it.

Note what the flag claims: a genuinely long blocking tool and a frozen host are
**indistinguishable** from a transcript, and `unobservable` is exactly that
admission, not an assertion the agent is gone. An **idle** tail is never split —
silence is what idle looks like, so it confirms the state rather than
undermining it, and a parked dispatcher never decays into `unobservable`.

Horizon: past `AGENT_PSI_LIVE_WINDOW_SECONDS` the transcript leaves the live set
entirely and stops being counted anywhere. A long suspension therefore shows up
as `unobservable` first and then drops out — the same live-window edge that
makes a long retry storm invisible here (see above).

### Per-model upstream impact

`agent_psi_api_errors{scope,window,model,kind}` counts the synthetic
`isApiErrorMessage` entries Claude Code writes when a request finally fails.
Two dimensions, both load-bearing:

* **`model`** — model families are provisioned and rate-limited independently,
  so the actionable statement is "opus took N capacity errors this window while
  fable took none". An agent's model is fixed for its lifetime, so each failure
  attributes cleanly. This is what makes model-routing policy (opus vs sonnet)
  measurable rather than a hunch.
* **`kind`** — one "api errors" number would be useless, because the causes map
  to completely different knobs:

| `kind` | matches | knob |
|---|---|---|
| `capacity` | 529 / 5xx / overloaded / server error | which model, when, retry budget |
| `rate_limit` | 429 / quota / rate limit | our concurrency and fleet size |
| `network` | connection error / reset / timeout | ours to chase, not the provider's |
| `context_overflow` | prompt is too long / context window | context management — self-inflicted, and the direct outcome of a routing choice, so it must never read as provider trouble |
| `refusal` | safeguards / content filtering / AUP | not a performance signal at all |
| `auth` | 401 / login expired / OAuth | our credentials |
| `other` | recognised as an API error, cause unmatched | a **visible** catch-all, never a silent fold into `capacity` |

Causes that are *ours* (auth, oversized context, a refusal) are matched **before**
the provider's, so a message carrying an incidental status code
("Please run /login · API Error: 401 OAuth access token has expired") is
attributed to the cause that needs acting on rather than inflating `capacity`.

The class set is deliberately provider-agnostic: the same (model × kind) schema
applies to a Bedrock- or gateway-fronted fleet whose failure mix is different,
so panels built on it transfer without a schema change.

These are counts of **observed events** only. The task-level cost of a retry —
an agent dying and being respawned, work redone — needs cross-agent semantics
this layer does not have and is deliberately not modelled here; that belongs to
the work-queue layer.

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
ceiling on silent time attributable to an API stall),
`AGENT_PSI_STALE_AFTER_SECONDS` (default 300 — silence on an *active* trailing
interval past which the state is reported `unobservable` instead of asserted).

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
- Tool-kind labels (compute-tool vs wait-tool), refined from the live series.
- A deeper **idle taxonomy**. `idle` and `waiting_human` are now distinct
  states, but "parked on an event hold", "parked after returning a result" and
  "gone quiet for no visible reason" all land in `idle`: the transcript records
  a returned `end_turn` and then nothing, and *why* the loop is parked is not in
  it. Separating them needs a signal from the daemon's own view of the session
  (what it is waiting on), which is a different data source, not a classifier
  refinement.
- **Time**-in-state for API failures, not just event counts. `api_errors`
  answers "how often", and `mean_agents{state="inference_stalled"}` answers "how
  many agents were stalled", but attributing a specific stalled stretch to a
  specific error class needs the request/retry timeline — an SDK/HTTP-layer
  signal, per the last bullet.
- Finer **intra-turn** stall detection. An in-flight turn is now flagged once it
  has been silent past the API-stall threshold, but that is still a blunt
  duration test: time-to-first-token, inter-delta gaps and explicit 429
  `Retry-After` values need instrumentation at the SDK / HTTP streaming layer
  (timestamp SSE deltas per request, capture 429 events), which the
  turn-granular transcript does not carry.
