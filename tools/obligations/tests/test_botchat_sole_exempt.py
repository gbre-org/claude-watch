#!/usr/bin/env python3
"""End-to-end test for the botchat READ-exemption "sole command" hardening.

Background
----------
The botchat mark-read gate (``tool_pattern:"*"``, ``drain_before_dispatch``)
blocks every MAIN-loop tool call while inbound botchat is unread, exempting
the read CLIs (``botchat-show`` / ``botchat-history``) so the loop can read
the messages. A SEPARATE ``no_pipe_pattern`` gate (``Bash:botchat-(show|
history)``) denies a PIPED botchat read because a pipe strips the attachment
lines (image/file references silently dropped -- the #3989 failure).

The bug: while the drain gate is FIRING, its ``exempt_patterns`` are ELEVATED
into the framework recovery floor (so the clear-path punches through every
co-firing gate). The old read exempts were head-match forms
(``Bash:botchat-show`` re.search / ``Bashcmd:...botchat-show``), and a
pipeline ``botchat-show 42 | tail`` has ``botchat-show`` as A head -> it
satisfied the elevated exempt -> the floor short-circuited ALL gates,
INCLUDING the no-pipe gate. So the piped read was ALLOWED and the attachment
was stripped.

The fix: express the read exempts with ``Bashsole:botchat-show,
botchat-history`` -- satisfied only when the botchat read is the SOLE simple
command (no pipe / redirect / list). A piped read is no longer exempt, is not
elevated, and stays DENIED by the no-pipe gate. A bare read still works.

Each assertion below exercises ``_check_core`` against a synthetic obligation
set mirroring the live gates. Runs under pytest OR standalone
(``python3 test_botchat_sole_exempt.py``); no pytest import required.

Run::

    uv run --python 3.11 --with pytest \\
        pytest tools/obligations/tests/test_botchat_sole_exempt.py -v
"""

import importlib.machinery
import importlib.util
import json
import tempfile
from pathlib import Path

HERE = Path(__file__).resolve().parent
OBLIGATIONS = HERE.parent / "obligations"

PIPE = chr(124)  # keep a literal pipe out of the source text where possible


def _load_obligations():
    spec = importlib.util.spec_from_loader(
        "obligations_cli",
        importlib.machinery.SourceFileLoader("obligations_cli", str(OBLIGATIONS)),
    )
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


obl = _load_obligations()

# The FIXED read exempts (as landed in claude-config botchat-mark-read.json):
# botchat-show/history are Bashsole (raw-form-only); the state-op CLIs stay
# head-match; the clear-path send stays arg-conditional regex.
_SEND_CLEAR = (
    "Bash:^(?=.*\\bbotchat-send\\s+-)(?=.*\\s--(?:mark-read|ack)\\b)"
    "(?!.*\\s--(?:body|to|reply-to|topic|image|file|message-file)\\b)"
    "(?!.*\\s-[bF]\\b).*"
)
FIXED_EXEMPTS = [
    _SEND_CLEAR,
    "Bashsole:botchat-show,botchat-history",
    "Bashcmd:botchat-unread-check,botchat-wait,botchat-ack",
    "Read",
]
# The OLD (leaky) read exempts, for the regression contrast test.
OLD_EXEMPTS = [
    _SEND_CLEAR,
    "Bash:botchat-show",
    "Bash:botchat-history",
    "Bashcmd:botchat-unread-check,botchat-history,botchat-show,botchat-wait,botchat-ack",
    "Read",
]


