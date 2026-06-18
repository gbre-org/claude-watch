#!/usr/bin/env python3
"""CLAUDE.md size guard — fail when an always-in-context CLAUDE.md is too big.

Why this exists: every CLAUDE.md (managed-policy, user, project) is loaded
into Claude Code's context at session start and stays there for the whole
session. Claude Code's `/doctor` recommends each such file stay under ~40,000
*characters* (chars/bytes, NOT tokens) so it doesn't crowd out the working
context. This script makes oversize visible at commit time (via the
pre-commit hook) and in CI — CI being the real enforcement, since local hooks
are bypassable with `git commit --no-verify`.

The same script is the single source of truth for the limit, called by BOTH:
  * scripts/git-hooks/pre-commit  (local gate)
  * the `container-shell-tests` CI job via `make test-claude-md-size`

Budget model
------------
  * HARD_LIMIT (40000): the generic ceiling for any tracked CLAUDE.md. Over
    this => FAIL.
  * WARN_LIMIT (32000): an aspirational soft band (80% of hard). Over this but
    under the file's effective ceiling => print a WARN line, but do NOT fail.
  * ALLOWLIST: per-file *ratchet ceilings* for files that are intentionally
    over the generic HARD_LIMIT today. The ceiling is pinned at (or just
    above) the file's CURRENT size so the file CANNOT GROW — any byte added
    fails the gate. The ratchet only ever moves DOWN: when a file is trimmed,
    lower its ceiling here so it can never grow back. This is the lever that
    drives an over-budget file back under HARD_LIMIT over time.

container/baked-CLAUDE.md is the one allowlisted file: it is baked into the
container image at /etc/claude-code/CLAUDE.md and is ~76k today (~1.9x the
40k limit). Trimming it is a separate follow-up; this guard pins it so it
stops growing in the meantime.

Exit code 0 = all files within their effective ceiling; 1 = at least one
file over its ceiling (or the script self-test failed).

Usage:
  scripts/check-claude-md-size.py            # check all tracked CLAUDE.md
  scripts/check-claude-md-size.py FILE...    # check specific file(s)
  scripts/check-claude-md-size.py --self-test  # run embedded unit tests
"""

from __future__ import annotations

import subprocess
import sys
from pathlib import Path

# --- Budget constants (single source of truth) -------------------------------

# Generic per-file hard ceiling, in CHARACTERS (bytes). Over => FAIL.
# Matches Claude Code /doctor's ~40,000-char always-in-context recommendation.
HARD_LIMIT = 40_000

# Aspirational soft band (80% of hard). Over this but within the effective
# ceiling => WARN (non-fatal), to flag files creeping toward the limit.
WARN_LIMIT = 32_000

# Per-file RATCHET ceilings for files intentionally over HARD_LIMIT today.
# The value is a "do-not-regress" cap pinned at the file's current size: the
# file may not grow past it. Lower these as files are trimmed — NEVER raise.
#
# container/baked-CLAUDE.md: current size ~74,869 chars (2026-06-18, after
# removing the Signal-messenger docs + adding the MCP auto-approve / triage
# notes). Pinned just above current so it cannot grow. TODO: trim below
# HARD_LIMIT (40k) and delete this entry once it fits the generic budget.
ALLOWLIST: dict[str, int] = {
    "container/baked-CLAUDE.md": 75_000,
}


def repo_root() -> Path:
    out = subprocess.run(
        ["git", "rev-parse", "--show-toplevel"],
        capture_output=True, text=True, check=True,
    )
    return Path(out.stdout.strip())


def tracked_claude_md(root: Path) -> list[str]:
    """Return repo-relative paths of all tracked CLAUDE.md-family files.

    Covers both the canonical name `CLAUDE.md` (top-level + nested) and the
    baked managed-policy variant `container/baked-CLAUDE.md`. The glob
    `*CLAUDE.md` matches any basename ending in `CLAUDE.md`, which is exactly
    the set of always-in-context managed-policy / memory files we want to
    bound (CLAUDE.md, baked-CLAUDE.md). We keep that ending-match rather than
    an exact `== CLAUDE.md` so the baked source file is included.
    """
    out = subprocess.run(
        ["git", "ls-files", "*CLAUDE.md"],
        cwd=root, capture_output=True, text=True, check=True,
    )
    paths = {line for line in out.stdout.splitlines() if line.strip()}
    return sorted(p for p in paths if Path(p).name.endswith("CLAUDE.md"))


def effective_ceiling(rel_path: str) -> int:
    """The size ceiling that applies to a given repo-relative path."""
    return ALLOWLIST.get(rel_path, HARD_LIMIT)


