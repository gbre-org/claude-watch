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


def test_bashcmd_glob_matches_family():
    # A whole command family can be named without enumerating it.
    assert m("Bashcmd:botchat-*", "Bash", "botchat-history --unread") is True
    assert m("Bashcmd:botchat-*", "Bash", "botchat-show 2008") is True
    assert m("Bashcmd:botchat-*", "Bash", "signal-history") is False


def test_bashcmd_glob_still_head_only():
    # The glob is only ever tested against a real command HEAD -- a mention
    # as an argument or inside a quoted string never matches.
    assert m("Bashcmd:botchat-*", "Bash", "grep botchat-show f") is False
    assert m(
        "Bashcmd:botchat-*", "Bash", "grep -n 'botchat-show' Dockerfile | head"
    ) is False


def test_bashcmd_glob_failsafe_on_unparseable():
    # Unparseable => word-boundary fallback, glob translated to \S*.
    assert m("Bashcmd:botchat-*", "Bash", "botchat-show 'unterminated") is True
    assert m("Bashcmd:botchat-*", "Bash", "echo 'unterminated") is False


def test_bashcmd_failsafe_on_unparseable():
    # Unterminated quote => ShellParseError => fail-safe word-boundary match.
    # The command is unparseable AND does contain the name as a word, so the
    # fail-safe conservatively matches (preserves pre-AST behavior).
    assert m("Bashcmd:watcher-ctl", "Bash", "echo 'watcher-ctl") is True
    # ...and does NOT match when the name is absent from the raw string.
    assert m("Bashcmd:watcher-ctl", "Bash", "echo 'unterminated") is False


# --- host-bash MCP tools: AST-aware command-HEAD matching (like Bashcmd:) ---
#
# In production the gate hook (`_short_command_string`) renders the host-bash
# tool_input via json.dumps before matching, so command_string is a JSON blob
# carrying the REAL command/script text. A bare-name REST must match only the
# command HEAD of that body, NOT anywhere in the rendered JSON (the bug: a
# gated name in a --body / heredoc / PR-body false-matched a body-wide
# re.search). A regex REST stays a body-wide re.search (backward compat).

import json  # noqa: E402


def _run_command(cmd: str) -> str:
    """Render a run_command tool_input the way the gate hook does."""
    return json.dumps({"command": cmd})


def _run_script(script: str, interpreter: str = "bash") -> str:
    """Render a run_script tool_input the way the gate hook does."""
    return json.dumps({"interpreter": interpreter, "script": script})


def test_hostbash_run_command_real_invocation_matches():
    # Real botchat invocation via run_command -> still matches (would deny).
    assert m(
        "mcp__host-bash__run_command:botchat-send",
        "mcp__host-bash__run_command",
        _run_command("botchat-send --mark-read 5 --ack 5"),
    ) is True


def test_hostbash_run_command_env_and_path_prefixed_matches():
    # Env-prefixed + absolute-path invocation (the canonical botchat shape)
    # still matches -- prefix stripping is AST-aware, not body-wide.
    assert m(
        "mcp__host-bash__run_command:botchat-send",
        "mcp__host-bash__run_command",
        _run_command(
            "BOTCHAT_API_BASE=http://x /home/h/repos/botchat/bin/botchat-send"
            " --mark-read 5"
        ),
    ) is True
    assert m(
        "mcp__host-bash__run_command:botchat-ack",
        "mcp__host-bash__run_command",
        _run_command("cd ~ && botchat-ack 7"),
    ) is True


def test_hostbash_run_command_body_arg_mention_not_matched():
    # `botchat-wait` only inside a --body arg -> no match (would allow). This
    # is the bug fix: the old body-wide re.search over the JSON matched here.
    assert m(
        "mcp__host-bash__run_command:botchat-wait",
        "mcp__host-bash__run_command",
        _run_command('gh pr create --body "waits like botchat-wait does"'),
    ) is False
    # Same for a short -m mention.
    assert m(
        "mcp__host-bash__run_command:botchat-send",
        "mcp__host-bash__run_command",
        _run_command('git commit -m "mirror the botchat-send clear-path"'),
    ) is False


def test_hostbash_run_script_real_invocation_matches():
    # A run_script whose body is a real botchat-send -> matches.
    assert m(
        "mcp__host-bash__run_script:botchat-send",
        "mcp__host-bash__run_script",
        _run_script("botchat-send --mark-read 5 --ack 5"),
    ) is True


def test_hostbash_run_script_heredoc_body_mention_not_matched():
    # A run_script whose heredoc / PR-body merely mentions the token -> no
    # match (would allow). The gated name is inside the heredoc body, not a
    # command head.
    script = (
        "gh pr create --base main --body \"$(cat <<'EOF'\n"
        "This PR mirrors the botchat-send clear-path exempt.\n"
        "EOF\n"
        ")\""
    )
    assert m(
        "mcp__host-bash__run_script:botchat-send",
        "mcp__host-bash__run_script",
        _run_script(script),
    ) is False


def test_hostbash_glob_family_head_only():
    # Glob spec names a whole family, still only against a real head.
    assert m(
        "mcp__host-bash__run_command:botchat-*",
        "mcp__host-bash__run_command",
        _run_command("botchat-history --unread"),
    ) is True
    assert m(
        "mcp__host-bash__run_command:botchat-*",
        "mcp__host-bash__run_command",
        _run_command("grep -n botchat-history file"),
    ) is False


def test_hostbash_regex_rest_still_body_wide():
    # A REGEX spec (anchors / lookaheads) stays a body-wide re.search for
    # backward compat with the arg-conditional botchat clear-path exempt.
    # This mark-read-only invocation matches the lookahead exempt...
    clear_path = (
        r"^(?=.*\bbotchat-send\s+-)(?=.*\s--(?:mark-read|ack)\b)"
        r"(?!.*\s--(?:body|to|reply-to|topic|image|file|message-file)\b)"
        r"(?!.*\s-[bF]\b).*"
    )
    assert m(
        "mcp__host-bash__run_command:" + clear_path,
        "mcp__host-bash__run_command",
        _run_command("botchat-send --mark-read 5 --ack 5"),
    ) is True
    # ...but a compose (with --body) does NOT match the exempt (stays gated).
    assert m(
        "mcp__host-bash__run_command:" + clear_path,
        "mcp__host-bash__run_command",
        _run_command("botchat-send --to andrew --body hi --ack 5"),
    ) is False


def test_hostbash_raw_text_fallback_still_matches():
    # Fail-safe: a raw (non-JSON) command_string passed directly still gets
    # AST head matching (the pre-existing test convention / defensive path).
    assert m(
        "mcp__host-bash__run_command:botchat-send",
        "mcp__host-bash__run_command",
        "botchat-send --mark-read 1",
    ) is True
    assert m(
        "mcp__host-bash__run_command:botchat-send",
        "mcp__host-bash__run_command",
        "echo botchat-send",
    ) is False


def test_hostbash_wrong_tool_name_no_match():
    # The host-bash pattern never matches the Bash tool (head must equal
    # tool_name).
    assert m(
        "mcp__host-bash__run_command:botchat-send",
        "Bash",
        "botchat-send",
    ) is False
