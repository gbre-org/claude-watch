#!/usr/bin/env python3
"""Tests for ``satisfied_by.body_pattern`` — a SECOND, independent regex for
the payload a command carried, ANDed with ``command_pattern``.

Background. A single ``command_pattern`` has to do two unrelated jobs: say
which command counts (so the rule does not clear on an unrelated
invocation) and say what it carried (so the rule does not clear on a send
that never mentioned the thing the obligation is about). Written as one
regex the two halves fight each other.

The natural shape is ``prog\\b(?=.*A)(?=.*B)``. Both lookaheads are
evaluated at the offset just past the program token and ``.`` does not cross
newlines, so they see only the rest of THAT line. When the payload was
written by an earlier line of the same command — a ``cat > "$f" <<EOF …
EOF`` heredoc staging a body, the shape forced on a CLI that forbids stdin
— the payload text is present in the command string yet unreachable. The
obligation can never self-satisfy, which is worse than having no obligation:
the work gets done, the gate keeps blocking, and the only exit is an
override.

``file_arg_flags`` does not rescue that pattern either, and these tests pin
why: a payload file holds the message and nothing else, so a pattern
anchored on command shape can never match a file body.

Run::

    uv run --python 3.11 --with pytest \\
        pytest tools/obligations/tests/test_satisfied_by_body_pattern.py -v
"""

import importlib.machinery
import importlib.util
import json
import os
import re
import subprocess
import sys
from pathlib import Path

import pytest

HERE = Path(__file__).resolve().parent
OBLIGATIONS = HERE.parent / "obligations"


def _load_obligations():
    spec = importlib.util.spec_from_loader(
        "obligations_cli_bp",
        importlib.machinery.SourceFileLoader(
            "obligations_cli_bp", str(OBLIGATIONS)),
    )
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


obl = _load_obligations()

# A generic stand-in for the real-world shape: a sender CLI that refuses
# stdin, so a multi-line body must be staged to a file and handed over with
# `-F`. Nothing here names a real tool, person or service.
SENDER = "mysender"
RECIPIENT = "alice"

# The pattern an obligation would naturally be given today: one regex that
# names the program, the recipient AND the payload anchor.
COMBINED_PAT = (
    r"mysender\b(?=.*(?:--to\s+['\"]?alice\b))"
    r"(?=.*(?:Order\s*#\s*42\b|Widgets))"
)
# The same rule, split into its two real jobs.
CMD_PAT = r"mysender\b(?=.*(?:--to\s+['\"]?alice\b))"
BODY_PAT = r"Order\s*#\s*42\b|Widgets"

BODY = (
    "**Widgets are in stock.** Order #42 done.\n"
    "\n"
    "Shipping tomorrow, tracking to follow.\n"
)

FLAGS = ["-F", "--file"]


def _heredoc_cmd(path_word):
    """The mandated multi-line workflow, as one Bash call.

    Body staged by a heredoc on earlier lines, then handed to the sender by
    file. ``path_word`` is whatever the send actually writes for the path.
    """
    return (
        'f=$(mkstage); cat > "$f" <<\'EOF\'\n'
        f"{BODY}"
        "EOF\n"
        f"mysender --to {RECIPIENT} -F {path_word}"
    )


def _satisfies(command_string, sb):
    """The REAL decision function, not a model of it.

    `_satisfy_by_completion` calls exactly this after its tool_pattern
    check, so a regression in the AND-ing or the fail-closed handling shows
    up here and not only in the slower end-to-end cases.
    """
    return obl._satisfied_by_regexes_match(sb, command_string)


# --------------------------------------------------------------------------
# The bug, pinned. These two must stay red-shaped: if either starts
# satisfying, the combined-pattern shape silently "works" again and the
# split below stops being justified.
# --------------------------------------------------------------------------


def test_combined_pattern_cannot_reach_a_heredoc_staged_body():
    """The payload IS in the command string and still cannot be matched.

    The lookaheads sit just past `mysender`; `.` does not cross newlines;
    the body was written on earlier lines. This is the reported failure.
    """
    cmd = _heredoc_cmd('"$f"')
    assert "Order #42" in cmd  # the anchor really is present in the haystack
    assert re.search(COMBINED_PAT, cmd) is None


