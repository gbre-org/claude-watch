#!/usr/bin/env python3
"""Archived queue logs must not retain credentials pasted into transcripts.

Run::

    uv run --python 3.11 --with pytest \
        pytest tools/session-task/tests/test_archive_redaction.py -v
"""

import importlib.machinery
import importlib.util
import json
from pathlib import Path

SRC = Path(__file__).resolve().parent.parent / "session-task"


def _load():
    loader = importlib.machinery.SourceFileLoader("session_task_mod", str(SRC))
    spec = importlib.util.spec_from_loader("session_task_mod", loader)
    mod = importlib.util.module_from_spec(spec)
    loader.exec_module(mod)
    return mod


st = _load()
SECRET = "Zx9Qw8Er7Ty6Ui5Op4As3Df2Gh1Jk0LzXcVbNmQwErTyUiOp"


def test_url_password_and_env_value_redacted_json_stays_valid(tmp_path):
    line = json.dumps({"out": f"BOTCHAT_DSN=postgresql://botchat:{SECRET}@db:5432/x\n"
                              f"POSTGRES_PASSWORD={SECRET}\nOTHER=1\n"})
    src, dest = tmp_path / "a.jsonl", tmp_path / "b.jsonl"
    src.write_text(line + "\n")
    st._copy_archive_redacted(src, dest)
    out = dest.read_text()
    assert SECRET not in out
    parsed = json.loads(out.splitlines()[0])  # still valid JSON
    assert "postgresql://botchat:<REDACTED>@db:5432/x" in parsed["out"]
    assert "POSTGRES_PASSWORD=<REDACTED>" in parsed["out"]
    assert "OTHER=1" in parsed["out"]


def test_no_false_positives_on_ordinary_text():
    for ok in ("https://example.com/a/b", "ssh://git@host/repo.git",
               "PASSWORD=***", "TOKEN=short", "user:name@host is fine",
               "the password field is required", "https://u:p@h/"):
        assert st._redact_archive_text(ok) == ok
