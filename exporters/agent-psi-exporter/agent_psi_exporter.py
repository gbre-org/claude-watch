#!/usr/bin/env python3
"""Prometheus exporter for agent-PSI — pressure-stall metrics over a Claude
Code agent fleet.

On every scrape it discovers the live session + sub-agent transcripts under
``CLAUDE_PROJECTS_DIR``, reconstructs each agent's inference / tool / idle /
waiting_human / overhead intervals, and emits:

  HEADLINE — fleet & per-session-subtree pressure (some/full) over 10s / 60s /
  300s sliding windows. Fleet ``full`` on inference is the money metric: every
  live agent stalled on the model at once => API / rate-limit bound, more
  parallelism buys nothing.

  BYPRODUCT — per-agent (and main-loop) duty-cycle: the time-share of each
  category, a cheap fall-out of the same parser.

The concept, categories, scopes, and the (deliberately rough, phase-1)
classifier live in ``agent_psi.py`` next to this file — read its module
docstring first. This file is only the Prometheus plumbing, and mirrors the
sibling exporters in ../work-queue-exporter and ../claude-events-exporter:
stdlib HTTP server, one ``CollectorRegistry``, gauges re-cleared and refilled
per scrape.

Metrics
-------
  - agent_psi_inference_some{scope,window,model}   gauge  [HEADLINE]
  - agent_psi_inference_full{scope,window,model}   gauge  [HEADLINE]
  - agent_psi_tool_some{scope,window,model}        gauge  [HEADLINE]
  - agent_psi_tool_full{scope,window,model}        gauge  [HEADLINE]
        Fraction of the trailing ``window`` seconds in which, for the set of
        agents named by ``scope``, >=1 agent (some) / every active agent
        (full) was blocked on inference / tool. ``scope`` is "fleet"
  - agent_psi_inference_stalled_some{scope,window,model} gauge [HEADLINE]
  - agent_psi_inference_stalled_full{scope,window,model} gauge [HEADLINE]
        The STALLED slice of inference pressure: same some/full semantics and
        the same scope/window/model labels as agent_psi_inference_{some,full},
        but restricted to inference gaps whose output-token throughput fell
        below AGENT_PSI_STALLED_TOKENS_PER_SEC (429 back-off / network / TTFT /
        queueing rather than generation). It is a SUBSET of inference_* — the
        latter stays the total. Fleet ``stalled_full`` near 1.0 means every
        live worker is rate-limited at once, disentangled from "everyone
        generating hard" (which inference_full alone conflated). ``scope`` is
        "fleet"
        (sub-agents only — the main loop is EXCLUDED), "main" (the main loop /
        dispatcher on its own), or "session:<8-char id>" (a main loop + its
        live sub-agents). ``window`` is "10" / "60" / "300". ``model`` is "all"
        for the cross-model aggregate, or a model family (opus / sonnet / ...)
        for the per-model breakdown emitted on the "fleet" scope so a single
        model's rate-limiting isolates as e.g.
        agent_psi_inference_full{scope="fleet",model="opus"}.
  - agent_psi_scope_agents{scope,model}      gauge
        Count of live agents contributing to each (scope, model) pressure line.
  - agent_psi_live_agents                    gauge
        SUB-AGENTS actually still running this scrape (the main loop is excluded
        — it is a dispatcher, not a worker). "Running" = the transcript does not
        end in a completed final turn (an assistant end_turn with no pending
        tool); a finished agent drops immediately instead of lingering for the
        file-mtime live window, and an agent mid-turn / mid-tool-wait (open
        trailing interval) stays counted.
  - agent_duty_ratio{agent_id,category}      gauge
        Per-agent duty-cycle: share of ACTIVE time (total − idle −
        waiting_human) for category in {inference,tool,overhead}. For a serial
        agent this IS its some==full pressure. agent_id is the sub-agent id or
        "main_loop:<8-char session id>".
  - agent_duty_seconds{agent_id,category}    gauge
        Raw seconds per category (all five, including idle / waiting_human)
        over the transcript window — unambiguous building block for Grafana.
  - agent_psi_scrape_errors_total            counter
  - agent_psi_exporter_build_info{commit,version,source} gauge (always 1)
"""

import logging
import os
import time
from http.server import BaseHTTPRequestHandler, HTTPServer

from prometheus_client import (
    CollectorRegistry,
    Counter,
    Gauge,
    generate_latest,
    CONTENT_TYPE_LATEST,
)

import agent_psi

logging.basicConfig(level=logging.INFO, format="%(asctime)s %(levelname)s %(message)s")
log = logging.getLogger("agent-psi-exporter")

