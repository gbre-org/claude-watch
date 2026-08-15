#!/usr/bin/env python3
"""Tests for ``satisfied_by.file_arg_flags`` — matching the auto-satisfy
regex against the BODY of a file the command names, not just the command
string.

Background: ``satisfied_by.command_pattern`` is a regex over the Bash
command string. When a CLI takes its payload as ``--file <path>`` (because
piping into it is banned and an inline ``"a\\nb"`` argument writes a literal
backslash-n), the content never appears in the command string, so a real
invocation can never auto-satisfy its own obligation and a gate-mode row
stays standing until a human clears it.

``file_arg_flags`` opts an obligation into ALSO searching the named file.
These tests pin the additive semantics and, more importantly, every
fail-closed edge: unresolvable paths, relative paths, missing files, FIFOs,
directories, oversized files, quoted/heredoc decoys.

Run::

    uv run --python 3.11 --with pytest \\
        pytest tools/obligations/tests/test_satisfied_by_file_body.py -v
"""

import importlib.machinery
import importlib.util
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
        "obligations_cli_fb",
        importlib.machinery.SourceFileLoader(
            "obligations_cli_fb", str(OBLIGATIONS)),
    )
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


obl = _load_obligations()

ANCHOR = "fulfilment confirmed"
PATT = re.compile(re.escape(ANCHOR))
FLAGS = ["-F", "--file"]


def _match(cmd, flags=FLAGS, patt=PATT):
    return obl._satisfied_by_pattern_matches(patt, cmd, flags)


# --- backward compatibility: the command string is still the first haystack


def test_command_string_match_still_wins_without_flags():
    assert _match(f"mysender --to a '{ANCHOR}'", flags=[]) is True


def test_command_string_match_still_wins_with_flags():
    # Additive: enabling file_arg_flags never breaks an inline match.
    assert _match(f"mysender --to a '{ANCHOR}'") is True


def test_no_anchor_anywhere_does_not_match():
    assert _match("mysender --to a 'something else'") is False


def test_heredoc_body_in_command_string_matches_as_before():
    # The whole staging+send idiom in ONE Bash call puts the body in the
    # command string, so this case already worked and must keep working.
    cmd = (
        'f=$(stage); cat > "$f" <<\'EOF\'\n'
        f"{ANCHOR}\n"
        "EOF\n"
        'mysender --to a -F "$f"'
    )
    assert _match(cmd) is True


# --- the actual fix: body of a -F file


def test_body_of_dash_F_file_matches(tmp_path):
    p = tmp_path / "staged.md"
    p.write_text(f"line one\n{ANCHOR}\nline three\n")
    assert _match(f"mysender --to a -F {p}") is True


def test_body_of_long_flag_equals_form_matches(tmp_path):
    p = tmp_path / "staged.md"
    p.write_text(f"{ANCHOR}\n")
    assert _match(f"mysender --to a --file={p}") is True


def test_body_without_the_anchor_does_not_match(tmp_path):
    p = tmp_path / "staged.md"
    p.write_text("an entirely unrelated multi-line\nmessage to the same person\n")
    assert _match(f"mysender --to a -F {p}") is False


def test_flag_not_in_the_configured_list_is_ignored(tmp_path):
    p = tmp_path / "staged.md"
    p.write_text(f"{ANCHOR}\n")
    # Only -F/--file are configured; --attach must not be followed.
    assert _match(f"mysender --to a --attach {p}") is False


def test_file_flags_off_means_body_is_never_read(tmp_path):
    p = tmp_path / "staged.md"
    p.write_text(f"{ANCHOR}\n")
    assert _match(f"mysender --to a -F {p}", flags=[]) is False


def test_empty_pattern_is_not_widened_by_file_flags(tmp_path):
    # The anchor must still be found in REAL text. A command that merely
    # carries a file, with no anchor in it, never satisfies.
    p = tmp_path / "staged.md"
    p.write_text("no anchor here at all\n")
    assert _match(f"mysender --dm someone -F {p}") is False


# --- fail-closed edges


def test_missing_file_is_not_a_match(tmp_path):
    p = tmp_path / "already-unlinked.md"
    assert not p.exists()
    assert _match(f"mysender --to a -F {p}") is False


def test_unexpanded_variable_path_is_not_a_match(tmp_path):
    # `-F "$f"` in a call where the staging happened in an EARLIER tool call:
    # we cannot know what $f was, and must not guess.
    assert _match('mysender --to a -F "$f"') is False


def test_command_substitution_path_is_not_a_match():
    assert _match('mysender --to a -F "$(mktemp)"') is False


def test_glob_path_is_not_a_match(tmp_path):
    p = tmp_path / "staged.md"
    p.write_text(f"{ANCHOR}\n")
    assert _match(f"mysender --to a -F {tmp_path}/*.md") is False


