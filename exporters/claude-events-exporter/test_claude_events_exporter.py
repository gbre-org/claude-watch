#!/usr/bin/env python3
"""Tests for claude-events-exporter's claude_events_last_heartbeat_timestamp_seconds
gauge (backs the dashboard's "Last Ack Age" stat) and the
claude_events_heartbeat_marker_present gauge beside it.

In-dir-scan scenarios (the legacy fallback path, kept working):

  (1) Fresh exporter, no heartbeat-tick ever seen -> gauge stays 0. The
      dashboard maps the resulting implausible age to "n/a".
  (2) A heartbeat-tick file lands in the queue dir -> gauge picks up its
      filename-encoded ns-timestamp.
  (3) claude-event-watch consumes (deletes) that file before the next
      scrape -> gauge PERSISTS the last-seen value.
  (4) A newer heartbeat-tick arrives -> gauge advances to it.
  (5) A late-arriving OLDER heartbeat-tick does not regress the gauge.
  (6) An unrelated bus event (any other source/tag) does not move it.

Marker scenarios (the durable consumer-written signal, the reason this
metric is observable at all -- a tick lives on the queue for seconds, far
too briefly for a 30s scrape to reliably catch):

  (7)  Marker present, queue empty -> gauge reads the marker's
       event_timestamp, marker_present = 1.
  (8)  No marker -> marker_present = 0, in-dir scan still drives the gauge.
  (9)  Malformed marker (not JSON) -> treated as absent: marker_present = 0,
       fallback value retained, no exception.
  (10) Structurally-wrong markers (not an object / missing field /
       non-numeric / non-positive) -> same as (9).
  (11) WEDGED CONSUMER: a stale marker plus FRESH unconsumed heartbeat-tick
       files piling up in the queue -> the gauge stays stale. This is the
       whole point of not using max(marker, in-dir scan): a dead consumer
       must make "Last Ack Age" grow, not be masked by the emitter still
       emitting.
  (12) Marker monotonicity -> a marker rewritten with an older timestamp
       does not rewind the gauge.
  (13) Marker takes over from the scan fallback once it appears.

Run:  python3 test_claude_events_exporter.py
Exits 0 on success, 1 on first failure with a diagnostic.
"""

import json
import os
import sys
import tempfile
import time
from importlib.util import spec_from_file_location, module_from_spec

HERE = os.path.dirname(os.path.abspath(__file__))


def load_exporter(events_dir):
    """Reload the exporter module under a fresh CLAUDE_EVENTS_DIR so its
    module-level EVENTS_DIR constant (and all _seen_filenames /
    _last_heartbeat_ts state) starts clean for each scenario."""
    saved = os.environ.get("CLAUDE_EVENTS_DIR")
    os.environ["CLAUDE_EVENTS_DIR"] = events_dir
    try:
        spec = spec_from_file_location(
            "claude_events_exporter_under_test",
            os.path.join(HERE, "claude_events_exporter.py"),
        )
        mod = module_from_spec(spec)
        spec.loader.exec_module(mod)
        return mod
    finally:
        if saved is None:
            os.environ.pop("CLAUDE_EVENTS_DIR", None)
        else:
            os.environ["CLAUDE_EVENTS_DIR"] = saved


def write_event(events_dir, tag, *, source="claude-watch", ns_ago=0):
    """Drop a bus event file named like the real emitter:
    <ns_timestamp>_<safe_tag>.json"""
    ts_ns = int(time.time() * 1e9) - int(ns_ago * 1e9)
    path = os.path.join(events_dir, f"{ts_ns}_{tag}.json")
    with open(path, "w") as f:
        json.dump({"source": source, "tag": tag}, f)
    return path


def marker_path(events_dir):
    return os.path.join(events_dir, ".state", "last-heartbeat.json")


def write_marker(events_dir, *, ts=None, ago=0.0, raw=None, payload=None):
    """Write the marker claude-event-watch produces.

    `raw` writes arbitrary bytes (malformed-marker cases); `payload` writes an
    arbitrary JSON value; otherwise a well-formed marker `ago` seconds old (or
    at the absolute `ts` given) is written.
    """
    path = marker_path(events_dir)
    os.makedirs(os.path.dirname(path), exist_ok=True)
    if raw is not None:
        with open(path, "w") as f:
            f.write(raw)
        return path
    if payload is None:
        ev_ts = ts if ts is not None else time.time() - ago
        payload = {
            "tag": "heartbeat-tick",
            "event_timestamp": ev_ts,
            "processed_at": ev_ts + 0.2,
            "event_file": f"{int(ev_ts * 1e9)}_heartbeat-tick.json",
        }
    with open(path, "w") as f:
        json.dump(payload, f)
    return path


def find_sample(mod, metric_name):
    for fam in mod.REG.collect():
        if fam.name != metric_name:
            continue
        for sample in fam.samples:
            if sample.name == metric_name:
                return sample.value
    return None


