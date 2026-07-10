#!/usr/bin/env python3
"""Tests for _tool_pattern_matches, focused on the new ``Bashcmd:`` form.

``Bashcmd:<names>`` is an AST-aware exempt-matcher form: it matches the Bash
tool iff any comma-separated command NAME is the effective command HEAD
(basename, with leading ``VAR=val`` env-assignments and wrapper words like
``sudo`` / ``env`` / ``nohup`` stripped) of a top-level command segment. This
replaces the raw ``re.search(rest, command_string)`` of the ``Bash:<regex>``
form for exempt lists, eliminating the ^-anchor bug (a prefixed invocation
like ``cd ~ && watcher-ctl run x`` is now exempt) and the arg-mention
false-exempt risk (``echo watcher-ctl`` no longer matches).

Loads the ``obligations`` CLI (which has no .py suffix) as a module via
importlib so the module-level functions can be exercised directly. The
existing ``Bash``/``Bash:<regex>``/``*``/bare-tool paths are asserted
unchanged for backward compatibility.

Run::

    uv run --python 3.11 --with pytest \\
        pytest tools/obligations/tests/test_tool_pattern_matches.py -v
"""

import importlib.util
from pathlib import Path

HERE = Path(__file__).resolve().parent
OBLIGATIONS = HERE.parent / "obligations"


def _load_obligations():
    spec = importlib.util.spec_from_loader(
        "obligations_cli",
        importlib.machinery.SourceFileLoader("obligations_cli", str(OBLIGATIONS)),
    )
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


import importlib.machinery  # noqa: E402

obl = _load_obligations()
m = obl._tool_pattern_matches


# --- backward-compat: existing forms unchanged ---

def test_wildcard_matches_everything():
    assert m("*", "Bash", "anything") is True
    assert m("*", "Read", "") is True


def test_bare_tool_name():
    assert m("Bash", "Bash", "ls") is True
    assert m("Read", "Read", "") is True
    assert m("Bash", "Read", "") is False


def test_bash_regex_form():
    assert m("Bash:^watcher-ctl", "Bash", "watcher-ctl run x") is True
    # ^-anchor bug: prefixed invocation does NOT match the anchored regex.
    assert m("Bash:^watcher-ctl", "Bash", "cd ~ && watcher-ctl run x") is False
    # regex only ever matches the Bash tool.
    assert m("Bash:watcher-ctl", "Read", "watcher-ctl") is False


def test_non_bash_named_tool_with_colon():
    # e.g. mcp__host-bash__run_command:botchat-send
    assert m(
        "mcp__host-bash__run_command:botchat-send",
        "mcp__host-bash__run_command",
        "botchat-send --mark-read 1",
    ) is True
    assert m(
        "mcp__host-bash__run_command:botchat-send",
        "Bash",
        "botchat-send",
    ) is False


# --- Bashcmd: the new AST-aware command-name form ---

def test_bashcmd_bare():
    assert m("Bashcmd:watcher-ctl", "Bash", "watcher-ctl run signal") is True


def test_bashcmd_only_bash_tool():
    assert m("Bashcmd:watcher-ctl", "Read", "watcher-ctl") is False


def test_bashcmd_env_prefix_stripped():
    assert m("Bashcmd:watcher-ctl", "Bash", "FOO=bar watcher-ctl run") is True


def test_bashcmd_path_prefixed():
    assert m(
        "Bashcmd:watcher-ctl", "Bash", "/usr/local/bin/watcher-ctl run"
    ) is True


def test_bashcmd_sudo_wrapped():
    assert m("Bashcmd:watcher-ctl", "Bash", "sudo watcher-ctl run") is True


def test_bashcmd_compound_prefix():
    # The ^-anchor bug case: prefixed compound invocation IS exempt now.
    assert m("Bashcmd:watcher-ctl", "Bash", "cd ~ && watcher-ctl run x") is True


def test_bashcmd_arg_only_mention_not_matched():
    assert m("Bashcmd:watcher-ctl", "Bash", "echo watcher-ctl") is False


def test_bashcmd_quoted_mention_not_matched():
    assert m(
        "Bashcmd:watcher-ctl", "Bash", "echo 'run watcher-ctl now'"
    ) is False


def test_bashcmd_heredoc_body_not_matched():
    assert m(
        "Bashcmd:watcher-ctl", "Bash", "cat <<'EOF'\nwatcher-ctl run\nEOF"
    ) is False


def test_bashcmd_multiple_names():
    pat = "Bashcmd:watcher-ctl,watcher-restart,event-ack"
    assert m(pat, "Bash", "event-ack list") is True
    assert m(pat, "Bash", "watcher-restart") is True
    assert m(pat, "Bash", "something-else") is False


def test_bashcmd_failsafe_on_unparseable():
    # Unterminated quote => ShellParseError => fail-safe word-boundary match.
    # The command is unparseable AND does contain the name as a word, so the
    # fail-safe conservatively matches (preserves pre-AST behavior).
    assert m("Bashcmd:watcher-ctl", "Bash", "echo 'watcher-ctl") is True
    # ...and does NOT match when the name is absent from the raw string.
    assert m("Bashcmd:watcher-ctl", "Bash", "echo 'unterminated") is False
