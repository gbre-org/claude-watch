#!/usr/bin/env python3
"""Tests for the free-text file/stdin inputs that avoid shell expansion.

Bash expands a backtick or $(...) inside a double-quoted argument BEFORE the
CLI starts. These flags let callers feed free text through a QUOTED heredoc
(``--desc-file - <<'EOF'``) or a file, so it is stored byte-for-byte:

  * queue add  --desc-file FILE|-   (description positional becomes optional)
  * queue add  --summary-file FILE|-
  * set / complete --desc-file FILE|-
  * queue block / abandon --reason-file FILE|-

Every scenario runs against a scratch HOME (never the live queue).

Run:
    uv run --python 3.11 --with pytest pytest tests/test_free_text_file_inputs.py -v
"""

import json
import os
import subprocess
import sys
import tempfile
from pathlib import Path

SESSION_TASK = Path(__file__).resolve().parent.parent / "session-task"
BT = chr(96)
NASTY = (f"run {BT}touch CANARY{BT} then $(touch CANARY2) and $HOME and "
         "\\n 'quotes' \"dq\"\nsecond line")


def _env(tmp):
    env = dict(os.environ)
    env["HOME"] = str(tmp)
    env["PINGME_SESSION_TASK"] = "0"
    env["SESSION_TASK_REPOS_NO_VALIDATE"] = "1"
    env["CLAUDE_AGENTS_STATE"] = str(Path(tmp, "active-agents.json"))
    env["CLAUDE_AGENTS_STATE_FALLBACK_BIN"] = ""
    Path(tmp, ".config/session").mkdir(parents=True, exist_ok=True)
    return env


def _run(env, *argv, stdin=None):
    return subprocess.run([sys.executable, str(SESSION_TASK)] + list(argv),
                          capture_output=True, text=True, env=env,
                          timeout=20, input=stdin)


def _queue_bytes(tmp):
    p = Path(tmp, ".config/session/queue.json")
    return p.read_bytes() if p.exists() else b""


def _show(env, qid):
    return json.loads(_run(env, "queue", "show", qid).stdout)


def _add_file(env, text, *extra):
    r = _run(env, "queue", "add", "--scope", "repo:foo", "--summary", "s",
             "--desc-file", "-", "--json", *extra, stdin=text)
    assert r.returncode == 0, r.stderr
    return json.loads(r.stdout)


def test_add_desc_file_stdin_literal():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env(tmp)
        a = _add_file(env, NASTY + "\n")
        assert _show(env, a["id"])["description"] == NASTY
        assert not Path("CANARY").exists()


def test_add_desc_file_path_literal():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env(tmp)
        f = Path(tmp, "d.txt")
        f.write_text(NASTY)
        r = _run(env, "queue", "add", "--scope", "repo:foo", "--summary", "s",
                 "--desc-file", str(f), "--json")
        assert r.returncode == 0, r.stderr
        assert _show(env, json.loads(r.stdout)["id"])["description"] == NASTY


def test_add_summary_file_literal():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env(tmp)
        f = Path(tmp, "s.txt")
        f.write_text(f"headline {BT}x{BT} $(y)\n")
        r = _run(env, "queue", "add", "d", "--scope", "repo:foo",
                 "--summary-file", str(f), "--json")
        assert r.returncode == 0, r.stderr
        assert (_show(env, json.loads(r.stdout)["id"])["summary"]
                == f"headline {BT}x{BT} $(y)")


def test_add_positional_still_works():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env(tmp)
        r = _run(env, "queue", "add", "plain", "--scope", "repo:foo",
                 "--summary", "s", "--json")
        assert r.returncode == 0, r.stderr
        assert _show(env, json.loads(r.stdout)["id"])["description"] == "plain"


def _assert_no_mutation(tmp, env, argv, stdin=None):
    _add_file(env, "seed")
    before = _queue_bytes(tmp)
    r = _run(env, *argv, stdin=stdin)
    assert r.returncode == 1, (r.returncode, r.stderr)
    assert "ERROR" in r.stderr
    assert _queue_bytes(tmp) == before, "queue.json must not change"


def test_add_both_positional_and_file_rejected():
    with tempfile.TemporaryDirectory() as tmp:
        _assert_no_mutation(tmp, _env(tmp),
                            ["queue", "add", "pos", "--scope", "repo:foo",
                             "--desc-file", "-"], stdin="x")


def test_add_neither_rejected():
    with tempfile.TemporaryDirectory() as tmp:
        _assert_no_mutation(tmp, _env(tmp),
                            ["queue", "add", "--scope", "repo:foo"])


def test_add_empty_rejected():
    with tempfile.TemporaryDirectory() as tmp:
        _assert_no_mutation(tmp, _env(tmp),
                            ["queue", "add", "--scope", "repo:foo",
                             "--desc-file", "-"], stdin="  \n")


def test_add_missing_file_rejected():
    with tempfile.TemporaryDirectory() as tmp:
        _assert_no_mutation(tmp, _env(tmp),
                            ["queue", "add", "--scope", "repo:foo",
                             "--desc-file", "/nonexistent/zzz"])


def test_add_failed_validation_leaves_queue_unchanged():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env(tmp)
        _add_file(env, "seed")
        before = _queue_bytes(tmp)
        # No --scope: validation fails after the file was read.
        r = _run(env, "queue", "add", "--desc-file", "-", stdin="hello")
        assert r.returncode != 0
        assert _queue_bytes(tmp) == before


def test_set_and_complete_desc_file():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env(tmp)
        r = _run(env, "set", "--desc-file", "-", stdin=NASTY)
        assert r.returncode == 0, r.stderr
        got = json.loads(_run(env, "get").stdout)
        assert got["task"] == NASTY
        r = _run(env, "complete", "--desc-file", "-", stdin=NASTY)
        assert r.returncode == 0, r.stderr
        assert NASTY in r.stdout
        # mutual exclusion / neither
        assert _run(env, "set", "x", "--desc-file", "-", stdin="y").returncode == 1
        assert _run(env, "set").returncode == 1
        assert _run(env, "complete").returncode == 1


def test_block_and_abandon_reason_file():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env(tmp)
        a = _add_file(env, "d1")
        b = _add_file(env, "d2", "--force-enqueue")
        r = _run(env, "queue", "block", a["id"], "--reason-file", "-",
                 "--silent", stdin=NASTY)
        assert r.returncode == 0, r.stderr
        assert _show(env, a["id"])["block_reason"] == NASTY
        r = _run(env, "queue", "abandon", b["id"], "--reason-file", "-",
                 "--silent", stdin=NASTY)
        assert r.returncode == 0, r.stderr
        assert _show(env, b["id"])["abandon_reason"] == NASTY
        # exclusion + empty + missing-reason errors leave state alone
        c = _add_file(env, "d3", "--force-enqueue")
        before = _queue_bytes(tmp)
        assert _run(env, "queue", "block", c["id"], "--reason", "x",
                    "--reason-file", "-", stdin="y").returncode == 1
        assert _run(env, "queue", "block", c["id"], "--reason-file", "-",
                    stdin=" ").returncode == 1
        assert _run(env, "queue", "block", c["id"]).returncode == 1
        assert _queue_bytes(tmp) == before


def test_help_shows_heredoc_form_first():
    r = _run({**os.environ}, "queue", "add", "--help")
    assert r.returncode == 0
    assert "--desc-file - <<'EOF'" in r.stdout
    assert r.stdout.index("<<'EOF'") < r.stdout.index("positional arguments")
