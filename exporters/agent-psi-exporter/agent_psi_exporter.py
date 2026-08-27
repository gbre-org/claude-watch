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
  - agent_psi_inference_some{scope,window}   gauge  [HEADLINE]
  - agent_psi_inference_full{scope,window}   gauge  [HEADLINE]
  - agent_psi_tool_some{scope,window}        gauge  [HEADLINE]
  - agent_psi_tool_full{scope,window}        gauge  [HEADLINE]
        Fraction of the trailing ``window`` seconds in which, for the set of
        agents named by ``scope``, >=1 agent (some) / every active agent
        (full) was blocked on inference / tool. ``scope`` is "fleet" (all live
        transcripts) or "session:<8-char id>" (a main loop + its live
        sub-agents). ``window`` is "10" / "60" / "300".
  - agent_psi_scope_agents{scope}            gauge
        Count of live agents contributing to each pressure scope.
  - agent_psi_live_agents                    gauge
        Total live transcripts seen this scrape.
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
                f"`scope` was blocked on {_cat}."
            ),
            ["scope", "window"],
            registry=REG,
        )

g_scope_agents = Gauge(
    "agent_psi_scope_agents",
    "Count of live agents contributing to each pressure scope.",
    ["scope"],
    registry=REG,
)
g_live_agents = Gauge(
    "agent_psi_live_agents",
    "Total live transcripts seen this scrape.",
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


def _emit_pressure(scope, agent_intervals, now):
    """Emit some/full for every stall category and window for one scope."""
    for window in WINDOWS:
        ratios = agent_psi.compute_pressure(agent_intervals, now - window, now)
        for (cat, kind), value in ratios.items():
            _PRESSURE_GAUGES[(cat, kind)].labels(
                scope=scope, window=str(window)
            ).set(value)


def collect():
    """Re-scan transcripts and refresh all metrics."""
    now = time.time()
    try:
        transcripts = agent_psi.collect_live_transcripts(
            PROJECTS_DIR, now,
            max_gap=MAX_GAP_SECONDS, live_window=LIVE_WINDOW_SECONDS,
        )
    except Exception as e:  # pragma: no cover - defensive
        log.error("Failed to read %s: %s", PROJECTS_DIR, e)
        c_scrape_errors.inc()
        return

    for gauge in _PRESSURE_GAUGES.values():
        gauge.clear()
    g_scope_agents.clear()
    g_duty_ratio.clear()
    g_duty_seconds.clear()

    g_live_agents.set(len(transcripts))

    # Per-agent duty cycle (byproduct).
    for t in transcripts:
        secs, total, active, ratios = agent_psi.duty_cycle(t.intervals)
        for cat, value in secs.items():
            g_duty_seconds.labels(agent_id=t.agent_id, category=cat).set(value)
        for cat, value in ratios.items():
            g_duty_ratio.labels(agent_id=t.agent_id, category=cat).set(value)

    # Fleet pressure (headline).
    fleet = {t.agent_id: t.intervals for t in transcripts}
    _emit_pressure("fleet", fleet, now)
    g_scope_agents.labels(scope="fleet").set(len(fleet))

    # Per-session subtree pressure (headline).
    by_session = {}
    for t in transcripts:
        by_session.setdefault(t.session_id or "unknown", {})[t.agent_id] = t.intervals
    for session_id, members in by_session.items():
        scope = "session:" + (session_id or "unknown")[:8]
        _emit_pressure(scope, members, now)
        g_scope_agents.labels(scope=scope).set(len(members))


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
    collect()
    HTTPServer(("0.0.0.0", PORT), MetricsHandler).serve_forever()


if __name__ == "__main__":
    main()
