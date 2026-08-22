#!/usr/bin/env python3
"""Prometheus exporter for the claude-events file-based event bus.

Reads ~/claude-events/ on every scrape (cheap — usually 0 to a few small
JSON files) and exposes metrics at /metrics on PORT.

Producers (cron jobs, alertmanager webhooks, session-task queue, torrent
done-script, claude-watch alerts) drop JSON files into the queue dir; the
`claude-event-watch` watcher reads + removes them, surfacing each event to
the main loop. This exporter gives us visibility into:

  - Events emitted (per source/tag) — derived from filename timestamps so
    we don't double-count across scrapes
  - Current backlog depth (number of files waiting to be consumed)
  - Age of the oldest unconsumed event (catches a wedged main loop /
    dead claude-event-watch watcher)

Cardinality is bounded: source/tag values not in the known-good set are
collapsed into "other".

Metrics:
  - claude_events_total{source,tag}              counter (events ever seen by exporter)
  - claude_events_queue_depth                    gauge  (files in queue dir right now)
  - claude_events_age_seconds                    gauge  (age of oldest queued event)
  - claude_events_processed_total{outcome}       counter (consumed = total_seen - depth)
  - claude_events_dir_last_modified              gauge  (mtime of queue dir)
  - claude_events_scrape_errors_total            counter
  - claude_events_last_heartbeat_timestamp_seconds gauge (epoch of the most
    recent heartbeat-tick the CONSUMER observed -- backs the dashboard's
    "Last Ack Age" stat. See "Heartbeat observation" below.)
  - claude_events_heartbeat_marker_present       gauge  (1 when the durable
    consumer-written marker was readable + parseable on this scrape)

Heartbeat observation
---------------------
`claude_events_last_heartbeat_timestamp_seconds` means "epoch of the most
recent claude-watch heartbeat-tick that the MAIN LOOP's consumer actually
observed on the bus". The dashboard turns it into an age.

It cannot be derived by scanning the queue dir. claude-event-watch drains a
tick within SECONDS of it landing, ticks are ~15 minutes apart and the scrape
interval is 30s, so a scrape essentially never catches one on disk -- the
gauge sat at its cold-start 0 forever and the dashboard tile rendered nothing.

So claude-event-watch writes a durable marker when it surfaces a tick:

    $CLAUDE_EVENT_QUEUE/.state/last-heartbeat.json
      {"tag": "heartbeat-tick", "event_timestamp": <epoch of emit>,
       "processed_at": <epoch of consumption>, "event_file": "<name>"}

written atomically (temp file + rename in the same dir) so a scrape landing
mid-write reads the old file, never a partial one. It lives UNDER the queue
dir, which this exporter already bind-mounts, so no deploy change is needed.

Precedence, deliberately NOT a max():

  - Marker readable  -> the marker's event_timestamp IS the gauge.
  - Marker absent or unparseable -> fall back to the legacy in-dir scan
    (the pre-marker behavior), which stays 0 on a host whose consumer does
    not write markers yet.

Taking max(marker, in-dir scan) would defeat the metric's purpose: if
claude-event-watch is dead or wedged, ticks PILE UP in the queue dir, and a
max() would keep advancing the gauge off those unconsumed files -- reporting
a healthy ack age precisely when nothing is being acked. Queue backlog is
already covered by claude_events_queue_depth / claude_events_age_seconds.

Both sources are monotonic within a process lifetime (a late-arriving older
tick never rewinds the gauge).
"""

import json
import logging
import os
import re
import time
from http.server import BaseHTTPRequestHandler, HTTPServer

from prometheus_client import (
    CollectorRegistry,
    Counter,
    Gauge,
    generate_latest,
    CONTENT_TYPE_LATEST,
)

logging.basicConfig(level=logging.INFO, format="%(asctime)s %(levelname)s %(message)s")
log = logging.getLogger("claude-events-exporter")

PORT = int(os.environ.get("PORT", "9103"))
EVENTS_DIR = os.environ.get("CLAUDE_EVENTS_DIR", "/events")

# Durable consumer-written heartbeat marker (see "Heartbeat observation" in
# the module docstring). Default sits inside EVENTS_DIR, so the bind-mount
# this exporter already has covers it -- no new mount, no new env var needed
# in the compose stack. Overridable for tests / unusual layouts.
HEARTBEAT_MARKER = os.environ.get(
    "CLAUDE_EVENTS_HEARTBEAT_MARKER", os.path.join(EVENTS_DIR, ".state", "last-heartbeat.json")
)

