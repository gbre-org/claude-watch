#!/usr/bin/env python3
"""Tests for claude-events-exporter's claude_events_last_heartbeat_timestamp_seconds
gauge (backs the dashboard's "Last Ack Age" stat).

Scenarios:

  (1) Fresh exporter, no heartbeat-tick ever seen -> gauge stays 0 (the
      dashboard query filters this cold-start zero with `> 0`).
  (2) A heartbeat-tick file lands in the queue dir -> gauge picks up its
      filename-encoded ns-timestamp.
  (3) claude-event-watch consumes (deletes) that file before the next
      scrape -> gauge PERSISTS the last-seen value (the whole point: the
      signal must survive fast consumption, not reset to 0 the moment the
      file disappears).
  (4) A newer heartbeat-tick arrives -> gauge advances to it.
  (5) An unrelated bus event (any other source/tag) does not move the
      gauge.

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

    print()
    if failures:
        print(f"FAILED: {len(failures)} test(s)")
        for n, m in failures:
            print(f"  - {n}: {m}")
        sys.exit(1)
    print("OK: all scenarios passed")


if __name__ == "__main__":
    run_scenarios()
