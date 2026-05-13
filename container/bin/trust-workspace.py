#!/usr/bin/env python3
"""Idempotently mark the in-container workspace directory as trusted in
~/.claude.json so the in-container Claude Code skips its first-launch
"Quick safety check: Is this a project you created or one you trust?"
prompt.

Trust state in Claude Code is stored at
    ~/.claude.json -> projects[<absolute-path>].hasTrustDialogAccepted

That file is bind-mounted from the host in the example compose stack
(${HOME}/.claude.json -> /home/hndrewaall/.claude.json), so the host
operator's other projects (their per-path entries) MUST be preserved.
This script reads the file, merges-in the workspace path entry without
overwriting any unrelated keys, and writes back atomically (tmp + rename).

Idempotency: if the entry already exists with hasTrustDialogAccepted=true,
the file is left untouched (we still re-read + re-compare to be sure, but
the writeback is skipped). Re-running on every container boot is therefore
a no-op after the first successful run.

Graceful no-op cases (exit 0 with a warning to stderr):
  - ~/.claude.json missing AND HOME is read-only (can't create)
  - ~/.claude.json present but unreadable (perm issue)
  - ~/.claude.json present but not valid JSON (don't trash an existing
    file we can't parse)
  - bind-mount is read-only

The trust prompt is recoverable inside Claude Code — the operator just
hits "1. Yes, I trust this folder" once. So a skip-with-warning is the
right shape; we never want a container boot to fail because of this.

Usage:
    trust-workspace.py [WORKSPACE_PATH]
    trust-workspace.py --test

WORKSPACE_PATH defaults to the value of the WORKSPACE env var, then to
/workspace (the entrypoint's tmux session and the Dockerfile WORKDIR).
"""

from __future__ import annotations

import errno
import json
import os
import shutil
import sys
import tempfile
import unittest
from pathlib import Path


def trust_workspace(workspace: str, config_path: Path) -> str:
    """Merge trust state for ``workspace`` into ``config_path``.

    Returns one of: ``"trusted"`` (wrote a new/updated entry), ``"already"``
    (entry already trusted, no write needed), or ``"skip: <reason>"``
    (recoverable error — caller logs and falls back to the in-Claude
    prompt). Never raises on recoverable errors.
    """
    # Canonicalize without requiring the path to exist — Path.resolve(strict=False)
    # is the right call here. The trust check inside Claude Code uses the cwd
    # at session start, so we want the same shape (an absolute path string).
    workspace = str(Path(workspace).resolve(strict=False))

    if config_path.exists():
        try:
            with config_path.open("r", encoding="utf-8") as f:
                config = json.load(f)
        except (OSError, json.JSONDecodeError) as exc:
            return f"skip: read {config_path}: {exc}"
        if not isinstance(config, dict):
            return f"skip: {config_path}: top-level not a dict"
    else:
        # No file yet: create a minimal one. Claude Code will fill in other
        # top-level keys (numStartups, etc.) when it first runs.
        config = {}

    projects = config.setdefault("projects", {})
    if not isinstance(projects, dict):
        return f"skip: {config_path}: projects not a dict"

    entry = projects.setdefault(workspace, {})
    if not isinstance(entry, dict):
        return f"skip: {config_path}: projects[{workspace}] not a dict"

    if entry.get("hasTrustDialogAccepted") is True:
        return "already"

    entry["hasTrustDialogAccepted"] = True
    # hasCompletedProjectOnboarding suppresses the secondary onboarding tour
    # Claude Code shows on a fresh project entry. Without it the trust
    # prompt is gone but the onboarding tour still pops on first launch.
    entry.setdefault("hasCompletedProjectOnboarding", True)
    # projectOnboardingSeenCount mirrors the counter Claude Code keeps
    # itself; seeding a positive value reinforces the "already onboarded"
    # state across version bumps that re-check this key.
    entry.setdefault("projectOnboardingSeenCount", 1)

    # Write strategy: try the safe atomic-rename path first (tmpfile in
    # the same dir + os.replace), and on EBUSY fall back to in-place
    # truncate+rewrite.
    #
    # Why the fallback exists: ~/.claude.json is bind-mounted as a FILE
    # (not as part of a directory mount) in the example compose stack, and
    # Linux refuses to rename() over an active bind-mount point with EBUSY
    # ("Device or resource busy"). The atomic-rename path works fine when
    # the file lives entirely in the container's overlay FS (a stripped-
    # down `docker run` without the bind-mount) — we keep it as the
    # preferred path so a crash mid-write there leaves no half-written
    # file. The in-place path is non-atomic (a SIGKILL between truncate
    # and final write could leave a partial JSON), but for a single
    # boot-time write of a small file by a single process the window is
    # microseconds — acceptable for the bind-mount case where we have no
    # better option.
    try:
        _write_atomic(config, config_path)
    except OSError as exc:
        if getattr(exc, "errno", None) == errno.EBUSY:
            try:
                _write_in_place(config, config_path)
            except OSError as exc2:
                return f"skip: write {config_path}: {exc2}"
        else:
            return f"skip: write {config_path}: {exc}"

    return "trusted"