# Known-good label values; anything outside these collapses to "other" to
# keep cardinality bounded (no per-event-id labels, no unbounded user input).
KNOWN_SOURCES = {
    "cron",
    "alertmanager",
    "queue",
    "torrent",
    "security",
    "manual",
    "claude-watch",
}

# Known-good tag prefixes — full tag must equal or start-with one of these
# (with a separator) to map to itself. Otherwise → "other".
KNOWN_TAGS = {
    "tv-check",
    "security-check",
    "security-scan",
    "queue-added",
    "queue-running",
    "queue-done",
    "queue-abandoned",
    "queue-idle-pending",
    "claude-watch-alert",
    "torrent-completed",
}

REG = CollectorRegistry()

c_events_total = Counter(
    "claude_events",
    "Total claude-events ever observed by this exporter, by source+tag",
    ["source", "tag"],
    registry=REG,
)
g_queue_depth = Gauge(
    "claude_events_queue_depth",
    "Current number of unconsumed event JSON files in the queue dir",
    registry=REG,
)
g_oldest_age = Gauge(
    "claude_events_age_seconds",
    "Age in seconds of the oldest unconsumed event (0 if queue empty)",
    registry=REG,
)
c_processed_total = Counter(
    "claude_events_processed",
    "Events that have been consumed by claude-event-watch (derived: total_seen - current_depth)",
    ["outcome"],
    registry=REG,
)
g_dir_mtime = Gauge(
    "claude_events_dir_last_modified",
    "Unix mtime of the events queue directory",
    registry=REG,
)
c_scrape_errors = Counter(
    "claude_events_scrape_errors",
    "Number of failed reads of the events queue directory",
    registry=REG,
)
g_last_heartbeat_ts = Gauge(
    "claude_events_last_heartbeat_timestamp_seconds",
    "Epoch (emit time) of the most recent claude-watch heartbeat-tick that "
    "the bus CONSUMER observed. Read from the durable marker "
    "claude-event-watch writes when it surfaces a tick; falls back to the "
    "legacy in-dir scan when no marker exists. Monotonic for the life of "
    "this process. Stays 0 until the first heartbeat-tick is observed, so a "
    "cold-start exporter reports an implausibly large age rather than a "
    "fake-fresh one -- the dashboard maps that band to 'n/a'.",
    registry=REG,
)
g_heartbeat_marker_present = Gauge(
    "claude_events_heartbeat_marker_present",
    "1 when the consumer-written heartbeat marker was found and parsed on "
    "this scrape, 0 otherwise (absent, unreadable, or malformed). 0 with a "
    "nonzero last_heartbeat timestamp means the gauge is running on the "
    "legacy in-dir-scan fallback.",
    registry=REG,
)

# Filename pattern: <ns_timestamp>_<safe_tag>.json (per claude-event emitter)
FILENAME_RE = re.compile(r"^(?P<ns>\d+)_(?P<tag>[A-Za-z0-9_-]+)\.json$")

# Track which event filenames we've already counted (dedup across scrapes
# that happen before the watcher removes the file). Bounded by file churn —
# the watcher removes files quickly, so this set stays small.
_seen_filenames: set[str] = set()
_total_seen = 0  # cumulative count of distinct events ever seen by this exporter
# Newest heartbeat-tick emit time from each source, monotonic per process.
_marker_heartbeat_ts: float | None = None  # from the consumer-written marker
_scan_heartbeat_ts: float | None = None  # legacy fallback: file seen in the queue dir


def read_heartbeat_marker(path: str | None = None) -> float | None:
    """Return the marker's heartbeat emit timestamp, or None.

    None covers every failure mode identically -- absent, unreadable, not
    JSON, not an object, missing/non-numeric/non-positive event_timestamp --
    because the caller's response to all of them is the same: keep the last
    known value and fall back to the in-dir scan. Never raises.
    """
    try:
        with open(path if path is not None else HEARTBEAT_MARKER, "r") as f:
            data = json.load(f)
        if not isinstance(data, dict):
            return None
        ts = float(data["event_timestamp"])
    except (OSError, ValueError, TypeError, KeyError, json.JSONDecodeError):
        return None
    # NaN/inf would poison the gauge; a non-positive stamp is a placeholder.
    if not (ts > 0) or ts == float("inf"):
        return None
    return ts


def _normalize_source(s: str | None) -> str:
    if not s:
        return "other"
    return s if s in KNOWN_SOURCES else "other"


