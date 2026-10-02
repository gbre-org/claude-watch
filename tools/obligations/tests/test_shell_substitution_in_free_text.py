#!/usr/bin/env python3
"""Tests for the ``shell_substitution_in_free_text`` predicate.

Pins the guard against the "backtick in a double-quoted CLI argument" failure:
bash expands (and RUNS) a backtick or $(...) inside a double-quoted argument
before the CLI starts. The predicate DENIES a Bash command where a free-text
CLI (session-task queue add/block/abandon/set/complete, agent-msg send,
event-ack ack, the botchat send CLI) has such an argument, or an UNQUOTED
heredoc that expands, while allowing single quotes, QUOTED heredocs, read-only
queue subcommands and recovery CLIs, and failing OPEN on parse errors.

Modelled on test_subagent_queue_mutating_banned_ast.py.

Run::

    uv run --python 3.11 --with pytest \\
        pytest tools/obligations/tests/test_shell_substitution_in_free_text.py -v
"""

import importlib.machinery
import importlib.util
import os
import subprocess
import sys
import tempfile
from pathlib import Path

import pytest

HERE = Path(__file__).resolve().parent
OBLIGATIONS = HERE.parent / "obligations"
BT = chr(96)
BC = "botchat" + "-send"


def _load():
    spec = importlib.util.spec_from_loader(
        "obligations_cli",
        importlib.machinery.SourceFileLoader("obligations_cli",
                                             str(OBLIGATIONS)))
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


obl = _load()
PRED = {"kind": "shell_substitution_in_free_text", "params": {}}


def allowed(cmd, tool="Bash", **ctx):
    ok, _why = obl._eval_predicate(PRED, tool, cmd, **ctx)
    return ok


DENY = [
    f'session-task queue add "x {BT}foo{BT} y" --scope repo:a',
    'session-task queue add "$(foo)" --scope repo:a',
    'cd /x && session-task queue add "$(foo)" --scope repo:a',
    'env FOO=1 session-task queue add "$(foo)"',
    'true; session-task queue add "$(foo)"',
    f"bash -c 'session-task queue add \"{BT}foo{BT}\"'",
    f'session-task queue block q-1 --reason "need {BT}x{BT}"',
    'session-task queue abandon q-1 --reason "$(x)"',
    'session-task set "run $(make x)"',
    f'session-task complete "did {BT}y{BT}"',
    'agent-msg send a1 "hi $(x)"',
    f'event-ack ack key --action "{BT}x{BT}"',
    f'{BC} "needs {BT}terraform import{BT}"',
    "session-task queue add --desc-file - <<EOF\nbody $(x)\nEOF",
    f"session-task queue add --desc-file - <<EOF\nbody {BT}x{BT}\nEOF",
    "/usr/local/bin/session-task queue add \"$(x)\"",
]

ALLOW = [
    f"session-task queue add 'x {BT}foo{BT} $(bar)' --scope repo:a",
    f"session-task queue add --scope repo:a --desc-file - <<'EOF'\n"
    f"text {BT}foo{BT} and $(bar) and $HOME\nEOF",
    f'session-task queue add --desc-file - <<"EOF"\nx {BT}y{BT}\nEOF',
    "session-task queue add --desc-file - <<EOF\nplain $HOME var\nEOF",
    'session-task queue add "plain text" --scope repo:a',
    'session-task queue list "$(x)"',          # read-only subcommand
    'session-task queue show q-1',
    'session-task queue done q-1',             # not a free-text subcommand
    'obligations override "$(x)" --duration 5m',
    'claude-watch-ack "$(x)"',
    'echo "$(date)"',
    'ls -la',
    'agent-msg inbox a1 --all',
    'event-ack list',
    'session-task queue add x 2>&1 | tail -3',
    "session-task queue add --desc-file - <<'EOF' && echo done\nhi\nEOF",
]


@pytest.mark.parametrize("cmd", DENY)
def test_denied(cmd):
    assert not allowed(cmd), cmd


