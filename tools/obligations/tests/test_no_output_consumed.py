#!/usr/bin/env python3
"""Tests for the ``no_output_consumed`` obligations predicate.

Background
----------
"Never filter this tool's output" used to be expressed as a
``no_pipe_pattern`` regex over the RAW Bash command string. That has two
failure modes, both of which were observed live:

1. **It leaks.** A pattern written for ``| tail -N`` / ``| head -N``
   (i.e. requiring a dash flag) silently permitted bare ``| head``,
   ``| grep``, ``| jq``, ``| wc`` and ``> /dev/null``. A gate with a hole
   that shape is worse than no gate, because it reads as enforced.
2. **It over-fires.** Widening the regex made it match any command that
   merely *mentions* the tool name while piping -- e.g.
   ``grep -n "botchat-show" Dockerfile | head``, where no gated command
   runs at all.

Both are the same root cause: a regex sees characters, not shell
structure. ``no_output_consumed`` asks the AST the question the rule
actually cares about -- "is this command's stdout consumed?" -- which
needs no consumer enumeration and only ever inspects real command heads.

Every case below is one of the regex's own failures, and each one flips.

Run::

    uv run --python 3.11 --with pytest \\
        pytest tools/obligations/tests/test_no_output_consumed.py -v
"""

import importlib.machinery
import importlib.util
from pathlib import Path

import pytest

HERE = Path(__file__).resolve().parent
OBLIGATIONS = HERE.parent / "obligations"


def _load_obligations():
    spec = importlib.util.spec_from_loader(
        "obligations_cli",
        importlib.machinery.SourceFileLoader(
            "obligations_cli", str(OBLIGATIONS)),
    )
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


obl = _load_obligations()

PRED = {"kind": "no_output_consumed", "params": {"commands": ["botchat-*"]}}


def _eval(cmd, params=None, tool="Bash"):
    """Return (satisfied, why) for a Bash command under the predicate."""
    pred = {"kind": "no_output_consumed",
            "params": params if params is not None else PRED["params"]}
    return obl._eval_predicate(pred, tool, cmd)


def _denied(cmd, **kw):
    satisfied, _why = _eval(cmd, **kw)
    return not satisfied


# ---------------------------------------------------------------------------
# DENY: the output really is consumed. Every dash-less form here PASSED the
# old regex.
# ---------------------------------------------------------------------------

@pytest.mark.parametrize("cmd", [
    "botchat-show 2008 | head",
    "botchat-history | grep foo",
    "botchat-show 2007-2008 > /dev/null",
    # the dash-flag forms the old regex did catch -- must stay caught
    "botchat-show 2008 | head -20",
    "botchat-history | tail -n 5",
    # consumers the old regex never listed
    "botchat-history | jq .",
    "botchat-show 2008 | wc -l",
    "botchat-history | sed -n 1,5p",
    "botchat-show 2008 | python3 -c 'import sys'",
    # multi-stage pipeline: the LHS is still the gated command
    "botchat-history | grep foo | head",
    # inside a compound statement
    "date && botchat-history | wc -l",
    # env-prefixed + absolute path (the canonical invocation shape)
    "BOTCHAT_API_BASE=x /home/u/repos/botchat/bin/botchat-show 1 | jq .",
    # explicit-fd and both-streams redirections to /dev/null
    "botchat-show 2008 1>/dev/null",
    "botchat-show 2008 &>/dev/null",
    # command substitution captures stdout by definition
    "msg=$(botchat-show 2008)",
    "echo \"$(botchat-history | head)\"",
])
def test_output_consumption_is_denied(cmd):
    satisfied, why = _eval(cmd)
    assert satisfied is False, f"expected DENY for {cmd!r} (why={why})"
    # The denial must be actionable: it names the escape hatch.
    assert "obligations override" in why


# ---------------------------------------------------------------------------
# ALLOW: nothing the rule cares about is consumed. The mention-only cases
# are the ones the widened regex wrongly DENIED.
# ---------------------------------------------------------------------------

