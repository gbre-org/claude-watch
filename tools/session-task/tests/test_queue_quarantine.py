#!/usr/bin/env python3
"""Tests for the `quarantined` queue lifecycle state.

Background: `queue abandon` used to free an item's scope immediately, on the
strength of an INFERENCE that the owning agent had died (no output file,
stale mtime, "no child process"). An agent presumed dead that way was in
fact still running; it worked for another ~48 minutes while a replacement
agent, spawned into the freed scope, redid the same work. Both wrote, and a
duplicate escaped to a user. Silence is not death, and a guess must not
dissolve the lock that exists to prevent duplicate work.

So an abandon of a SCOPE-OWNING item now lands in `quarantined`: not
terminal, still holding the scope, visibly distinct from a healthy runner.

Covers:

  * abandon of a running/wedged item -> quarantined, scope STILL HELD
    (this is the incident: the second spawn must be refused)
  * abandon of a genuinely dead agent still permits a respawn --
    `queue resurrect` accepts a quarantined item and carries the scope to
    the new row, so a real death needs no manual unwedging
  * `queue done` clears the quarantine (the agent came back and finished --
    the strongest possible evidence it was alive)
  * `queue release` is the operator's explicit "it really is gone"
  * the liveness guard REFUSES a release while claude-watch reports a live
    agent, and `--force` overrides it
  * NEGATIVE: absence of an agent record does NOT auto-release a quarantine,
    and `queue release` refuses on any non-quarantined status
  * pending / blocked abandons stay terminal (no agent could be running)
  * `--confirmed-dead` skips quarantine (positive evidence of exit)
  * `queue list` renders the state and its escape hatches
  * `queue register` refuses to re-claim a quarantined item

All tests run against a temp HOME so the live ~/.config/session/queue.json
is never touched.

Run:
    uv run --python 3.11 --with pytest \\
        pytest tools/session-task/tests/test_queue_quarantine.py -v
"""

import json
import os
import subprocess
import sys
import tempfile
from pathlib import Path

SESSION_TASK = Path(__file__).resolve().parent.parent / "session-task"


def _env_for_tmp(tmp):
    env = os.environ.copy()
    env["HOME"] = tmp
    env["PINGME_SESSION_TASK"] = "0"
    env["CLAUDE_EVENT_SESSION_TASK"] = "0"
    # No active-agents state by default: the liveness guard must have
    # nothing to say, which is precisely the case the quarantine covers.
    env["CLAUDE_AGENTS_STATE"] = str(Path(tmp) / "no-such-agents.json")
    env["CLAUDE_AGENTS_STATE_FALLBACK_BIN"] = ""
    return env


def _write_agents_state(env, tmp, queue_id, *, alive, agent_id="agent-1"):
    """Point CLAUDE_AGENTS_STATE at a curated active-agents blob."""
    path = Path(tmp) / "agents.json"
    path.write_text(json.dumps({
        "agents": [
            {"agent_id": agent_id, "queue_id": queue_id, "alive": alive},
        ],
    }))
    env["CLAUDE_AGENTS_STATE"] = str(path)
    return path


def _run(env, *argv, expect_exit=None):
    cmd = [sys.executable, str(SESSION_TASK)] + list(argv)
    r = subprocess.run(cmd, capture_output=True, text=True, env=env, timeout=30)
    if expect_exit is not None and r.returncode != expect_exit:
        raise RuntimeError(
            f"expected exit {expect_exit} got {r.returncode}: argv={argv}\n"
            f"stdout={r.stdout!r}\nstderr={r.stderr!r}"
        )
    return r


def _add(env, desc, scopes, *extra):
    args = ["queue", "add", desc, "--json"]
    for s in scopes:
        args.extend(["--scope", s])
    args.extend(extra)
    r = _run(env, *args, expect_exit=0)
    return json.loads(r.stdout)


def _show(env, qid):
    r = _run(env, "queue", "show", qid, expect_exit=0)
    return json.loads(r.stdout)


def _register(env, qid, expect_exit=0):
    return _run(env, "queue", "register", qid, "--json",
                expect_exit=expect_exit)