def test_relative_path_is_not_a_match(tmp_path, monkeypatch):
    p = tmp_path / "staged.md"
    p.write_text(f"{ANCHOR}\n")
    monkeypatch.chdir(tmp_path)
    # Even though CWD would resolve it, the tool's cwd is not ours to assume.
    assert _match("mysender --to a -F staged.md") is False


def test_directory_is_not_a_match(tmp_path):
    d = tmp_path / "adir"
    d.mkdir()
    assert _match(f"mysender --to a -F {d}") is False


def test_fifo_is_not_a_match_and_does_not_hang(tmp_path):
    fifo = tmp_path / "afifo"
    os.mkfifo(fifo)
    # No writer exists. O_NONBLOCK + S_ISREG check means we decline instead
    # of parking the PostToolUse hook forever.
    assert _match(f"mysender --to a -F {fifo}") is False


def test_dev_zero_is_not_a_match():
    if not os.path.exists("/dev/zero"):
        pytest.skip("/dev/zero unavailable")
    assert _match("mysender --to a -F /dev/zero") is False


def test_oversized_file_is_declined(tmp_path):
    p = tmp_path / "huge.md"
    body = ("x" * 1024 + "\n") * 300  # ~300 KiB > 256 KiB cap
    p.write_text(f"{ANCHOR}\n" + body)
    assert len(p.read_bytes()) > obl.SATISFIED_BY_FILE_MAX_BYTES
    assert _match(f"mysender --to a -F {p}") is False


def test_file_just_under_the_cap_still_matches(tmp_path):
    p = tmp_path / "big-but-ok.md"
    pad = "y" * (obl.SATISFIED_BY_FILE_MAX_BYTES - len(ANCHOR) - 10)
    p.write_text(f"{ANCHOR}\n{pad}")
    assert len(p.read_bytes()) <= obl.SATISFIED_BY_FILE_MAX_BYTES
    assert _match(f"mysender --to a -F {p}") is True


def test_symlink_to_regular_file_matches(tmp_path):
    real = tmp_path / "real.md"
    real.write_text(f"{ANCHOR}\n")
    link = tmp_path / "link.md"
    link.symlink_to(real)
    assert _match(f"mysender --to a -F {link}") is True


def test_symlink_to_fifo_is_not_a_match(tmp_path):
    fifo = tmp_path / "afifo"
    os.mkfifo(fifo)
    link = tmp_path / "link"
    link.symlink_to(fifo)
    assert _match(f"mysender --to a -F {link}") is False


def test_binary_file_does_not_crash(tmp_path):
    p = tmp_path / "blob.bin"
    p.write_bytes(bytes(range(256)) * 16)
    assert _match(f"mysender --to a -F {p}") is False


def test_invalid_utf8_with_anchor_still_matches(tmp_path):
    p = tmp_path / "mixed.bin"
    p.write_bytes(b"\xff\xfe " + ANCHOR.encode() + b" \xff")
    assert _match(f"mysender --to a -F {p}") is True


def test_at_most_four_files_are_consulted(tmp_path):
    paths = []
    for i in range(6):
        q = tmp_path / f"f{i}.md"
        q.write_text(f"{ANCHOR}\n" if i == 5 else "filler\n")
        paths.append(q)
    resolved = obl._file_arg_paths(
        "mysender " + " ".join(f"-F {q}" for q in paths), FLAGS)
    assert len(resolved) == obl.SATISFIED_BY_FILE_MAX_FILES
    # The anchor lives in the 6th file, past the cap -> no match.
    assert _match("mysender " + " ".join(f"-F {q}" for q in paths)) is False


# --- AST awareness: a flag inside quoted data is not a flag


def test_flag_inside_quoted_argument_is_not_followed(tmp_path):
    p = tmp_path / "staged.md"
    p.write_text(f"{ANCHOR}\n")
    cmd = f"echo 'remember to use -F {p} next time'"
    assert obl._file_arg_paths(cmd, FLAGS) == []
    assert _match(cmd) is False


def test_flag_inside_heredoc_body_is_not_followed(tmp_path):
    p = tmp_path / "staged.md"
    p.write_text(f"{ANCHOR}\n")
    cmd = (
        "cat > /dev/null <<'EOF'\n"
        f"docs: pass the body with -F {p}\n"
        "EOF\n"
        "true"
    )
    assert obl._file_arg_paths(cmd, FLAGS) == []
    assert _match(cmd) is False


def test_trailing_flag_with_no_argument_is_ignored():
    assert obl._file_arg_paths("mysender --to a -F", FLAGS) == []


def test_unparseable_command_yields_no_file_evidence(tmp_path):
    # Unterminated quote -> shell_ast raises -> no file evidence, no match.
    p = tmp_path / "staged.md"
    p.write_text(f"{ANCHOR}\n")
    assert obl._file_arg_paths(f"mysender -F {p} 'unterminated", FLAGS) == []