PORT = int(os.environ.get("PORT", "9104"))
# Root of the Claude Code per-project transcript store. Host default is
# ~/.claude/projects; a container bind-mounts it read-only.
PROJECTS_DIR = os.environ.get(
    "CLAUDE_PROJECTS_DIR",
    os.path.join(os.path.expanduser("~"), ".claude", "projects"),
)
MAX_GAP_SECONDS = float(
    os.environ.get("AGENT_PSI_MAX_GAP_SECONDS", str(agent_psi.DEFAULT_MAX_GAP_SECONDS))
)
LIVE_WINDOW_SECONDS = float(
    os.environ.get(
        "AGENT_PSI_LIVE_WINDOW_SECONDS", str(agent_psi.DEFAULT_LIVE_WINDOW_SECONDS)
    )
)
WINDOWS = tuple(
    int(w)
    for w in os.environ.get(
        "AGENT_PSI_WINDOWS", ",".join(str(w) for w in agent_psi.DEFAULT_WINDOWS)
    ).split(",")
    if w.strip()
)
STALLED_TOKENS_PER_SEC = float(
    os.environ.get(
        "AGENT_PSI_STALLED_TOKENS_PER_SEC",
        str(agent_psi.DEFAULT_STALLED_TOKENS_PER_SEC),
    )
)
MIN_STALL_GAP_SECONDS = float(
    os.environ.get(
        "AGENT_PSI_MIN_STALL_GAP_SECONDS",
        str(agent_psi.DEFAULT_MIN_STALL_GAP_SECONDS),
    )
)

EXPORTER_COMMIT = os.environ.get("AGENT_PSI_EXPORTER_COMMIT", "").strip() or "unknown"
EXPORTER_VERSION = os.environ.get("AGENT_PSI_EXPORTER_VERSION", "").strip() or "0.0.0"
EXPORTER_SOURCE = os.environ.get("AGENT_PSI_EXPORTER_SOURCE", "").strip() or "host"

REG = CollectorRegistry()

# One gauge per (category, kind) — names match the design doc's
# agent_psi_{inference,tool}_{some,full} shape, with scope+window as labels.
_PRESSURE_GAUGES = {}
for _cat in agent_psi.STALL_CATEGORIES:
    for _kind in ("some", "full"):
        _PRESSURE_GAUGES[(_cat, _kind)] = Gauge(
            f"agent_psi_{_cat}_{_kind}",
            (
                f"Fraction of the trailing `window` seconds in which "
                f"{'>=1 agent' if _kind == 'some' else 'every active agent'} in "
                f"`scope` (restricted to `model`, or model=all) was blocked on "
                f"{_cat}."
            ),
            ["scope", "window", "model"],
            registry=REG,
        )

# Stalled-inference pressure — the low-throughput subset of inference_*, same
# scope/window/model label scheme so the dashboard consumes it identically.
_STALLED_GAUGES = {}
for _kind in ("some", "full"):
    _STALLED_GAUGES[_kind] = Gauge(
        f"agent_psi_inference_stalled_{_kind}",
        (
            f"Fraction of the trailing `window` seconds in which "
            f"{'>=1 agent' if _kind == 'some' else 'every active agent'} in "
            f"`scope` (restricted to `model`, or model=all) was in a STALLED "
            f"inference gap (output-token throughput below the stall floor: "
            f"429 back-off / network / TTFT / queueing). Subset of "
            f"agent_psi_inference_{_kind}."
        ),
        ["scope", "window", "model"],
        registry=REG,
    )

g_scope_agents = Gauge(
    "agent_psi_scope_agents",
    "Count of live agents contributing to each (scope, model) pressure line.",
    ["scope", "model"],
    registry=REG,
)
g_live_agents = Gauge(
    "agent_psi_live_agents",
    (
        "SUB-AGENTS actually still running this scrape — transcript not ended "
        "in a completed final turn (main loop excluded). A finished agent drops "
        "immediately; a mid-tool-wait agent stays counted."
    ),
    registry=REG,
)
g_duty_ratio = Gauge(
    "agent_duty_ratio",
    (
        "Per-agent duty-cycle: share of ACTIVE time (total - idle - "
        "waiting_human) spent on category in {inference,tool,overhead}. For a "
        "serial agent this equals its some==full pressure."
    ),
    ["agent_id", "category"],
    registry=REG,
)
g_duty_seconds = Gauge(
    "agent_duty_seconds",
    (
        "Raw seconds per category (all five, incl. idle/waiting_human) over "
        "the agent's transcript window."
    ),
    ["agent_id", "category"],
    registry=REG,
)
c_scrape_errors = Counter(
    "agent_psi_scrape_errors",
    "Number of scrapes that failed to read the projects dir.",
    registry=REG,
)
g_build_info = Gauge(
    "agent_psi_exporter_build_info",
    (
        "Build identity of the running agent-psi-exporter; always 1, the "
        "labels carry the payload. commit=\"unknown\" means nothing stamped "
        "the build."
    ),
    ["commit", "version", "source"],
    registry=REG,
)
g_build_info.labels(
    commit=EXPORTER_COMMIT, version=EXPORTER_VERSION, source=EXPORTER_SOURCE
).set(1)


