#!/usr/bin/env python3
"""Tests for `path:<repo>/<subdir>` scope tokens.

Added 2026-05-11. Context: Andrew flagged that the queue was
over-serializing when two items both tagged `repo:X` even though they
touched disjoint subtrees of X. `path:X/a/b` is a finer-grained claim
that overlaps `repo:X` (broad claim still wins) but does NOT overlap
sibling `path:X/c/d`.

Overlap rules tested here:
  * `repo:X` overlaps any `path:X/...`           (broad claims everything)
  * `path:X/a` overlaps `path:X/a/b`             (prefix relationship)
  * `path:X/a` does NOT overlap `path:X/b`       (siblings disjoint)
  * `path:X/a` does NOT overlap `path:Y/a`       (cross-repo never overlaps)
  * mixed-token compositions still work          (workload:foo dominates)

Run:
    uv run --python 3.11 --with pytest \\
        pytest tools/session-task/tests/test_queue_scope_path.py -v
"""

import json
import os
import subprocess
import sys
import tempfile
from pathlib import Path

SESSION_TASK = Path(__file__).resolve().parent.parent / "session-task"


def _env_for_tmp(tmp):
    env = dict(os.environ)
    env["HOME"] = str(tmp)
    Path(tmp, ".config/session").mkdir(parents=True, exist_ok=True)
    return env


def _run(env, *argv, check=False, timeout=15):
    cmd = [sys.executable, str(SESSION_TASK)] + list(argv)
    r = subprocess.run(cmd, capture_output=True, text=True, env=env,
                       timeout=timeout)
    if check and r.returncode != 0:
        raise RuntimeError(
            f"command failed rc={r.returncode}\n"
            f"  cmd: {' '.join(argv)}\n"
            f"  stdout: {r.stdout}\n"
            f"  stderr: {r.stderr}"
        )
    return r


def _add(env, desc, scope_args, *extra):
    cmd = ["queue", "add", desc, "--summary", "t", "--json"]
    for s in scope_args:
        cmd.extend(["--scope", s])
    cmd.extend(extra)
    # Auto-pass --force-enqueue when any scope token is `workload:<label>`
    # so the cmd_queue_add hard-fail (refuses manual workload-scope adds)
    # doesn't trip these scope-overlap tests, which legitimately need to
    # exercise workload tokens. The workload runner itself passes the
    # same flag on its auto-add path.
    if any(s.startswith("workload:") for s in scope_args) and "--force-enqueue" not in cmd:
        cmd.append("--force-enqueue")
    return _run(env, *cmd)


def _register(env, qid, *extra):
    return _run(env, "queue", "register", qid, *extra)


def _done(env, qid):
    return _run(env, "queue", "done", qid, check=True)


# ---------------------------------------------------------------------------
# 1. repo:X overlaps path:X/a -- the broad claim still wins
# ---------------------------------------------------------------------------


def test_repo_overlaps_path_in_same_repo():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)

        r1 = _add(env, "broad", ["repo:dockergom"])
        assert r1.returncode == 0, r1.stderr
        d1 = json.loads(r1.stdout)
        assert d1["ready_now"] is True
        rr = _register(env, d1["id"], "--json")
        assert rr.returncode == 0, rr.stderr

        # A path: claim INSIDE the running repo must DETECT the conflict.
        # As of 2026-05-19 the default soft-serializes (exit 0, ready=false,
        # serialized_after records the running peer) instead of hard-failing.
        r2 = _add(env, "narrow", ["path:dockergom/monitoring"])
        assert r2.returncode == 0, (
            f"expected exit 0 (soft-serialize behind repo:dockergom), "
            f"got rc={r2.returncode}\n"
            f"  stdout: {r2.stdout}\n"
            f"  stderr: {r2.stderr}"
        )
        d2 = json.loads(r2.stdout)
        assert d2["ready_now"] is False, d2
        assert d1["id"] in d2["serialized_after"], d2