# --- path resolution helper


@pytest.mark.parametrize("word", [
    "", "relative/path", "./x", "$HOME/x", "`x`", "a*b", "a?b", "a[b]",
    "~nosuchuser12345/x",
])
def test_resolve_literal_path_declines(word):
    assert obl._resolve_literal_path(word) is None


def test_resolve_literal_path_expands_tilde(monkeypatch, tmp_path):
    monkeypatch.setenv("HOME", str(tmp_path))
    assert obl._resolve_literal_path("~/x.md") == str(tmp_path / "x.md")


def test_resolve_literal_path_normalizes():
    assert obl._resolve_literal_path("/a/b/../c") == "/a/c"


# --- end-to-end through the real CLI + a temp HOME


def _run_cli(tmp_home, *argv):
    env = dict(os.environ)
    env["HOME"] = str(tmp_home)
    env.pop("OBLIGATIONS_BYPASS", None)
    return subprocess.run(
        [sys.executable, str(OBLIGATIONS), *argv],
        capture_output=True, text=True, env=env, timeout=30, check=False,
    )


def _add_ob(tmp_home, *extra):
    return _run_cli(
        tmp_home, "add",
        "--tool-pattern", "Bash",
        "--predicate", "file_exists",
        "--params", '{"path": "/nonexistent-marker-for-tests"}',
        "--ttl", "0",
        "--deny-msg", "must announce the delivery first",
        "--satisfied-by-tool", "Bash",
        "--satisfied-by-cmd-regex", ANCHOR,
        *extra,
    )


def test_cli_rejects_file_flag_without_regex(tmp_path):
    home = tmp_path / "home"
    (home / ".config" / "claude").mkdir(parents=True)
    proc = _run_cli(
        home, "add",
        "--tool-pattern", "Bash",
        "--predicate", "file_exists",
        "--params", '{"path": "/nope"}',
        "--ttl", "0",
        "--deny-msg", "x",
        "--satisfied-by-tool", "Bash",
        "--satisfied-by-file-flag=-F",
    )
    assert proc.returncode == 2, proc.stdout + proc.stderr
    assert "requires --satisfied-by-cmd-regex" in proc.stderr


def test_cli_end_to_end_file_body_satisfies(tmp_path):
    home = tmp_path / "home"
    (home / ".config" / "claude").mkdir(parents=True)
    add = _add_ob(home, "--satisfied-by-file-flag=-F")
    assert add.returncode == 0, add.stdout + add.stderr

    staged = tmp_path / "staged.md"
    staged.write_text(f"heads up\n{ANCHOR} for the thing\n")

    post = _run_cli(home, "post-satisfy", "--tool", "Bash",
                    "--command-string", f"mysender --to a -F {staged}",
                    "--json")
    assert post.returncode == 0, post.stdout + post.stderr
    assert "satisfied" not in post.stderr
    import json as _json
    assert len(_json.loads(post.stdout)["removed"]) == 1

    lst = _run_cli(home, "list", "--json")
    assert _json.loads(lst.stdout)["obligations"] == []


def test_cli_end_to_end_missing_file_does_not_satisfy(tmp_path):
    home = tmp_path / "home"
    (home / ".config" / "claude").mkdir(parents=True)
    add = _add_ob(home, "--satisfied-by-file-flag=-F")
    assert add.returncode == 0, add.stdout + add.stderr

    gone = tmp_path / "gone.md"
    post = _run_cli(home, "post-satisfy", "--tool", "Bash",
                    "--command-string", f"mysender --to a -F {gone}",
                    "--json")
    assert post.returncode == 0, post.stdout + post.stderr
    import json as _json
    assert _json.loads(post.stdout)["removed"] == []

    lst = _run_cli(home, "list", "--json")
    assert len(_json.loads(lst.stdout)["obligations"]) == 1


def test_cli_end_to_end_legacy_row_unaffected(tmp_path):
    # No file_arg_flags -> the file body is never consulted (old behaviour).
    home = tmp_path / "home"
    (home / ".config" / "claude").mkdir(parents=True)
    add = _add_ob(home)
    assert add.returncode == 0, add.stdout + add.stderr

    staged = tmp_path / "staged.md"
    staged.write_text(f"{ANCHOR}\n")
    post = _run_cli(home, "post-satisfy", "--tool", "Bash",
                    "--command-string", f"mysender --to a -F {staged}",
                    "--json")
    import json as _json
    assert _json.loads(post.stdout)["removed"] == []

    # ...but an inline match still clears it.
    post2 = _run_cli(home, "post-satisfy", "--tool", "Bash",
                     "--command-string", f"mysender --to a '{ANCHOR}'",
                     "--json")
    assert len(_json.loads(post2.stdout)["removed"]) == 1