def _build(exempts, unread):
    """mark-read drain gate (evaluator false==unread) + no-pipe gate."""
    unread_cmd = "false" if unread else "true"  # exit 1 => unread => DENY
    return {
        "obligations": [
            {
                "id": "ob-mark-read",
                "tool_pattern": "*",
                "predicate": {"kind": "all_of", "params": {"predicates": [
                    {"kind": "is_main_loop", "params": {}},
                    {"kind": "evaluator",
                     "params": {"cmd": unread_cmd, "decision_mode": "exit_code"}},
                ]}},
                "enforcement": "gate",
                "drain_before_dispatch": True,
                "exempt_patterns": exempts,
                "deny_message": "unread botchat",
            },
            {
                "id": "ob-no-pipe",
                "tool_pattern": "Bash:botchat-(show" + PIPE + "history)",
                "predicate": {"kind": "no_pipe_pattern",
                              "params": {"regex": "(?<!\\" + PIPE + ")\\"
                                         + PIPE + "(?!\\" + PIPE + ")"}},
                "enforcement": "gate",
                "deny_message": "read botchat raw",
            },
        ],
        "overrides": [],
    }


def _check(cmd, exempts=None, unread=True, agent_id="", agent_type=""):
    """Return (allowed: bool, blocking_ids: list[str])."""
    if exempts is None:
        exempts = FIXED_EXEMPTS
    tmp = tempfile.NamedTemporaryFile("w", suffix=".json", delete=False)
    tmp.write(json.dumps(_build(exempts, unread)))
    tmp.close()
    obl.OBLIGATIONS_FILE = Path(tmp.name)
    ok, blocking = obl._check_core("Bash", cmd, agent_id=agent_id,
                                   agent_type=agent_type)
    return ok, [b["id"] for b in blocking]


BARE = "botchat-show 42"
PIPED = "botchat-show 42 " + PIPE + " tail -5"
REDIR = "botchat-show 42 > /tmp/out"
SEND_MARKREAD = "botchat-send --mark-read 42 --ack 42"


# --- the fix: main loop, unread present ---

def test_bare_read_allowed():
    ok, _ids = _check(BARE)
    assert ok is True


def test_piped_read_denied():
    ok, ids = _check(PIPED)
    assert ok is False
    # denied by BOTH gates now (belt and suspenders); at minimum the no-pipe
    # gate must be one of them (proving the elevation no longer bypasses it).
    assert "ob-no-pipe" in ids


def test_redirect_read_denied():
    ok, ids = _check(REDIR)
    assert ok is False
    assert "ob-mark-read" in ids


def test_real_send_markread_clearpath_works():
    # The state-op clear-path (mark-read/ack, no body) must stay exempt so the
    # loop can actually clear the gate.
    ok, _ids = _check(SEND_MARKREAD)
    assert ok is True


def test_compose_send_still_gated():
    # A genuine compose (body) is NOT the clear-path -> stays gated by the
    # drain gate while unread. (Unchanged behavior; guards against regressing
    # the arg-conditional send exempt.)
    ok, ids = _check("botchat-send --to andrew --body hi")
    assert ok is False
    assert "ob-mark-read" in ids


# --- without unread, only the no-pipe gate is in force ---

def test_piped_read_denied_even_without_unread():
    ok, ids = _check(PIPED, unread=False)
    assert ok is False
    assert "ob-no-pipe" in ids


def test_bare_read_allowed_without_unread():
    ok, _ids = _check(BARE, unread=False)
    assert ok is True


# --- regression contrast: the OLD exempts let the pipe through ---

def test_old_exempts_leak_piped_read():
    # Demonstrates the bug the fix closes: with the OLD head-match read
    # exempts, the elevated clear-path matched the piped botchat-show and the
    # floor short-circuited the no-pipe gate -> ALLOWED (attachment stripped).
    ok, _ids = _check(PIPED, exempts=OLD_EXEMPTS)
    assert ok is True  # <-- the leak (pre-fix behavior)


if __name__ == "__main__":
    import sys
    fns = [v for k, v in sorted(globals().items())
           if k.startswith("test_") and callable(v)]
    failed = 0
    for fn in fns:
        try:
            fn()
            print("PASS", fn.__name__)
        except AssertionError as e:
            failed += 1
            print("FAIL", fn.__name__, "--", e)
    print(f"\n{len(fns) - failed}/{len(fns)} passed")
    sys.exit(1 if failed else 0)
