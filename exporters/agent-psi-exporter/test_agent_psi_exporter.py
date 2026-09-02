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
  (2) waiting_human + the max-gap idle cap, and the phase-2 idle-uncap: a long
      gap before a human prompt is waiting_human; a long closed gap ending at an
      assistant turn is capped to idle; a long PARKED idle tail is NOT capped
      (the main-loop idle undercount fix); a long blocking-Bash tool tail stays
      tool; an inference tail past the API-stall ceiling is still capped to
      idle.
  (3) Trailing open interval: an in-flight tool (dispatched, no result yet)
      reads as current tool time to `now`; a returned end_turn reads as idle;
      a pending next turn (last line a tool_result) reads as inference.
  (4) Two-agent fleet some/full: A always on inference, B on tool then
      inference -> some/full match the hand-computed fractions, and an idle
      agent does NOT block `full`.
  (4b) Model family extraction (opus/sonnet/<synthetic>/unknown) and per-model
      some/full = the fleet math restricted to one model's agents.
  (4c) Overhead some/full: the same math as the stall categories, an idle agent
      doesn't block overhead full, an agent in overhead is ACTIVE (so it breaks
      inference_full), and the three pressure categories partition active time.
  (5) End-to-end scrape: a synthetic projects dir with a main-loop transcript
      (fable) and one sub-agent transcript (opus) -> live_agents=1 (subagents
      only, main excluded), fleet scope excludes the main loop, a separate
      `main` scope, per-model fleet lines (opus present, fable absent),
      pressure gauges in [0,1] (inference/tool/overhead AND the stalled
      subset), build_info emitted.
  (6) Inference-gap throughput split: a fast-tokens gap is productive, a long
      low-token gap is stalled, a sub-min-gap gap is never stalled, a gap with
      no output_tokens is not judged, and the zero-duration guard holds.
  (7) Stalled-inference some/full mirrors the inference some/full math (idle
      agents don't block full), and demonstrates the disentangling: an
      all-productive fleet has inference_full=1 but stalled_full=0.
  (8) live_agents reflects STILL-RUNNING sub-agents (transcript not ended in a
      completed final turn), not merely file-recent: a finished agent drops
      immediately, a mid-tool-wait agent stays live.
  (9) API retry back-off reads as stall: a long-silent in-flight turn is
      stalled inference (and lifts stalled_full), a brief blip is not, a gap
      ending at an API-error entry escapes the dormancy cap and is stalled
      while the same gap without the marker does not, and both rules stop at
      the API-stall ceiling.

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


def assistant(ts, *, tool_use_ids=(), stop_reason=None, model=None,
              output_tokens=None):
    content = [{"type": "text", "text": "..."}]
    for tid in tool_use_ids:
        content.append({"type": "tool_use", "id": tid, "name": "Bash"})
    if stop_reason is None:
        stop_reason = "tool_use" if tool_use_ids else "end_turn"
    msg = {"role": "assistant", "content": content, "stop_reason": stop_reason}
    if model is not None:
        msg["model"] = model
    if output_tokens is not None:
        # Mirror the real transcript shape: usage.output_tokens already folds in
        # output_tokens_details.thinking_tokens.
        msg["usage"] = {
            "output_tokens": output_tokens,
            "output_tokens_details": {"thinking_tokens": 0},
        }
    return {
        "type": "assistant",
        "timestamp": iso(ts),
        "message": msg,
    }


def api_error(ts, text="API Error: 529 Overloaded", output_tokens=0):
    """The synthetic assistant line Claude Code writes when a request finally
    fails: model "<synthetic>", isApiErrorMessage true, zero output tokens."""
    e = assistant(ts, model="<synthetic>", stop_reason="stop_sequence",
                  output_tokens=output_tokens)
    e["message"]["content"] = [{"type": "text", "text": text}]
    e["isApiErrorMessage"] = True
    return e


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


def bookkeeping(ts, entry_type="system"):
    """A line that is neither an assistant turn nor a user message (system /
    queue-operation / attachment / ...): the loop's own between-turn
    bookkeeping, which bounds an ``overhead`` gap."""
    return {"type": entry_type, "timestamp": iso(ts), "content": "..."}


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

    # A parked main loop's between-turn idle tail is NOT capped: a returned
    # end_turn followed by a long wait to `now` counts as full-length idle, not
    # truncated to max_gap (the main-loop idle undercount fix, #4029/#4031).
    parked = [prompt(0), assistant(2)]  # end_turn, then parked
    secs_idle = agent_psi.duty_seconds(
        agent_psi.parse_intervals(parked, now=2 + 1000)
    )
    check("long parked idle tail is uncapped (not max_gap)",
          approx(secs_idle[agent_psi.IDLE], 1000.0),
          f"got {secs_idle}")
    # A foreground blocking Bash wait is the loop IN a tool_use: a dispatched-
    # but-unfinished tool stays TOOL to `now` even past max_gap — it is NOT idle.
    blocking_wait = [prompt(0), assistant(2, tool_use_ids=["W"])]  # no result yet
    secs_tool = agent_psi.duty_seconds(
        agent_psi.parse_intervals(blocking_wait, now=2 + 1000)
    )
    check("long blocking-bash wait stays tool (not idle)",
          approx(secs_tool[agent_psi.TOOL], 1000.0)
          and approx(secs_tool[agent_psi.IDLE], 0.0),
          f"got {secs_tool}")
    # An inference tail past the API-stall ceiling (and any overhead tail past
    # max_gap) is still capped to idle: a dormant / resumed-session gap, not a
    # model stall. Between the API-stall threshold and that ceiling it reads as
    # stalled inference instead -- scenario 9.
    dormant = [assistant(0, tool_use_ids=["R"]), tool_result(1, "R")]  # pending turn
    secs_dorm = agent_psi.duty_seconds(
        agent_psi.parse_intervals(dormant, now=1 + 1000)
    )
    check("inference tail past the API-stall ceiling still capped to idle",
          approx(secs_dorm[agent_psi.IDLE], 300.0)
          and approx(secs_dorm[agent_psi.INFERENCE], 0.0),
          f"got {secs_dorm}")

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

    # ---- Scenario 4b: model extraction + per-model fleet pressure ------
    print("\nScenario 4b: model family extraction and per-model some/full")
    check("family from claude-opus-5",
          agent_psi.model_family("claude-opus-5") == "opus",
          f"got {agent_psi.model_family('claude-opus-5')}")
    check("family from bare sonnet",
          agent_psi.model_family("sonnet") == "sonnet",
          f"got {agent_psi.model_family('sonnet')}")
    check("<synthetic> is not a model",
          agent_psi.model_family("<synthetic>") is None,
          f"got {agent_psi.model_family('<synthetic>')}")
    check("empty/None is not a model",
          agent_psi.model_family("") is None and agent_psi.model_family(None) is None,
          "got a family")
    # Dominant real family wins over a stray synthetic line.
    mixed = [
        assistant(0, model="<synthetic>"),
        assistant(1, model="claude-opus-4-8"),
        assistant(2, model="claude-opus-5"),
    ]
    check("extract_model picks dominant real family",
          agent_psi.extract_model(mixed) == "opus",
          f"got {agent_psi.extract_model(mixed)}")
    check("extract_model with no real model -> unknown",
          agent_psi.extract_model([assistant(0, model="<synthetic>")])
          == agent_psi.UNKNOWN_MODEL,
          f"got {agent_psi.extract_model([assistant(0, model='<synthetic>')])}")

    # Per-model some/full is the fleet math restricted to one model's agents.
    # Two opus workers both on inference over [0,10] -> full=1.0 for opus; a
    # lone sonnet worker on tool -> some_tool=1.0 / full_inference=0 for sonnet.
    opus_members = {
        "o1": [I(0, 10, agent_psi.INFERENCE)],
        "o2": [I(0, 10, agent_psi.INFERENCE)],
    }
    p_opus = agent_psi.compute_pressure(opus_members, 0, 10)
    check("per-model opus full_inference = 1.0",
          approx(p_opus[(agent_psi.INFERENCE, "full")], 1.0),
          f"got {p_opus[(agent_psi.INFERENCE, 'full')]}")
    sonnet_members = {"s1": [I(0, 10, agent_psi.TOOL)]}
    p_sonnet = agent_psi.compute_pressure(sonnet_members, 0, 10)
    check("per-model sonnet some_tool = 1.0 / full_inference = 0",
          approx(p_sonnet[(agent_psi.TOOL, "some")], 1.0)
          and approx(p_sonnet[(agent_psi.INFERENCE, "full")], 0.0),
          f"got {p_sonnet}")

    # ---- Scenario 4c: overhead some/full -------------------------------
    print("\nScenario 4c: overhead some/full (the non-stall active category)")
    check("overhead is a pressure category but not a stall category",
          agent_psi.OVERHEAD in agent_psi.PRESSURE_CATEGORIES
          and agent_psi.OVERHEAD not in agent_psi.STALL_CATEGORIES,
          f"got {agent_psi.PRESSURE_CATEGORIES} / {agent_psi.STALL_CATEGORIES}")

    # A bookkeeping line bounds an overhead gap in a real transcript.
    booked = [prompt(0), assistant(2), bookkeeping(6)]
    secs_book = agent_psi.duty_seconds(agent_psi.parse_intervals(booked, now=None))
    check("gap ending at a bookkeeping line -> overhead",
          approx(secs_book[agent_psi.OVERHEAD], 4.0), f"got {secs_book}")

    # Same shape as scenario 4, one category over: A always overhead, B tool
    # then overhead -> some=1.0 over the window, full only where both overlap.
    a_o = [I(0, 10, agent_psi.OVERHEAD)]
    b_o = [I(0, 5, agent_psi.TOOL), I(5, 10, agent_psi.OVERHEAD)]
    po = agent_psi.compute_pressure({"a": a_o, "b": b_o}, 0, 10)
    check("some_overhead = 1.0", approx(po[(agent_psi.OVERHEAD, "some")], 1.0),
          f"got {po[(agent_psi.OVERHEAD, 'some')]}")
    check("full_overhead = 0.5", approx(po[(agent_psi.OVERHEAD, "full")], 0.5),
          f"got {po[(agent_psi.OVERHEAD, 'full')]}")

    # An idle agent must NOT block full_overhead (mirrors the inference rule).
    po2 = agent_psi.compute_pressure({"a": a_o, "c": c}, 0, 10)
    check("idle agent doesn't block full_overhead",
          approx(po2[(agent_psi.OVERHEAD, "full")], 1.0),
          f"got {po2[(agent_psi.OVERHEAD, 'full')]}")

    # An agent IN overhead is active-but-not-stalled, so it breaks
    # full_inference — overhead is emitted, not folded into the stalls.
    po3 = agent_psi.compute_pressure({"a": a, "o": a_o}, 0, 10)
    check("overhead agent breaks full_inference",
          approx(po3[(agent_psi.INFERENCE, "full")], 0.0)
          and approx(po3[(agent_psi.INFERENCE, "some")], 1.0),
          f"got {po3}")

    # The three pressure categories partition ACTIVE time: for one always-active
    # agent (serial, so some == full), the three some values sum to 1.0 — the
    # accounting that emitting overhead buys the fleet panels.
    serial = {
        "s": [
            I(0, 4, agent_psi.INFERENCE),
            I(4, 7, agent_psi.TOOL),
            I(7, 10, agent_psi.OVERHEAD),
        ]
    }
    ps_serial = agent_psi.compute_pressure(serial, 0, 10)
    total_some = sum(
        ps_serial[(cat, "some")] for cat in agent_psi.PRESSURE_CATEGORIES
    )
    check("inference+tool+overhead some partitions active time (sums to 1.0)",
          approx(total_some, 1.0)
          and approx(ps_serial[(agent_psi.OVERHEAD, "some")], 0.3)
          and approx(ps_serial[(agent_psi.OVERHEAD, "full")], 0.3),
          f"got {ps_serial}")

    # ---- Scenario 6: productive vs stalled inference classification ----
    print("\nScenario 6: inference-gap throughput -> productive / stalled")

    def only_inference(entries):
        ivs = agent_psi.parse_intervals(entries, now=None)
        inf = [iv for iv in ivs if iv.category == agent_psi.INFERENCE]
        assert len(inf) == 1, f"expected 1 inference interval, got {inf}"
        return inf[0]

    # Fast tokens over a multi-second gap -> productive (stalled=False).
    fast = only_inference([tool_result(0, "X"), assistant(10, output_tokens=500)])
    check("fast-tokens inference gap is productive (50 tok/s)",
          fast.stalled is False, f"got stalled={fast.stalled}")
    # Long gap, few tokens -> stalled (2 tok/s < 8 floor).
    slow = only_inference([tool_result(0, "X"), assistant(30, output_tokens=60)])
    check("long low-token inference gap is stalled (2 tok/s)",
          slow.stalled is True, f"got stalled={slow.stalled}")
    # Tiny gap below the min-gap guard is never stalled, even at 0.5 tok/s.
    tiny = only_inference([tool_result(0, "X"), assistant(2, output_tokens=1)])
    check("sub-min-gap inference gap is never stalled (guard)",
          tiny.stalled is False, f"got stalled={tiny.stalled}")
    # No usage datum -> not judged stalled (no evidence, no false alarm).
    notok = only_inference([tool_result(0, "X"), assistant(30)])
    check("inference gap with no output_tokens is not stalled",
          notok.stalled is False, f"got stalled={notok.stalled}")
    # Direct divide-by-zero / degenerate-duration guard on the helper.
    check("_is_stalled_inference guards zero duration",
          agent_psi._is_stalled_inference(0.0, 100, 8.0, 5.0) is False,
          "zero-duration gap classified stalled")

    # ---- Scenario 7: stalled-inference some/full pressure --------------
    print("\nScenario 7: stalled-inference some/full mirrors inference some/full")
    # A stalled the whole window; B productive [0,5] then stalled [5,10].
    a_s = [I(0, 10, agent_psi.INFERENCE, True)]
    b_s = [I(0, 5, agent_psi.INFERENCE, False), I(5, 10, agent_psi.INFERENCE, True)]
    ps = agent_psi.compute_stalled_inference_pressure({"a": a_s, "b": b_s}, 0, 10)
    check("stalled some = 1.0 (>=1 agent stalled throughout)",
          approx(ps["some"], 1.0), f"got {ps['some']}")
    check("stalled full = 0.5 (both stalled only in [5,10])",
          approx(ps["full"], 0.5), f"got {ps['full']}")
    # An idle agent must NOT block stalled full (mirrors the inference_full rule).
    idle = [I(0, 10, agent_psi.IDLE)]
    ps2 = agent_psi.compute_stalled_inference_pressure({"a": a_s, "c": idle}, 0, 10)
    check("idle agent doesn't block stalled full",
          approx(ps2["full"], 1.0), f"got {ps2['full']}")
    # The disentangling: a fleet all PRODUCTIVELY on inference has inference_full
    # = 1.0 but stalled_full = 0 (this is exactly what the split buys us).
    prod = {
        "a": [I(0, 10, agent_psi.INFERENCE, False)],
        "b": [I(0, 10, agent_psi.INFERENCE, False)],
    }
    p_inf = agent_psi.compute_pressure(prod, 0, 10)
    p_stall = agent_psi.compute_stalled_inference_pressure(prod, 0, 10)
    check("all-productive fleet: inference_full=1 but stalled_full=0",
          approx(p_inf[(agent_psi.INFERENCE, "full")], 1.0)
          and approx(p_stall["full"], 0.0),
          f"got inference_full={p_inf[(agent_psi.INFERENCE, 'full')]} "
          f"stalled_full={p_stall['full']}")

    # ---- Scenario 5: end-to-end scrape ---------------------------------
    print("\nScenario 5: end-to-end scrape over a synthetic projects dir")
    import time

    tmp = tempfile.mkdtemp(prefix="agent-psi-test-")
    slug = os.path.join(tmp, "-home-someone")
    sess = "abcd1234-0000-0000-0000-000000000000"
    os.makedirs(os.path.join(slug, sess, "subagents"))
    now = time.time()
    # Main-loop transcript: recent activity so it is "live". Runs on fable, and
    # ends with a bookkeeping line so the scope has real OVERHEAD time.
    main_entries = [
        prompt(now - 8),
        assistant(now - 6, tool_use_ids=["M"], model="claude-fable-5"),
        tool_result(now - 4, "M"),
        assistant(now - 2, model="claude-fable-5"),
        bookkeeping(now - 1),
    ]
    with open(os.path.join(slug, f"{sess}.jsonl"), "w") as fh:
        for e in main_entries:
            fh.write(json.dumps(e) + "\n")
    # Sub-agent transcript: a worker on opus.
    sub_entries = [
        prompt(now - 9),
        assistant(now - 7, tool_use_ids=["S"], model="claude-opus-5"),
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

    # live_agents counts SUB-AGENTS ONLY — the main loop is excluded (#4043).
    live = sample("agent_psi_live_agents")
    check("live_agents = 1 (subagents only, main excluded)", live == 1,
          f"got {live}")
    # fleet scope = subagents only, main-loop on its own scope (#4041).
    fleet_agents = sample("agent_psi_scope_agents", scope="fleet", model="all")
    check("fleet scope has 1 agent (main excluded)", fleet_agents == 1,
          f"got {fleet_agents}")
    main_agents = sample("agent_psi_scope_agents", scope="main", model="all")
    check("main scope has 1 agent", main_agents == 1, f"got {main_agents}")
    some = sample("agent_psi_inference_some", scope="fleet", window="60",
                  model="all")
    check("fleet inference some emitted in [0,1]",
          some is not None and 0.0 <= some <= 1.0, f"got {some}")
    main_full = sample("agent_psi_inference_full", scope="main", window="60",
                       model="all")
    check("main inference_full emitted in [0,1]",
          main_full is not None and 0.0 <= main_full <= 1.0, f"got {main_full}")
    # Per-model fleet line for the opus worker (main's fable must NOT appear on
    # the fleet scope).
    opus_fleet = sample("agent_psi_scope_agents", scope="fleet", model="opus")
    check("per-model fleet line for opus worker", opus_fleet == 1,
          f"got {opus_fleet}")
    fable_fleet = sample("agent_psi_scope_agents", scope="fleet", model="fable")
    check("main-loop model (fable) NOT on fleet scope", fable_fleet is None,
          f"got {fable_fleet}")
    opus_full = sample("agent_psi_inference_full", scope="fleet", window="60",
                       model="opus")
    check("per-model opus inference_full emitted in [0,1]",
          opus_full is not None and 0.0 <= opus_full <= 1.0, f"got {opus_full}")
    # Overhead pressure is emitted at scope level with the same label scheme —
    # the fleet/main panels' "tool use AND overhead" series. The main loop's
    # trailing bookkeeping makes its 60s overhead_full strictly positive, so
    # this is not merely an "the series exists" check.
    overhead_some = sample("agent_psi_overhead_some", scope="fleet",
                           window="60", model="all")
    check("fleet overhead_some emitted in [0,1]",
          overhead_some is not None and 0.0 <= overhead_some <= 1.0,
          f"got {overhead_some}")
    overhead_full_main = sample("agent_psi_overhead_full", scope="main",
                                window="60", model="all")
    check("main overhead_full emitted and positive (bookkeeping tail)",
          overhead_full_main is not None and 0.0 < overhead_full_main <= 1.0,
          f"got {overhead_full_main}")
    overhead_opus = sample("agent_psi_overhead_full", scope="fleet",
                           window="60", model="opus")
    check("per-model overhead_full emitted in [0,1]",
          overhead_opus is not None and 0.0 <= overhead_opus <= 1.0,
          f"got {overhead_opus}")

    # Stalled-inference gauges are emitted with the same scope/window/model
    # label scheme, values in [0,1] (the synthetic worker has no output_tokens
    # so it is not judged stalled -> 0, but the series must exist).
    stalled_some = sample("agent_psi_inference_stalled_some", scope="fleet",
                          window="60", model="all")
    check("fleet inference_stalled_some emitted in [0,1]",
          stalled_some is not None and 0.0 <= stalled_some <= 1.0,
          f"got {stalled_some}")
    stalled_full_main = sample("agent_psi_inference_stalled_full", scope="main",
                               window="60", model="all")
    check("main inference_stalled_full emitted in [0,1]",
          stalled_full_main is not None and 0.0 <= stalled_full_main <= 1.0,
          f"got {stalled_full_main}")
    session_scope = sample("agent_psi_scope_agents", scope="session:abcd1234",
                           model="all")
    check("session subtree scope emitted", session_scope == 2, f"got {session_scope}")
    build = sample("agent_psi_exporter_build_info", commit="unknown",
                   version="0.0.0", source="host")
    check("build_info emitted", build == 1, f"got {build}")

    # ---- Scenario 8: live_agents = still-running, not file-recent ------
    print("\nScenario 8: live_agents reflects still-running agents")
    # is_running by trailing state (pure).
    finished = [prompt(0), assistant(2, tool_use_ids=["X"]),
                tool_result(4, "X"), assistant(5)]  # ends end_turn, no pending
    check("finished agent (trailing end_turn) is NOT running",
          agent_psi.is_running_transcript(finished) is False, "reported running")
    mid_tool = [prompt(0), assistant(2, tool_use_ids=["W"])]  # tool in flight
    check("mid-tool-wait agent stays running",
          agent_psi.is_running_transcript(mid_tool) is True, "reported finished")
    pending_turn = [assistant(0, tool_use_ids=["R"]), tool_result(1, "R")]
    check("trailing tool_result (next turn pending) is running",
          agent_psi.is_running_transcript(pending_turn) is True,
          "reported finished")
    check("empty transcript is not running",
          agent_psi.is_running_transcript([]) is False, "reported running")

    # End-to-end: a running worker + a FINISHED worker, both file-recent within
    # the live window -> live_agents drops the finished one immediately (1), yet
    # both still count toward fleet scope membership (2), because the finished
    # one legitimately contributed to the trailing window.
    tmp2 = tempfile.mkdtemp(prefix="agent-psi-live-")
    slug2 = os.path.join(tmp2, "-home-someone")
    sess2 = "beef5678-0000-0000-0000-000000000000"
    subs2 = os.path.join(slug2, sess2, "subagents")
    os.makedirs(subs2)
    now2 = time.time()
    running_worker = [
        prompt(now2 - 8),
        assistant(now2 - 6, tool_use_ids=["A"], model="claude-opus-5"),
        tool_result(now2 - 1, "A"),  # trailing tool_result -> running
    ]
    finished_worker = [
        prompt(now2 - 40),
        assistant(now2 - 38, tool_use_ids=["B"], model="claude-opus-5"),
        tool_result(now2 - 35, "B"),
        assistant(now2 - 30, model="claude-opus-5"),  # end_turn -> finished
    ]
    with open(os.path.join(subs2, "agent-1111.jsonl"), "w") as fh:
        for e in running_worker:
            fh.write(json.dumps(e) + "\n")
    with open(os.path.join(subs2, "agent-2222.jsonl"), "w") as fh:
        for e in finished_worker:
            fh.write(json.dumps(e) + "\n")
    live_ts = agent_psi.collect_live_transcripts(tmp2, now2)
    running_ct = sum(1 for t in live_ts if not t.is_main_loop and t.running)
    filerecent_ct = sum(1 for t in live_ts if not t.is_main_loop)
    check("both workers are file-recent (in live window)", filerecent_ct == 2,
          f"got {filerecent_ct}")
    check("only the running worker counts as live", running_ct == 1,
          f"got {running_ct}")

    # ---- Scenario 9: API retry back-off reads as stall -----------------
    print("\nScenario 9: API retry back-off counts as stalled inference")
    TAIL = agent_psi.DEFAULT_API_STALL_TAIL_SECONDS   # 120
    AMAX = agent_psi.DEFAULT_API_STALL_MAX_SECONDS    # 900

    def tail_iv(entries, now):
        """The trailing open interval of a parsed transcript."""
        ivs = agent_psi.parse_intervals(entries, now=now)
        return ivs[-1] if ivs else None

    # The reported case: a turn dispatched, then the client sits in
    # "Waiting for API response - will retry in 1m 14s" writing nothing.
    stuck = [assistant(0, tool_use_ids=["R"]), tool_result(1, "R")]
    iv = tail_iv(stuck, now=1 + 400)
    check("in-flight turn silent 400s -> stalled inference at true length",
          iv is not None and iv.category == agent_psi.INFERENCE and iv.stalled
          and approx(iv.end - iv.start, 400.0),
          f"got {iv}")

    # ... and it shows up in the stalled pressure the dashboards read.
    p = agent_psi.compute_stalled_inference_pressure(
        {"stuck": agent_psi.parse_intervals(stuck, now=1 + 400)}, 1 + 100, 1 + 400
    )
    check("stuck agent -> stalled_full = 1.0", approx(p["full"], 1.0),
          f"got {p}")

    # Hysteresis: a brief retry blip / a normal turn is NOT a stall. Measured
    # p99 of real inference gaps is 29s, max 68s -- all well under the 120s
    # threshold.
    iv = tail_iv(stuck, now=1 + 30)
    check("brief in-flight turn (30s) -> inference, NOT stalled",
          iv is not None and iv.category == agent_psi.INFERENCE
          and not iv.stalled and approx(iv.end - iv.start, 30.0),
          f"got {iv}")
    iv = tail_iv(stuck, now=1 + TAIL - 1)
    check("just under the tail threshold -> not stalled",
          iv is not None and not iv.stalled, f"got {iv}")

    # Past the API-stall ceiling the transcript is dormant/killed, not
    # stalled: the old max-gap idle cap still applies (scenario 2's case).
    iv = tail_iv(stuck, now=1 + AMAX + 100)
    check("silent past the ceiling -> idle-capped, not stalled",
          iv is not None and iv.category == agent_psi.IDLE and not iv.stalled
          and approx(iv.end - iv.start, 300.0),
          f"got {iv}")

    # A closed gap ending at an API-error entry is inference for its whole
    # length -- exempt from the dormancy cap -- and always stalled. A real
    # overload episode runs ~220-240s per failed request; a slower endpoint
    # pushes it past the 300s cap, where it used to vanish into idle.
    errored = [tool_result(0, "Z"), api_error(400)]
    secs = agent_psi.duty_seconds(agent_psi.parse_intervals(errored, now=None))
    check("400s gap ending in an API error -> inference, not idle",
          approx(secs[agent_psi.INFERENCE], 400.0)
          and approx(secs[agent_psi.IDLE], 0.0),
          f"got {secs}")
    ivs = agent_psi.parse_intervals(errored, now=None)
    check("that gap is tagged stalled", ivs[0].stalled, f"got {ivs}")

    # Same shape without the API-error marker stays capped to idle (the
    # sabotage control: the marker is what does the work, not the duration).
    plain = [tool_result(0, "Z"), assistant(400, output_tokens=0)]
    secs = agent_psi.duty_seconds(agent_psi.parse_intervals(plain, now=None))
    check("same gap without the API-error marker -> still idle-capped",
          approx(secs[agent_psi.IDLE], 400.0)
          and approx(secs[agent_psi.INFERENCE], 0.0),
          f"got {secs}")

    # An API-error gap past the ceiling is dormancy again, not a 20-minute
    # stall (a resumed session whose first request failed).
    stale_err = [tool_result(0, "Z"), api_error(AMAX + 100)]
    secs = agent_psi.duty_seconds(agent_psi.parse_intervals(stale_err, now=None))
    check("API-error gap past the ceiling -> idle",
          approx(secs[agent_psi.IDLE], AMAX + 100)
          and approx(secs[agent_psi.INFERENCE], 0.0),
          f"got {secs}")

    # A fast failure is still too short to judge (min-stall-gap guard).
    quick_err = [tool_result(0, "Z"), api_error(2)]
    ivs = agent_psi.parse_intervals(quick_err, now=None)
    check("2s API-error gap -> not stalled (under min gap)",
          not ivs[0].stalled, f"got {ivs}")

    # ---- summary -------------------------------------------------------
    print()
    if failures:
        print(f"FAILED ({len(failures)}): {', '.join(failures)}")
        return 1
    print("ALL PASS")
    return 0


if __name__ == "__main__":
    sys.exit(run())