@pytest.mark.parametrize("cmd", [
    "botchat-show 2008",
    "botchat-show 2018-2024",
    "botchat-history --unread",
    # name appears only as an ARGUMENT / string to some other command
    "grep -n 'botchat-show' Dockerfile | head",
    'grep -n "botchat-show" Dockerfile | head',
    "echo 'botchat-send' | wc -l",
    "rg botchat- ~/repos | head -20",
    # name inside a heredoc body is data, not structure
    "cat <<'EOF'\nbotchat-show 1 | head\nEOF",
    # botchat on the RHS of a pipe: its own stdout is free
    "cat draft.txt | botchat-send -F -",
    # && / ; are not output consumption
    "botchat-show 2008 && echo done",
    "botchat-show 2008 ; date",
    # stderr-only redirection leaves stdout on the terminal
    "botchat-show 2008 2>/dev/null",
    # an unrelated command being filtered
    "signal-history --tail 20 | head",
])
def test_non_consumption_is_allowed(cmd):
    satisfied, why = _eval(cmd)
    assert satisfied is True, f"expected ALLOW for {cmd!r} (why={why})"


# ---------------------------------------------------------------------------
# Params
# ---------------------------------------------------------------------------

def test_redirect_mode_any_catches_file_redirect():
    cmd = "botchat-show 2008 > /tmp/out.txt"
    assert _eval(cmd)[0] is True  # default: devnull only
    assert _denied(cmd, params={"commands": ["botchat-*"],
                                "redirect_mode": "any"})


def test_redirect_mode_none_ignores_devnull():
    assert _eval("botchat-show 2008 > /dev/null",
                 params={"commands": ["botchat-*"],
                         "redirect_mode": "none"})[0] is True


def test_include_substitution_false():
    assert _eval("msg=$(botchat-show 2008)",
                 params={"commands": ["botchat-*"],
                         "include_substitution": False})[0] is True


def test_literal_command_names():
    params = {"commands": ["botchat-show", "botchat-history"]}
    assert _denied("botchat-show 1 | head", params=params)
    # a sibling command not listed is not gated
    assert _eval("botchat-send hi | head", params=params)[0] is True


def test_comma_string_commands_accepted():
    params = {"commands": "botchat-show, botchat-history"}
    assert _denied("botchat-show 1 | head", params=params)


def test_no_commands_configured_allows():
    assert _eval("botchat-show 1 | head", params={"commands": []})[0] is True


def test_non_bash_tool_is_na():
    satisfied, why = _eval("botchat-show 1 | head", tool="Read")
    assert satisfied is True
    assert "not Bash" in why


# ---------------------------------------------------------------------------
# Fail-closed
# ---------------------------------------------------------------------------

def test_unparseable_command_is_denied():
    # Unterminated quote: the parser cannot say whether the output is
    # filtered, so the gate must DENY rather than wave it through.
    satisfied, why = _eval("botchat-show 'unterminated | head")
    assert satisfied is False
    assert "fail-closed" in why
    assert "obligations override" in why


def test_missing_parser_module_is_denied(monkeypatch):
    monkeypatch.setattr(obl, "_shell_ast", None)
    satisfied, why = _eval("botchat-show 2008")
    assert satisfied is False
    assert "fail-closed" in why


def test_helper_exception_is_denied(monkeypatch):
    class Boom:
        ShellParseError = obl._shell_ast.ShellParseError

        @staticmethod
        def output_consumed_by(*_a, **_kw):
            raise RuntimeError("kaboom")

    monkeypatch.setattr(obl, "_shell_ast", Boom)
    satisfied, why = _eval("botchat-show 2008")
    assert satisfied is False
    assert "kaboom" in why


# ---------------------------------------------------------------------------
# Sabotage check -- an all-negative suite passes vacuously. Break the
# predicate and prove the DENY cases go red.
# ---------------------------------------------------------------------------

def test_sabotage_predicate_makes_deny_cases_fail(monkeypatch):
    """Neuter the consumption detector; every DENY case must stop denying.

    Without this, a bug that made ``output_consumed_by`` always return []
    would leave the ALLOW half green and the DENY half silently... also
    green, if the DENY assertions were written the wrong way round. This
    asserts the suite has teeth.
    """
    shell_ast = obl._shell_ast

    class Neutered:
        ShellParseError = shell_ast.ShellParseError

        @staticmethod
        def output_consumed_by(*_a, **_kw):
            return []  # "nothing is ever consumed"

    monkeypatch.setattr(obl, "_shell_ast", Neutered)
    for cmd in ("botchat-show 2008 | head",
                "botchat-history | grep foo",
                "botchat-show 2007-2008 > /dev/null",
                "botchat-show 2008 | head -20"):
        satisfied, _why = _eval(cmd)
        assert satisfied is True, (
            f"sabotage did not take effect for {cmd!r} -- the DENY "
            "assertions above would pass vacuously")