def run_scenarios():
    failures = []

    def check(name, predicate, msg):
        if predicate:
            print(f"  PASS: {name}")
        else:
            print(f"  FAIL: {name} -- {msg}")
            failures.append((name, msg))

    METRIC = "claude_events_last_heartbeat_timestamp_seconds"

    # ---- Scenario 1: cold start, no events at all.
    print("\nScenario 1: fresh exporter, no heartbeat-tick seen -> gauge stays 0")
    tmpdir = tempfile.mkdtemp(prefix="cee-test-")
    mod = load_exporter(tmpdir)
    mod.collect()
    v = find_sample(mod, METRIC)
    check("S1 gauge is 0 (cold start)", v == 0.0, f"got {v!r}")

    # ---- Scenario 2: a heartbeat-tick lands in the queue.
    print("\nScenario 2: heartbeat-tick file present -> gauge picks up its timestamp")
    tmpdir = tempfile.mkdtemp(prefix="cee-test-")
    mod = load_exporter(tmpdir)
    write_event(tmpdir, "heartbeat-tick", ns_ago=30)
    mod.collect()
    v = find_sample(mod, METRIC)
    check("S2 gauge set", v is not None and v > 0, f"got {v!r}")
    check("S2 age ~30s", v is not None and abs(time.time() - v - 30) < 5,
          f"age computed as {time.time() - (v or 0)!r}")

    # ---- Scenario 3: watcher consumes (deletes) the file -- value persists.
    print("\nScenario 3: file consumed before next scrape -> gauge PERSISTS")
    for name in os.listdir(tmpdir):
        os.remove(os.path.join(tmpdir, name))
    mod.collect()
    v2 = find_sample(mod, METRIC)
    check("S3 gauge unchanged after file removed", v2 == v, f"got {v2!r}, expected {v!r}")

    # ---- Scenario 4: a newer heartbeat-tick advances the gauge.
    print("\nScenario 4: newer heartbeat-tick -> gauge advances")
    write_event(tmpdir, "heartbeat-tick", ns_ago=0)
    mod.collect()
    v3 = find_sample(mod, METRIC)
    check("S4 gauge advanced", v3 is not None and v3 > v2, f"got {v3!r} vs prior {v2!r}")

    # ---- Scenario 5: an older heartbeat-tick never regresses the gauge.
    print("\nScenario 5: older heartbeat-tick arriving late -> gauge does not regress")
    write_event(tmpdir, "heartbeat-tick", ns_ago=600)
    mod.collect()
    v4 = find_sample(mod, METRIC)
    check("S5 gauge did not regress", v4 == v3, f"got {v4!r}, expected {v3!r}")

    # ---- Scenario 6: unrelated bus events don't move the gauge.
    print("\nScenario 6: unrelated event (queue-added) -> gauge untouched")
    for name in os.listdir(tmpdir):
        os.remove(os.path.join(tmpdir, name))
    write_event(tmpdir, "queue-added", source="queue")
    mod.collect()
    v5 = find_sample(mod, METRIC)
    check("S6 gauge unaffected by unrelated event", v5 == v3, f"got {v5!r}, expected {v3!r}")

    MARKER = "claude_events_heartbeat_marker_present"

    # ---- Scenario 7: marker present, queue empty -> marker drives the gauge.
    print("\nScenario 7: marker present, queue empty -> gauge reads the marker")
    tmpdir = tempfile.mkdtemp(prefix="cee-test-")
    marker_ts = time.time() - 45
    write_marker(tmpdir, ts=marker_ts)
    mod = load_exporter(tmpdir)
    mod.collect()
    v = find_sample(mod, METRIC)
    check("S7 gauge == marker event_timestamp",
          v is not None and abs(v - marker_ts) < 0.01, f"got {v!r}, want {marker_ts!r}")
    check("S7 marker_present == 1", find_sample(mod, MARKER) == 1.0,
          f"got {find_sample(mod, MARKER)!r}")
    check("S7 marker file is not counted as a bus event",
          find_sample(mod, "claude_events_queue_depth") == 0.0,
          f"depth {find_sample(mod, 'claude_events_queue_depth')!r}")

    # ---- Scenario 8: no marker -> fallback path, marker_present == 0.
    print("\nScenario 8: no marker -> marker_present 0, in-dir scan still works")
    tmpdir = tempfile.mkdtemp(prefix="cee-test-")
    mod = load_exporter(tmpdir)
    mod.collect()
    check("S8 marker_present == 0 (no marker)", find_sample(mod, MARKER) == 0.0,
          f"got {find_sample(mod, MARKER)!r}")
    write_event(tmpdir, "heartbeat-tick", ns_ago=10)
    mod.collect()
    scan_v = find_sample(mod, METRIC)
    check("S8 scan fallback still sets the gauge", scan_v is not None and scan_v > 0,
          f"got {scan_v!r}")

    # ---- Scenario 9: malformed marker (not JSON) -> treated as absent.
    print("\nScenario 9: malformed marker -> ignored, fallback retained")
    write_marker(tmpdir, raw="{not json at all")
    mod.collect()
    check("S9 marker_present == 0 (malformed)", find_sample(mod, MARKER) == 0.0,
          f"got {find_sample(mod, MARKER)!r}")
    check("S9 gauge keeps the fallback value", find_sample(mod, METRIC) == scan_v,
          f"got {find_sample(mod, METRIC)!r}, want {scan_v!r}")

    # ---- Scenario 10: structurally-wrong markers.
    print("\nScenario 10: wrong-shaped markers -> ignored, no exception")
    for label, kwargs in (
        ("array instead of object", {"payload": [1, 2, 3]}),
        ("missing event_timestamp", {"payload": {"tag": "heartbeat-tick"}}),
        ("non-numeric event_timestamp", {"payload": {"event_timestamp": "soon"}}),
        ("null event_timestamp", {"payload": {"event_timestamp": None}}),
        ("zero event_timestamp", {"payload": {"event_timestamp": 0}}),
        ("negative event_timestamp", {"payload": {"event_timestamp": -5}}),
        ("empty file", {"raw": ""}),
    ):
        write_marker(tmpdir, **kwargs)
        try:
            mod.collect()
            ok = find_sample(mod, MARKER) == 0.0 and find_sample(mod, METRIC) == scan_v
            detail = f"present={find_sample(mod, MARKER)!r} gauge={find_sample(mod, METRIC)!r}"
        except Exception as e:  # noqa: BLE001 - the point is that it cannot raise
            ok, detail = False, f"raised {e!r}"
        check(f"S10 {label} ignored", ok, detail)

    # ---- Scenario 11: wedged consumer -> gauge must go STALE, not be masked.
    print("\nScenario 11: stale marker + fresh unconsumed ticks -> gauge stays stale")
    tmpdir = tempfile.mkdtemp(prefix="cee-test-")
    stale_ts = time.time() - 3600
    write_marker(tmpdir, ts=stale_ts)
    mod = load_exporter(tmpdir)
    mod.collect()
    # Consumer dies; the emitter keeps emitting and ticks pile up unconsumed.
    write_event(tmpdir, "heartbeat-tick", ns_ago=60)
    write_event(tmpdir, "heartbeat-tick", ns_ago=1)
    mod.collect()
    v = find_sample(mod, METRIC)
    check("S11 gauge still the STALE marker value (not the fresh queue file)",
          v is not None and abs(v - stale_ts) < 0.01, f"got {v!r}, want {stale_ts!r}")
    check("S11 backlog is visible on queue_depth instead",
          find_sample(mod, "claude_events_queue_depth") == 2.0,
          f"depth {find_sample(mod, 'claude_events_queue_depth')!r}")

    # ---- Scenario 12: marker monotonicity.
    print("\nScenario 12: marker rewritten older -> gauge does not regress")
    tmpdir = tempfile.mkdtemp(prefix="cee-test-")
    newer_ts = time.time() - 10
    write_marker(tmpdir, ts=newer_ts)
    mod = load_exporter(tmpdir)
    mod.collect()
    write_marker(tmpdir, ts=newer_ts - 500)
    mod.collect()
    v = find_sample(mod, METRIC)
    check("S12 gauge held at the newer marker value",
          v is not None and abs(v - newer_ts) < 0.01, f"got {v!r}, want {newer_ts!r}")

    # ---- Scenario 13: marker takes over from the scan fallback.
    print("\nScenario 13: marker appearing later takes over from the scan fallback")
    tmpdir = tempfile.mkdtemp(prefix="cee-test-")
    mod = load_exporter(tmpdir)
    write_event(tmpdir, "heartbeat-tick", ns_ago=90)
    mod.collect()
    before = find_sample(mod, METRIC)
    check("S13 scan value in effect first", before is not None and before > 0,
          f"got {before!r}")
    take_over_ts = time.time() - 5
    write_marker(tmpdir, ts=take_over_ts)
    mod.collect()
    v = find_sample(mod, METRIC)
    check("S13 marker now drives the gauge",
          v is not None and abs(v - take_over_ts) < 0.01, f"got {v!r}, want {take_over_ts!r}")
    check("S13 marker_present == 1", find_sample(mod, MARKER) == 1.0,
          f"got {find_sample(mod, MARKER)!r}")

    print()
    if failures:
        print(f"FAILED: {len(failures)} test(s)")
        for n, m in failures:
            print(f"  - {n}: {m}")
        sys.exit(1)
    print("OK: all scenarios passed")


if __name__ == "__main__":
    run_scenarios()