def test_path_overlaps_repo_when_path_added_first():
    """Mirror of the above: path: running, repo: incoming."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)

        r1 = _add(env, "narrow", ["path:dockergom/monitoring/ecowitt"])
        assert r1.returncode == 0, r1.stderr
        d1 = json.loads(r1.stdout)
        assert d1["ready_now"] is True
        rr = _register(env, d1["id"], "--json")
        assert rr.returncode == 0, rr.stderr

        # Whole-repo claim must conflict with the running narrow path
        # (soft-serialize: exit 0, ready=false, serialized_after set).
        r2 = _add(env, "broad", ["repo:dockergom"])
        assert r2.returncode == 0, (
            f"expected exit 0 (soft-serialize behind path:dockergom/...), "
            f"got rc={r2.returncode}\n"
            f"  stdout: {r2.stdout}\n"
            f"  stderr: {r2.stderr}"
        )
        d2 = json.loads(r2.stdout)
        assert d2["ready_now"] is False, d2
        assert d1["id"] in d2["serialized_after"], d2


# ---------------------------------------------------------------------------
# 2. Sibling path: tokens in the same repo are disjoint (the headline fix)
# ---------------------------------------------------------------------------


def test_sibling_path_tokens_do_not_overlap():
    """Two unrelated subdirectories (e.g. monitoring/ + a sibling exporter
    dir) on the same repo should NOT block each other's queue items.
    """
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)

        r1 = _add(env, "monitoring", ["path:dockergom/monitoring"])
        assert r1.returncode == 0, r1.stderr
        d1 = json.loads(r1.stdout)
        assert d1["ready_now"] is True
        rr = _register(env, d1["id"], "--json")
        assert rr.returncode == 0, rr.stderr

        # Disjoint sibling subdir on the same repo should sail through.
        r2 = _add(env, "minisite", ["path:dockergom/queue-minisite"])
        assert r2.returncode == 0, (
            f"expected siblings to not conflict, got rc={r2.returncode}\n"
            f"  stdout: {r2.stdout}\n"
            f"  stderr: {r2.stderr}"
        )
        d2 = json.loads(r2.stdout)
        assert d2["ready_now"] is True, d2


# ---------------------------------------------------------------------------
# 3. Prefix relationship: path:X/a overlaps path:X/a/b
# ---------------------------------------------------------------------------


def test_prefix_path_tokens_overlap():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)

        r1 = _add(env, "shallow", ["path:dockergom/monitoring"])
        assert r1.returncode == 0, r1.stderr
        d1 = json.loads(r1.stdout)
        rr = _register(env, d1["id"], "--json")
        assert rr.returncode == 0, rr.stderr

        # Deeper path under the shallow claim must conflict (soft-serialize).
        r2 = _add(env, "deep", ["path:dockergom/monitoring/ecowitt"])
        assert r2.returncode == 0, (
            f"expected exit 0 (soft-serialize on prefix overlap), "
            f"got rc={r2.returncode}\n"
            f"  stdout: {r2.stdout}\n"
            f"  stderr: {r2.stderr}"
        )
        d2 = json.loads(r2.stdout)
        assert d2["ready_now"] is False, d2
        assert d1["id"] in d2["serialized_after"], d2


def test_prefix_path_tokens_overlap_other_direction():
    """Deeper path running, shallower path incoming -- still overlap."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)

        r1 = _add(env, "deep", ["path:dockergom/monitoring/ecowitt"])
        assert r1.returncode == 0, r1.stderr
        d1 = json.loads(r1.stdout)
        rr = _register(env, d1["id"], "--json")
        assert rr.returncode == 0, rr.stderr

        r2 = _add(env, "shallow", ["path:dockergom/monitoring"])
        assert r2.returncode == 0, (
            f"expected exit 0 (soft-serialize on reverse prefix overlap), "
            f"got rc={r2.returncode}\n"
            f"  stdout: {r2.stdout}\n"
            f"  stderr: {r2.stderr}"
        )
        d2 = json.loads(r2.stdout)
        assert d2["ready_now"] is False, d2
        assert d1["id"] in d2["serialized_after"], d2


# ---------------------------------------------------------------------------
# 4. Cross-repo path: tokens never overlap, even with identical subdir
# ---------------------------------------------------------------------------


