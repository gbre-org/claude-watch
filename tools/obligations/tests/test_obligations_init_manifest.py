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

import importlib.machinery
import importlib.util
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


# ---------------------------------------------------------------------------
# ADD-FIRST invariant (botchat #1697 root cause: the gate was left ABSENT).
#
# The prior order was satisfy-old-THEN-add. If the `add` transiently failed
# AFTER the satisfy removed the prior row, the row was left ABSENT -- the
# `tool_pattern:*` botchat mark-read gate then enforced NOTHING, so unread
# inbound messages never blocked the main loop (msgs #1693/#1695 missed). The
# fix adds the fresh row FIRST (with retry) and only removes OLD duplicates
# once the new row is confirmed present, so there is never a zero-row window.
# These tests drive `_apply_one_manifest` directly with a stubbed CLI so we
# can force `add` failures deterministically.
# ---------------------------------------------------------------------------

def _load_init_module():
    """Import the obligations-init script as a module (it has no .py suffix)."""
    import importlib.util
    spec = importlib.util.spec_from_loader(
        "obligations_init_mod",
        importlib.machinery.SourceFileLoader(
            "obligations_init_mod", str(OBLIGATIONS_INIT)),
    )
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


def _botchat_like_spec():
    """A tool_pattern:* evaluator gate shaped like the botchat mark-read gate."""
    return {
        "tool_pattern": "*",
        "predicate": "all_of",
        "params": {"predicates": [
            {"kind": "is_main_loop", "params": {}},
            {"kind": "evaluator", "params": {
                "cmd": "/bin/true", "decision_mode": "exit_code",
                "allow_on_zero_exit": True, "timeout_ms": 4000}},
        ]},
        "ttl": 0,
        "enforcement": "gate",
        "deny_msg": "Unread inbound botchat message(s).",
        "created_by": "botchat:mark-read-gate",
        "exempt_tool_patterns": ["Read", "Bash:botchat-send"],
    }


def test_add_first_never_leaves_gate_absent_on_transient_add_failure(
        tmp_path, monkeypatch):
    """If `add` fails transiently while a PRIOR row from this manifest is
    present, the prior row MUST stay (gate stays armed) -- never satisfied out
    from under a failed add. This is the core botchat #1697 invariant."""
    mod = _load_init_module()
    spec = _botchat_like_spec()
    manifest_dir = tmp_path / "cfg"
    manifest_dir.mkdir()
    path = _write_manifest(manifest_dir, "botchat.json", spec)

    # A prior incarnation of the same logical row (same signature).
    prior = {
        "id": "ob-prior-0001",
        "tool_pattern": "*",
        "predicate": {"kind": "all_of", "params": {}},
        "created_by": "botchat:mark-read-gate [user-manifest]",
    }

    calls = {"satisfy": [], "add": 0}

    def fake_run(argv, *a, **kw):
        class R:
            returncode = 0
            stdout = "{}"
            stderr = ""
        # argv[1] is the subcommand.
        sub = argv[1] if len(argv) > 1 else ""
        if sub == "add":
            calls["add"] += 1
            r = R(); r.returncode = 1; r.stderr = "simulated transient add failure"
            return r
        if sub == "satisfy":
            calls["satisfy"].append(argv[2] if len(argv) > 2 else "")
            return R()
        if sub == "list":
            # Reflect current state: prior row still present (never satisfied).
            r = R(); r.stdout = json.dumps({"obligations": [prior]}); return r
        return R()

    monkeypatch.setattr(mod.subprocess, "run", fake_run)
    rc = mod._apply_one_manifest(
        "obligations", str(path), existing=[prior],
        dry_run=False, verbose=False)

    assert rc == 1, "a failed add must surface a non-zero rc"
    assert calls["add"] >= 2, "add must be retried before giving up"
    assert calls["satisfy"] == [], (
        "on add failure the PRIOR row must NOT be satisfied -- the gate must "
        f"stay armed; got satisfy calls {calls['satisfy']}"
    )


def test_add_first_then_removes_old_duplicate_on_success(tmp_path, monkeypatch):
    """On a successful add, the OLD duplicate is removed (replace semantics),
    but only AFTER the new row exists, and the new id is never satisfied."""
    mod = _load_init_module()
    spec = _botchat_like_spec()
    manifest_dir = tmp_path / "cfg"
    manifest_dir.mkdir()
    path = _write_manifest(manifest_dir, "botchat.json", spec)

    prior = {
        "id": "ob-prior-0001",
        "tool_pattern": "*",
        "predicate": {"kind": "all_of", "params": {}},
        "created_by": "botchat:mark-read-gate [user-manifest]",
    }
    new_row = {
        "id": "ob-new-0002",
        "tool_pattern": "*",
        "predicate": {"kind": "all_of", "params": {}},
        "created_by": "botchat:mark-read-gate [user-manifest]",
    }

    calls = {"satisfy": [], "add": 0}

    def fake_run(argv, *a, **kw):
        class R:
            returncode = 0
            stdout = "{}"
            stderr = ""
        sub = argv[1] if len(argv) > 1 else ""
        if sub == "add":
            calls["add"] += 1
            r = R(); r.stdout = json.dumps(new_row); return r
        if sub == "satisfy":
            calls["satisfy"].append(argv[2] if len(argv) > 2 else "")
            return R()
        if sub == "list":
            # After add, both rows present (verify sees the signature live).
            r = R(); r.stdout = json.dumps({"obligations": [prior, new_row]})
            return r
        return R()

    monkeypatch.setattr(mod.subprocess, "run", fake_run)
    rc = mod._apply_one_manifest(
        "obligations", str(path), existing=[prior],
        dry_run=False, verbose=False)

    assert rc == 0, "successful add + verify must return 0"
    assert calls["add"] == 1
    assert calls["satisfy"] == ["ob-prior-0001"], (
        "exactly the OLD duplicate is satisfied (never the new id); "
        f"got {calls['satisfy']}"
    )


def test_verify_fails_loud_when_row_not_live_post_apply(tmp_path, monkeypatch):
    """If, post-add, NO active row matches the signature (e.g. a concurrent
    run satisfied it), _apply_one_manifest returns 1 -- fail loud, never a
    silent un-armed gate."""
    mod = _load_init_module()
    spec = _botchat_like_spec()
    manifest_dir = tmp_path / "cfg"
    manifest_dir.mkdir()
    path = _write_manifest(manifest_dir, "botchat.json", spec)

    def fake_run(argv, *a, **kw):
        class R:
            returncode = 0
            stdout = "{}"
            stderr = ""
        sub = argv[1] if len(argv) > 1 else ""
        if sub == "add":
            r = R(); r.stdout = json.dumps({"id": "ob-new-0002"}); return r
        if sub == "list":
            # Verify re-list shows NOTHING matching (row vanished).
            r = R(); r.stdout = json.dumps({"obligations": []}); return r
        return R()

    monkeypatch.setattr(mod.subprocess, "run", fake_run)
    rc = mod._apply_one_manifest(
        "obligations", str(path), existing=[],
        dry_run=False, verbose=False)
    assert rc == 1, "an un-armed gate post-apply must return a non-zero rc"