@pytest.mark.parametrize("cmd", ALLOW)
def test_allowed(cmd):
    assert allowed(cmd), cmd


@pytest.mark.parametrize("cmd", [
    'session-task queue add "unterminated',
    "session-task queue add '",
    'session-task queue add "$(unbalanced',
    f"session-task queue add {BT}unbalanced",
])
def test_unparseable_fails_open(cmd):
    assert allowed(cmd)


def test_non_bash_and_empty_allowed():
    assert allowed('session-task queue add "$(x)"', tool="Read")
    assert allowed("")
    assert allowed("   ")


def test_deny_message_shows_heredoc_form():
    ok, why = obl._eval_predicate(
        PRED, "Bash", 'session-task queue add "$(x)" --scope repo:a')
    assert not ok
    assert "<<'EOF'" in why and "--desc-file -" in why


def test_subagent_and_main_loop_both_denied():
    cmd = 'session-task queue add "$(x)"'
    assert not allowed(cmd, agent_id="agent-1", agent_type="general-purpose")
    assert not allowed(cmd, agent_id=None, agent_type="repl_main_thread")


def test_universal_floor_carve_out():
    """`Bash:^session-task` floors every session-task command; a command with
    an expanding substitution must fall through so the row can DENY it, while
    clean commands stay floored (recovery never gated)."""
    f = obl._universal_recovery_exempt_match
    bad = 'session-task queue add "$(x)" --scope repo:a'
    assert f("Bash", bad)[0] is False
    for ok_cmd in [
        "session-task queue add --desc-file - <<'EOF'\nx\nEOF",
        "session-task queue show q-1",
        "session-task queue register q-1",
        'obligations override "r" --duration 5m',
    ]:
        assert f("Bash", ok_cmd)[0] is True, ok_cmd


def test_recovery_clis_exempt_by_floor_or_predicate():
    # Recovery CLIs are never targets of the predicate.
    for cmd in ["claude-watch-dispatch reset", "claude-watch-ack x",
                "event-ack list", "agent-msg ack a1", "agent-tail --list",
                "obligations list"]:
        assert allowed(cmd), cmd


def test_obligations_init_seeds_row():
    init = HERE.parent / "obligations-init"
    r = subprocess.run([sys.executable, str(init), "--help"],
                       capture_output=True, text=True)
    txt = init.read_text()
    assert "seed_shell_substitution_in_free_text" in txt
    assert '"shell_substitution_in_free_text": seed_' in txt
    assert r.returncode == 0


def test_e2e_canary_guard_denies_old_form_and_heredoc_form_is_safe():
    """End-to-end in a scratch HOME (never the live queue): the old
    double-quoted form is DENIED by the guard (so bash never runs it and the
    canary is never touched); the quoted-heredoc form run for real through
    session-task stores the text literally and does not run the embedded
    command."""
    st = HERE.parent.parent / "session-task" / "session-task"
    with tempfile.TemporaryDirectory() as tmp:
        canary = Path(tmp, "canary")
        old = f'session-task queue add "d {BT}touch {canary}{BT}" --scope repo:x'
        assert not allowed(old)
        assert not obl._universal_recovery_exempt_match("Bash", old)[0]
        assert not canary.exists()          # denied => never executed

        new = (f"{st} queue add --scope repo:x --summary s --desc-file - "
               f"--json <<'EOF'\nd {BT}touch {canary}{BT} $(touch {canary})\n"
               f"EOF")
        assert allowed(new)
        env = dict(os.environ, HOME=tmp, PINGME_SESSION_TASK="0",
                   SESSION_TASK_REPOS_NO_VALIDATE="1")
        Path(tmp, ".config/session").mkdir(parents=True)
        r = subprocess.run(["bash", "-c", new], capture_output=True,
                           text=True, env=env, timeout=30)
        assert r.returncode == 0, r.stderr
        assert not canary.exists()
        q = Path(tmp, ".config/session/queue.json").read_text()
        assert f"touch {canary}" in q