def test_combined_pattern_cannot_match_a_payload_file_either(tmp_path):
    """file_arg_flags hands a command-shaped pattern a useless haystack.

    A payload file holds the message and nothing else — no program token,
    no recipient argument — so the command anchor can never be found there.
    """
    staged = tmp_path / "staged.md"
    staged.write_text(BODY)
    cmd = f"mysender --to {RECIPIENT} -F {staged}"
    sb = {"command_pattern": COMBINED_PAT, "file_arg_flags": FLAGS}
    assert obl._file_arg_paths(cmd, FLAGS) == [str(staged)]  # path resolved
    assert staged.exists()                                   # file present
    assert _satisfies(cmd, sb) is False                      # still no match


# --------------------------------------------------------------------------
# The fix: split the rule in two.
# --------------------------------------------------------------------------


def test_split_pattern_satisfies_the_heredoc_workflow():
    cmd = _heredoc_cmd('"$f"')
    sb = {"command_pattern": CMD_PAT, "body_pattern": BODY_PAT}
    assert _satisfies(cmd, sb) is True


def test_split_pattern_satisfies_an_inline_single_line_send():
    cmd = f"mysender --to {RECIPIENT} 'Widgets are up, Order #42'"
    sb = {"command_pattern": CMD_PAT, "body_pattern": BODY_PAT}
    assert _satisfies(cmd, sb) is True


def test_split_pattern_reads_a_payload_file_when_opted_in(tmp_path):
    staged = tmp_path / "staged.md"
    staged.write_text(BODY)
    cmd = f"mysender --to {RECIPIENT} -F {staged}"
    sb = {"command_pattern": CMD_PAT, "body_pattern": BODY_PAT,
          "file_arg_flags": FLAGS}
    assert _satisfies(cmd, sb) is True


def test_payload_file_deleted_before_match_does_not_satisfy(tmp_path):
    """Senders routinely unlink a staged body on success. Gone != satisfied."""
    staged = tmp_path / "staged.md"
    staged.write_text(BODY)
    cmd = f"mysender --to {RECIPIENT} -F {staged}"
    sb = {"command_pattern": CMD_PAT, "body_pattern": BODY_PAT,
          "file_arg_flags": FLAGS}
    assert _satisfies(cmd, sb) is True
    staged.unlink()
    assert _satisfies(cmd, sb) is False


def test_variable_path_argument_is_still_declined(tmp_path):
    """`-F "$f"` is not a resolvable path, so the FILE is never consulted.

    Documented residual gap: when the body lives only in the file and the
    path is a shell variable, there is nothing to read. Only the heredoc
    form (body also in the command string) clears in that shape.
    """
    staged = tmp_path / "staged.md"
    staged.write_text(BODY)
    cmd = f'f={staged}\nmysender --to {RECIPIENT} -F "$f"'
    sb = {"command_pattern": CMD_PAT, "body_pattern": BODY_PAT,
          "file_arg_flags": FLAGS}
    assert obl._file_arg_paths(cmd, FLAGS) == []
    assert _satisfies(cmd, sb) is False


# --------------------------------------------------------------------------
# Narrowing, never loosening.
# --------------------------------------------------------------------------


def test_body_pattern_that_does_not_match_blocks_satisfaction():
    """A send to the right recipient that never mentions the payload."""
    cmd = f"mysender --to {RECIPIENT} 'unrelated chatter'"
    assert _satisfies(cmd, {"command_pattern": CMD_PAT}) is True
    sb = {"command_pattern": CMD_PAT, "body_pattern": BODY_PAT}
    assert _satisfies(cmd, sb) is False


def test_command_pattern_that_does_not_match_blocks_satisfaction():
    """The payload named, but sent to somebody else."""
    cmd = "mysender --to bob 'Widgets are up, Order #42'"
    sb = {"command_pattern": CMD_PAT, "body_pattern": BODY_PAT}
    assert _satisfies(cmd, sb) is False


def test_body_pattern_alone_still_requires_its_anchor():
    sb = {"body_pattern": BODY_PAT}
    assert _satisfies("mysender --to bob 'nothing to see'", sb) is False
    assert _satisfies(_heredoc_cmd('"$f"'), sb) is True


def test_legacy_row_without_body_pattern_is_unchanged():
    cmd = f"mysender --to {RECIPIENT} 'Widgets are up, Order #42'"
    assert _satisfies(cmd, {"command_pattern": COMBINED_PAT}) is True


# --------------------------------------------------------------------------
# End-to-end through the real CLI against a temp HOME.
# --------------------------------------------------------------------------


def _run_cli(tmp_home, *argv):
    env = dict(os.environ)
    env["HOME"] = str(tmp_home)
    env.pop("OBLIGATIONS_BYPASS", None)
    return subprocess.run(
        [sys.executable, str(OBLIGATIONS), *argv],
        capture_output=True, text=True, env=env, timeout=30, check=False,
    )


