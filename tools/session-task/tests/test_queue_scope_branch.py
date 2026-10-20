#!/usr/bin/env python3
"""Tests for `branch:<repo>@<branch>` scope tokens (worktree-derived scope,
phase 0: token type + overlap + read-only `worktree` field; no producer).

Run:
    uv run --python 3.11 --with pytest \
        pytest tools/session-task/tests/test_queue_scope_branch.py -v
"""

import importlib.machinery
import importlib.util
import itertools
import json
import os
import subprocess
import sys
import tempfile
from pathlib import Path

import pytest

SESSION_TASK = Path(__file__).resolve().parent.parent / "session-task"


def _load():
    loader = importlib.machinery.SourceFileLoader("session_task_mod", str(SESSION_TASK))
    spec = importlib.util.spec_from_loader("session_task_mod", loader)
    mod = importlib.util.module_from_spec(spec)
    loader.exec_module(mod)
    return mod


st = _load()


def _env(tmp):
    env = dict(os.environ)
    env["HOME"] = str(tmp)
    env["SESSION_TASK_REPOS_NO_VALIDATE"] = "1"
    Path(tmp, ".config/session").mkdir(parents=True, exist_ok=True)
    return env


def _run(env, *argv):
    return subprocess.run([sys.executable, str(SESSION_TASK)] + list(argv),
                          capture_output=True, text=True, env=env, timeout=30)


def _add(env, scopes, desc="t"):
    cmd = ["queue", "add", desc, "--summary", "t", "--json"]
    for s in scopes:
        cmd += ["--scope", s]
    return _run(env, *cmd)


# --- normalization -------------------------------------------------------

@pytest.mark.parametrize("tok", [
    "branch:repoA@main",
    "branch:repoA@user/feature-1",
    "branch:repo-a@release/1.2",
])
def test_normalize_valid(tok):
    assert st._normalize_token(tok) == tok


@pytest.mark.parametrize("tok", [
    "branch:", "branch:repoA", "branch:repoA@", "branch:@main",
    "branch:a/b@main", "branch:repoA@has space", "branch:repoA@a..b",
    "branch:repoA@x@{1}", "branch:repoA@a~1", "branch:repoA@a:b",
    "branch:repoA@/lead", "branch:repoA@trail/", "branch:repoA@a//b",
    "branch:repoA@x.lock", "branch:repoA@-dash",
])
def test_normalize_invalid(tok):
    with pytest.raises(ValueError):
        st._normalize_token(tok)


# --- overlap -------------------------------------------------------------

def ov(a, b):
    r1, r2 = st._tokens_overlap(a, b), st._tokens_overlap(b, a)
    assert r1 == r2, (a, b)
    return r1


def test_overlap_rules():
    assert ov("branch:R@b", "branch:R@b")
    assert not ov("branch:R@b", "branch:R@c")
    assert not ov("branch:R@b", "branch:S@b")
    assert ov("repo:R", "branch:R@b")
    assert not ov("repo:S", "branch:R@b")
    assert ov("path:R/x/y", "branch:R@b")
    assert not ov("path:S/x", "branch:R@b")
    assert ov("*", "branch:R@b")
    assert not ov("branch:R@b", "hostjob:R")
    assert not ov("branch:R@b", "resource:R")
    assert not ov("branch:R@b", "file:/tmp/R")
    assert not ov("branch:R@b", "task:q-1")
    # branch names containing '/' are compared verbatim
    assert ov("branch:R@u/f", "branch:R@u/f")
    assert not ov("branch:R@u/f", "branch:R@u")


def test_overlap_symmetric_matrix():
    toks = ["*", "repo:R", "repo:S", "path:R/a", "path:S/a", "branch:R@b",
            "branch:R@c", "branch:S@b", "hostjob:x", "resource:r", "task:q-1"]
    for a, b in itertools.product(toks, toks):
        assert st._tokens_overlap(a, b) == st._tokens_overlap(b, a), (a, b)


# --- regression: pre-existing token types behave as before ---------------

@pytest.mark.parametrize("a,b,expected", [
    ("repo:R", "repo:R", True), ("repo:R", "repo:S", False),
    ("repo:R", "path:R/x", True), ("repo:R", "path:S/x", False),
    ("path:R/a", "path:R/a/b", True), ("path:R/a", "path:R/b", False),
    ("path:R/a", "path:S/a", False),
    ("file:/a/b", "file:/a/b/c", True), ("file:/a/b", "file:/a/bc", False),
    ("tool:x", "tool:x", True), ("tool:x", "tool:y", False),
    ("hostjob:x", "hostjob:x", True), ("hostjob:x", "hostjob:y", False),
    ("cluster:c", "cluster:c", True), ("cluster:c", "cluster:d", False),
    ("workload:w", "workload:w", True), ("workload:w", "workload:v", False),
    ("resource:r", "resource:r", True), ("resource:r", "resource:s", False),
    ("task:q-1", "task:q-1", True), ("task:q-1", "task:q-2", False),
    ("task:q-1", "repo:R", False),
    ("*", "repo:R", True), ("*", "task:q-1", True),
    ("repo:R", "hostjob:R", False),
])
def test_regression_existing_overlap(a, b, expected):
    assert st._tokens_overlap(a, b) is expected
    assert st._tokens_overlap(b, a) is expected