def _running_item(env, desc, scopes):
    """Add + register an item so it owns its scope."""
    added = _add(env, desc, scopes, "--summary", desc[:40])
    _register(env, added["id"])
    return added["id"]


# -------------------- abandon -> quarantine --------------------


def test_abandon_running_quarantines_instead_of_freeing_scope():
    """The core change: abandon on a running item does NOT go terminal."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        qid = _running_item(env, "source the books", ["resource:books"])

        r = _run(env, "queue", "abandon", qid, "--reason",
                 "no .output file, presumed dead", "--silent", expect_exit=0)
        assert "quarantined" in r.stdout.lower(), r.stdout
        # The operator must be told the scope is still held and how to
        # release it -- the incident began with someone assuming otherwise.
        assert "SCOPE IS STILL HELD" in r.stdout, r.stdout
        assert f"queue release {qid}" in r.stdout, r.stdout
        assert f"queue resurrect {qid}" in r.stdout, r.stdout

        shown = _show(env, qid)
        assert shown["status"] == "quarantined"
        assert shown["quarantine_reason"] == "no .output file, presumed dead"
        assert shown["quarantined_at"]
        assert shown["quarantined_from"] == "running"
        # NOT terminal: no abandoned stamps.
        assert "abandoned_at" not in shown, shown


def test_abandon_wedged_quarantines_too():
    """A wedged item also owns its scope, so it quarantines as well."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        qid = _running_item(env, "wedged work", ["resource:wedged-q"])
        _run(env, "queue", "wedge", qid, "--reason", "context_limit",
             "--silent", expect_exit=0)

        _run(env, "queue", "abandon", qid, "--reason", "gave up",
             "--silent", expect_exit=0)
        shown = _show(env, qid)
        assert shown["status"] == "quarantined"
        assert shown["quarantined_from"] == "wedged"


def test_second_spawn_on_quarantined_scope_is_refused():
    """THE INCIDENT.

    Abandon the item, then try to spawn a replacement on the same scope
    while the original agent is (unknown to us) still alive. Registering
    the replacement must be REFUSED -- previously the scope was free and
    both agents ran, producing duplicate work.
    """
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        first = _running_item(env, "source four books", ["resource:ebooks"])
        _run(env, "queue", "abandon", first, "--reason", "looks dead",
             "--silent", expect_exit=0)

        # A replacement can be QUEUED (that's harmless) ...
        replacement = _add(env, "redo the four books", ["resource:ebooks"],
                           "--summary", "redo")
        assert replacement["ready_now"] is False, replacement

        # ... but it must NOT be registrable, which is what gates the spawn.
        r = _register(env, replacement["id"], expect_exit=2)
        assert "NOT CLEAR TO SPAWN" in r.stderr, r.stderr
        assert "QUARANTINED" in r.stderr, r.stderr
        # And the refusal must name the way out, not just say "no".
        assert "queue resurrect" in r.stderr, r.stderr

        # spawn-check agrees (exit 2 = blocked).
        sc = _run(env, "queue", "spawn-check", replacement["id"],
                  expect_exit=2)
        assert "QUARANTINED" in sc.stderr, sc.stderr


def test_register_refuses_to_reclaim_the_quarantined_item_itself():
    """Re-registering the quarantined row in place is the same double-spawn."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        qid = _running_item(env, "reclaim me", ["resource:reclaim"])
        _run(env, "queue", "abandon", qid, "--reason", "looks dead",
             "--silent", expect_exit=0)

        r = _register(env, qid, expect_exit=2)
        assert "IS QUARANTINED" in r.stderr, r.stderr
        assert "STILL HOLDS THE SCOPE" in r.stderr, r.stderr


def test_abandon_is_idempotent_on_quarantined():
    """A second abandon neither double-stamps nor escalates to terminal."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        qid = _running_item(env, "twice", ["resource:twice"])
        _run(env, "queue", "abandon", qid, "--reason", "first",
             "--silent", expect_exit=0)
        first_at = _show(env, qid)["quarantined_at"]

        r = _run(env, "queue", "abandon", qid, "--reason", "second",
                 "--silent", expect_exit=0)
        assert "already quarantined" in r.stdout, r.stdout
        shown = _show(env, qid)
        assert shown["status"] == "quarantined"
        assert shown["quarantined_at"] == first_at
        assert shown["quarantine_reason"] == "first"