def _emit_pressure(scope, agent_intervals, now, model="all"):
    """Emit some/full for every stall category and window for one scope+model,
    plus the stalled-inference some/full subset."""
    for window in WINDOWS:
        ratios = agent_psi.compute_pressure(agent_intervals, now - window, now)
        for (cat, kind), value in ratios.items():
            _PRESSURE_GAUGES[(cat, kind)].labels(
                scope=scope, window=str(window), model=model
            ).set(value)
        stalled = agent_psi.compute_stalled_inference_pressure(
            agent_intervals, now - window, now
        )
        for kind, value in stalled.items():
            _STALLED_GAUGES[kind].labels(
                scope=scope, window=str(window), model=model
            ).set(value)


def collect():
    """Re-scan transcripts and refresh all metrics."""
    now = time.time()
    try:
        transcripts = agent_psi.collect_live_transcripts(
            PROJECTS_DIR, now,
            max_gap=MAX_GAP_SECONDS, live_window=LIVE_WINDOW_SECONDS,
            stalled_tps=STALLED_TOKENS_PER_SEC,
            min_stall_gap=MIN_STALL_GAP_SECONDS,
        )
    except Exception as e:  # pragma: no cover - defensive
        log.error("Failed to read %s: %s", PROJECTS_DIR, e)
        c_scrape_errors.inc()
        return

    for gauge in _PRESSURE_GAUGES.values():
        gauge.clear()
    for gauge in _STALLED_GAUGES.values():
        gauge.clear()
    g_scope_agents.clear()
    g_duty_ratio.clear()
    g_duty_seconds.clear()

    # The main loop is a dispatcher (mostly parked between turns); its profile
    # is nothing like a worker sub-agent, so it is split out of the fleet and
    # the live-agent count, and reported under its own scope side-by-side.
    sub_transcripts = [t for t in transcripts if not t.is_main_loop]
    main_transcripts = [t for t in transcripts if t.is_main_loop]

    # live_agents = sub-agents ACTUALLY still running now (transcript not ended
    # in a completed final turn), not merely file-recent. A finished sub-agent
    # drops immediately instead of lingering for the whole file-mtime live
    # window; a mid-tool-wait agent (open trailing interval) stays counted.
    # Pressure/scope membership still spans every file-recent sub-agent, since
    # one that finished mid-window legitimately contributed to that window.
    g_live_agents.set(sum(1 for t in sub_transcripts if t.running))

    # Per-agent duty cycle (byproduct) — every live transcript, main + workers.
    for t in transcripts:
        secs, total, active, ratios = agent_psi.duty_cycle(t.intervals)
        for cat, value in secs.items():
            g_duty_seconds.labels(agent_id=t.agent_id, category=cat).set(value)
        for cat, value in ratios.items():
            g_duty_ratio.labels(agent_id=t.agent_id, category=cat).set(value)

    # Fleet pressure (headline) — SUB-AGENTS ONLY, main loop excluded.
    fleet = {t.agent_id: t.intervals for t in sub_transcripts}
    _emit_pressure("fleet", fleet, now, model="all")
    g_scope_agents.labels(scope="fleet", model="all").set(len(fleet))

    # Per-model fleet pressure (headline) — the same some/full math restricted
    # to the workers on each model family, so per-model rate-limiting isolates.
    by_model = {}
    for t in sub_transcripts:
        by_model.setdefault(t.model or agent_psi.UNKNOWN_MODEL, {})[
            t.agent_id
        ] = t.intervals
    for model, members in by_model.items():
        _emit_pressure("fleet", members, now, model=model)
        g_scope_agents.labels(scope="fleet", model=model).set(len(members))

    # Main-loop pressure (headline) — the dispatcher on its own scope/line.
    main = {t.agent_id: t.intervals for t in main_transcripts}
    _emit_pressure("main", main, now, model="all")
    g_scope_agents.labels(scope="main", model="all").set(len(main))

    # Per-session subtree pressure (headline) — main loop + its live workers.
    by_session = {}
    for t in transcripts:
        by_session.setdefault(t.session_id or "unknown", {})[t.agent_id] = t.intervals
    for session_id, members in by_session.items():
        scope = "session:" + (session_id or "unknown")[:8]
        _emit_pressure(scope, members, now, model="all")
        g_scope_agents.labels(scope=scope, model="all").set(len(members))


class MetricsHandler(BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path.split("?", 1)[0] != "/metrics":
            self.send_response(404)
            self.end_headers()
            self.wfile.write(b"not found\n")
            return
        collect()
        body = generate_latest(REG)
        self.send_response(200)
        self.send_header("Content-Type", CONTENT_TYPE_LATEST)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, fmt, *args):
        log.debug(fmt, *args)


def main():
    log.info(
        "Starting agent-psi exporter on :%d (projects=%s, windows=%s)",
        PORT, PROJECTS_DIR, WINDOWS,
    )
    log.info(
        "Build: commit=%s version=%s source=%s",
        EXPORTER_COMMIT, EXPORTER_VERSION, EXPORTER_SOURCE,
    )
    log.info(
        "Stall split: <%.1f tok/s over a >=%.1fs inference gap => stalled",
        STALLED_TOKENS_PER_SEC, MIN_STALL_GAP_SECONDS,
    )
    collect()
    HTTPServer(("0.0.0.0", PORT), MetricsHandler).serve_forever()


if __name__ == "__main__":
    main()