@pytest.mark.parametrize("tok,out", [
    ("repo:R", "repo:R"), ("resource:r", "resource:r"), ("book:b", "book:b"),
    ("agent-proto:p", "agent-proto:p"), ("task:q-1", "task:q-1"),
    ("hostjob:x", "hostjob:x"), ("cluster:c", "cluster:c"),
    ("workload:w", "workload:w"), ("*", "*"),
    ("path://R//a/b/", "path:R/a/b"), ("file:/a/b/**", "file:/a/b"),
    ("somefreeform", "somefreeform"), ("branchless", "branchless"),
])
def test_regression_existing_normalize(tok, out):
    assert st._normalize_token(tok) == out


def test_regression_path_still_rejects_bare():
    with pytest.raises(ValueError):
        st._normalize_token("path:R")


# --- scheduling ----------------------------------------------------------

def test_same_branch_serializes_different_branch_parallel():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env(tmp)
        d1 = json.loads(_add(env, ["branch:R@a"]).stdout)
        assert d1["ready_now"] is True
        assert _run(env, "queue", "register", d1["id"], "--json").returncode == 0
        d2 = json.loads(_add(env, ["branch:R@b"]).stdout)
        assert d2["ready_now"] is True, d2
        d3 = json.loads(_add(env, ["branch:R@a"]).stdout)
        assert d3["ready_now"] is False and d1["id"] in d3["serialized_after"], d3
        d4 = json.loads(_add(env, ["repo:R"]).stdout)
        assert d4["ready_now"] is False and d1["id"] in d4["serialized_after"], d4
        d5 = json.loads(_add(env, ["branch:S@a"]).stdout)
        assert d5["ready_now"] is True, d5


def test_repo_running_blocks_branch_add():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env(tmp)
        d1 = json.loads(_add(env, ["repo:R"]).stdout)
        assert _run(env, "queue", "register", d1["id"], "--json").returncode == 0
        d2 = json.loads(_add(env, ["branch:R@a"]).stdout)
        assert d2["ready_now"] is False and d1["id"] in d2["serialized_after"], d2


def test_additive_scope_retained_alongside_branch():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env(tmp)
        d = json.loads(_add(env, ["branch:R@a", "hostjob:x"]).stdout)
        shown = json.loads(_run(env, "queue", "show", d["id"]).stdout)
        assert shown["scope"] == ["branch:R@a", "hostjob:x"]


def test_invalid_branch_token_rejected_by_add():
    with tempfile.TemporaryDirectory() as tmp:
        r = _add(_env(tmp), ["branch:R"])
        assert r.returncode != 0


# --- optional `worktree` field is read-only / harmless -------------------

def test_worktree_field_roundtrips_and_legacy_unaffected():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env(tmp)
        legacy = json.loads(_add(env, ["hostjob:legacy"]).stdout)
        withwt = json.loads(_add(env, ["branch:R@a"]).stdout)
        qf = None
        for p in Path(tmp).rglob("*.json"):
            try:
                data = json.loads(p.read_text())
            except Exception:
                continue
            if isinstance(data, dict) and any(
                    i.get("id") == withwt["id"] for i in data.get("items", [])
                    if isinstance(i, dict)):
                qf = p
                break
        assert qf is not None, "queue file not found"
        data = json.loads(qf.read_text())
        for it in data["items"]:
            if it["id"] == withwt["id"]:
                it["worktree"] = {"repo": "R", "branch": "a",
                                  "path": "/x", "source": "explicit",
                                  "token": "branch:R@a", "future_key": 1}
        qf.write_text(json.dumps(data))
        # Reads, lists, spawn-check, and an unrelated write all still work and
        # the field survives the round trip untouched.
        assert _run(env, "queue", "list", "--json").returncode == 0
        assert _run(env, "queue", "spawn-check", withwt["id"]).returncode in (0, 1, 2, 3)
        assert _run(env, "queue", "register", legacy["id"], "--json").returncode == 0
        shown = json.loads(_run(env, "queue", "show", withwt["id"]).stdout)
        assert shown["worktree"]["future_key"] == 1
        assert shown["scope"] == ["branch:R@a"]
        assert "worktree" not in json.loads(_run(env, "queue", "show", legacy["id"]).stdout)