# -------------------- release paths --------------------


def test_done_clears_the_quarantine():
    """The agent came back and finished: strongest evidence it was alive."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        qid = _running_item(env, "still working", ["resource:alive"])
        _run(env, "queue", "abandon", qid, "--reason", "presumed dead",
             "--silent", expect_exit=0)

        r = _run(env, "queue", "done", qid, "--silent", expect_exit=0)
        assert "QUARANTINED" in r.stdout, r.stdout  # the near-miss is announced

        shown = _show(env, qid)
        assert shown["status"] == "done"
        assert shown["quarantine_released_by"] == "agent-completed"
        assert shown["quarantine_released_at"]

        # Scope is free again: a fresh item on the same scope can register.
        nxt = _add(env, "follow-up", ["resource:alive"], "--summary", "next")
        _register(env, nxt["id"], expect_exit=0)


def test_release_frees_the_scope():
    """The explicit operator command -- the documented unwedge."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        qid = _running_item(env, "really dead", ["resource:reldead"])
        _run(env, "queue", "abandon", qid, "--reason", "presumed dead",
             "--silent", expect_exit=0)

        r = _run(env, "queue", "release", qid, "--reason",
                 "checked the host, process is gone", "--silent",
                 expect_exit=0)
        assert "scope freed" in r.stdout, r.stdout

        shown = _show(env, qid)
        assert shown["status"] == "abandoned"
        assert shown["abandoned_at"]
        assert shown["quarantine_released_by"] == "operator"
        assert shown["quarantine_released_at"]
        # Both reasons survive: why it was quarantined and why released.
        assert "presumed dead" in shown["abandon_reason"]
        assert "process is gone" in shown["abandon_reason"]

        nxt = _add(env, "replacement", ["resource:reldead"],
                   "--summary", "repl")
        _register(env, nxt["id"], expect_exit=0)


def test_resurrect_accepts_a_quarantined_item():
    """A genuinely dead agent must stay respawnable WITHOUT manual unwedging.

    `queue resurrect` is the existing respawn path; it accepts a quarantined
    item directly, abandons the old row and carries the scope to the new
    one -- so the lock is never dropped and no operator step is inserted
    into the normal death-and-respawn flow.
    """
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        added = _add(env, "work that died", ["resource:resq"],
                     "--summary", "died")
        qid = added["id"]
        _register(env, qid)
        _run(env, "queue", "abandon", qid, "--reason", "API 500, agent died",
             "--silent", expect_exit=0)

        # No transcript on disk in a tmp HOME, so supply the spawn prompt.
        transcript = Path(tmp) / "agent.jsonl"
        transcript.write_text(json.dumps({
            "type": "user",
            "message": {"role": "user",
                        "content": f"Queue item: {qid}\nDo the work."},
        }) + "\n")

        r = _run(env, "queue", "resurrect", qid, "--from-transcript",
                 str(transcript), "--reason", "agent died", "--json",
                 expect_exit=0)
        out = json.loads(r.stdout)
        assert out["old_abandoned"] is True
        new_id = out["new_id"] if "new_id" in out else out["id"]

        old = _show(env, qid)
        assert old["status"] == "abandoned"
        assert old["quarantine_released_by"].startswith("resurrect")

        new = _show(env, new_id)
        assert new["status"] == "pending"
        assert "resource:resq" in new["scope"]
        # The replacement is spawnable -- the scope moved, it never opened.
        _register(env, new_id, expect_exit=0)


# -------------------- liveness guard --------------------


