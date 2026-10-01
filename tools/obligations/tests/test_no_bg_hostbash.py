#!/usr/bin/env python3
"""Tests for the ``no_bg_hostbash`` evaluator (container/bin/eval-no-bg-hostbash)
and its obligations-init seeding.

Run::

    uv run --python 3.11 --with pytest \\
        pytest tools/obligations/tests/test_no_bg_hostbash.py -v
"""
import json
import os
import subprocess
import sys
from pathlib import Path

import pytest

HERE = Path(__file__).resolve().parent
OBLIGATIONS_DIR = HERE.parent
REPO = OBLIGATIONS_DIR.parent.parent
EVAL = REPO / "container" / "bin" / "eval-no-bg-hostbash"
OBLIGATIONS_INIT = OBLIGATIONS_DIR / "obligations-init"
OBLIGATIONS = OBLIGATIONS_DIR / "obligations"


def _run(payload, raw=False):
    stdin = payload if raw else json.dumps(payload)
    return subprocess.run([sys.executable, str(EVAL)], input=stdin,
                          capture_output=True, text=True, timeout=15)


def _script(body, interpreter="bash"):
    return _run({"script": body, "interpreter": interpreter})


def _command(cmd):
    return _run({"command": cmd})


DENY = [
    # The incident that motivated the gate.
    "nohup envchain sf-pce-aws helmfile -e dev -l name=model-cache apply "
    "> /tmp/mc-apply.log 2>&1 &",
    "long-thing &",
    "long-thing 2>&1 &",
    "(long-thing &)",
    "sleep 1 & wait",
    "nohup foo",
    "foo; disown",
    "setsid foo",
    "env X=1 setsid foo",
    "bash -c 'foo &'",
    "cd /x && nohup foo > out 2>&1 &",
]

ALLOW = [
    "a && b",
    "git pull && make",
    "cmd 2>&1",
    "cmd >&2",
    "cmd &> file",
    "cmd 2>&1 | tee f",
    "echo nohup",
    "grep -r 'nohup foo &' .",
    "git commit -m 'stop using nohup and &'",
    # Heredoc data mentioning the words is not a command.
    "cat <<'EOF'\nnohup foo &\ndisown\nEOF",
    # Whole-line comments are not commands.
    "# nohup foo &\nls",
    # hostjob / workload background internally and are the sanctioned route.
    "hostjob run --label L --cwd /tmp -- make build",
    "hostjob run --label L --cwd /tmp -- nohup foo &",
    "workload run L -- rsync a b",
    "hostjob run --label L -- x &",
    # Escape hatch.
    "# BACKGROUND_OK: one-off daemon start\nnohup foo &",
    "nohup foo &  # BACKGROUND_OK: operator asked for it",
    "",
]


@pytest.mark.parametrize("body", DENY)
def test_script_denies(body):
    r = _script(body)
    assert r.returncode == 1, (body, r.stderr)
    assert "hostjob run --label L --cwd D -- cmd" in r.stderr
    assert "hostjob wait L" in r.stderr
    assert "workload run" in r.stderr


@pytest.mark.parametrize("body", ALLOW)
def test_script_allows(body):
    r = _script(body)
    assert r.returncode == 0, (body, r.stderr)


@pytest.mark.parametrize("body", DENY[:4])
def test_run_command_field_denies(body):
    assert _command(body).returncode == 1


def test_run_command_allows_and():
    assert _command("a && b").returncode == 0


def test_raw_string_stdin_supported():
    assert _run("nohup foo &", raw=True).returncode == 1
    assert _run("a && b", raw=True).returncode == 0


def test_non_shell_interpreter_allowed():
    # Python body with a '&' operator is not a shell script.
    assert _script("x = a & b\nprint(x)", "python3").returncode == 0


def test_default_open_on_garbage():
    assert _run("{not json", raw=True).returncode == 0
    assert _run({"script": None}).returncode == 0
    assert _run({"script": "echo 'unterminated"}).returncode == 0


# --- seeding --------------------------------------------------------------

def _env(tmp_path):
    env = os.environ.copy()
    env["HOME"] = str(tmp_path)
    env["CLAUDE_OBLIGATIONS_MANIFEST_DIR"] = str(tmp_path / "none")
    env["CW_EVAL_BIN_DIR"] = str(EVAL.parent)
    env["PATH"] = str(OBLIGATIONS_DIR) + os.pathsep + env.get("PATH", "")
    return env


def _rows(env):
    r = subprocess.run([sys.executable, str(OBLIGATIONS), "list", "--json"],
                       capture_output=True, text=True, env=env, timeout=15)
    assert r.returncode == 0, r.stderr
    return [o for o in json.loads(r.stdout).get("obligations", [])
            if "no_bg_hostbash" in (o.get("deny_message") or "")]


def test_seed_idempotent_both_tools(tmp_path):
    env = _env(tmp_path)
    for _ in range(2):
        r = subprocess.run(
            [sys.executable, str(OBLIGATIONS_INIT), "--only", "no_bg_hostbash"],
            capture_output=True, text=True, env=env, timeout=30)
        assert r.returncode == 0, r.stderr
    rows = _rows(env)
    assert sorted(o["tool_pattern"] for o in rows) == [
        "mcp__host-bash__run_command", "mcp__host-bash__run_script"]


def _check(env, tool, tool_input):
    return subprocess.run(
        [sys.executable, str(OBLIGATIONS), "check", "--tool", tool,
         "--command-string", json.dumps(tool_input), "--json"],
        capture_output=True, text=True, env=env, timeout=20)


def test_end_to_end_gate_denies_and_allows(tmp_path):
    env = _env(tmp_path)
    subprocess.run([sys.executable, str(OBLIGATIONS_INIT), "--only",
                    "no_bg_hostbash"], env=env, check=True, timeout=30,
                   capture_output=True)
    bad = _check(env, "mcp__host-bash__run_script",
                 {"interpreter": "bash", "script": "nohup x > /tmp/l 2>&1 &"})
    good = _check(env, "mcp__host-bash__run_script",
                  {"interpreter": "bash",
                   "script": "hostjob run --label L --cwd /tmp -- x"})
    assert bad.returncode != 0, bad.stdout
    assert good.returncode == 0, good.stdout + good.stderr
    bad_cmd = _check(env, "mcp__host-bash__run_command",
                     {"command": "nohup x &"})
    assert bad_cmd.returncode != 0
