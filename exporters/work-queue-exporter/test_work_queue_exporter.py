#!/usr/bin/env python3
"""End-to-end tests for the work-queue-exporter has_live_owner gauge.

Rev 2026-05-01-v3: source of truth is claude-watch active-agents JSON.

Scenarios covered:

  (1) running queue item HAS a matching agent record, alive=true
      -> has_live_owner = 1, agent_jsonl_age_seconds emitted.
  (2) running queue item HAS a matching agent record, alive=false
      -> has_live_owner = 0 (orphan alert fires), age emitted.
  (3) running queue item has NO matching agent record
      -> has_live_owner absent (no signal beats false-anything),
         age also absent.
  (4) agent state file MISSING (claude-watch not publishing)
      -> agent_state_last_modified=0, no live_owner gauges,
         exporter does not crash.
  (5) duplicate queue_id across multiple agent records: prefer the
      live one.
  (6) duplicate queue_id, both stale: prefer the fresher (smaller
      jsonl_age_seconds).
  (7) ready_age + status counts still emit normally regardless of
      agent state.
  (23) container-spawned agent: no pid, owner named only by the
      register-time `agent_id` stamp, active-agents record keyed under
      a DIFFERENT qid -> has_live_owner=1 with agent_id populated;
      stale stamped owner -> 0; unresolved stamp -> presumed alive;
      the stamp outranks a disagreeing arm-hook binding.
  (24) in-flight tool call (alive=true with a 443s transcript, the
      post-#690 shape) -> has_live_owner=1; liveness is read from
      `alive`, never re-derived from the transcript age and never
      from a pid.
  (25) BOTH owner inputs unreadable (a container missing its bind
      mounts) -> loud warning naming each path + env var,
      worktask_queue_owner_input_available=0 for each, and
      has_live_owner ABSENT rather than 0 for every running item.
      One readable input is enough to keep the orphan fallback live.

Run:  python3 test_work_queue_exporter.py
Exits 0 on success, 1 on first failure with a diagnostic.
"""

import json
import logging
import os
import sys
import tempfile
import time
from datetime import datetime, timedelta, timezone
from importlib.util import spec_from_file_location, module_from_spec


HERE = os.path.dirname(os.path.abspath(__file__))


def load_exporter(env):
    """Reload the exporter module under a fresh env so module-level
    config constants pick up our overrides."""
    saved = {}
    for k in ("PORT", "QUEUE_JSON", "AGENT_STATE_JSON", "AGENT_QUEUE_BINDINGS_JSON"):
        saved[k] = os.environ.get(k)
    for k, v in env.items():
        os.environ[k] = v
    try:
        spec = spec_from_file_location(
            "work_queue_exporter_under_test",
            os.path.join(HERE, "work_queue_exporter.py"),
        )
        mod = module_from_spec(spec)
        spec.loader.exec_module(mod)
        return mod
    finally:
        for k, v in saved.items():
            if v is None:
                os.environ.pop(k, None)
            else:
                os.environ[k] = v


def write_queue(path, items, *, locked_scopes=None):
    payload = {"items": items}
    if locked_scopes:
        # locked_scopes is a dict {scope_token: {reason, locked_at}}
        payload["locked_scopes"] = locked_scopes
    with open(path, "w") as f:
        json.dump(payload, f)


def write_agent_state(path, agents, *, subagents=None, workloads=None):
    payload = {
        "subagents": subagents or [],
        "workloads": workloads or [],
        "agents": agents,
    }
    with open(path, "w") as f:
        json.dump(payload, f)


def write_bindings(path, qid_to_aid):
    """Write an arm-hook agent-queue-bindings.json (agent_id -> record).

    Input is a queue_id -> agent_id map (the owner relation); we emit the
    on-disk shape post-tool-agent-arm-hook writes, keyed by agent_id with a
    monotonically-increasing registered_at so newest-wins is deterministic.
    """
    bindings = {}
    for i, (qid, aid) in enumerate(qid_to_aid.items()):
        bindings[aid] = {"queue_id": qid, "registered_at": 1000 + i}
    with open(path, "w") as f:
        json.dump({"bindings": bindings}, f)


def find_sample(mod, metric_name, label_filters):
    for fam in mod.REG.collect():
        if fam.name != metric_name:
            continue
        for sample in fam.samples:
            if sample.name != metric_name:
                continue
            ok = True
            for k, v in label_filters.items():
                if sample.labels.get(k) != v:
                    ok = False
                    break
            if ok:
                return sample.value
    return None


def find_any_sample(mod, metric_name, id_value):
    """Return first sample where labels[id]==id_value, else None."""
    for fam in mod.REG.collect():
        if fam.name != metric_name:
            continue
        for sample in fam.samples:
            if sample.labels.get("id") == id_value:
                return sample.value
    return None


def make_running_item(iid, summary):
    """Build a queue item dict with status=running."""
    now = datetime.now(timezone.utc)
    return {
        "id": iid,
        "summary": summary,
        "scope": ["repo:test"],
        "group_id": "g-test",
        "group_head": True,
        "status": "running",
        "priority": 5,
        "created_at": now.isoformat(),
        "created_by": "test",
        "started_at": now.isoformat(),
        "registered_at": now.isoformat(),
    }


def make_pending_item(iid, summary, *, age_seconds=120):
    """Build a pending item that's been ready for `age_seconds`."""
    created = datetime.now(timezone.utc) - timedelta(seconds=age_seconds)
    return {
        "id": iid,
        "summary": summary,
        "scope": ["repo:test"],
        "group_id": f"g-{iid}",
        "group_head": True,
        "status": "pending",
        "priority": 5,
        "created_at": created.isoformat(),
        "created_by": "test",
    }


def make_blocked_item(iid, summary, *, block_reason="awaiting greenlight"):
    """Build a queue item dict with status=blocked."""
    now = datetime.now(timezone.utc)
    return {
        "id": iid,
        "summary": summary,
        "scope": ["repo:test"],
        "group_id": f"g-{iid}",
        "group_head": True,
        "status": "blocked",
        "priority": 5,
        "created_at": now.isoformat(),
        "created_by": "test",
        "started_at": now.isoformat(),
        "registered_at": now.isoformat(),
        "blocked_at": now.isoformat(),
        "block_reason": block_reason,
    }