def test_release_refused_while_agent_is_alive():
    """Positive evidence of LIFE beats the operator's assertion of death."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        qid = _running_item(env, "very much alive", ["resource:alive2"])
        _run(env, "queue", "abandon", qid, "--reason", "looks dead",
             "--silent", expect_exit=0)
        _write_agents_state(env, tmp, qid, alive=True, agent_id="agent-live")

        r = _run(env, "queue", "release", qid, "--reason", "sure it's gone",
                 "--silent", expect_exit=1)
        assert "still ALIVE" in r.stderr, r.stderr
        assert "agent-live" in r.stderr, r.stderr
        assert _show(env, qid)["status"] == "quarantined"

        # --force is the escape hatch for a wrong state file.
        _run(env, "queue", "release", qid, "--force", "--reason", "I checked",
             "--silent", expect_exit=0)
        shown = _show(env, qid)
        assert shown["status"] == "abandoned"
        assert shown["quarantine_released_by"] == "operator --force"


def test_confirmed_dead_refused_while_agent_is_alive():
    """--confirmed-dead is a claim, and a live agent record refutes it."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        qid = _running_item(env, "alive again", ["resource:alive3"])
        _write_agents_state(env, tmp, qid, alive=True, agent_id="agent-live")

        r = _run(env, "queue", "abandon", qid, "--confirmed-dead", "--reason",
                 "rc=1", "--silent", expect_exit=1)
        assert "still ALIVE" in r.stderr, r.stderr
        assert _show(env, qid)["status"] == "running"


# -------------------- NEGATIVE tests --------------------


def test_missing_agent_record_does_not_release_the_quarantine():
    """NEGATIVE: absence of an agent record authorizes nothing.

    "claude-watch has no record of this agent" is exactly the inference that
    caused the incident. It must not silently free the scope on any read
    path -- only an explicit release, a resurrect, or the agent's own `done`
    may do that. A dead-looking agent with an `alive: false` record is
    equally powerless.
    """
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        qid = _running_item(env, "no record", ["resource:norecord"])
        _run(env, "queue", "abandon", qid, "--reason", "quiet too long",
             "--silent", expect_exit=0)

        # Explicit "not alive" record -- the weakest evidence there is.
        _write_agents_state(env, tmp, qid, alive=False)

        # Reading the queue in every shape must leave the state untouched.
        _run(env, "queue", "list", expect_exit=0)
        _run(env, "queue", "ready", expect_exit=0)
        _run(env, "queue", "groups", expect_exit=0)
        assert _show(env, qid)["status"] == "quarantined"

        # And the scope is still not spawnable.
        peer = _add(env, "peer work", ["resource:norecord"],
                    "--summary", "peer")
        _register(env, peer["id"], expect_exit=2)


def test_release_refused_on_non_quarantined_statuses():
    """NEGATIVE: `release` is not a general-purpose abandon."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)

        pending = _add(env, "pending item", ["resource:relp"],
                       "--summary", "p")["id"]
        r = _run(env, "queue", "release", pending, "--silent", expect_exit=1)
        assert "must be quarantined" in r.stderr, r.stderr
        assert _show(env, pending)["status"] == "pending"

        running = _running_item(env, "running item", ["resource:relr"])
        r = _run(env, "queue", "release", running, "--silent", expect_exit=1)
        assert "must be quarantined" in r.stderr, r.stderr
        assert "queue abandon" in r.stderr, r.stderr
        assert _show(env, running)["status"] == "running"

        done = _running_item(env, "done item", ["resource:reld"])
        _run(env, "queue", "done", done, "--silent", expect_exit=0)
        r = _run(env, "queue", "release", done, "--silent", expect_exit=1)
        assert "must be quarantined" in r.stderr, r.stderr

        r = _run(env, "queue", "release", "q-2026-01-01-dead", "--silent",
                 expect_exit=1)
        assert "not found" in r.stderr, r.stderr


def test_release_unknown_item_does_not_create_one():
    """NEGATIVE: releasing a nonexistent id is an error, not a no-op write."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        _add(env, "real item", ["resource:realonly"], "--summary", "r")
        _run(env, "queue", "release", "q-2026-01-01-zzzz", "--silent",
             expect_exit=1)
        listed = json.loads(_run(env, "queue", "list", "--all", "--json",
                                 expect_exit=0).stdout)
        assert len(listed) == 1, listed


