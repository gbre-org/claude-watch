#!/usr/bin/env python3
"""Dep-blocked pending items must NOT count as stale-ready.

Deps are stored as ``task:<id>`` scope tokens (legacy ``depends_on`` is
migrated into them on read), so the predicate has to honour both forms.

Run::

    uv run --python 3.11 --with pytest \\
        pytest tools/obligations/tests/test_stale_ready_dep_blocked.py -v
"""
import importlib.machinery
import importlib.util
import json
from pathlib import Path

OBLIGATIONS = Path(__file__).resolve().parent.parent / "obligations"
_loader = importlib.machinery.SourceFileLoader("obligations_mod", str(OBLIGATIONS))
_spec = importlib.util.spec_from_loader("obligations_mod", _loader)
ob = importlib.util.module_from_spec(_spec)
_loader.exec_module(ob)


def _item(i, status="pending", **kw):
    d = {"id": i, "status": status, "created_at": "2026-01-01T00:00:00+00:00"}
    d.update(kw)
    return d


def test_task_scope_token_dep_blocks():
    items = [_item("q-a", "running"), _item("q-b", scope=["repo:x", "task:q-a"])]
    assert not ob._queue_item_is_ready_now(items, items[1])


def test_task_scope_token_dep_done_is_ready():
    items = [_item("q-a", "done"), _item("q-b", scope=["task:q-a"])]
    assert ob._queue_item_is_ready_now(items, items[1])


def test_legacy_depends_on_still_blocks():
    items = [_item("q-a", "pending"), _item("q-b", depends_on=["q-a"])]
    assert not ob._queue_item_is_ready_now(items, items[1])


def test_hostjob_created_rows_not_stale_ready(tmp_path):
    q = tmp_path / "queue.json"
    items = [
        _item("q-hj", scope=["hostjob:x"], created_by="hostjob"),
        _item("q-human", scope=["repo:y"], created_by="main-loop"),
    ]
    q.write_text(json.dumps({"items": items}))
    got = ob._stale_ready_items(str(q), 60, ob._now())
    assert [i["id"] for i in got] == ["q-human"]


def test_dep_blocked_not_stale_ready(tmp_path):
    q = tmp_path / "queue.json"
    items = [_item("q-a", "running"), _item("q-b", scope=["task:q-a"])]
    q.write_text(json.dumps({"items": items}))
    assert ob._stale_ready_items(str(q), 60, ob._now()) == []