def _write_atomic(config: dict, config_path: Path) -> None:
    """Atomic write via tmpfile + os.replace. Raises OSError on failure."""
    tmp_path: Path | None = None
    try:
        with tempfile.NamedTemporaryFile(
            mode="w",
            encoding="utf-8",
            dir=str(config_path.parent),
            prefix=".claude.json.",
            suffix=".tmp",
            delete=False,
        ) as tmp:
            tmp_path = Path(tmp.name)
            json.dump(config, tmp, indent=2)
            tmp.flush()
            os.fsync(tmp.fileno())
        os.replace(tmp_path, config_path)
        tmp_path = None  # ownership transferred
    finally:
        if tmp_path is not None:
            try:
                tmp_path.unlink(missing_ok=True)
            except Exception:
                pass


def _write_in_place(config: dict, config_path: Path) -> None:
    """In-place truncate + rewrite. Non-atomic but works on Linux Docker
    file bind-mounts where os.replace fails with EBUSY. Raises OSError
    on failure."""
    with config_path.open("w", encoding="utf-8") as f:
        json.dump(config, f, indent=2)
        f.flush()
        os.fsync(f.fileno())


def main(argv: list[str]) -> int:
    if len(argv) > 1 and argv[1] == "--test":
        return _run_tests()

    workspace = (
        argv[1]
        if len(argv) > 1 and argv[1]
        else os.environ.get("WORKSPACE") or "/workspace"
    )

    home = Path(os.environ.get("HOME") or os.path.expanduser("~"))
    config_path = home / ".claude.json"

    result = trust_workspace(workspace, config_path)
    if result == "trusted":
        print(
            f"trust-workspace: marked {workspace} as trusted in {config_path}",
            file=sys.stderr,
        )
    elif result.startswith("skip:"):
        print(
            f"trust-workspace: {result[5:].strip()} — "
            "trust prompt will appear on first launch",
            file=sys.stderr,
        )
    # "already" -> silent no-op
    return 0


# --- embedded test suite -----------------------------------------------------


