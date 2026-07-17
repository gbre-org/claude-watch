#!/usr/bin/env python3
"""Tests for `repo:<name>` scope validation in `queue add` / `queue update-scope`.

Context (botchat #2043, 2026-07-16): the main loop kept INVENTING repo
scope names that matched no real repo dir (`repo:ACME_ORG-2421-classify`,
`repo:botchat-ui`, `repo:botchat-renderer`, ...). Two agents meant to
serialize on the SAME repo used different invented scope names, so they
didn't serialize -- contributing to a merge/conflict mess. Fix: at add
time, a `repo:<name>` scope token must name an actual directory living in
the configured repos dir (SESSION_TASK_REPOS_DIR, default ~/repos).

Design invariants exercised here:
  * Only the `repo:` prefix is validated; every other namespace is free-form.
  * FAIL-OPEN when the repos dir is absent / not a dir (so stripped-down
    deploys and the wider test suite -- which use HOME=tmpdir with no
    repos/ subtree and fictional repo names -- keep working).
  * Reject with an actionable message naming the token, the repos dir, and
    the valid repo list; exit 1.
  * Bypass via SESSION_TASK_REPOS_NO_VALIDATE=1.
  * SESSION_TASK_REPOS_DIR overrides the root.

Run:
    uv run --python 3.11 --with pytest \\
        pytest tools/session-task/tests/test_queue_repo_scope_validation.py -v

Or directly (no pytest needed):
    python3 tools/session-task/tests/test_queue_repo_scope_validation.py
"""

import json
import os
import subprocess
import sys
import tempfile
from pathlib import Path

SESSION_TASK = Path(__file__).resolve().parent.parent / "session-task"


def _env_for_tmp(tmp, *, make_repos=("claude-watch", "ACME_ORG")):
    """Build a test env with HOME=tmp and an optional repos/ subtree.

    Pass make_repos=None to leave the repos dir ABSENT (fail-open case).
    """
    env = dict(os.environ)
    env["HOME"] = str(tmp)
    env["PINGME_SESSION_TASK"] = "0"
    Path(tmp, ".config/session").mkdir(parents=True, exist_ok=True)
    if make_repos is not None:
        for name in make_repos:
            Path(tmp, "repos", name).mkdir(parents=True, exist_ok=True)
    return env


def _run(env, *argv, timeout=15):
    cmd = [sys.executable, str(SESSION_TASK)] + list(argv)
    return subprocess.run(
        cmd, capture_output=True, text=True, env=env, timeout=timeout
    )


def _add(env, desc, scope_args, *extra):
    cmd = ["queue", "add", desc, "--summary", "t", "--json"]
    for s in scope_args:
        cmd.extend(["--scope", s])
    cmd.extend(extra)
    return _run(env, *cmd)


def test_valid_repo_scope_accepted():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        r = _add(env, "valid", ["repo:claude-watch"])
        assert r.returncode == 0, r.stderr
        d = json.loads(r.stdout)
        assert d["scope"] == ["repo:claude-watch"]


def test_invalid_repo_scope_rejected():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        r = _add(env, "invalid", ["repo:botchat-ui"])
        assert r.returncode == 1, (r.returncode, r.stdout, r.stderr)
        # Actionable message: names the token, the repos dir, valid list.
        assert "repo:botchat-ui" in r.stderr
        assert "no directory 'botchat-ui'" in r.stderr
        assert "repos" in r.stderr
        assert "claude-watch" in r.stderr  # valid list surfaced
        assert "ACME_ORG" in r.stderr


