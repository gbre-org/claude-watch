#!/usr/bin/env python3
"""Tests for claude-events-exporter's QUEUE metrics.

The exporter answers one question: what is sitting in the event queue right
now, and how old is it. It deliberately does NOT answer "is the main loop
alive" — that is the age of the main loop's last ack, exported by
`claude-watch metrics` as `claude_mainloop_last_ack_timestamp_seconds`.

Scenarios:

  (1) Cold start on an empty dir -> depth 0, oldest age 0, no scrape errors.
  (2) Events present -> depth counts them and the oldest age is derived from
      the filename ns-timestamp (not file mtime, which a copy would reset).
  (3) Events consumed between scrapes -> depth falls back to 0 and the
      processed counter advances by exactly the number consumed.
  (4) Per source/tag counting is one-shot: rescraping the same file must not
      double-count it.
  (5) An unknown tag/source collapses to "other" so cardinality stays bounded.
  (6) `keepalive` gets its own label, and so does the legacy `heartbeat-tick`
      spelling for one release.
  (7) A dot-prefixed temp file (claude-event's atomic-write staging) is NOT
      counted as a queued event.
  (8) A SUBDIRECTORY in the queue dir is not counted either. This is the #681
      regression in exporter form: state under the queue dir got counted as
      events elsewhere in the stack and fired false WATCHER DOWN alerts.
  (9) RETIRED GAUGES: the heartbeat marker gauges must not come back. A
      dashboard or alert rule keying on a silently-dead series is worse than
      one keying on a missing one.
 (10) A missing events dir increments the scrape-error counter instead of
      raising.

Run:  python3 test_claude_events_exporter.py
Exits 0 on success, 1 on first failure with a diagnostic.
"""

import json
import os
import shutil
import sys
import tempfile
import time
from importlib.util import spec_from_file_location, module_from_spec

HERE = os.path.dirname(os.path.abspath(__file__))


def load_exporter(events_dir):
    """Reload the exporter module under a fresh CLAUDE_EVENTS_DIR so its
    module-level EVENTS_DIR constant (and all _seen_filenames state) starts
    clean for each scenario."""
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


def write_event(events_dir, tag, *, source="claude-watch", ns_ago=0, name=None):
    """Drop a bus event file named like the real emitter:
    <ns_timestamp>_<safe_tag>.json"""
    ts_ns = int(time.time() * 1e9) - int(ns_ago * 1e9)
    path = os.path.join(events_dir, name or f"{ts_ns}_{tag}.json")
    with open(path, "w") as f:
        json.dump({"source": source, "tag": tag}, f)
    return path


def find_sample(mod, metric_name, **labels):
    # Match on the SAMPLE name, not the family name: prometheus_client strips
    # the _total suffix from a Counter family, so matching families would
    # silently miss every counter here and report None as if the metric were
    # absent.
    for fam in mod.REG.collect():
        for sample in fam.samples:
            if sample.name != metric_name:
                continue
            if labels and any(sample.labels.get(k) != v for k, v in labels.items()):
                continue
            return sample.value
    return None


def all_metric_names(mod):
    names = set()
    for fam in mod.REG.collect():
        names.add(fam.name)
        names.update(s.name for s in fam.samples)
    return names