class _TrustWorkspaceTests(unittest.TestCase):
    def setUp(self) -> None:
        self._tmp = tempfile.mkdtemp(prefix="trust-workspace-test.")
        self._home = Path(self._tmp)
        self._config = self._home / ".claude.json"

    def tearDown(self) -> None:
        shutil.rmtree(self._tmp, ignore_errors=True)

    def _read(self) -> dict:
        with self._config.open("r", encoding="utf-8") as f:
            return json.load(f)

    def test_fresh_no_file_creates_minimal_config(self) -> None:
        result = trust_workspace("/workspace", self._config)
        self.assertEqual(result, "trusted")
        cfg = self._read()
        self.assertEqual(
            cfg["projects"]["/workspace"]["hasTrustDialogAccepted"], True
        )
        self.assertEqual(
            cfg["projects"]["/workspace"]["hasCompletedProjectOnboarding"], True
        )
        # No spurious top-level keys.
        self.assertEqual(set(cfg.keys()), {"projects"})

    def test_existing_file_preserves_other_projects_and_top_level(self) -> None:
        seed = {
            "numStartups": 42,
            "theme": "dark",
            "projects": {
                "/home/foo": {
                    "hasTrustDialogAccepted": True,
                    "lastCost": 1.23,
                    "customKey": "keep-me",
                },
                "/workspace": {"hasTrustDialogAccepted": False},
            },
        }
        with self._config.open("w", encoding="utf-8") as f:
            json.dump(seed, f)

        result = trust_workspace("/workspace", self._config)
        self.assertEqual(result, "trusted")
        cfg = self._read()
        self.assertEqual(cfg["numStartups"], 42)
        self.assertEqual(cfg["theme"], "dark")
        self.assertEqual(cfg["projects"]["/home/foo"]["customKey"], "keep-me")
        self.assertEqual(cfg["projects"]["/home/foo"]["lastCost"], 1.23)
        self.assertEqual(
            cfg["projects"]["/workspace"]["hasTrustDialogAccepted"], True
        )

    def test_idempotent_second_run_is_already(self) -> None:
        self.assertEqual(trust_workspace("/workspace", self._config), "trusted")
        before = self._config.stat().st_mtime_ns
        # The "already" path doesn't touch the file at all, so mtime
        # is unchanged.
        self.assertEqual(trust_workspace("/workspace", self._config), "already")
        after = self._config.stat().st_mtime_ns
        self.assertEqual(before, after)

    def test_malformed_json_skips_without_corrupting(self) -> None:
        original = "{this is not valid json"
        self._config.write_text(original, encoding="utf-8")
        result = trust_workspace("/workspace", self._config)
        self.assertTrue(result.startswith("skip:"), result)
        # File untouched on the skip path.
        self.assertEqual(self._config.read_text(encoding="utf-8"), original)

    def test_top_level_not_dict_skips(self) -> None:
        self._config.write_text("[]", encoding="utf-8")
        result = trust_workspace("/workspace", self._config)
        self.assertTrue(result.startswith("skip:"), result)

    def test_projects_not_dict_skips(self) -> None:
        self._config.write_text('{"projects": "oops"}', encoding="utf-8")
        result = trust_workspace("/workspace", self._config)
        self.assertTrue(result.startswith("skip:"), result)

    def test_project_entry_not_dict_skips(self) -> None:
        self._config.write_text(
            '{"projects": {"/workspace": "broken"}}', encoding="utf-8"
        )
        result = trust_workspace("/workspace", self._config)
        self.assertTrue(result.startswith("skip:"), result)

    def test_readonly_dir_skips(self) -> None:
        # Seed a file then make the parent dir read-only so the tmpfile
        # create + rename both fail. Skip rather than raise.
        self._config.write_text('{"projects": {}}', encoding="utf-8")
        os.chmod(self._home, 0o555)
        try:
            result = trust_workspace("/workspace", self._config)
            self.assertTrue(result.startswith("skip:"), result)
        finally:
            os.chmod(self._home, 0o755)

    def test_custom_workspace_path(self) -> None:
        result = trust_workspace("/custom/path", self._config)
        self.assertEqual(result, "trusted")
        cfg = self._read()
        self.assertIn("/custom/path", cfg["projects"])
        self.assertEqual(
            cfg["projects"]["/custom/path"]["hasTrustDialogAccepted"], True
        )

    def test_ebusy_fallback_uses_in_place_write(self) -> None:
        # Simulate the Linux Docker file-bind-mount case: os.replace raises
        # OSError(EBUSY) when renaming over a bind-mounted file. Force that
        # by monkeypatching os.replace for the duration of one call and
        # confirm the in-place fallback writes the change anyway.
        seed = {
            "numStartups": 7,
            "projects": {"/other": {"hasTrustDialogAccepted": True}},
        }
        with self._config.open("w", encoding="utf-8") as f:
            json.dump(seed, f)

        original_replace = os.replace
        replace_calls = {"n": 0}

        def fake_replace(src, dst):
            replace_calls["n"] += 1
            raise OSError(errno.EBUSY, "Device or resource busy")

        os.replace = fake_replace
        try:
            result = trust_workspace("/workspace", self._config)
        finally:
            os.replace = original_replace

        self.assertEqual(result, "trusted")
        self.assertGreaterEqual(replace_calls["n"], 1)
        cfg = self._read()
        # In-place fallback should have written through.
        self.assertEqual(
            cfg["projects"]["/workspace"]["hasTrustDialogAccepted"], True
        )
        # And preserved the other top-level + project entries.
        self.assertEqual(cfg["numStartups"], 7)
        self.assertEqual(
            cfg["projects"]["/other"]["hasTrustDialogAccepted"], True
        )

    def test_non_ebusy_oserror_still_skips(self) -> None:
        # Non-EBUSY OSErrors should NOT trigger the in-place fallback —
        # we want them to surface as a "skip:" so the operator sees
        # what's broken instead of silently degrading to the non-atomic
        # path on every boot.
        self._config.write_text('{"projects": {}}', encoding="utf-8")

        original_replace = os.replace
        in_place_called = {"n": 0}
        original_in_place = _write_in_place

        def fake_replace(src, dst):
            raise OSError(errno.EACCES, "Permission denied")

        # Track whether the in-place fallback runs (it should NOT for EACCES).
        def tracking_in_place(cfg, path):
            in_place_called["n"] += 1
            return original_in_place(cfg, path)

        os.replace = fake_replace
        # Patch the module-level binding the helper resolves.
        import sys

        mod = sys.modules[__name__]
        mod._write_in_place = tracking_in_place  # type: ignore[attr-defined]
        try:
            result = trust_workspace("/workspace", self._config)
        finally:
            os.replace = original_replace
            mod._write_in_place = original_in_place  # type: ignore[attr-defined]

        self.assertTrue(result.startswith("skip:"), result)
        self.assertEqual(in_place_called["n"], 0)


def _run_tests() -> int:
    suite = unittest.TestLoader().loadTestsFromTestCase(_TrustWorkspaceTests)
    runner = unittest.TextTestRunner(verbosity=2)
    result = runner.run(suite)
    return 0 if result.wasSuccessful() else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv))