def test_cross_repo_path_tokens_disjoint():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)

        r1 = _add(env, "repoX", ["path:repoX/monitoring"])
        assert r1.returncode == 0, r1.stderr
        d1 = json.loads(r1.stdout)
        rr = _register(env, d1["id"], "--json")
        assert rr.returncode == 0, rr.stderr

        # Different repo, same-named subdir -- must not collide.
        r2 = _add(env, "repoY", ["path:repoY/monitoring"])
        assert r2.returncode == 0, (
            f"expected cross-repo disjoint, got rc={r2.returncode}\n"
            f"  stdout: {r2.stdout}\n"
            f"  stderr: {r2.stderr}"
        )
        d2 = json.loads(r2.stdout)
        assert d2["ready_now"] is True


# ---------------------------------------------------------------------------
# 5. Mixed-token composition: path:X/a + workload:foo vs path:X/b + workload:foo
#    should STILL overlap because workload:foo matches exactly. The path
#    siblings would not, but the workload should still serialize them.
# ---------------------------------------------------------------------------


def test_mixed_path_workload_overlaps_on_workload():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)

        r1 = _add(env, "first",
                  ["path:dockergom/monitoring", "workload:foo"])
        assert r1.returncode == 0, r1.stderr
        d1 = json.loads(r1.stdout)
        rr = _register(env, d1["id"], "--json")
        assert rr.returncode == 0, rr.stderr

        # Disjoint path siblings but shared workload -- must still conflict
        # (soft-serialize behind the running peer that holds workload:foo).
        r2 = _add(env, "second",
                  ["path:dockergom/queue-minisite", "workload:foo"])
        assert r2.returncode == 0, (
            f"expected exit 0 (soft-serialize, workload dominates), "
            f"got rc={r2.returncode}\n"
            f"  stdout: {r2.stdout}\n"
            f"  stderr: {r2.stderr}"
        )
        d2 = json.loads(r2.stdout)
        assert d2["ready_now"] is False, d2
        assert d1["id"] in d2["serialized_after"], d2


# ---------------------------------------------------------------------------
# 6. Normalization: trailing slash, repeated slashes, leading slash all
#    collapse to the canonical form.
# ---------------------------------------------------------------------------


def test_path_token_normalization_trailing_slash():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)

        r = _add(env, "norm", ["path:dockergom/monitoring/"])
        assert r.returncode == 0, r.stderr
        d = json.loads(r.stdout)
        assert d["scope"] == ["path:dockergom/monitoring"], d["scope"]


def test_path_token_normalization_repeated_slashes():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)

        r = _add(env, "norm", ["path:dockergom//monitoring///ecowitt/"])
        assert r.returncode == 0, r.stderr
        d = json.loads(r.stdout)
        assert d["scope"] == [
            "path:dockergom/monitoring/ecowitt"
        ], d["scope"]


def test_path_token_bare_repo_form_rejected():
    """`path:X` (no subdir) is meaningless -- use `repo:X` instead."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)

        r = _add(env, "bare", ["path:dockergom"])
        # Bare path: tokens MUST be rejected so callers don't accidentally
        # build whole-repo claims that would behave inconsistently with
        # `repo:X`. The CLI surfaces the ValueError as a non-zero exit.
        assert r.returncode != 0, (
            f"expected rejection of bare path: token, got rc={r.returncode}\n"
            f"  stdout: {r.stdout}\n"
            f"  stderr: {r.stderr}"
        )


# ---------------------------------------------------------------------------
# 7. Disjoint siblings can be registered concurrently AND complete cleanly.
# ---------------------------------------------------------------------------


def test_disjoint_siblings_run_in_parallel():
    """End-to-end smoke: register two sibling-path items at the same time,
    both should be ready, both should register successfully, both should
    complete via `queue done`.
    """
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)

        r1 = _add(env, "mon", ["path:dockergom/monitoring"])
        assert r1.returncode == 0, r1.stderr
        d1 = json.loads(r1.stdout)
        assert d1["ready_now"] is True

        r2 = _add(env, "ms", ["path:dockergom/queue-minisite"])
        assert r2.returncode == 0, r2.stderr
        d2 = json.loads(r2.stdout)
        assert d2["ready_now"] is True

        rr1 = _register(env, d1["id"], "--json")
        assert rr1.returncode == 0, rr1.stderr
        rr2 = _register(env, d2["id"], "--json")
        assert rr2.returncode == 0, rr2.stderr

        _done(env, d1["id"])
        _done(env, d2["id"])