def run_scenarios():
    failures = []

    def check(name, predicate, msg):
        if predicate:
            print(f"  PASS: {name}")
        else:
            print(f"  FAIL: {name} -- {msg}")
            failures.append((name, msg))

    DEPTH = "claude_events_queue_depth"
    AGE = "claude_events_age_seconds"
    TOTAL = "claude_events_total"
    PROCESSED = "claude_events_processed_total"
    ERRORS = "claude_events_scrape_errors_total"

    # ---- Scenario 1: cold start on an empty dir.
    print("\nScenario 1: empty queue -> depth 0, age 0, no errors")
    tmpdir = tempfile.mkdtemp(prefix="cee-test-")
    mod = load_exporter(tmpdir)
    mod.collect()
    check("S1 depth 0", find_sample(mod, DEPTH) == 0.0, f"got {find_sample(mod, DEPTH)!r}")
    check("S1 age 0", find_sample(mod, AGE) == 0.0, f"got {find_sample(mod, AGE)!r}")
    check("S1 no scrape errors", find_sample(mod, ERRORS) == 0.0,
          f"got {find_sample(mod, ERRORS)!r}")

    # ---- Scenario 2: events present -> depth + oldest age.
    print("\nScenario 2: queued events -> depth counts them, age is the OLDEST")
    tmpdir = tempfile.mkdtemp(prefix="cee-test-")
    mod = load_exporter(tmpdir)
    write_event(tmpdir, "keepalive", ns_ago=300)
    write_event(tmpdir, "queue-added", source="queue", ns_ago=10)
    mod.collect()
    check("S2 depth 2", find_sample(mod, DEPTH) == 2.0, f"got {find_sample(mod, DEPTH)!r}")
    age = find_sample(mod, AGE)
    # From the filename ns-prefix, so it survives a copy that resets mtime.
    check("S2 age ~300s (the oldest)", age is not None and 295 <= age <= 320,
          f"got {age!r}")

    # ---- Scenario 3: consumption between scrapes.
    print("\nScenario 3: consumed events -> depth drops, processed advances")
    for name in os.listdir(tmpdir):
        os.unlink(os.path.join(tmpdir, name))
    mod.collect()
    check("S3 depth back to 0", find_sample(mod, DEPTH) == 0.0,
          f"got {find_sample(mod, DEPTH)!r}")
    check("S3 age back to 0", find_sample(mod, AGE) == 0.0,
          f"got {find_sample(mod, AGE)!r}")
    check("S3 processed == 2", find_sample(mod, PROCESSED, outcome="consumed") == 2.0,
          f"got {find_sample(mod, PROCESSED, outcome='consumed')!r}")

    # ---- Scenario 4: one-shot counting across rescrapes.
    print("\nScenario 4: rescraping the same file does not double-count it")
    tmpdir = tempfile.mkdtemp(prefix="cee-test-")
    mod = load_exporter(tmpdir)
    write_event(tmpdir, "keepalive")
    mod.collect()
    mod.collect()
    mod.collect()
    v = find_sample(mod, TOTAL, source="claude-watch", tag="keepalive")
    check("S4 counted exactly once", v == 1.0, f"got {v!r}")

    # ---- Scenario 5: unknown labels collapse to "other".
    print("\nScenario 5: unknown source/tag collapse to 'other' (bounded cardinality)")
    tmpdir = tempfile.mkdtemp(prefix="cee-test-")
    mod = load_exporter(tmpdir)
    write_event(tmpdir, "some-brand-new-tag", source="some-brand-new-source")
    mod.collect()
    check("S5 collapsed to other/other",
          find_sample(mod, TOTAL, source="other", tag="other") == 1.0,
          f"got {find_sample(mod, TOTAL, source='other', tag='other')!r}")

    # ---- Scenario 6: keepalive + its legacy spelling both get real labels.
    print("\nScenario 6: keepalive and legacy heartbeat-tick keep their own labels")
    tmpdir = tempfile.mkdtemp(prefix="cee-test-")
    mod = load_exporter(tmpdir)
    write_event(tmpdir, "keepalive")
    write_event(tmpdir, "heartbeat-tick")
    mod.collect()
    check("S6 keepalive labelled",
          find_sample(mod, TOTAL, source="claude-watch", tag="keepalive") == 1.0,
          f"got {find_sample(mod, TOTAL, source='claude-watch', tag='keepalive')!r}")
    check("S6 legacy heartbeat-tick labelled",
          find_sample(mod, TOTAL, source="claude-watch", tag="heartbeat-tick") == 1.0,
          f"got {find_sample(mod, TOTAL, source='claude-watch', tag='heartbeat-tick')!r}")

    # ---- Scenario 7: dot-prefixed temp files are not events.
    print("\nScenario 7: atomic-write temp file is not counted as a queued event")
    tmpdir = tempfile.mkdtemp(prefix="cee-test-")
    mod = load_exporter(tmpdir)
    write_event(tmpdir, "tmp", name=".1750000000_tmp.json")
    mod.collect()
    check("S7 depth 0", find_sample(mod, DEPTH) == 0.0, f"got {find_sample(mod, DEPTH)!r}")

    # ---- Scenario 8: a subdirectory is not an event (#681 regression).
    print("\nScenario 8: a subdir in the queue dir is not counted as an event")
    tmpdir = tempfile.mkdtemp(prefix="cee-test-")
    mod = load_exporter(tmpdir)
    os.makedirs(os.path.join(tmpdir, ".state"))
    with open(os.path.join(tmpdir, ".state", "some-state.json"), "w") as f:
        json.dump({"not": "an event"}, f)
    mod.collect()
    check("S8 depth 0", find_sample(mod, DEPTH) == 0.0, f"got {find_sample(mod, DEPTH)!r}")
    check("S8 age 0", find_sample(mod, AGE) == 0.0, f"got {find_sample(mod, AGE)!r}")

    # ---- Scenario 9: retired gauges stay retired.
    print("\nScenario 9: the retired heartbeat-marker gauges are gone")
    names = all_metric_names(mod)
    for retired in (
        "claude_events_last_heartbeat_timestamp",
        "claude_events_last_heartbeat_timestamp_seconds",
        "claude_events_heartbeat_marker_present",
    ):
        check(f"S9 {retired} absent", retired not in names,
              f"still exported: {sorted(names)}")
    check("S9 no marker reader left on the module",
          not hasattr(mod, "read_heartbeat_marker"),
          "read_heartbeat_marker still defined")

    # ---- Scenario 10: a missing dir is an error counter, not a crash.
    print("\nScenario 10: missing events dir -> scrape error, no exception")
    tmpdir = tempfile.mkdtemp(prefix="cee-test-")
    mod = load_exporter(tmpdir)
    shutil.rmtree(tmpdir)
    mod.collect()
    check("S10 scrape error counted", find_sample(mod, ERRORS) == 1.0,
          f"got {find_sample(mod, ERRORS)!r}")

    print()
    if failures:
        print(f"FAILED: {len(failures)} test(s)")
        for n, m in failures:
            print(f"  - {n}: {m}")
        sys.exit(1)
    print("OK: all scenarios passed")


if __name__ == "__main__":
    run_scenarios()