def check_paths(root: Path, rel_paths: list[str]) -> int:
    """Check each path against its effective ceiling. Return # of failures."""
    failures = 0
    for rel in rel_paths:
        f = root / rel
        if not f.is_file():
            print(f"  SKIP {rel} (not a regular file)")
            continue
        size = len(f.read_bytes())
        ceiling = effective_ceiling(rel)
        allowlisted = rel in ALLOWLIST
        tag = " [allowlisted ratchet]" if allowlisted else ""
        if size > ceiling:
            failures += 1
            print(
                f"  FAIL {rel}: {size} chars > ceiling {ceiling}{tag}"
            )
            if allowlisted:
                print(
                    f"       this file is pinned by the ratchet allowlist; "
                    f"it must not GROW. Trim it, do not raise the ceiling."
                )
            else:
                print(
                    f"       CLAUDE.md files must stay <= {HARD_LIMIT} chars "
                    f"(Claude Code /doctor always-in-context budget)."
                )
        elif size > WARN_LIMIT:
            print(
                f"  WARN {rel}: {size} chars (warn band > {WARN_LIMIT}, "
                f"ceiling {ceiling}){tag}"
            )
        else:
            print(f"  ok   {rel}: {size} chars (<= {WARN_LIMIT})")
    return failures


def run_check(rel_paths: list[str] | None = None) -> int:
    root = repo_root()
    paths = rel_paths if rel_paths else tracked_claude_md(root)
    if not paths:
        print("No tracked CLAUDE.md files found.")
        return 0
    print(
        f"== CLAUDE.md size guard (HARD_LIMIT={HARD_LIMIT}, "
        f"WARN_LIMIT={WARN_LIMIT}) =="
    )
    failures = check_paths(root, paths)
    print()
    if failures:
        print(f"FAILED: {failures} CLAUDE.md file(s) over budget.")
        return 1
    print("PASSED: all CLAUDE.md files within budget.")
    return 0


# --- Embedded self-test ------------------------------------------------------

def _self_test() -> int:
    import tempfile

    failures: list[str] = []

    def check(cond: bool, msg: str) -> None:
        if cond:
            print(f"  ok: {msg}")
        else:
            print(f"  FAIL: {msg}")
            failures.append(msg)

    print("== check-claude-md-size self-test ==")

    # Constants sanity.
    check(WARN_LIMIT < HARD_LIMIT, "WARN_LIMIT < HARD_LIMIT")
    check(HARD_LIMIT == 40_000, "HARD_LIMIT is 40000 (doctor budget)")
    check(
        "container/baked-CLAUDE.md" in ALLOWLIST,
        "baked-CLAUDE.md is in the ratchet allowlist",
    )
    check(
        ALLOWLIST["container/baked-CLAUDE.md"] > HARD_LIMIT,
        "baked-CLAUDE.md ceiling is above the generic HARD_LIMIT (it's "
        "intentionally large today)",
    )

    # effective_ceiling routing.
    check(
        effective_ceiling("CLAUDE.md") == HARD_LIMIT,
        "generic file gets HARD_LIMIT",
    )
    check(
        effective_ceiling("container/baked-CLAUDE.md")
        == ALLOWLIST["container/baked-CLAUDE.md"],
        "allowlisted file gets its ratchet ceiling",
    )

    # Functional: build a throwaway repo with fixture files and assert
    # PASS / FAIL behaviour against the real check_paths logic.
    with tempfile.TemporaryDirectory() as td:
        root = Path(td)

        # A normal CLAUDE.md just under the hard limit -> ok.
        small = root / "CLAUDE.md"
        small.write_bytes(b"x" * (HARD_LIMIT - 10))
        check(
            check_paths(root, ["CLAUDE.md"]) == 0,
            "normal CLAUDE.md just under HARD_LIMIT passes",
        )

        # Same file pushed over the hard limit -> 1 failure.
        small.write_bytes(b"x" * (HARD_LIMIT + 10))
        check(
            check_paths(root, ["CLAUDE.md"]) == 1,
            "normal CLAUDE.md over HARD_LIMIT fails",
        )

        # Allowlisted baked file under its ratchet ceiling -> ok.
        baked = root / "container" / "baked-CLAUDE.md"
        baked.parent.mkdir(parents=True)
        ceiling = ALLOWLIST["container/baked-CLAUDE.md"]
        baked.write_bytes(b"x" * (ceiling - 100))
        check(
            check_paths(root, ["container/baked-CLAUDE.md"]) == 0,
            "baked file under ratchet ceiling passes (even though > 40k)",
        )

        # Allowlisted baked file pushed over its ratchet ceiling -> fail.
        baked.write_bytes(b"x" * (ceiling + 100))
        check(
            check_paths(root, ["container/baked-CLAUDE.md"]) == 1,
            "baked file over ratchet ceiling fails (ratchet blocks growth)",
        )

    print()
    if failures:
        print(f"SELF-TEST FAILED: {len(failures)} check(s)")
        return 1
    print("SELF-TEST PASSED")
    return 0


def main(argv: list[str]) -> int:
    args = argv[1:]
    if "--self-test" in args:
        return _self_test()
    return run_check(args or None)


if __name__ == "__main__":
    sys.exit(main(sys.argv))
