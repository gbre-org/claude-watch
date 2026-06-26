#!/usr/bin/env python3
"""Tests for obligations-init's user-manifest application idempotency.

obligations-init applies operator obligation manifests from a bind-mounted
config dir ($CLAUDE_OBLIGATIONS_MANIFEST_DIR) on EVERY container start. The
named default-seed rows are idempotent (skip when already present); the
user-manifest rows must be idempotent the SAME way -- re-applying an
UNCHANGED manifest is a no-op (replace-in-place), and applying a CHANGED
manifest replaces rather than duplicates.

REGRESSION: the row's stored ``predicate`` field is a DICT
(``{"kind": ..., "params": ...}``, stamped by ``obligations add``) while a
manifest spec's ``predicate`` is a bare STRING. The idempotency signature
compared the two raw shapes, which never matched, so the satisfy-before-add
replace branch never fired -- every run / restart added a fresh duplicate of
each user-manifest row. Fixed by normalizing the predicate to its ``kind``
string in the signature (``_predicate_kind`` / ``_manifest_row_signature``).

Self-contained: runs the real ``obligations-init`` + ``obligations`` CLIs
against a tempdir HOME so the live ~/.config/claude/obligations.json is never
touched.

Run::

    uv run --python 3.11 --with pytest \\
        pytest tools/obligations/tests/test_obligations_init_manifest.py -v
"""

import json
import os
import subprocess
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
OBLIGATIONS_DIR = HERE.parent
OBLIGATIONS_INIT = OBLIGATIONS_DIR / "obligations-init"
OBLIGATIONS = OBLIGATIONS_DIR / "obligations"


def _env(tmp_path, manifest_dir):
    """Env that isolates state to tmp HOME and points the manifest dir."""
    env = os.environ.copy()
    env["HOME"] = str(tmp_path)
    env["CLAUDE_OBLIGATIONS_MANIFEST_DIR"] = str(manifest_dir)
    # The init resolves the `obligations` CLI via shutil.which first; make
    # sure the in-tree CLI is found regardless of PATH by prepending its dir.
    env["PATH"] = str(OBLIGATIONS_DIR) + os.pathsep + env.get("PATH", "")
    return env


def _run_init(env):
    r = subprocess.run(
        [sys.executable, str(OBLIGATIONS_INIT), "--verbose"],
        capture_output=True, text=True, env=env, timeout=30,
    )
    assert r.returncode == 0, (
        f"obligations-init failed rc={r.returncode}\n"
        f"STDOUT:\n{r.stdout}\nSTDERR:\n{r.stderr}"
    )
    return r


def _list_rows(env):
    r = subprocess.run(
        [sys.executable, str(OBLIGATIONS), "list", "--json"],
        capture_output=True, text=True, env=env, timeout=15,
    )
    assert r.returncode == 0, (
        f"obligations list failed rc={r.returncode}\n{r.stderr}"
    )
    return json.loads(r.stdout).get("obligations", [])


def _manifest_rows(rows):
    """The rows applied from a user manifest (created_by carries the marker)."""
    return [
        ob for ob in rows
        if "[user-manifest]" in (ob.get("created_by") or "")
    ]


def _write_manifest(manifest_dir, name, spec):
    p = Path(manifest_dir) / name
    p.write_text(json.dumps(spec), encoding="utf-8")
    return p


def _presence_gate_spec(max_age=90):
    return {
        "tool_pattern": "AskUserQuestion",
        "predicate": "file_mtime_within",
        "params": {"path": "~/.claude/operator-present",
                   "max_age_secs": max_age},
        "ttl": 0,
        "enforcement": "gate",
        "deny_msg": "Operator presence unknown/stale -- AskUserQuestion blocked.",
        "created_by": "claude-config:presence-gate",
        "mandatory": False,
    }


def test_reapplying_unchanged_manifest_yields_one_row(tmp_path):
    """Applying the same manifest twice (the restart scenario) must leave
    exactly ONE row -- the regression added a duplicate on the second run."""
    manifest_dir = tmp_path / "obligations-config"
    manifest_dir.mkdir()
    _write_manifest(manifest_dir, "presence-gate.json", _presence_gate_spec())

    env = _env(tmp_path, manifest_dir)

    _run_init(env)
    rows1 = _manifest_rows(_list_rows(env))
    assert len(rows1) == 1, f"expected 1 manifest row after first run, got {rows1}"

    # Second run = a container restart re-running the entrypoint.
    _run_init(env)
    rows2 = _manifest_rows(_list_rows(env))
    assert len(rows2) == 1, (
        "re-applying an UNCHANGED manifest must NOT duplicate the row "
        f"(restart scenario); got {len(rows2)} rows: {rows2}"
    )

    # A third run for good measure -- duplicates compounded across runs.
    _run_init(env)
    rows3 = _manifest_rows(_list_rows(env))
    assert len(rows3) == 1, (
        f"manifest rows must stay at 1 across repeated runs; got {rows3}"
    )

    row = rows3[0]
    assert row["tool_pattern"] == "AskUserQuestion"
    assert row["predicate"]["kind"] == "file_mtime_within"
    assert "[user-manifest]" in row["created_by"]


def test_changed_manifest_replaces_not_duplicates(tmp_path):
    """A CHANGED manifest (same created_by+tool_pattern+predicate, different
    params/deny_msg) must REPLACE the existing row, not add a duplicate."""
    manifest_dir = tmp_path / "obligations-config"
    manifest_dir.mkdir()
    _write_manifest(manifest_dir, "presence-gate.json",
                    _presence_gate_spec(max_age=90))

    env = _env(tmp_path, manifest_dir)
    _run_init(env)
    rows1 = _manifest_rows(_list_rows(env))
    assert len(rows1) == 1

    # Re-tune the manifest (new max_age + reworded deny_msg) and re-apply.
    changed = _presence_gate_spec(max_age=120)
    changed["deny_msg"] = "Reworded: presence stale, blocked."
    _write_manifest(manifest_dir, "presence-gate.json", changed)
    _run_init(env)

    rows2 = _manifest_rows(_list_rows(env))
    assert len(rows2) == 1, (
        "a re-tuned manifest must REPLACE, not duplicate; "
        f"got {len(rows2)} rows: {rows2}"
    )
    row = rows2[0]
    assert row["predicate"]["params"]["max_age_secs"] == 120, (
        "replacement must carry the NEW params"
    )
    assert row["deny_message"] == "Reworded: presence stale, blocked."


def test_multiple_distinct_manifests_each_stay_single(tmp_path):
    """Two DIFFERENT manifests yield two rows, each idempotent across runs
    (a duplicate-on-one wouldn't be caught by the single-manifest tests)."""
    manifest_dir = tmp_path / "obligations-config"
    manifest_dir.mkdir()
    _write_manifest(manifest_dir, "presence-gate.json", _presence_gate_spec())
    _write_manifest(manifest_dir, "other-gate.json", {
        "tool_pattern": "Bash",
        "predicate": "file_exists",
        "params": {"path": "~/.cache/some-marker"},
        "ttl": 0,
        "enforcement": "gate",
        "deny_msg": "marker absent",
        "created_by": "claude-config:other-gate",
    })

    env = _env(tmp_path, manifest_dir)
    _run_init(env)
    _run_init(env)  # restart
    rows = _manifest_rows(_list_rows(env))
    assert len(rows) == 2, (
        f"two distinct manifests must yield exactly two rows; got {rows}"
    )
    created_bys = sorted(ob["created_by"] for ob in rows)
    assert created_bys == [
        "claude-config:other-gate [user-manifest]",
        "claude-config:presence-gate [user-manifest]",
    ], created_bys