def _home(tmp_path):
    home = tmp_path / "home"
    (home / ".config" / "claude").mkdir(parents=True)
    return home


def _add(home, *extra):
    return _run_cli(
        home, "add",
        "--tool-pattern", "Bash",
        "--predicate", "file_exists",
        "--params", '{"path": "/nonexistent-marker-for-tests"}',
        "--ttl", "0",
        "--deny-msg", "tell the requester first",
        "--satisfied-by-tool", "Bash",
        *extra,
    )


def test_cli_end_to_end_heredoc_send_clears_the_gate(tmp_path):
    home = _home(tmp_path)
    add = _add(home,
               "--satisfied-by-cmd-regex", CMD_PAT,
               "--satisfied-by-body-regex", BODY_PAT)
    assert add.returncode == 0, add.stdout + add.stderr

    post = _run_cli(home, "post-satisfy", "--tool", "Bash",
                    "--command-string", _heredoc_cmd('"$f"'), "--json")
    assert post.returncode == 0, post.stdout + post.stderr
    assert len(json.loads(post.stdout)["removed"]) == 1
    assert json.loads(_run_cli(home, "list", "--json").stdout)["obligations"] == []


def test_cli_end_to_end_combined_pattern_reproduces_the_stuck_gate(tmp_path):
    """The pre-fix registration shape, end to end: it never clears."""
    home = _home(tmp_path)
    add = _add(home, "--satisfied-by-cmd-regex", COMBINED_PAT)
    assert add.returncode == 0, add.stdout + add.stderr

    post = _run_cli(home, "post-satisfy", "--tool", "Bash",
                    "--command-string", _heredoc_cmd('"$f"'), "--json")
    assert post.returncode == 0, post.stdout + post.stderr
    assert json.loads(post.stdout)["removed"] == []
    assert len(json.loads(_run_cli(home, "list", "--json").stdout)["obligations"]) == 1


def test_cli_persists_body_pattern(tmp_path):
    home = _home(tmp_path)
    assert _add(home, "--satisfied-by-cmd-regex", CMD_PAT,
                "--satisfied-by-body-regex", BODY_PAT).returncode == 0
    rows = json.loads(_run_cli(home, "list", "--json").stdout)["obligations"]
    assert rows[0]["satisfied_by"]["body_pattern"] == BODY_PAT
    assert rows[0]["satisfied_by"]["command_pattern"] == CMD_PAT


def test_cli_accepts_file_flag_with_only_a_body_regex(tmp_path):
    home = _home(tmp_path)
    proc = _add(home, "--satisfied-by-body-regex", BODY_PAT,
                "--satisfied-by-file-flag=-F")
    assert proc.returncode == 0, proc.stdout + proc.stderr
    rows = json.loads(_run_cli(home, "list", "--json").stdout)["obligations"]
    assert rows[0]["satisfied_by"]["file_arg_flags"] == ["-F"]


def test_cli_rejects_file_flag_with_neither_regex(tmp_path):
    home = _home(tmp_path)
    proc = _add(home, "--satisfied-by-file-flag=-F")
    assert proc.returncode == 2, proc.stdout + proc.stderr
    assert "--satisfied-by-body-regex" in proc.stderr


@pytest.mark.parametrize("flag", ["--satisfied-by-cmd-regex",
                                  "--satisfied-by-body-regex"])
def test_cli_rejects_an_uncompilable_regex(tmp_path, flag):
    """A rule that can never fire must not be registrable."""
    home = _home(tmp_path)
    proc = _add(home, flag, "Order(#42")
    assert proc.returncode == 2, proc.stdout + proc.stderr
    assert "not a valid regex" in proc.stderr


def test_stored_uncompilable_body_pattern_fails_closed(tmp_path):
    """A hand-edited bad pattern keeps the row; it never clears it."""
    home = _home(tmp_path)
    assert _add(home, "--satisfied-by-cmd-regex", CMD_PAT).returncode == 0
    store = home / ".config" / "claude" / "obligations.json"
    data = json.loads(store.read_text())
    data["obligations"][0]["satisfied_by"]["body_pattern"] = "Order(#42"
    store.write_text(json.dumps(data))

    post = _run_cli(home, "post-satisfy", "--tool", "Bash",
                    "--command-string", _heredoc_cmd('"$f"'), "--json")
    assert post.returncode == 0, post.stdout + post.stderr
    assert json.loads(post.stdout)["removed"] == []