# -------------------- statuses that stay terminal --------------------


def test_abandon_pending_is_still_terminal():
    """A pending item never had an agent, so there is nothing to quarantine."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        qid = _add(env, "never spawned", ["resource:pend"],
                   "--summary", "p")["id"]
        _run(env, "queue", "abandon", qid, "--reason", "changed my mind",
             "--silent", expect_exit=0)
        shown = _show(env, qid)
        assert shown["status"] == "abandoned"
        assert shown["abandon_reason"] == "changed my mind"


def test_abandon_blocked_is_still_terminal():
    """A blocked item already released its scope lock -- nothing to hold."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        qid = _running_item(env, "parked work", ["resource:blk"])
        _run(env, "queue", "block", qid, "--reason", "waiting on a human",
             "--silent", expect_exit=0)
        _run(env, "queue", "abandon", qid, "--reason", "gave up",
             "--silent", expect_exit=0)
        assert _show(env, qid)["status"] == "abandoned"


def test_confirmed_dead_skips_quarantine():
    """Positive evidence of exit (an rc) earns the terminal transition."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        qid = _running_item(env, "workload row", ["resource:cdead"])
        _run(env, "queue", "abandon", qid, "--confirmed-dead", "--reason",
             "workload exited non-zero rc=7", "--silent", expect_exit=0)
        shown = _show(env, qid)
        assert shown["status"] == "abandoned"
        assert shown["death_evidence"] == "caller asserted --confirmed-dead"

        nxt = _add(env, "next", ["resource:cdead"], "--summary", "n")
        _register(env, nxt["id"], expect_exit=0)


def test_confirmed_dead_on_a_quarantined_item_finalizes_it():
    """Evidence arriving later still ends the quarantine."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        qid = _running_item(env, "late evidence", ["resource:late"])
        _run(env, "queue", "abandon", qid, "--reason", "guess", "--silent",
             expect_exit=0)
        _run(env, "queue", "abandon", qid, "--confirmed-dead", "--reason",
             "reaped, rc=137", "--silent", expect_exit=0)
        assert _show(env, qid)["status"] == "abandoned"


# -------------------- visibility --------------------


def test_list_renders_quarantine_distinctly():
    """`queue list` must make presumed-dead legible, not just non-running."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        qid = _running_item(env, "listed", ["resource:listq"])
        _run(env, "queue", "abandon", qid, "--reason", "stale mtime",
             "--silent", expect_exit=0)

        r = _run(env, "queue", "list", expect_exit=0)
        assert qid in r.stdout
        assert "quarantined" in r.stdout
        assert "SCOPE STILL HELD" in r.stdout, r.stdout
        assert "stale mtime" in r.stdout, r.stdout
        assert f"queue release {qid}" in r.stdout, r.stdout

        listed = json.loads(_run(env, "queue", "list", "--json",
                                 expect_exit=0).stdout)
        assert [it["status"] for it in listed] == ["quarantined"]


def test_release_is_discoverable_in_help():
    """An operator who wedges themselves must find the way out from --help."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        top = _run(env, "queue", "--help", expect_exit=0).stdout
        assert "release" in top, top

        rel = _run(env, "queue", "release", "--help", expect_exit=0).stdout
        assert "quarantin" in rel.lower(), rel
        assert "--force" in rel, rel

        ab = _run(env, "queue", "abandon", "--help", expect_exit=0).stdout
        assert "--confirmed-dead" in ab, ab
        assert "queue release" in ab, ab
        assert "queue resurrect" in ab, ab


if __name__ == "__main__":
    import sys as _sys
    failures = 0
    for name, fn in sorted(list(globals().items())):
        if name.startswith("test_") and callable(fn):
            try:
                fn()
                print(f"PASS {name}")
            except Exception as exc:  # noqa: BLE001
                failures += 1
                print(f"FAIL {name}: {exc}")
    _sys.exit(1 if failures else 0)