def test_non_repo_scopes_are_free_form():
    """Only the repo: prefix is validated; other namespaces pass through."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        for tok in [
            "hostjob:whatever",
            "resource:hardware",
            "flaky-cli",
            "slack-mcp-foo",
            "host-git-config",
            "*",
        ]:
            r = _add(env, f"free-{tok}", [tok])
            assert r.returncode == 0, f"{tok!r} should be free-form: {r.stderr}"


def test_fail_open_when_repos_dir_absent():
    """No repos/ subtree => cannot validate => allow (fail open)."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp, make_repos=None)
        r = _add(env, "failopen", ["repo:anything-goes"])
        assert r.returncode == 0, (r.returncode, r.stderr)


def test_bypass_env_var():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        env["SESSION_TASK_REPOS_NO_VALIDATE"] = "1"
        r = _add(env, "bypass", ["repo:not-a-real-repo"])
        assert r.returncode == 0, (r.returncode, r.stderr)


def test_bypass_env_var_zero_does_not_bypass():
    """SESSION_TASK_REPOS_NO_VALIDATE=0 must NOT bypass (still validates)."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        env["SESSION_TASK_REPOS_NO_VALIDATE"] = "0"
        r = _add(env, "still-validates", ["repo:not-a-real-repo"])
        assert r.returncode == 1, (r.returncode, r.stdout, r.stderr)


def test_repos_dir_override():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)  # repos/claude-watch + repos/ACME_ORG
        alt = Path(tmp) / "altroot"
        (alt / "myproj").mkdir(parents=True)
        env["SESSION_TASK_REPOS_DIR"] = str(alt)
        # myproj is valid under the override root.
        r = _add(env, "alt-ok", ["repo:myproj"])
        assert r.returncode == 0, r.stderr
        # claude-watch exists under ~/repos but NOT under the override root.
        r2 = _add(env, "alt-bad", ["repo:claude-watch"])
        assert r2.returncode == 1, (r2.returncode, r2.stderr)


def test_multi_scope_one_invalid_rejected():
    """A valid + an invalid repo token in one add is rejected as a whole."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        r = _add(env, "mixed", ["repo:claude-watch", "repo:ghost"])
        assert r.returncode == 1, (r.returncode, r.stderr)
        assert "repo:ghost" in r.stderr


def test_hidden_dirs_not_valid_repos():
    """A leading-dot dir under repos/ is not a valid repo target."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        Path(tmp, "repos", ".hidden").mkdir(parents=True, exist_ok=True)
        r = _add(env, "hidden", ["repo:.hidden"])
        assert r.returncode == 1, (r.returncode, r.stderr)


def test_update_scope_add_validates():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        r = _add(env, "seed", ["repo:ACME_ORG"])
        qid = json.loads(r.stdout)["id"]
        # Adding an invalid repo token via update-scope is rejected.
        r2 = _run(env, "queue", "update-scope", qid, "repo:ghost")
        assert r2.returncode == 1, (r2.returncode, r2.stderr)
        assert "repo:ghost" in r2.stderr
        # Adding a valid one succeeds.
        r3 = _run(env, "queue", "update-scope", qid, "repo:claude-watch")
        assert r3.returncode == 0, r3.stderr


def test_update_scope_remove_not_validated():
    """--remove must work even for an (invalid) token, to strip stale ones."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        r = _add(env, "seed", ["repo:ACME_ORG"])
        qid = json.loads(r.stdout)["id"]
        # Removing a token that isn't a valid repo must not be blocked by
        # validation (no-op or removal, but never an add-time reject).
        r2 = _run(env, "queue", "update-scope", qid, "repo:ghost", "--remove")
        assert r2.returncode == 0, (r2.returncode, r2.stderr)


if __name__ == "__main__":
    import traceback

    tests = [v for k, v in list(globals().items()) if k.startswith("test_")]
    failed = 0
    for t in tests:
        try:
            t()
            print(f"PASS {t.__name__}")
        except Exception:
            failed += 1
            print(f"FAIL {t.__name__}")
            traceback.print_exc()
    if failed:
        print(f"\n{failed}/{len(tests)} failed")
        sys.exit(1)
    print(f"\n{len(tests)}/{len(tests)} passed")
