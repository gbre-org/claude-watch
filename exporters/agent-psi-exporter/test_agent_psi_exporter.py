#!/usr/bin/env python3
"""Tests for agent-psi: the interval categorizer, per-agent duty-cycle, and
the two-agent some/full pressure math, plus one end-to-end scrape.

The categorizer and pressure math in ``agent_psi.py`` are pure functions, so
each case runs against a synthetic transcript (a list of JSONL-shaped dicts)
or directly-built intervals — no real ~/.claude is ever read except in the
final end-to-end scenario, which uses a throwaway tmpdir.

Scenarios:

  (1) Categorizer: a prompt -> inference -> tool -> inference -> tool ->
      end_turn transcript folds into the expected inference / tool seconds and
      duty-cycle ratios.
  (2) waiting_human + the max-gap idle cap: a long gap before a human prompt
      is waiting_human; a long gap that ends at an assistant turn is capped to
      idle, not counted as a multi-minute inference stall.
  (3) Trailing open interval: an in-flight tool (dispatched, no result yet)
      reads as current tool time to `now`; a returned end_turn reads as idle;
      a pending next turn (last line a tool_result) reads as inference.
  (4) Two-agent fleet some/full: A always on inference, B on tool then
      inference -> some/full match the hand-computed fractions, and an idle
      agent does NOT block `full`.
  (5) End-to-end scrape: a synthetic projects dir with a main-loop transcript
      and one sub-agent transcript -> agent_psi_live_agents=2, pressure gauges
      present and in [0,1], build_info emitted.

Run:  python3 test_agent_psi_exporter.py
Exits 0 on success, 1 on first failure with a diagnostic.
"""

import json
import os
import sys
import tempfile
from datetime import datetime, timezone
from importlib.util import spec_from_file_location, module_from_spec

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

import agent_psi  # noqa: E402


# --- fixtures ------------------------------------------------------------
def iso(epoch):
    return (
        datetime.fromtimestamp(epoch, tz=timezone.utc)
        .isoformat()
        .replace("+00:00", "Z")
    )


def assistant(ts, *, tool_use_ids=(), stop_reason=None):
    content = [{"type": "text", "text": "..."}]
    for tid in tool_use_ids:
        content.append({"type": "tool_use", "id": tid, "name": "Bash"})
    if stop_reason is None:
        stop_reason = "tool_use" if tool_use_ids else "end_turn"
    return {
        "type": "assistant",
        "timestamp": iso(ts),
        "message": {"role": "assistant", "content": content, "stop_reason": stop_reason},
    }


def tool_result(ts, tool_use_id):
    return {
        "type": "user",
        "timestamp": iso(ts),
        "toolUseResult": {"stdout": "ok"},
        "message": {
            "role": "user",
            "content": [{"type": "tool_result", "tool_use_id": tool_use_id}],
        },
    }


def prompt(ts, text="hi"):
    return {
        "type": "user",
        "timestamp": iso(ts),
        "message": {"role": "user", "content": [{"type": "text", "text": text}]},
    }


def approx(a, b, tol=1e-6):
    return abs(a - b) <= tol


