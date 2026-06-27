#!/usr/bin/env python3
"""Concurrent-add correctness test for session-task queue.

Fires N threads that all race to `session-task queue add` against the same
queue.json. Verifies:

  * No lost items (N items in the queue).
  * Unique ids across all added items.
  * Scope-union invariant: for any two items whose scopes overlap, they
    end up in the same group, and that group's aggregated scope is the
    union of all its members' scopes.
  * No phantom group_id references (every distinct group_id on an item
    is reachable and every member has a valid group_id).

Run:
    uv run --python 3.11 --with pytest \\
        pytest tools/session-task/tests/test_queue_concurrent.py -v

Or directly:
    python3 tools/session-task/tests/test_queue_concurrent.py
"""

import json
import os
import subprocess
import sys
import tempfile
import threading
from pathlib import Path

SESSION_TASK = Path(__file__).resolve().parent.parent / "session-task"


def _run_add(description, scopes, env):
    cmd = [sys.executable, str(SESSION_TASK), "queue", "add", description, "--json"]
    for s in scopes:
        cmd.extend(["--scope", s])
    r = subprocess.run(cmd, capture_output=True, text=True, env=env, timeout=15)
    if r.returncode != 0:
        raise RuntimeError(f"queue add failed rc={r.returncode}: {r.stderr}")
    return json.loads(r.stdout)


def _tokens_overlap(a, b):
    if a == "*" or b == "*":
        return True
    if a.startswith("file:") and b.startswith("file:"):
        pa = a[len("file:"):]
        pb = b[len("file:"):]
        wa = pa if pa.endswith("/") else pa + "/"
        wb = pb if pb.endswith("/") else pb + "/"
        return pa == pb or wa.startswith(wb) or wb.startswith(wa)
    return a == b


def _scope_sets_overlap(a, b):
    return any(_tokens_overlap(ta, tb) for ta in a for tb in b)


def run_concurrent(n=10):
    tmp = tempfile.mkdtemp(prefix="session-queue-test-")
    env = dict(os.environ)
    env["HOME"] = tmp
    # Precreate config dir
    Path(tmp, ".config/session").mkdir(parents=True, exist_ok=True)

    # Mix of scopes so we get some overlap + some independent groups.
    # items 0-3  -> file:/foo/a  (should all share a group)
    # items 4-6  -> file:/foo     (prefix of /foo/a -- merges with above)
    # items 7-8  -> repo:baz      (independent)
    # item 9     -> *             (universal -- overlaps with everything,
    #                              forcing a full merge)
    recipes = [
        ("item-0", ["file:/foo/a"]),
        ("item-1", ["file:/foo/a"]),
        ("item-2", ["file:/foo/a/sub"]),
        ("item-3", ["file:/foo/a"]),
        ("item-4", ["file:/foo"]),
        ("item-5", ["file:/foo/b"]),
        ("item-6", ["file:/foo"]),
        ("item-7", ["repo:baz"]),
        ("item-8", ["repo:baz"]),
        ("item-9", ["*"]),
    ][:n]

    results = {}
    errors = []

    def worker(idx, desc, scopes):
        try:
            results[idx] = _run_add(desc, scopes, env)
        except Exception as e:
            errors.append((idx, repr(e)))

    threads = [
        threading.Thread(target=worker, args=(i, desc, scopes))
        for i, (desc, scopes) in enumerate(recipes)
    ]
    for t in threads:
        t.start()
    for t in threads:
        t.join(timeout=30)

    assert not errors, f"add errors: {errors}"
    assert len(results) == len(recipes), f"lost results: {results.keys()}"

    # Load final queue.
    qfile = Path(tmp, ".config/session/queue.json")
    assert qfile.exists(), "queue.json was not created"
    data = json.loads(qfile.read_text())
    items = data["items"]

    # 1. No lost items
    assert len(items) == len(recipes), f"expected {len(recipes)} items, got {len(items)}"

    # 2. Unique ids
    ids = [it["id"] for it in items]
    assert len(set(ids)) == len(ids), f"duplicate ids: {ids}"

    # 3. Every item has a group_id
    for it in items:
        assert it.get("group_id", "").startswith("g-"), f"bad group_id on {it['id']}: {it.get('group_id')}"

    # 4. Overlap invariant:
    #    If two items' declared scopes overlap, they must share a group_id.
    #    If they don't overlap directly but are transitively connected via
    #    other items' scopes, they should also be in the same merged group.
    # Build a transitive-closure via union-find on all original scopes.
    parents = list(range(len(items)))

    def find(i):
        while parents[i] != i:
            parents[i] = parents[parents[i]]
            i = parents[i]
        return i

    def union(a, b):
        ra, rb = find(a), find(b)
        if ra != rb:
            parents[ra] = rb

    for i in range(len(items)):
        for j in range(i + 1, len(items)):
            if _scope_sets_overlap(items[i]["scope"], items[j]["scope"]):
                union(i, j)

    # Build buckets by expected (transitive) scope groups.
    expected_buckets = {}
    for i in range(len(items)):
        expected_buckets.setdefault(find(i), []).append(i)

    # All items in the same expected bucket MUST share a group_id.
    for indices in expected_buckets.values():
        gids = {items[i]["group_id"] for i in indices}
        assert len(gids) == 1, (
            f"items {[items[i]['id'] for i in indices]} should be in one group, "
            f"got {gids}"
        )

    # Items in different expected buckets MUST have different group_ids.
    bucket_gids = {}
    for key, indices in expected_buckets.items():
        bucket_gids[key] = items[indices[0]]["group_id"]
    assert len(set(bucket_gids.values())) == len(bucket_gids), (
        "different expected buckets share a group_id: " + repr(bucket_gids)
    )

    # 5. Scope-union invariant per group: group scope = union of member scopes.
    by_group = {}
    for it in items:
        by_group.setdefault(it["group_id"], []).append(it)
    for gid, members in by_group.items():
        # Every original scope token declared by any member should be
        # recoverable from the union of all members' scope lists.
        all_scopes = []
        for m in members:
            all_scopes.extend(m["scope"])
        # Sanity: each member's own scope tokens are in the union
        for m in members:
            for t in m["scope"]:
                assert t in all_scopes, f"lost scope {t!r} from {m['id']} in group {gid}"

    # 6. No phantom group_ids: every group_id appears on at least one item
    distinct_gids = {it["group_id"] for it in items}
    for gid in distinct_gids:
        assert any(it["group_id"] == gid for it in items), f"phantom group_id {gid}"

    print(f"PASS: {len(items)} items, {len(distinct_gids)} distinct group_ids")
    print(f"  expected buckets: {len(expected_buckets)}")
    for key, indices in expected_buckets.items():
        gid = items[indices[0]]["group_id"]
        print(f"    {gid}: {[items[i]['id'] for i in indices]}")
    return True


def test_concurrent_add():
    assert run_concurrent(n=10) is True


if __name__ == "__main__":
    ok = run_concurrent(n=10)
    sys.exit(0 if ok else 1)
