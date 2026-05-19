#!/usr/bin/env python3
"""Tests for the --scope comma-splitting normalization in `queue add`.

Bug context (2026-04-27): `--scope repo:foo,resource:bar` used to be
stored as the single opaque token `["repo:foo,resource:bar"]` instead of
`["repo:foo", "resource:bar"]`. Conflict detection is exact-match per
token, so the broken storage silently dropped serialization for almost
every multi-scope add. Fix: split each --scope value on commas, strip
whitespace, dedupe.

Run:
    uv run --python 3.11 --with pytest \\
        pytest ~/repos/config/tests/test_queue_scope_split.py -v

Or directly:
    python3 ~/repos/config/tests/test_queue_scope_split.py
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
    return _run(env, *cmd)


def _register(env, qid, *extra):
    return _run(env, "queue", "register", qid, *extra)


# ---------------------------------------------------------------------------
# 1. Comma-joined single --scope splits into individual tokens
# ---------------------------------------------------------------------------


def test_comma_joined_scope_splits():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)

        r = _add(env, "comma", ["repo:foo,resource:bar"])
        assert r.returncode == 0, r.stderr
        d = json.loads(r.stdout)
        assert d["scope"] == ["repo:foo", "resource:bar"], d["scope"]


# ---------------------------------------------------------------------------
# 2. Repeated --scope flag produces same result as comma-joined
# ---------------------------------------------------------------------------


def test_repeated_scope_flag_matches_comma_joined():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)

        # Comma-joined
        r1 = _add(env, "first", ["repo:alpha,resource:beta"])
        assert r1.returncode == 0, r1.stderr
        d1 = json.loads(r1.stdout)

        # Done with the first so it doesn't conflict on the second add.
        rr = _register(env, d1["id"], "--json")
        assert rr.returncode == 0, rr.stderr
        rd = _run(env, "queue", "done", d1["id"], check=True)

        # Repeated --scope
        r2 = _add(env, "second", ["repo:alpha", "resource:beta"])
        assert r2.returncode == 0, r2.stderr
        d2 = json.loads(r2.stdout)

        assert d1["scope"] == d2["scope"] == ["repo:alpha", "resource:beta"]


# ---------------------------------------------------------------------------
# 3. Whitespace around tokens is stripped
# ---------------------------------------------------------------------------


def test_whitespace_around_tokens_stripped():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)

        r = _add(env, "ws", ["repo:foo, resource:bar"])
        assert r.returncode == 0, r.stderr
        d = json.loads(r.stdout)
        assert d["scope"] == ["repo:foo", "resource:bar"], d["scope"]


# ---------------------------------------------------------------------------
# 3b. Empty fragments (trailing/leading commas) dropped, not crashed on
# ---------------------------------------------------------------------------


def test_empty_fragments_dropped():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)

        r = _add(env, "edge", ["repo:foo,,resource:bar,"])
        assert r.returncode == 0, r.stderr
        d = json.loads(r.stdout)
        assert d["scope"] == ["repo:foo", "resource:bar"], d["scope"]


# ---------------------------------------------------------------------------
# 4. Conflict detection across the comma-split tokens actually fires.
#    This is the real-world failure mode Andrew flagged: an item with
#    --scope foo,bar should serialize behind a running item with --scope foo.
# ---------------------------------------------------------------------------


def test_comma_split_conflict_detection_works():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)

        # First, with a single token, register so it's running.
        r1 = _add(env, "first", ["repo:foo"])
        assert r1.returncode == 0, r1.stderr
        d1 = json.loads(r1.stdout)
        assert d1["ready_now"] is True
        rr = _register(env, d1["id"], "--json")
        assert rr.returncode == 0, rr.stderr

        # Second, with a comma-joined scope that includes repo:foo. The
        # pre-fix bug stored scope as ["repo:foo,resource:bar"] (single
        # token) and the conflict check silently missed it. After the fix
        # the scope is comma-split, so this DOES detect the conflict --
        # which (as of 2026-05-19) soft-serializes rather than hard-fails.
        # Exit 0, item enqueued, ready_now=false, serialized_after points
        # at the running peer.
        r2 = _add(env, "second", ["repo:foo,resource:bar"])
        assert r2.returncode == 0, (
            f"expected exit 0 (soft-serialize), got {r2.returncode}\n"
            f"  stdout: {r2.stdout}\n"
            f"  stderr: {r2.stderr}"
        )
        d2 = json.loads(r2.stdout)
        assert d2["ready_now"] is False, d2
        assert d1["id"] in d2["serialized_after"], d2
        # Both items are in the queue.
        ls = _run(env, "queue", "list", "--json", check=True)
        items = json.loads(ls.stdout)
        assert len(items) == 2, items


# ---------------------------------------------------------------------------
# 5. Single-token --scope still works (no regression for the common case)
# ---------------------------------------------------------------------------


def test_single_token_scope_unchanged():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)

        r = _add(env, "single", ["repo:lonely"])
        assert r.returncode == 0, r.stderr
        d = json.loads(r.stdout)
        assert d["scope"] == ["repo:lonely"], d["scope"]


# ---------------------------------------------------------------------------
# 6. Mixed: some flags comma-joined, some not, dedupe across
# ---------------------------------------------------------------------------


def test_mixed_repeated_and_comma_joined_with_dedupe():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)

        # repo:foo appears twice (once in the comma-joined, once standalone).
        # Expected: deduped, single occurrence, in first-seen order.
        r = _add(env, "mixed", ["repo:foo,resource:bar", "repo:foo",
                                 "agent-proto:baz"])
        assert r.returncode == 0, r.stderr
        d = json.loads(r.stdout)
        assert d["scope"] == ["repo:foo", "resource:bar", "agent-proto:baz"], (
            d["scope"]
        )


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