def run_scenarios():
    failures = []

    tmpdir = tempfile.mkdtemp(prefix="wqe-test-")
    qjson = os.path.join(tmpdir, "queue.json")
    astate = os.path.join(tmpdir, "active-agents.json")

    env = {
        "QUEUE_JSON": qjson,
        "AGENT_STATE_JSON": astate,
        "PORT": "9099",
    }

    def check(name, predicate, msg):
        if predicate:
            print(f"  PASS: {name}")
        else:
            print(f"  FAIL: {name} -- {msg}")
            failures.append((name, msg))

    # ---- Scenario 1: matching agent record, alive=true.
    print("\nScenario 1: running item with live agent -> has_live_owner=1")
    items = [make_running_item("q-s1", "scenario 1 alive")]
    agents = [{
        "agent_id": "agent-aaaa1",
        "queue_id": "q-s1",
        "alive": True,
        "jsonl_age_seconds": 5,
    }]
    write_queue(qjson, items)
    write_agent_state(astate, agents)
    mod = load_exporter(env)
    mod.collect()
    v = find_sample(
        mod, "worktask_queue_item_has_live_owner",
        {"id": "q-s1", "agent_id": "agent-aaaa1"},
    )
    check("S1 has_live_owner == 1", v == 1.0, f"got {v!r}")
    age = find_sample(
        mod, "worktask_queue_item_agent_jsonl_age_seconds",
        {"id": "q-s1", "agent_id": "agent-aaaa1"},
    )
    check("S1 agent_jsonl_age_seconds emitted", age == 5.0, f"got {age!r}")

    # ---- Scenario 2: matching agent record, alive=false (orphan).
    print("\nScenario 2: running item with stale agent -> has_live_owner=0")
    items = [make_running_item("q-s2", "scenario 2 stale")]
    agents = [{
        "agent_id": "agent-bbbb2",
        "queue_id": "q-s2",
        "alive": False,
        "jsonl_age_seconds": 600,
    }]
    write_queue(qjson, items)
    write_agent_state(astate, agents)
    mod = load_exporter(env)
    mod.collect()
    v = find_sample(
        mod, "worktask_queue_item_has_live_owner",
        {"id": "q-s2", "agent_id": "agent-bbbb2"},
    )
    check("S2 has_live_owner == 0", v == 0.0, f"got {v!r}")
    age = find_sample(
        mod, "worktask_queue_item_agent_jsonl_age_seconds",
        {"id": "q-s2", "agent_id": "agent-bbbb2"},
    )
    check("S2 jsonl_age_seconds emitted", age == 600.0, f"got {age!r}")

    # ---- Scenario 3: no matching agent record.
    print("\nScenario 3: running item with NO matching agent -> silent")
    items = [make_running_item("q-s3", "scenario 3 no agent")]
    agents = [{
        "agent_id": "agent-zzzz",
        "queue_id": "q-OTHER",
        "alive": True,
        "jsonl_age_seconds": 0,
    }]
    write_queue(qjson, items)
    write_agent_state(astate, agents)
    mod = load_exporter(env)
    mod.collect()
    v = find_any_sample(mod, "worktask_queue_item_has_live_owner", "q-s3")
    check(
        "S3 has_live_owner absent (no signal)",
        v is None,
        f"expected None, got {v!r}",
    )
    age = find_any_sample(
        mod, "worktask_queue_item_agent_jsonl_age_seconds", "q-s3",
    )
    check("S3 jsonl_age also absent", age is None, f"got {age!r}")

    # ---- Scenario 4: agent state file MISSING.
    print("\nScenario 4: agent state missing -> exporter survives, no live_owner")
    items = [make_running_item("q-s4", "scenario 4 no state file")]
    write_queue(qjson, items)
    if os.path.exists(astate):
        os.remove(astate)
    mod = load_exporter(env)
    mod.collect()  # should NOT crash
    v = find_any_sample(mod, "worktask_queue_item_has_live_owner", "q-s4")
    check(
        "S4 has_live_owner absent",
        v is None,
        f"expected None, got {v!r}",
    )
    state_mt = find_sample(
        mod, "worktask_queue_agent_state_last_modified", {},
    )
    check(
        "S4 agent_state_last_modified == 0",
        state_mt == 0.0,
        f"expected 0, got {state_mt!r}",
    )

    # ---- Scenario 5: duplicate queue_id, prefer live.
    print("\nScenario 5: duplicate queue_id, alive wins over stale")
    items = [make_running_item("q-s5", "scenario 5 dup live")]
    agents = [
        {
            "agent_id": "agent-stale",
            "queue_id": "q-s5",
            "alive": False,
            "jsonl_age_seconds": 300,
        },
        {
            "agent_id": "agent-live",
            "queue_id": "q-s5",
            "alive": True,
            "jsonl_age_seconds": 1,
        },
    ]
    write_queue(qjson, items)
    write_agent_state(astate, agents)
    mod = load_exporter(env)
    mod.collect()
    v = find_sample(
        mod, "worktask_queue_item_has_live_owner",
        {"id": "q-s5", "agent_id": "agent-live"},
    )
    check(
        "S5 live record wins, has_live_owner == 1 with agent-live label",
        v == 1.0,
        f"got {v!r}",
    )
    # The stale-sibling label should NOT appear.
    v_stale = find_sample(
        mod, "worktask_queue_item_has_live_owner",
        {"id": "q-s5", "agent_id": "agent-stale"},
    )
    check(
        "S5 stale sibling NOT emitted",
        v_stale is None,
        f"got {v_stale!r}",
    )

    # ---- Scenario 6: duplicate queue_id, both stale -> prefer fresher.
    print("\nScenario 6: duplicate queue_id, both stale -> fresher wins")
    items = [make_running_item("q-s6", "scenario 6 dup stale")]
    agents = [
        {
            "agent_id": "agent-older",
            "queue_id": "q-s6",
            "alive": False,
            "jsonl_age_seconds": 999,
        },
        {
            "agent_id": "agent-newer",
            "queue_id": "q-s6",
            "alive": False,
            "jsonl_age_seconds": 200,
        },
    ]
    write_queue(qjson, items)
    write_agent_state(astate, agents)
    mod = load_exporter(env)
    mod.collect()
    v = find_sample(
        mod, "worktask_queue_item_has_live_owner",
        {"id": "q-s6", "agent_id": "agent-newer"},
    )
    check(
        "S6 fresher stale wins, has_live_owner == 0 with agent-newer label",
        v == 0.0,
        f"got {v!r}",
    )

    # ---- Scenario 7: pending items still get ready_age regardless.
    print("\nScenario 7: pending+ready item still emits ready_age")
    items = [
        make_running_item("q-s7r", "running for context"),
        make_pending_item("q-s7p", "pending ready", age_seconds=300),
    ]
    agents = [{
        "agent_id": "agent-aaaa",
        "queue_id": "q-s7r",
        "alive": True,
        "jsonl_age_seconds": 1,
    }]
    write_queue(qjson, items)
    write_agent_state(astate, agents)
    mod = load_exporter(env)
    mod.collect()
    ra = find_sample(
        mod, "worktask_queue_item_ready_age_seconds",
        {"id": "q-s7p"},
    )
    check(
        "S7 ready_age emitted for pending+head item",
        ra is not None and ra >= 300,
        f"got {ra!r}",
    )

    # ---- Scenario 8: locked item -> locked_age emitted, ready_age absent.
    print("\nScenario 8: pending item with matching lock -> locked_age, NOT ready_age")
    lock_ts = datetime.now(timezone.utc).isoformat()
    items = [make_pending_item("q-s8", "locked item", age_seconds=1200)]
    # Override scope to match the lock token.
    items[0]["scope"] = ["new-aqi-meter"]
    locked = {"new-aqi-meter": {"reason": "wait for hw install", "locked_at": lock_ts}}
    write_queue(qjson, items, locked_scopes=locked)
    write_agent_state(astate, [])
    mod = load_exporter(env)
    mod.collect()
    # locked_age should be emitted with lock_scope label
    la = find_sample(
        mod, "worktask_queue_item_locked_age_seconds",
        {"id": "q-s8", "lock_scope": "new-aqi-meter"},
    )
    check("S8 locked_age emitted", la is not None and la >= 1200, f"got {la!r}")
    # ready_age must NOT be emitted for this item
    ra = find_any_sample(mod, "worktask_queue_item_ready_age_seconds", "q-s8")
    check("S8 ready_age absent for locked item", ra is None, f"got {ra!r}")

    # ---- Scenario 9: partial lock — one item locked, sibling item ready.
    print("\nScenario 9: two pending items, one locked one not -> correct split")
    lock_ts = datetime.now(timezone.utc).isoformat()
    item_locked = make_pending_item("q-s9a", "locked", age_seconds=900)
    item_locked["scope"] = ["hw:sensor-install"]
    item_ready = make_pending_item("q-s9b", "ready", age_seconds=600)
    item_ready["scope"] = ["repo:grafana"]
    locked2 = {"hw:sensor-install": {"reason": "hw not installed", "locked_at": lock_ts}}
    write_queue(qjson, [item_locked, item_ready], locked_scopes=locked2)
    write_agent_state(astate, [])
    mod = load_exporter(env)
    mod.collect()
    # The locked item must appear in locked_age only.
    la9 = find_sample(
        mod, "worktask_queue_item_locked_age_seconds",
        {"id": "q-s9a", "lock_scope": "hw:sensor-install"},
    )
    check("S9 locked item in locked_age", la9 is not None and la9 >= 900, f"got {la9!r}")
    ra9_locked = find_any_sample(mod, "worktask_queue_item_ready_age_seconds", "q-s9a")
    check("S9 locked item absent from ready_age", ra9_locked is None, f"got {ra9_locked!r}")
    # The ready item must appear in ready_age only.
    ra9 = find_any_sample(mod, "worktask_queue_item_ready_age_seconds", "q-s9b")
    check("S9 ready item in ready_age", ra9 is not None and ra9 >= 600, f"got {ra9!r}")
    la9_ready = find_sample(
        mod, "worktask_queue_item_locked_age_seconds",
        {"id": "q-s9b"},
    )
    check("S9 ready item absent from locked_age", la9_ready is None, f"got {la9_ready!r}")

    # ---- Scenario 10: no locked_scopes key at all -> backwards compat.
    print("\nScenario 10: queue.json with no locked_scopes key -> no crash, ready_age emitted")
    items = [make_pending_item("q-s10", "no lock key", age_seconds=60)]
    # write_queue without locked_scopes= kwarg -> no locked_scopes in JSON
    write_queue(qjson, items)
    write_agent_state(astate, [])
    mod = load_exporter(env)
    mod.collect()
    ra10 = find_any_sample(mod, "worktask_queue_item_ready_age_seconds", "q-s10")
    check("S10 ready_age emitted normally", ra10 is not None and ra10 >= 60, f"got {ra10!r}")

    # ---- Scenario 10b: dep_blockers non-empty -> ready_age absent.
    # An item with `dep_blockers` is intentionally waiting on an upstream
    # depends_on task. It MUST NOT drive WorkQueueReadyStuck — that's a
    # false-positive on healthy serialized work. Mirror of S8 (locked
    # scope) but using the dep_blockers signal instead.
    print("\nScenario 10b: pending item with dep_blockers -> ready_age absent")
    blocked_item = make_pending_item(
        "q-s10b-blocked", "waiting on upstream", age_seconds=1800,
    )
    blocked_item["dep_blockers"] = ["q-s10b-upstream"]
    upstream_item = make_running_item("q-s10b-upstream", "upstream running")
    write_queue(qjson, [blocked_item, upstream_item])
    write_agent_state(astate, [])
    mod = load_exporter(env)
    mod.collect()
    ra_blocked = find_any_sample(
        mod, "worktask_queue_item_ready_age_seconds", "q-s10b-blocked",
    )
    check(
        "S10b ready_age absent for dep_blockers-blocked item",
        ra_blocked is None,
        f"expected None, got {ra_blocked!r}",
    )
    # And dep_blockers items should NOT leak into locked_age either —
    # they have no `lock_scope`, just an upstream dependency.
    la_blocked = find_any_sample(
        mod, "worktask_queue_item_locked_age_seconds", "q-s10b-blocked",
    )
    check(
        "S10b locked_age absent for dep_blockers-blocked item",
        la_blocked is None,
        f"expected None, got {la_blocked!r}",
    )

    # ---- Scenario 10c: empty dep_blockers list -> ready_age emitted.
    # Symmetric backwards-compat: an explicit empty list (the normal case
    # from session-task queue's JSON output) must still emit ready_age.
    print("\nScenario 10c: pending item with empty dep_blockers list -> ready_age emitted")
    ready_item = make_pending_item(
        "q-s10c-ready", "no deps", age_seconds=120,
    )
    ready_item["dep_blockers"] = []
    write_queue(qjson, [ready_item])
    write_agent_state(astate, [])
    mod = load_exporter(env)
    mod.collect()
    ra_ready = find_any_sample(
        mod, "worktask_queue_item_ready_age_seconds", "q-s10c-ready",
    )
    check(
        "S10c ready_age emitted with empty dep_blockers",
        ra_ready is not None and ra_ready >= 120,
        f"got {ra_ready!r}",
    )

    # ---- Scenario 10d: locked AND dep_blockers -> neither gauge emits.
    # The lock-scope short-circuit currently emits locked_age. With the
    # dep_blockers guard added upstream of the lock check, the item is
    # filtered out before we even classify it as locked. This is
    # consistent with "dep_blockers means: don't surface ready/locked
    # signals for this item at all".
    print("\nScenario 10d: pending item with BOTH dep_blockers AND scope lock -> neither emitted")
    lock_ts = datetime.now(timezone.utc).isoformat()
    double_blocked = make_pending_item(
        "q-s10d", "double blocked", age_seconds=2000,
    )
    double_blocked["scope"] = ["new-aqi-meter"]
    double_blocked["dep_blockers"] = ["q-some-upstream"]
    locked_dbl = {
        "new-aqi-meter": {"reason": "hw install", "locked_at": lock_ts},
    }
    write_queue(qjson, [double_blocked], locked_scopes=locked_dbl)
    write_agent_state(astate, [])
    mod = load_exporter(env)
    mod.collect()
    ra_dbl = find_any_sample(
        mod, "worktask_queue_item_ready_age_seconds", "q-s10d",
    )
    check(
        "S10d ready_age absent (dep_blockers wins)",
        ra_dbl is None,
        f"got {ra_dbl!r}",
    )
    la_dbl = find_any_sample(
        mod, "worktask_queue_item_locked_age_seconds", "q-s10d",
    )
    check(
        "S10d locked_age absent (dep_blockers short-circuits)",
        la_dbl is None,
        f"got {la_dbl!r}",
    )

    # ---- Scenario 11: blocked item -- has_live_owner labelled status="blocked"
    # so the WorkQueueOrphaned alert (which filters status="running") does
    # NOT fire on it. By design a blocked item has no live agent.
    print("\nScenario 11: blocked item -> has_live_owner emitted with status='blocked'")
    items = [make_blocked_item("q-s11", "scenario 11 blocked")]
    # Use a stale agent record so alive=False -- if the alert wasn't
    # filtering on status it'd fire here.
    agents = [{
        "agent_id": "agent-block11",
        "queue_id": "q-s11",
        "alive": False,
        "jsonl_age_seconds": 600,
    }]
    write_queue(qjson, items)
    write_agent_state(astate, agents)
    mod = load_exporter(env)
    mod.collect()
    v_running = find_sample(
        mod, "worktask_queue_item_has_live_owner",
        {"id": "q-s11", "status": "running"},
    )
    v_blocked = find_sample(
        mod, "worktask_queue_item_has_live_owner",
        {"id": "q-s11", "status": "blocked"},
    )
    check(
        "S11 has_live_owner emitted with status='blocked'",
        v_blocked == 0.0,
        f"got blocked={v_blocked!r}",
    )
    check(
        "S11 has_live_owner NOT emitted with status='running' (filtered out by status label)",
        v_running is None,
        f"got running={v_running!r}",
    )
    # status counts gauge should report blocked=1
    blocked_total = find_sample(
        mod, "worktask_queue_items_total", {"status": "blocked"},
    )
    check(
        "S11 worktask_queue_items_total{status='blocked'} == 1",
        blocked_total == 1.0,
        f"got {blocked_total!r}",
    )

    # ---- Scenario 12: running item still has status='running' label.
    # Verify the running case isn't accidentally relabelled as blocked.
    print("\nScenario 12: running item -> has_live_owner labelled status='running'")
    items = [make_running_item("q-s12", "scenario 12 running")]
    agents = [{
        "agent_id": "agent-run12",
        "queue_id": "q-s12",
        "alive": True,
        "jsonl_age_seconds": 3,
    }]
    write_queue(qjson, items)
    write_agent_state(astate, agents)
    mod = load_exporter(env)
    mod.collect()
    v12 = find_sample(
        mod, "worktask_queue_item_has_live_owner",
        {"id": "q-s12", "status": "running"},
    )
    check("S12 has_live_owner emitted with status='running'", v12 == 1.0,
          f"got {v12!r}")
    v12_block = find_sample(
        mod, "worktask_queue_item_has_live_owner",
        {"id": "q-s12", "status": "blocked"},
    )
    check("S12 has_live_owner NOT emitted with status='blocked'",
          v12_block is None, f"got {v12_block!r}")

    # ---- Scenario 13: running workload item with fresh heartbeat ->
    # progress_age emitted, value small. The point of this scenario is
    # the load-bearing one: an actively-progressing workload must NOT
    # trip WorkQueueStuck even if running_elapsed is large.
    print("\nScenario 13: workload item w/ fresh heartbeat -> progress_age emitted (small)")
    hb_dir = tempfile.mkdtemp(prefix="wqe-hb-")
    hb_path = os.path.join(hb_dir, "stv-promote-batch.heartbeat")
    with open(hb_path, "w") as f:
        f.write("progress\n")
    # Make the file very fresh -- now-ish.
    fresh_env = dict(env)
    fresh_env["WORKLOAD_HEARTBEAT_DIR"] = hb_dir
    item = make_running_item("q-s13", "stv-promote workload")
    item["scope"] = ["workload:stv-promote-batch", "repo:media-tools"]
    write_queue(qjson, [item])
    write_agent_state(astate, [])
    mod = load_exporter(fresh_env)
    mod.collect()
    pa = find_sample(
        mod, "worktask_queue_item_progress_age_seconds",
        {"id": "q-s13", "workload_label": "stv-promote-batch"},
    )
    check(
        "S13 progress_age emitted",
        pa is not None and 0.0 <= pa < 30.0,
        f"expected fresh value < 30s, got {pa!r}",
    )

    # ---- Scenario 14: running workload item with STALE heartbeat ->
    # progress_age emitted with large value. WorkQueueStuck should
    # eventually fire on this.
    print("\nScenario 14: workload item w/ stale heartbeat -> progress_age large")
    stale_hb = os.path.join(hb_dir, "stuck-rsync.heartbeat")
    with open(stale_hb, "w") as f:
        f.write("stale\n")
    # Back-date mtime to 2 hours ago.
    two_hr_ago = time.time() - 7200
    os.utime(stale_hb, (two_hr_ago, two_hr_ago))
    item = make_running_item("q-s14", "stuck rsync workload")
    item["scope"] = ["workload:stuck-rsync"]
    write_queue(qjson, [item])
    write_agent_state(astate, [])
    mod = load_exporter(fresh_env)
    mod.collect()
    pa = find_sample(
        mod, "worktask_queue_item_progress_age_seconds",
        {"id": "q-s14", "workload_label": "stuck-rsync"},
    )
    check(
        "S14 progress_age reflects stale mtime",
        pa is not None and pa >= 7000,
        f"expected >= 7000s, got {pa!r}",
    )

    # ---- Scenario 15: running workload item with NO heartbeat file ->
    # progress_age silent (load-bearing for the alert's `unless` clause).
    print("\nScenario 15: workload item w/ missing heartbeat -> progress_age absent")
    item = make_running_item("q-s15", "workload no heartbeat")
    item["scope"] = ["workload:never-started"]
    write_queue(qjson, [item])
    write_agent_state(astate, [])
    mod = load_exporter(fresh_env)
    mod.collect()
    pa = find_any_sample(mod, "worktask_queue_item_progress_age_seconds", "q-s15")
    check("S15 progress_age absent", pa is None, f"got {pa!r}")

    # ---- Scenario 16: running AGENT item (no workload scope) ->
    # progress_age absent. Agents don't have a progress signal of their
    # own; WorkQueueStuck relies on the `unless` clause to handle them
    # via the runtime floor only.
    print("\nScenario 16: agent item (no workload scope) -> progress_age absent")
    item = make_running_item("q-s16", "agent task")
    item["scope"] = ["repo:server-config"]
    write_queue(qjson, [item])
    write_agent_state(astate, [])
    mod = load_exporter(fresh_env)
    mod.collect()
    pa = find_any_sample(mod, "worktask_queue_item_progress_age_seconds", "q-s16")
    check("S16 progress_age absent for agent item", pa is None, f"got {pa!r}")

    # ---- Scenario 17: blocked workload item -> progress_age absent
    # (only running items emit the gauge). A blocked workload is parked
    # on an external blocker; "progress age" isn't a meaningful concept.
    print("\nScenario 17: blocked workload item -> progress_age absent")
    item = make_blocked_item("q-s17", "blocked workload")
    item["scope"] = ["workload:parked-job"]
    # Heartbeat file even exists, but blocked items shouldn't emit.
    parked_hb = os.path.join(hb_dir, "parked-job.heartbeat")
    with open(parked_hb, "w") as f:
        f.write("\n")
    write_queue(qjson, [item])
    write_agent_state(astate, [])
    mod = load_exporter(fresh_env)
    mod.collect()
    pa = find_any_sample(mod, "worktask_queue_item_progress_age_seconds", "q-s17")
    check("S17 progress_age absent for blocked item", pa is None, f"got {pa!r}")

    # ---- Scenario 17b: running hostjob item w/ fresh heartbeat ->
    # progress_age emitted (reuses the generic gauge with the hostjob
    # label in the workload_label dimension). hostjob heartbeat layout is
    # a per-label DIR: <HOSTJOB_HEARTBEAT_DIR>/<label>/heartbeat.
    print("\nScenario 17b: hostjob item w/ fresh heartbeat -> progress_age emitted (small)")
    hj_hb_dir = tempfile.mkdtemp(prefix="wqe-hjhb-")
    os.makedirs(os.path.join(hj_hb_dir, "stv-host-promote"))
    hj_fresh = os.path.join(hj_hb_dir, "stv-host-promote", "heartbeat")
    with open(hj_fresh, "w") as f:
        f.write("progress\n")
    hj_env = dict(env)
    hj_env["HOSTJOB_HEARTBEAT_DIR"] = hj_hb_dir
    item = make_running_item("q-s17b", "stv host promote hostjob")
    item["scope"] = ["hostjob:stv-host-promote", "repo:media-tools"]
    write_queue(qjson, [item])
    write_agent_state(astate, [])
    mod = load_exporter(hj_env)
    mod.collect()
    pa = find_sample(
        mod, "worktask_queue_item_progress_age_seconds",
        {"id": "q-s17b", "workload_label": "stv-host-promote"},
    )
    check(
        "S17b hostjob progress_age emitted",
        pa is not None and 0.0 <= pa < 30.0,
        f"expected fresh value < 30s, got {pa!r}",
    )

    # ---- Scenario 17c: running hostjob item w/ STALE heartbeat ->
    # progress_age large (WorkQueueStuck should fire on this too).
    print("\nScenario 17c: hostjob item w/ stale heartbeat -> progress_age large")
    os.makedirs(os.path.join(hj_hb_dir, "stuck-hostjob"))
    hj_stale = os.path.join(hj_hb_dir, "stuck-hostjob", "heartbeat")
    with open(hj_stale, "w") as f:
        f.write("stale\n")
    hj_two_hr_ago = time.time() - 7200
    os.utime(hj_stale, (hj_two_hr_ago, hj_two_hr_ago))
    item = make_running_item("q-s17c", "stuck hostjob")
    item["scope"] = ["hostjob:stuck-hostjob"]
    write_queue(qjson, [item])
    write_agent_state(astate, [])
    mod = load_exporter(hj_env)
    mod.collect()
    pa = find_sample(
        mod, "worktask_queue_item_progress_age_seconds",
        {"id": "q-s17c", "workload_label": "stuck-hostjob"},
    )
    check(
        "S17c hostjob progress_age reflects stale mtime",
        pa is not None and pa >= 7000,
        f"expected >= 7000s, got {pa!r}",
    )

    # ---- Scenario 17d: hostjob item w/ NO heartbeat dir -> absent.
    print("\nScenario 17d: hostjob item w/ missing heartbeat -> progress_age absent")
    item = make_running_item("q-s17d", "hostjob no heartbeat")
    item["scope"] = ["hostjob:never-started-hj"]
    write_queue(qjson, [item])
    write_agent_state(astate, [])
    mod = load_exporter(hj_env)
    mod.collect()
    pa = find_any_sample(mod, "worktask_queue_item_progress_age_seconds", "q-s17d")
    check("S17d hostjob progress_age absent", pa is None, f"got {pa!r}")

    # ---- Scenario 18: helper edge cases.
    print("\nScenario 18: _workload_label_from_scope + _hostjob_label_from_scope edge cases")
    mod = load_exporter(env)
    check("S18 None scope -> None",
          mod._workload_label_from_scope(None) is None, "got non-None")
    check("S18 empty scope -> None",
          mod._workload_label_from_scope([]) is None, "got non-None")
    check("S18 non-workload scope -> None",
          mod._workload_label_from_scope(["repo:foo"]) is None, "got non-None")
    check("S18 workload scope -> label",
          mod._workload_label_from_scope(["workload:abc"]) == "abc", "wrong label")
    check("S18 mixed scope -> label",
          mod._workload_label_from_scope(["repo:x", "workload:y"]) == "y", "wrong label")
    check("S18 empty label -> None",
          mod._workload_label_from_scope(["workload:"]) is None, "got non-None")
    # hostjob helper parallels the workload helper.
    check("S18 hj None scope -> None",
          mod._hostjob_label_from_scope(None) is None, "got non-None")
    check("S18 hj non-hostjob scope -> None",
          mod._hostjob_label_from_scope(["repo:foo"]) is None, "got non-None")
    check("S18 hj hostjob scope -> label",
          mod._hostjob_label_from_scope(["hostjob:abc"]) == "abc", "wrong label")
    check("S18 hj mixed scope -> label",
          mod._hostjob_label_from_scope(["repo:x", "hostjob:y"]) == "y", "wrong label")
    check("S18 hj empty label -> None",
          mod._hostjob_label_from_scope(["hostjob:"]) is None, "got non-None")

    # ---- Scenario 19: never-spawned running orphan (NO agent record).
    # A running item whose Agent was never fired has no agent binding at
    # all. The exporter falls back to last_heartbeat_at staleness and
    # emits has_live_owner=0 with agent_id empty so WorkQueueOrphaned can
    # fire. A fresh heartbeat stays silent (no false-alert). Blocked items
    # are exempt even when stale.
    print("\nScenario 19: never-spawned running orphan -- heartbeat-staleness fallback")

    # 19a: running, NO agent record, STALE last_heartbeat_at, orphan fires.
    stale_iso = (datetime.now(timezone.utc) - timedelta(seconds=1200)).isoformat()
    item = make_running_item("q-s19a", "never-spawned stale")
    item["registered_at"] = stale_iso
    item["started_at"] = stale_iso
    item["last_heartbeat_at"] = stale_iso
    write_queue(qjson, [item])
    write_agent_state(astate, [])
    mod = load_exporter(env)
    mod.collect()
    v = find_sample(
        mod, "worktask_queue_item_has_live_owner",
        {"id": "q-s19a", "agent_id": ""},
    )
    check(
        "S19a never-spawned stale orphan has_live_owner == 0, agent_id empty",
        v == 0.0,
        "got " + repr(v),
    )

    # 19b: running, NO agent record, FRESH last_heartbeat_at, stays silent.
    fresh_iso = datetime.now(timezone.utc).isoformat()
    item = make_running_item("q-s19b", "just-spawned fresh")
    item["last_heartbeat_at"] = fresh_iso
    write_queue(qjson, [item])
    write_agent_state(astate, [])
    mod = load_exporter(env)
    mod.collect()
    v = find_any_sample(mod, "worktask_queue_item_has_live_owner", "q-s19b")
    check(
        "S19b fresh heartbeat has_live_owner absent (no false-alert)",
        v is None,
        "expected None, got " + repr(v),
    )

    # 19c: blocked, NO agent record, STALE heartbeat, exempt (absent).
    item = make_blocked_item("q-s19c", "blocked stale")
    item["last_heartbeat_at"] = stale_iso
    write_queue(qjson, [item])
    write_agent_state(astate, [])
    mod = load_exporter(env)
    mod.collect()
    v = find_any_sample(mod, "worktask_queue_item_has_live_owner", "q-s19c")
    check(
        "S19c blocked stale item exempt has_live_owner absent",
        v is None,
        "expected None, got " + repr(v),
    )

    # ---- Scenario 20: arm-hook binding + live agent under a DIFFERENT qid.
    # A running item whose owning Agent is bound (arm-hook wrote a
    # queue_id -> agent_id binding) but whose active-agents record is keyed
    # under the agent's ORIGINAL spawn qid (not this item's qid) -- e.g. a
    # SendMessage-rotated qid, or active-agents lag. Without the binding
    # consult this item has NO record for its qid + a stale heartbeat, so it
    # would false-orphan (has_live_owner=0). The binding resolves the owner
    # by agent_id and reflects its LIVE flag: has_live_owner=1, no orphan.
    print("\nScenario 20: bound owner (live) under a different qid -> owned, no orphan")
    bpath = os.path.join(tmpdir, "bindings-s20.json")
    stale_iso = (datetime.now(timezone.utc) - timedelta(seconds=1200)).isoformat()
    item = make_running_item("q-bind20", "bound live different qid")
    item["registered_at"] = stale_iso
    item["started_at"] = stale_iso
    item["last_heartbeat_at"] = stale_iso
    write_queue(qjson, [item])
    write_agent_state(astate, [{
        "agent_id": "agent-b20", "queue_id": "q-orig20",
        "alive": True, "jsonl_age_seconds": 7,
    }])
    write_bindings(bpath, {"q-bind20": "agent-b20"})
    mod = load_exporter({**env, "AGENT_QUEUE_BINDINGS_JSON": bpath})
    mod.collect()
    v = find_sample(
        mod, "worktask_queue_item_has_live_owner",
        {"id": "q-bind20", "agent_id": "agent-b20"},
    )
    check("S20 bound live owner has_live_owner == 1", v == 1.0, "got " + repr(v))
    age = find_sample(
        mod, "worktask_queue_item_agent_jsonl_age_seconds",
        {"id": "q-bind20", "agent_id": "agent-b20"},
    )
    check("S20 bound owner jsonl_age emitted", age == 7.0, "got " + repr(age))
    v_empty = find_sample(
        mod, "worktask_queue_item_has_live_owner",
        {"id": "q-bind20", "agent_id": ""},
    )
    check("S20 no false empty-agent orphan series", v_empty is None,
          "expected None, got " + repr(v_empty))

    # ---- Scenario 21: bound owner whose transcript went STALE (died).
    # Binding resolves to an agent_id whose active-agents record is alive=0:
    # a genuine died-after-spawn orphan -> has_live_owner=0 WITH the agent_id.
    print("\nScenario 21: bound owner (dead transcript) -> orphan with agent_id")
    bpath = os.path.join(tmpdir, "bindings-s21.json")
    item = make_running_item("q-bind21", "bound dead")
    item["registered_at"] = stale_iso
    item["started_at"] = stale_iso
    item["last_heartbeat_at"] = stale_iso
    write_queue(qjson, [item])
    write_agent_state(astate, [{
        "agent_id": "agent-b21", "queue_id": "q-orig21",
        "alive": False, "jsonl_age_seconds": 900,
    }])
    write_bindings(bpath, {"q-bind21": "agent-b21"})
    mod = load_exporter({**env, "AGENT_QUEUE_BINDINGS_JSON": bpath})
    mod.collect()
    v = find_sample(
        mod, "worktask_queue_item_has_live_owner",
        {"id": "q-bind21", "agent_id": "agent-b21"},
    )
    check("S21 bound dead owner has_live_owner == 0", v == 0.0, "got " + repr(v))

    # ---- Scenario 22: bound owner NOT resolvable in active-agents.
    # A binding proves an agent was spawned, but no active-agents record
    # carries that agent_id yet (poll lag / between transcript writes). Even
    # with a stale heartbeat we presume alive (emit 1) rather than
    # false-orphan -- the honest "owner known, liveness ambiguous" posture.
    # Contrast S19a (NO binding + stale -> 0).
    print("\nScenario 22: bound owner unresolved in active-agents -> presume alive")
    bpath = os.path.join(tmpdir, "bindings-s22.json")
    item = make_running_item("q-bind22", "bound unresolved")
    item["registered_at"] = stale_iso
    item["started_at"] = stale_iso
    item["last_heartbeat_at"] = stale_iso
    write_queue(qjson, [item])
    write_agent_state(astate, [])
    write_bindings(bpath, {"q-bind22": "agent-b22"})
    mod = load_exporter({**env, "AGENT_QUEUE_BINDINGS_JSON": bpath})
    mod.collect()
    v = find_sample(
        mod, "worktask_queue_item_has_live_owner",
        {"id": "q-bind22", "agent_id": "agent-b22"},
    )
    check("S22 bound-unresolved presume alive has_live_owner == 1", v == 1.0,
          "got " + repr(v))
    v_empty = find_sample(
        mod, "worktask_queue_item_has_live_owner",
        {"id": "q-bind22", "agent_id": ""},
    )
    check("S22 no false empty-agent orphan series", v_empty is None,
          "expected None, got " + repr(v_empty))

    # ---- Scenario 23: container-spawned agent -- register-time agent_id
    # stamp, NO pid on the item, active-agents record keyed under a
    # DIFFERENT qid. This is the shape that produced the reported bug:
    # has_live_owner=0 with agent_id="" for an agent the minisite showed
    # alive. The stamp is the minisite's step-2 owner signal and the
    # exporter now honours it too.
    print("\nScenario 23: register-time agent_id stamp -> owned (container agent)")

    # 23a: stamped owner, live record under another qid -> 1 with agent_id.
    stale_iso = (datetime.now(timezone.utc) - timedelta(seconds=1200)).isoformat()
    item = make_running_item("q-stamp23", "container agent, stamped owner")
    item["registered_at"] = stale_iso
    item["started_at"] = stale_iso
    item["last_heartbeat_at"] = stale_iso
    item["pid"] = None  # container-spawned: no resolvable pid, by design
    item["agent_id"] = "agent-c23"
    item["agent_id_source"] = "register"
    write_queue(qjson, [item])
    write_agent_state(astate, [{
        "agent_id": "agent-c23", "queue_id": "q-orig23",
        "alive": True, "jsonl_age_seconds": 12,
    }])
    mod = load_exporter(env)
    mod.collect()
    v = find_sample(
        mod, "worktask_queue_item_has_live_owner",
        {"id": "q-stamp23", "agent_id": "agent-c23"},
    )
    check("S23a stamped live owner has_live_owner == 1", v == 1.0, "got " + repr(v))
    age = find_sample(
        mod, "worktask_queue_item_agent_jsonl_age_seconds",
        {"id": "q-stamp23", "agent_id": "agent-c23"},
    )
    check("S23a stamped owner jsonl_age emitted", age == 12.0, "got " + repr(age))
    v_empty = find_sample(
        mod, "worktask_queue_item_has_live_owner",
        {"id": "q-stamp23", "agent_id": ""},
    )
    check("S23a no false empty-agent orphan series", v_empty is None,
          "expected None, got " + repr(v_empty))

    # 23b: stamped owner whose record went stale -> genuine orphan, WITH id.
    item = make_running_item("q-stamp23b", "stamped owner died")
    item["registered_at"] = stale_iso
    item["started_at"] = stale_iso
    item["last_heartbeat_at"] = stale_iso
    item["agent_id"] = "agent-c23b"
    write_queue(qjson, [item])
    write_agent_state(astate, [{
        "agent_id": "agent-c23b", "queue_id": "q-orig23b",
        "alive": False, "jsonl_age_seconds": 1500,
    }])
    mod = load_exporter(env)
    mod.collect()
    v = find_sample(
        mod, "worktask_queue_item_has_live_owner",
        {"id": "q-stamp23b", "agent_id": "agent-c23b"},
    )
    check("S23b stale stamped owner has_live_owner == 0", v == 0.0,
          "got " + repr(v))

    # 23c: stamped owner absent from active-agents -> presume alive (1).
    item = make_running_item("q-stamp23c", "stamped owner unresolved")
    item["registered_at"] = stale_iso
    item["started_at"] = stale_iso
    item["last_heartbeat_at"] = stale_iso
    item["agent_id"] = "agent-c23c"
    write_queue(qjson, [item])
    write_agent_state(astate, [])
    mod = load_exporter(env)
    mod.collect()
    v = find_sample(
        mod, "worktask_queue_item_has_live_owner",
        {"id": "q-stamp23c", "agent_id": "agent-c23c"},
    )
    check("S23c unresolved stamped owner presumed alive == 1", v == 1.0,
          "got " + repr(v))

    # 23d: stamp BEATS a disagreeing binding (minisite precedence order).
    bpath = os.path.join(tmpdir, "bindings-s23d.json")
    item = make_running_item("q-stamp23d", "stamp beats binding")
    item["registered_at"] = stale_iso
    item["started_at"] = stale_iso
    item["last_heartbeat_at"] = stale_iso
    item["agent_id"] = "agent-stamped23d"
    write_queue(qjson, [item])
    write_agent_state(astate, [
        {"agent_id": "agent-stamped23d", "queue_id": "q-o1",
         "alive": True, "jsonl_age_seconds": 5},
        {"agent_id": "agent-bound23d", "queue_id": "q-o2",
         "alive": False, "jsonl_age_seconds": 1500},
    ])
    write_bindings(bpath, {"q-stamp23d": "agent-bound23d"})
    mod = load_exporter({**env, "AGENT_QUEUE_BINDINGS_JSON": bpath})
    mod.collect()
    v = find_sample(
        mod, "worktask_queue_item_has_live_owner",
        {"id": "q-stamp23d", "agent_id": "agent-stamped23d"},
    )
    check("S23d stamp wins over binding (== 1)", v == 1.0, "got " + repr(v))
    v_bound = find_sample(
        mod, "worktask_queue_item_has_live_owner",
        {"id": "q-stamp23d", "agent_id": "agent-bound23d"},
    )
    check("S23d binding-derived series not emitted", v_bound is None,
          "expected None, got " + repr(v_bound))

    # ---- Scenario 24: liveness comes from `alive`, NOT transcript age.
    # Post-#690 an agent inside one long foreground tool call publishes
    # alive=true with a many-minute-old transcript (in_flight_tool_use).
    # The exporter must mirror `alive` and never re-derive it from the age.
    print("\nScenario 24: in-flight tool call (old transcript, alive=true) -> owned")
    item = make_running_item("q-inflight24", "agent inside a long tool call")
    item["registered_at"] = stale_iso
    item["started_at"] = stale_iso
    item["last_heartbeat_at"] = stale_iso
    write_queue(qjson, [item])
    write_agent_state(astate, [{
        "agent_id": "agent-i24", "queue_id": "q-inflight24",
        "alive": True, "jsonl_age_seconds": 443, "in_flight_tool_use": True,
    }])
    mod = load_exporter(env)
    mod.collect()
    v = find_sample(
        mod, "worktask_queue_item_has_live_owner",
        {"id": "q-inflight24", "agent_id": "agent-i24"},
    )
    check("S24 in-flight agent has_live_owner == 1", v == 1.0, "got " + repr(v))
    age = find_sample(
        mod, "worktask_queue_item_agent_jsonl_age_seconds",
        {"id": "q-inflight24", "agent_id": "agent-i24"},
    )
    check("S24 stale transcript age still exported", age == 443.0,
          "got " + repr(age))

    # ---- Scenario 25: owner inputs ABSENT -> warning + metric absent, not 0.
    # A container that never got its bind mounts sees neither input. Every
    # running item then looks ownerless, and the old code flagged the whole
    # queue orphaned. Now: has_live_owner is ABSENT, both
    # owner_input_available series read 0, and a WARNING names each path.
    print("\nScenario 25: both owner inputs missing -> warn + suppress orphan gauge")
    missing_bindings = os.path.join(tmpdir, "no-such-bindings.json")
    item = make_running_item("q-noinput25", "stale item, no owner inputs")
    item["registered_at"] = stale_iso
    item["started_at"] = stale_iso
    item["last_heartbeat_at"] = stale_iso
    write_queue(qjson, [item])
    if os.path.exists(astate):
        os.remove(astate)
    mod = load_exporter({**env, "AGENT_QUEUE_BINDINGS_JSON": missing_bindings})
    warnings = []

    class _Capture(logging.Handler):
        def emit(self, record):
            if record.levelno >= logging.WARNING:
                warnings.append(record.getMessage())

    mod.log.addHandler(_Capture())
    mod.collect()
    v = find_any_sample(mod, "worktask_queue_item_has_live_owner", "q-noinput25")
    check(
        "S25 has_live_owner ABSENT (not 0) with no owner inputs",
        v is None,
        "expected None, got " + repr(v),
    )
    st = find_sample(
        mod, "worktask_queue_owner_input_available", {"input": "agent_state"},
    )
    check("S25 owner_input_available{agent_state} == 0", st == 0.0,
          "got " + repr(st))
    bd = find_sample(
        mod, "worktask_queue_owner_input_available",
        {"input": "agent_queue_bindings"},
    )
    check("S25 owner_input_available{agent_queue_bindings} == 0", bd == 0.0,
          "got " + repr(bd))
    check(
        "S25 loud warning names both inputs",
        sum("OWNER-ATTRIBUTION INPUT" in w for w in warnings) == 2,
        "warnings=" + repr(warnings),
    )
    check(
        "S25 warning names the state path",
        any(astate in w for w in warnings),
        "warnings=" + repr(warnings),
    )
    # Repeat scrape: state unchanged -> no NEW warnings (loud once, not spam).
    before = len(warnings)
    mod.collect()
    check("S25 repeat scrape does not re-warn", len(warnings) == before,
          "warnings grew to " + repr(warnings))

    # 25b: ONE input readable is enough -- the orphan fallback still works.
    print("\nScenario 25b: agent-state readable, bindings missing -> orphan still fires")
    write_queue(qjson, [item])
    write_agent_state(astate, [])
    mod = load_exporter({**env, "AGENT_QUEUE_BINDINGS_JSON": missing_bindings})
    mod.collect()
    v = find_sample(
        mod, "worktask_queue_item_has_live_owner",
        {"id": "q-noinput25", "agent_id": ""},
    )
    check("S25b orphan still detected with one readable input", v == 0.0,
          "got " + repr(v))
    st = find_sample(
        mod, "worktask_queue_owner_input_available", {"input": "agent_state"},
    )
    check("S25b owner_input_available{agent_state} == 1", st == 1.0,
          "got " + repr(st))

    # 25c: MALFORMED agent state + missing bindings -> same suppression.
    print("\nScenario 25c: malformed agent state -> treated as no signal")
    with open(astate, "w") as f:
        f.write("{not json at all")
    write_queue(qjson, [item])
    mod = load_exporter({**env, "AGENT_QUEUE_BINDINGS_JSON": missing_bindings})
    mod.collect()
    v = find_any_sample(mod, "worktask_queue_item_has_live_owner", "q-noinput25")
    check("S25c malformed state -> has_live_owner absent", v is None,
          "expected None, got " + repr(v))
    st = find_sample(
        mod, "worktask_queue_owner_input_available", {"input": "agent_state"},
    )
    check("S25c owner_input_available{agent_state} == 0 on malformed", st == 0.0,
          "got " + repr(st))
    # Restore a good state file for anything appended after this point.
    write_agent_state(astate, [])

    print()
    if failures:
        print(f"FAILED: {len(failures)} test(s)")
        for n, m in failures:
            print(f"  - {n}: {m}")
        sys.exit(1)
    print("OK: all scenarios passed")


if __name__ == "__main__":
    run_scenarios()