def run():
    failures = []

    def check(name, ok, msg=""):
        if ok:
            print(f"  PASS: {name}")
        else:
            print(f"  FAIL: {name} -- {msg}")
            failures.append(name)

    # ---- Scenario 1: categorizer basic ---------------------------------
    print("\nScenario 1: categorizer folds a mixed transcript correctly")
    entries = [
        prompt(0),
        assistant(2, tool_use_ids=["X"]),   # inference [0,2] = 2
        tool_result(5, "X"),                # tool [2,5] = 3
        assistant(6, tool_use_ids=["Y"]),   # inference [5,6] = 1
        tool_result(16, "Y"),               # tool [6,16] = 10
        assistant(17),                       # inference [16,17] = 1 (end_turn)
    ]
    ivs = agent_psi.parse_intervals(entries, now=None)
    secs, total, active, ratios = agent_psi.duty_cycle(ivs)
    check("inference seconds", approx(secs[agent_psi.INFERENCE], 4.0),
          f"got {secs[agent_psi.INFERENCE]}")
    check("tool seconds", approx(secs[agent_psi.TOOL], 13.0),
          f"got {secs[agent_psi.TOOL]}")
    check("no idle/waiting/overhead",
          approx(secs[agent_psi.IDLE], 0) and approx(secs[agent_psi.WAITING_HUMAN], 0)
          and approx(secs[agent_psi.OVERHEAD], 0),
          f"got {secs}")
    check("active == total (no idle)", approx(active, 17.0), f"got {active}")
    check("duty inference ratio", approx(ratios[agent_psi.INFERENCE], 4.0 / 17.0),
          f"got {ratios[agent_psi.INFERENCE]}")
    check("duty tool ratio", approx(ratios[agent_psi.TOOL], 13.0 / 17.0),
          f"got {ratios[agent_psi.TOOL]}")

    # ---- Scenario 2: waiting_human + idle cap --------------------------
    print("\nScenario 2: waiting_human, and the max-gap cap -> idle")
    entries = [
        prompt(0),
        assistant(2, tool_use_ids=["X"]),   # inference 2
        tool_result(4, "X"),                # tool 2
        assistant(5),                        # inference 1 (end_turn)
        prompt(105),                         # waiting_human 100 (prev=asst, <cap)
        assistant(107),                      # inference 2
    ]
    secs, total, active, ratios = agent_psi.duty_cycle(
        agent_psi.parse_intervals(entries, now=None)
    )
    check("waiting_human = 100", approx(secs[agent_psi.WAITING_HUMAN], 100.0),
          f"got {secs[agent_psi.WAITING_HUMAN]}")
    check("inference = 5", approx(secs[agent_psi.INFERENCE], 5.0),
          f"got {secs[agent_psi.INFERENCE]}")
    check("active excludes waiting_human", approx(active, 7.0), f"got {active}")

    # A long gap ending at an ASSISTANT turn is capped to idle, not inference.
    capped = [tool_result(0, "Z"), assistant(400)]  # d=400 > 300 default cap
    secs2 = agent_psi.duty_seconds(agent_psi.parse_intervals(capped, now=None))
    check("capped gap -> idle not inference",
          approx(secs2[agent_psi.IDLE], 400.0) and approx(secs2[agent_psi.INFERENCE], 0.0),
          f"got {secs2}")
    # A long gap ending at a tool_result stays tool (a slow build is real).
    longtool = [assistant(0, tool_use_ids=["B"]), tool_result(600, "B")]
    secs3 = agent_psi.duty_seconds(agent_psi.parse_intervals(longtool, now=None))
    check("long tool stays tool (uncapped)", approx(secs3[agent_psi.TOOL], 600.0),
          f"got {secs3}")

    # ---- Scenario 3: trailing open interval ----------------------------
    print("\nScenario 3: trailing open interval = the agent's current state")
    # In-flight tool: dispatched, no result yet -> tool up to now.
    inflight = [prompt(0), assistant(2, tool_use_ids=["Q"])]
    secs = agent_psi.duty_seconds(agent_psi.parse_intervals(inflight, now=12))
    check("in-flight tool tail", approx(secs[agent_psi.TOOL], 10.0), f"got {secs}")
    # Returned end_turn -> idle tail.
    returned = [prompt(0), assistant(2)]
    secs = agent_psi.duty_seconds(agent_psi.parse_intervals(returned, now=7))
    check("returned end_turn tail -> idle", approx(secs[agent_psi.IDLE], 5.0),
          f"got {secs}")
    # Pending next turn (last line a tool_result) -> inference tail.
    pending = [assistant(0, tool_use_ids=["R"]), tool_result(1, "R")]
    secs = agent_psi.duty_seconds(agent_psi.parse_intervals(pending, now=11))
    check("pending next turn tail -> inference", approx(secs[agent_psi.INFERENCE], 10.0),
          f"got {secs}")

    # ---- Scenario 4: two-agent some/full -------------------------------
    print("\nScenario 4: two-agent fleet some/full")
    I = agent_psi.Interval
    a = [I(0, 10, agent_psi.INFERENCE)]
    b = [I(0, 5, agent_psi.TOOL), I(5, 10, agent_psi.INFERENCE)]
    p = agent_psi.compute_pressure({"a": a, "b": b}, 0, 10)
    check("some_inference = 1.0", approx(p[(agent_psi.INFERENCE, "some")], 1.0),
          f"got {p[(agent_psi.INFERENCE, 'some')]}")
    check("full_inference = 0.5", approx(p[(agent_psi.INFERENCE, "full")], 0.5),
          f"got {p[(agent_psi.INFERENCE, 'full')]}")
    check("some_tool = 0.5", approx(p[(agent_psi.TOOL, "some")], 0.5),
          f"got {p[(agent_psi.TOOL, 'some')]}")
    check("full_tool = 0.0", approx(p[(agent_psi.TOOL, "full")], 0.0),
          f"got {p[(agent_psi.TOOL, 'full')]}")

    # An idle agent must NOT block full: A inference, C idle -> full_inference=1.
    c = [I(0, 10, agent_psi.IDLE)]
    p2 = agent_psi.compute_pressure({"a": a, "c": c}, 0, 10)
    check("idle agent doesn't block full_inference",
          approx(p2[(agent_psi.INFERENCE, "full")], 1.0),
          f"got {p2[(agent_psi.INFERENCE, 'full')]}")

    # ---- Scenario 5: end-to-end scrape ---------------------------------
    print("\nScenario 5: end-to-end scrape over a synthetic projects dir")
    import time

    tmp = tempfile.mkdtemp(prefix="agent-psi-test-")
    slug = os.path.join(tmp, "-home-someone")
    sess = "abcd1234-0000-0000-0000-000000000000"
    os.makedirs(os.path.join(slug, sess, "subagents"))
    now = time.time()
    # Main-loop transcript: recent activity so it is "live".
    main_entries = [
        prompt(now - 8),
        assistant(now - 6, tool_use_ids=["M"]),
        tool_result(now - 4, "M"),
        assistant(now - 2),
    ]
    with open(os.path.join(slug, f"{sess}.jsonl"), "w") as fh:
        for e in main_entries:
            fh.write(json.dumps(e) + "\n")
    sub_entries = [
        prompt(now - 9),
        assistant(now - 7, tool_use_ids=["S"]),
        tool_result(now - 1, "S"),
    ]
    with open(os.path.join(slug, sess, "subagents", "agent-deadbeef.jsonl"), "w") as fh:
        for e in sub_entries:
            fh.write(json.dumps(e) + "\n")

    os.environ["CLAUDE_PROJECTS_DIR"] = tmp
    os.environ["PORT"] = "0"
    spec = spec_from_file_location(
        "agent_psi_exporter_under_test",
        os.path.join(HERE, "agent_psi_exporter.py"),
    )
    mod = module_from_spec(spec)
    spec.loader.exec_module(mod)
    mod.collect()

    def sample(name, **labels):
        for fam in mod.REG.collect():
            for s in fam.samples:
                if s.name != name:
                    continue
                if labels and any(s.labels.get(k) != v for k, v in labels.items()):
                    continue
                return s.value
        return None

    live = sample("agent_psi_live_agents")
    check("live_agents = 2", live == 2, f"got {live}")
    fleet_agents = sample("agent_psi_scope_agents", scope="fleet")
    check("fleet scope has 2 agents", fleet_agents == 2, f"got {fleet_agents}")
    some = sample("agent_psi_inference_some", scope="fleet", window="60")
    check("fleet inference some emitted in [0,1]",
          some is not None and 0.0 <= some <= 1.0, f"got {some}")
    session_scope = sample("agent_psi_scope_agents", scope="session:abcd1234")
    check("session subtree scope emitted", session_scope == 2, f"got {session_scope}")
    build = sample("agent_psi_exporter_build_info", commit="unknown",
                   version="0.0.0", source="host")
    check("build_info emitted", build == 1, f"got {build}")

    # ---- summary -------------------------------------------------------
    print()
    if failures:
        print(f"FAILED ({len(failures)}): {', '.join(failures)}")
        return 1
    print("ALL PASS")
    return 0


if __name__ == "__main__":
    sys.exit(run())