def _normalize_tag(t: str | None) -> str:
    if not t:
        return "other"
    return t if t in KNOWN_TAGS else "other"


def collect():
    """Re-scan the events dir and refresh metrics. Called on every /metrics scrape."""
    global _total_seen, _marker_heartbeat_ts, _scan_heartbeat_ts
    try:
        st = os.stat(EVENTS_DIR)
        g_dir_mtime.set(st.st_mtime)
        # Use scandir for cheap mtime/name access without an extra stat.
        entries = []
        with os.scandir(EVENTS_DIR) as it:
            for de in it:
                if not de.is_file(follow_symlinks=False):
                    continue
                name = de.name
                if not name.endswith(".json"):
                    continue
                if name.startswith("."):
                    # tmp files written by claude-event during atomic rename
                    continue
                entries.append((name, de))
    except OSError as e:
        log.error("Failed to read %s: %s", EVENTS_DIR, e)
        c_scrape_errors.inc()
        return

    g_queue_depth.set(len(entries))

    now = time.time()
    oldest_age = 0.0

    for name, de in entries:
        # Compute age from filename ns-timestamp when present, else file mtime.
        m = FILENAME_RE.match(name)
        ev_time: float | None = None
        if m:
            try:
                ev_time = int(m.group("ns")) / 1e9
            except (ValueError, OverflowError):
                ev_time = None
        if ev_time is None:
            try:
                ev_time = de.stat().st_mtime
            except OSError:
                ev_time = now
        age = max(0.0, now - ev_time)
        if age > oldest_age:
            oldest_age = age

        # LEGACY FALLBACK ONLY. Catching a heartbeat-tick still on disk is a
        # near-impossibility (the consumer drains within seconds), which is
        # exactly why the marker exists; this branch survives so a host whose
        # claude-event-watch predates the marker still gets a value, and so a
        # tick that IS caught mid-flight is not thrown away. It never
        # overrides the marker -- see the precedence note in the docstring.
        if m and m.group("tag") == "heartbeat-tick":
            if _scan_heartbeat_ts is None or ev_time > _scan_heartbeat_ts:
                _scan_heartbeat_ts = ev_time

        # First-time-seen counter increment + JSON parse for source/tag labels.
        if name not in _seen_filenames:
            _seen_filenames.add(name)
            _total_seen += 1
            source = "other"
            tag = "other"
            try:
                with open(os.path.join(EVENTS_DIR, name), "r") as f:
                    data = json.load(f)
                source = _normalize_source(data.get("source"))
                tag = _normalize_tag(data.get("tag"))
            except (OSError, json.JSONDecodeError) as e:
                # File may have been removed by the watcher mid-scrape; fall
                # back to filename-tag and "other" source.
                if m:
                    tag = _normalize_tag(m.group("tag"))
                log.debug("Could not parse event %s (%s); using fallback labels", name, e)
            c_events_total.labels(source=source, tag=tag).inc()

    g_oldest_age.set(oldest_age)

    # Heartbeat: marker first (authoritative -- it is the consumer's own
    # observation, so it goes stale when the consumer does), in-dir scan only
    # as the pre-marker fallback. Deliberately not a max(); see the docstring.
    marker_ts = read_heartbeat_marker()
    g_heartbeat_marker_present.set(1 if marker_ts is not None else 0)
    if marker_ts is not None and (
        _marker_heartbeat_ts is None or marker_ts > _marker_heartbeat_ts
    ):
        _marker_heartbeat_ts = marker_ts

    heartbeat_ts = (
        _marker_heartbeat_ts if _marker_heartbeat_ts is not None else _scan_heartbeat_ts
    )
    if heartbeat_ts is not None:
        g_last_heartbeat_ts.set(heartbeat_ts)

    # Derive processed count: every event the exporter has ever seen but that
    # is no longer in the queue dir was consumed by claude-event-watch (or
    # otherwise removed). This is monotonically increasing.
    consumed = _total_seen - len(entries)
    if consumed < 0:
        consumed = 0
    current_consumed_value = c_processed_total.labels(outcome="consumed")._value.get()  # type: ignore[attr-defined]
    delta = consumed - current_consumed_value
    if delta > 0:
        c_processed_total.labels(outcome="consumed").inc(delta)


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
    log.info("Starting claude-events exporter on :%d (reading %s)", PORT, EVENTS_DIR)
    # Prime metrics at startup so the first scrape isn't empty.
    collect()
    HTTPServer(("0.0.0.0", PORT), MetricsHandler).serve_forever()


if __name__ == "__main__":
    main()
