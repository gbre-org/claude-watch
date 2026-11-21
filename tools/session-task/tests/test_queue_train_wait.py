#!/usr/bin/env python3
"""`train:<repo>@<branch>` wait matching against train-* events.

Run:
    uv run --python 3.11 --with pytest \\
        pytest tools/session-task/tests/test_queue_train_wait.py -v
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

_loader = importlib.machinery.SourceFileLoader("session_task_train", str(SESSION_TASK))
st = importlib.util.module_from_spec(
    importlib.util.spec_from_loader("session_task_train", _loader))
_loader.exec_module(st)


def _landed(repo, branches, sha="4464e3dd"):
    return {"tag": "train-landed", "message": (
        f"{repo}: landed {len(branches)} branch(es) together in ONE merge to main "
        f"-> {sha}: {', '.join(branches)} (Gitea PR #2). One post-merge signed "
        f"build follows (Gitea push run for {sha}).")}


def _dropped(repo, br):
    return {"tag": "train-dropped", "message": (
        f"{repo}: {br} DROPPED: typecheck failed for tip abcd1234 (x). "
        "The rest still lands; owner must fix and push a new tip")}


def _conflict(repo, br):
    return {"tag": "train-conflict", "message": (
        f"{repo}: {br} conflicts with main 4464e3dd and was left out of the batch; "
        "owner must rebase onto main and push again")}


def _m(ref, ev):
    return st._wait_match({"kind": "train", "ref": ref}, ev)


def test_single_landed_branch():
    ev = _landed("gbfusion-ios", ["train/sso-icon"])
    assert _m("gbfusion-ios@train/sso-icon", ev) == (True, True)


def test_branch_without_train_prefix():
    ev = _landed("gbfusion-ios", ["train/sso-icon"])
    assert _m("gbfusion-ios@sso-icon", ev) == (True, True)


@pytest.mark.parametrize("br", ["train/reader-typography-icon",
                                "train/reader-small-fonts-preview",
                                "train/other"])
def test_multi_branch_batch(br):
    ev = _landed("gbpaper-ios", ["train/reader-typography-icon",
                                 "train/reader-small-fonts-preview", "train/other"])
    assert _m(f"gbpaper-ios@{br}", ev) == (True, True)


def test_owner_prefixed_repo_on_ref_and_event():
    ev = _landed("gbfusion-ios", ["train/sso-icon"])
    assert _m("andrew/gbfusion-ios@train/sso-icon", ev) == (True, True)
    ev = _landed("andrew/gbfusion-ios", ["train/sso-icon"])
    assert _m("gbfusion-ios@train/sso-icon", ev) == (True, True)


def test_structured_fields_win():
    ev = {"tag": "train-landed", "message": "whatever",
          "data": {"repo": "andrew/gbfusion-ios", "branches": ["train/sso-icon"]}}
    assert _m("gbfusion-ios@train/sso-icon", ev) == (True, True)


@pytest.mark.parametrize("mk", [_dropped, _conflict])
def test_dropped_and_conflict_match_as_failure(mk):
    ev = mk("gbfusion-ios", "train/sso-icon")
    assert _m("gbfusion-ios@train/sso-icon", ev) == (True, False)
    assert _m("andrew/gbfusion-ios@sso-icon", ev) == (True, False)


@pytest.mark.parametrize("ref", [
    "gbfusion-ios@train/sso",             # prefix of a branch, not equal
    "gbfusion-ios@train/sso-icon-2",
    "gbpaper-ios@train/sso-icon",         # wrong repo
])
def test_non_matching(ref):
    assert _m(ref, _landed("gbfusion-ios", ["train/sso-icon"])) == (False, False)
    assert _m(ref, _dropped("gbfusion-ios", "train/sso-icon")) == (False, False)


def test_dropped_free_text_branch_not_matched():
    ev = _dropped("gbfusion-ios", "train/a")
    ev["message"] += " (see train/b)"
    assert _m("gbfusion-ios@train/b", ev) == (False, False)


def test_other_tag_ignored():
    ev = _landed("gbfusion-ios", ["train/sso-icon"])
    ev["tag"] = "ci-run-finished"
    assert _m("gbfusion-ios@train/sso-icon", ev) == (False, False)


def test_sweep_closes_on_landed_and_wakes_on_dropped():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env(tmp)
        a = _running(env, "repo:t-a")
        b = _running(env, "repo:t-b")
        _run(env, "queue", "wait", a, "--on", "train:gbpaper-ios@train/x",
             "--done-on-success")
        _run(env, "queue", "wait", b, "--on", "train:gbfusion-ios@train/sso-icon",
             "--done-on-success")
        _emit(env, "train-landed", _landed("gbpaper-ios", ["train/w", "train/x"])["message"], {})
        _emit(env, "train-dropped", _dropped("gbfusion-ios", "train/sso-icon")["message"], {})
        out = json.loads(_run(env, "queue", "sweep", "--json").stdout)
        assert [x["id"] for x in out["closed"]] == [a]
        assert [x["id"] for x in out["woke"]] == [b] and out["woke"][0]["success"] is False
        assert _show(env, a)["status"] == "done"
        assert _show(env, b)["status"] == "pending"
