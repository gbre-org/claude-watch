#!/usr/bin/env python3
"""`ci:<repo>@<sha|run>` wait matching.

A ci: wait on a short sha must resolve when the CI event carries the full
40-char sha, with the owner prefix on the repo optional, and failed
conclusions must not close a --done-on-success wait.

Run:
    uv run --python 3.11 --with pytest \\
        pytest tools/session-task/tests/test_queue_ci_wait.py -v
"""

import importlib.machinery
import importlib.util
import json
import sys
import tempfile
from pathlib import Path

import pytest

sys.path.insert(0, str(Path(__file__).resolve().parent))
from test_queue_waiting import (SESSION_TASK, _emit, _env, _run, _running,  # noqa: E402
                                _show)

_loader = importlib.machinery.SourceFileLoader("session_task_ci", str(SESSION_TASK))
st = importlib.util.module_from_spec(
    importlib.util.spec_from_loader("session_task_ci", _loader))
_loader.exec_module(st)

FULL = "3740bfd45def2bd524ce8791ab99e3acbb138686"


def _ev(conclusion="success", repo="media-tools", sha=FULL, run="407"):
    return {"tag": "ci-run-finished",
            "message": f"{repo} run #{run} ({sha[:8]} main) finished",
            "data": {"repo": repo, "sha": sha, "run_number": run,
                     "run_id": "6115", "conclusion": conclusion}}


def _match(ref, ev):
    return st._wait_match({"kind": "ci", "ref": ref}, ev)


@pytest.mark.parametrize("ref", [
    "media-tools@3740bfd",                # short ref, full event sha
    f"media-tools@{FULL}",                # full ref
    f"media-tools@{FULL}deadbeef",        # ref longer than event sha
    "andrew/media-tools@3740bfd",         # owner prefix on the ref
    "MEDIA-TOOLS@3740BFD",                # case
    "media-tools@407",                    # run number
    "media-tools@6115",                   # run id
])
def test_ci_ref_matches(ref):
    assert _match(ref, _ev()) == (True, True)


def test_event_repo_with_owner_prefix_matches_bare_ref():
    assert _match("media-tools@3740bfd", _ev(repo="andrew/media-tools")) == (True, True)


@pytest.mark.parametrize("ref", [
    "other-repo@3740bfd",                 # wrong repo
    "media-tools@3740bf",                 # prefix below the 7-char minimum
    "media-tools@3740bfe",                # different sha
    "media-tools@40",                     # digits are a run id, never a substring
])
def test_ci_ref_does_not_match(ref):
    assert _match(ref, _ev()) == (False, False)


def test_failure_conclusion_matches_but_not_success():
    assert _match("media-tools@3740bfd", _ev("failure")) == (True, False)
    assert _match("media-tools@3740bfd", _ev("cancelled")) == (True, False)


def test_non_ci_tag_ignored():
    ev = _ev()
    ev["tag"] = "ci-run-started"
    assert _match("media-tools@3740bfd", ev) == (False, False)


def test_unstructured_event_falls_back_to_substring():
    ev = {"tag": "ci-run-finished", "data": {"conclusion": "success"},
          "message": "media-tools@3740bfd finished"}
    assert _match("media-tools@3740bfd", ev) == (True, True)


def test_sweep_closes_short_sha_wait_on_full_sha_event():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env(tmp)
        q = _running(env, "repo:ci-a")
        _run(env, "queue", "wait", q, "--on", "ci:media-tools@3740bfd",
             "--done-on-success")
        _emit(env, "ci-run-finished", "ok", _ev()["data"])
        out = json.loads(_run(env, "queue", "sweep", "--json").stdout)
        assert [x["id"] for x in out["closed"]] == [q]
        assert _show(env, q)["status"] == "done"


def test_sweep_failed_ci_reopens_instead_of_closing():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env(tmp)
        q = _running(env, "repo:ci-b")
        _run(env, "queue", "wait", q, "--on", "ci:andrew/media-tools@3740bfd",
             "--done-on-success")
        _emit(env, "ci-run-finished", "bad", _ev("failure")["data"])
        out = json.loads(_run(env, "queue", "sweep", "--json").stdout)
        assert out["closed"] == [] and out["woke"][0]["success"] is False
        assert _show(env, q)["status"] == "pending"
