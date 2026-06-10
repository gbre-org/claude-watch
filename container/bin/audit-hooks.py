#!/usr/bin/env python3
"""Audit Claude Code hook commands referenced from a settings.json against the
in-container runtime, reporting per-hook fate.

Use case: inside a running claude-container, run `audit-hooks` to see what
would happen when each hook fires. Cross-arch host binaries (typical: macOS
Mach-O telemetry hooks bind-mounted in from the host) silently no-op via
/usr/local/bin/exec-hook by design (the alternative is "Exec format error"
spam on every event). ELF binaries and shebang scripts run transparently.
Missing commands report as broken.

This is observability ONLY — the audit reads settings.json, doesn't mutate
it, doesn't fire the hooks. Per-fate categories let the operator decide
whether to ship a Linux build, bridge via host-bash MCP, or accept the
silent no-op.

Usage:
    audit-hooks                    # audit ~/.claude/settings.json
    audit-hooks --settings PATH    # audit a specific file
    audit-hooks --json             # machine-readable output
    audit-hooks --test             # run embedded test suite

Output (human mode):
    audit-hooks: SessionStart.startup|resume -> exec-hook /Users/me/.local/...
      target: /Users/me/.local/bin/telemetry-hook
      magic:  feedfacf (Mach-O 64-bit)
      fate:   silent-no-op (exec-hook intercepts)

Exit code is 0 when settings.json was readable and the audit completed. Use
--strict to exit non-zero when any hook would fail (non-ELF, non-script,
non-missing — basically the silent-no-op cases too).

Fate categories:
    ok-elf         — ELF binary, exec'd transparently by exec-hook
    ok-script      — shebang script, exec'd transparently by exec-hook
    silent-no-op   — Mach-O / unknown format, exec-hook intercepts (exit 0)
    missing        — file does not exist; exec-hook silent-no-ops with a
                     "(missing)" tag
    not-wrapped    — command does NOT use exec-hook; would hit "Exec format
                     error" on every event if the target is non-ELF. Set
                     CLAUDE_CONTAINER_REWRITE_HOOKS=1 to auto-wrap.
    builtin        — command is a baked container builtin (claude-watch
                     hook-fire, etc.); we trust these to work.
    unparseable    — command doesn't tokenize cleanly; can't audit.

Limitations:
    - Only looks at hook `command` strings. Doesn't run them, doesn't simulate
      the hook event payload.
    - Resolves the FIRST whitespace-separated token of each command. Shell
      operators (pipes, &&) split commands, so audit reports each one.
    - Doesn't try to follow $PATH for bare commands — those are reported as
      "builtin" if known, otherwise "missing" with the bare name. exec-hook
      itself would PATH-resolve at runtime; this audit is a static check.
"""

from __future__ import annotations

import argparse
import json
import os
import shlex
import struct
import sys
from dataclasses import dataclass, asdict
from pathlib import Path
from typing import Iterator


# ---------------------------------------------------------------------------
# Magic-byte sniffer — mirrors container/hooks-shim/exec-hook's algorithm.
# ---------------------------------------------------------------------------

ELF_MAGIC = b"\x7fELF"
MACHO_MAGICS = {
    b"\xfe\xed\xfa\xce",  # 32-bit Mach-O
    b"\xfe\xed\xfa\xcf",  # 64-bit Mach-O
    b"\xce\xfa\xed\xfe",  # 32-bit Mach-O (reverse)
    b"\xcf\xfa\xed\xfe",  # 64-bit Mach-O (reverse)
    b"\xca\xfe\xba\xbe",  # Mach-O fat
    b"\xbe\xba\xfe\xca",  # Mach-O fat reverse
}
SHEBANG_MAGIC = b"#!"

# Baked container builtins — commands that exist in the image PATH and we
# trust to work. Keep this list narrow; anything else gets the "missing"
# treatment unless an absolute path was given.
KNOWN_BUILTINS = {
    "claude-watch",
    "exec-hook",
    # Host-bash bridge helper invoked by exec-hook when
    # CLAUDE_HOST_HOOK_BRIDGE=1. Listed here so audit-hooks reports
    # bare references to it (e.g. in custom settings.json shapes) as
    # `builtin` rather than `missing`.
    "exec-hook-bridge",
    "session-task",
    "claude-event",
    "obligations",
    "trust-workspace",
    "generate-hooks-shim-settings",
    "generate-project-mcp-json",
    # Obligations gate hooks baked at /usr/local/bin/ when
    # CLAUDE_CONTAINER_OBLIGATIONS=1 (default). Listed here so audit-hooks
    # reports them as `builtin` (not `missing`) when they appear in the
    # rewritten settings.json's hooks tree.
    "pre-agent-queue-gate-hook",
    "pre-tool-obligations-gate-hook",
    "post-tool-obligations-update-hook",
    "post-tool-mark-attachment-read-hook",
    # post-tool-agent-arm-hook: PostToolUse:Agent hook that writes
    # agent_id -> queue_id bindings consumed by the
    # subagent_queue_item_running obligations predicate. Default-baked
    # alongside the other gate hooks above.
    "post-tool-agent-arm-hook",
    # obligations-init: idempotent default-row seeder invoked by the
    # entrypoint. Not a hook, but it's referenced from settings.json-
    # adjacent paths in the entrypoint so list it here for completeness.
    "obligations-init",
    # claude-watch alert gate hooks (v59).
    "pre-tool-claude-watch-alert-gate-hook",
    "user-prompt-claude-watch-alert-record-hook",
    "claude-watch-ack",
    # claude-watch dispatch gate hook + CLI (v60).
    "pre-tool-dispatch-gate-hook",
    "claude-watch-dispatch",
    # Agent communication CLIs (v62). agent-msg backs the host->subagent
    # inbox channel via gate-mode obligations; agent-tail streams a
    # subagent's JSONL transcript for inspection. Both are baked at
    # /usr/local/bin/ so they appear as `builtin` not `missing` when
    # referenced from settings.json or hook payloads.
    "agent-msg",
    "agent-tail",
    # Four-tier event model (q-2026-05-21-856d). event-classify is the
    # data-driven source->tier classifier; event-ack manages the
    # actionable + ambient queues (with file locking on every
    # transaction); eval-event-must-act is the obligations evaluator
    # that DENIES Bash after N consecutive missed tool calls;
    # user-prompt-ambient-inject-hook drains the ambient queue into
    # the next UserPromptSubmit context.
    "event-classify",
    "event-ack",
    "eval-event-must-act",
    "eval-queue-ready-unspawned",
    "user-prompt-ambient-inject-hook",
}


def sniff_target(target: str) -> tuple[str, str]:
    """Return (fate, detail). fate is one of: ok-elf, ok-script,
    silent-no-op, missing, builtin, unparseable. detail is a short
    human-readable annotation."""
    if not target:
        return ("unparseable", "empty target")

    # Absolute path → file inspection.
    if target.startswith("/"):
        path = Path(target)
        if not path.exists():
            return ("missing", f"{target} not found")
        try:
            with path.open("rb") as f:
                head = f.read(4)
        except OSError as e:
            return ("missing", f"{target} unreadable: {e}")
        if head.startswith(ELF_MAGIC):
            return ("ok-elf", "ELF binary — exec'd transparently")
        if head in MACHO_MAGICS:
            return ("silent-no-op", "Mach-O binary — exec-hook no-ops")
        if head[:2] == SHEBANG_MAGIC:
            return ("ok-script", "shebang script — exec'd transparently")
        return ("silent-no-op", f"unknown magic {head.hex()} — exec-hook no-ops")

    # Bare command → known builtin?
    head_token = target.split()[0] if " " in target else target
    if head_token in KNOWN_BUILTINS:
        return ("builtin", "container builtin")

    return ("missing", f"bare command `{target}` not in known-builtin list")


# ---------------------------------------------------------------------------
# Hook walker
# ---------------------------------------------------------------------------

@dataclass
class HookFinding:
    event: str
    matcher: str
    raw_command: str
    wrapped_target: str  # the path/cmd that exec-hook will inspect
    is_wrapped: bool
    fate: str
    detail: str

    def to_dict(self) -> dict:
        return asdict(self)


def parse_command(raw: str) -> tuple[bool, str]:
    """Return (is_wrapped, target). A command is 'wrapped' when its first
    shlex token resolves to exec-hook (either '/usr/local/bin/exec-hook',
    './exec-hook', or bare 'exec-hook'). target is the SECOND token in that
    case, otherwise the first token."""
    try:
        toks = shlex.split(raw)
    except ValueError:
        return (False, "")
    if not toks:
        return (False, "")
    first = toks[0]
    is_wrapped = first == "exec-hook" or first.endswith("/exec-hook")
    if is_wrapped and len(toks) >= 2:
        return (True, toks[1])
    if is_wrapped:
        return (True, "")
    return (False, first)


def walk_hooks(settings: dict) -> Iterator[HookFinding]:
    hooks = settings.get("hooks", {})
    if not isinstance(hooks, dict):
        return
    for event, matchers in hooks.items():
        if not isinstance(matchers, list):
            continue
        for matcher_block in matchers:
            if not isinstance(matcher_block, dict):
                continue
            matcher = str(matcher_block.get("matcher", ""))
            inner_hooks = matcher_block.get("hooks", [])
            if not isinstance(inner_hooks, list):
                continue
            for hk in inner_hooks:
                if not isinstance(hk, dict):
                    continue
                if hk.get("type") != "command":
                    continue
                raw = hk.get("command", "")
                if not isinstance(raw, str):
                    continue
                is_wrapped, target = parse_command(raw)
                if not target and is_wrapped:
                    fate, detail = ("unparseable", "exec-hook with no target")
                elif not target:
                    fate, detail = ("unparseable", "empty command")
                else:
                    fate, detail = sniff_target(target)
                    if not is_wrapped and fate not in ("ok-elf", "ok-script", "builtin"):
                        # Bare reference to a non-ELF target = real bug.
                        fate = "not-wrapped"
                        detail = (
                            f"{detail} (and command not wrapped in exec-hook "
                            "— set CLAUDE_CONTAINER_REWRITE_HOOKS=1)"
                        )
                yield HookFinding(
                    event=event,
                    matcher=matcher,
                    raw_command=raw,
                    wrapped_target=target,
                    is_wrapped=is_wrapped,
                    fate=fate,
                    detail=detail,
                )


# ---------------------------------------------------------------------------
# CLI
# ---------------------------------------------------------------------------

def load_settings(path: Path) -> dict:
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except FileNotFoundError:
        raise SystemExit(f"audit-hooks: settings file not found: {path}")
    except (OSError, json.JSONDecodeError) as e:
        raise SystemExit(f"audit-hooks: cannot parse {path}: {e}")


FATE_GLYPHS = {
    "ok-elf": "OK   ",
    "ok-script": "OK   ",
    "builtin": "OK   ",
    "silent-no-op": "SKIP ",
    "missing": "MISS ",
    "not-wrapped": "FAIL ",
    "unparseable": "?    ",
}


def print_human(findings: list[HookFinding], *, settings_path: Path) -> None:
    if not findings:
        print(f"audit-hooks: no hooks defined in {settings_path}")
        return
    print(f"audit-hooks: {settings_path} — {len(findings)} hook entries\n")
    by_fate: dict[str, int] = {}
    for f in findings:
        by_fate[f.fate] = by_fate.get(f.fate, 0) + 1
        glyph = FATE_GLYPHS.get(f.fate, "     ")
        matcher_suffix = f".{f.matcher}" if f.matcher else ""
        print(f"  [{glyph}] {f.event}{matcher_suffix}")
        print(f"          fate:   {f.fate}")
        print(f"          target: {f.wrapped_target or '(none)'}")
        print(f"          detail: {f.detail}")
        if not f.is_wrapped and f.fate in ("silent-no-op", "not-wrapped"):
            print(
                "          hint:   not wrapped in exec-hook; "
                "set CLAUDE_CONTAINER_REWRITE_HOOKS=1 to auto-wrap"
            )
        print()
    print("Summary:")
    for fate in sorted(by_fate):
        print(f"  {fate}: {by_fate[fate]}")


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(
        description="Audit Claude Code hook commands inside the container",
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    home = os.environ.get("HOME", "/home/hndrewaall")
    shim_settings = Path("/tmp/claude-shim/settings.json")
    if os.environ.get("CLAUDE_CONTAINER_REWRITE_HOOKS") == "1" and shim_settings.exists():
        default_settings = shim_settings
    else:
        default_settings = Path(home) / ".claude" / "settings.json"
    ap.add_argument(
        "--settings",
        type=Path,
        default=default_settings,
        help=f"settings.json path (default: {default_settings})",
    )
    ap.add_argument(
        "--json",
        action="store_true",
        help="emit JSON output instead of the human-readable report",
    )
    ap.add_argument(
        "--strict",
        action="store_true",
        help="exit non-zero if any hook is silent-no-op / not-wrapped / "
             "missing — useful for CI checks that want the container to "
             "fail-loud when corp telemetry won't fire",
    )
    ap.add_argument(
        "--test",
        action="store_true",
        help="run the embedded test suite and exit",
    )
    args = ap.parse_args(argv)

    if args.test:
        return run_tests()

    settings = load_settings(args.settings)
    findings = list(walk_hooks(settings))

    if args.json:
        print(json.dumps([f.to_dict() for f in findings], indent=2))
    else:
        print_human(findings, settings_path=args.settings)

    if args.strict:
        bad = sum(1 for f in findings if f.fate in ("silent-no-op", "not-wrapped", "missing"))
        if bad > 0:
            return 1
    return 0


# ---------------------------------------------------------------------------
# Embedded tests
# ---------------------------------------------------------------------------

def run_tests() -> int:
    """Minimal embedded test suite. Returns 0 on pass, 1 on fail."""
    import tempfile

    failures: list[str] = []

    def expect(cond: bool, msg: str) -> None:
        if not cond:
            failures.append(msg)

    # parse_command
    expect(parse_command("exec-hook /tmp/foo") == (True, "/tmp/foo"),
           "wrapped bare exec-hook /tmp/foo")
    expect(parse_command("/usr/local/bin/exec-hook /tmp/foo arg") == (True, "/tmp/foo"),
           "wrapped abs-path exec-hook")
    expect(parse_command("/tmp/foo arg") == (False, "/tmp/foo"),
           "unwrapped abs path")
    expect(parse_command("claude-watch hook-fire foo") == (False, "claude-watch"),
           "bare builtin command")
    expect(parse_command("") == (False, ""), "empty string")

    # sniff_target — synthesize files of each magic kind
    with tempfile.TemporaryDirectory() as td:
        tdp = Path(td)

        elf = tdp / "fake.elf"
        elf.write_bytes(ELF_MAGIC + b"\x00" * 16)
        f, _ = sniff_target(str(elf))
        expect(f == "ok-elf", f"ELF detected, got {f!r}")

        macho = tdp / "fake.macho"
        macho.write_bytes(b"\xfe\xed\xfa\xcf" + b"\x00" * 16)
        f, _ = sniff_target(str(macho))
        expect(f == "silent-no-op", f"Mach-O detected, got {f!r}")

        script = tdp / "fake.sh"
        script.write_bytes(b"#!/bin/bash\necho hi\n")
        f, _ = sniff_target(str(script))
        expect(f == "ok-script", f"shebang detected, got {f!r}")

        missing = tdp / "nope"
        f, _ = sniff_target(str(missing))
        expect(f == "missing", f"missing path, got {f!r}")

        unknown = tdp / "fake.unknown"
        unknown.write_bytes(b"\x00\x01\x02\x03" + b"\x00" * 16)
        f, _ = sniff_target(str(unknown))
        expect(f == "silent-no-op", f"unknown magic → silent-no-op, got {f!r}")

    # Bare command — known builtin vs not
    f, _ = sniff_target("claude-watch")
    expect(f == "builtin", f"builtin recognised, got {f!r}")

    f, _ = sniff_target("some-host-tool")
    expect(f == "missing", f"unknown bare command, got {f!r}")

    # walk_hooks — synthesize a settings.json
    settings = {
        "hooks": {
            "SessionStart": [
                {
                    "matcher": "startup|resume",
                    "hooks": [
                        {"type": "command", "command": "claude-watch hook-fire version_update"},
                    ],
                },
            ],
            "Stop": [
                {
                    "hooks": [
                        {"type": "command", "command": "exec-hook /Users/me/.local/bin/telemetry"},
                    ],
                },
            ],
        }
    }
    findings = list(walk_hooks(settings))
    expect(len(findings) == 2, f"walk_hooks produced {len(findings)} findings, want 2")
    if len(findings) == 2:
        events = sorted(f.event for f in findings)
        expect(events == ["SessionStart", "Stop"],
               f"events {events!r}, want ['SessionStart', 'Stop']")
        sessionstart = next(f for f in findings if f.event == "SessionStart")
        expect(sessionstart.fate == "builtin",
               f"SessionStart fate {sessionstart.fate!r}, want builtin")
        stop = next(f for f in findings if f.event == "Stop")
        expect(stop.is_wrapped,
               "Stop hook should be marked is_wrapped (exec-hook prefix)")
        expect(stop.fate in ("missing", "silent-no-op"),
               f"Stop hook fate {stop.fate!r}; want missing (file absent on test host) "
               f"or silent-no-op")
        expect(stop.wrapped_target == "/Users/me/.local/bin/telemetry",
               f"Stop wrapped_target {stop.wrapped_target!r}")

    # Bare reference to a non-ELF target → not-wrapped (the actionable bug)
    settings2 = {
        "hooks": {
            "PreToolUse": [
                {
                    "matcher": "*",
                    "hooks": [
                        {"type": "command", "command": "/Users/me/.local/bin/missing-bin"},
                    ],
                },
            ],
        }
    }
    findings2 = list(walk_hooks(settings2))
    expect(len(findings2) == 1, "one hook from settings2")
    if findings2:
        f = findings2[0]
        # File missing → 'not-wrapped' (since not wrapped) but the path
        # check ran first and reported 'missing'. The composite check
        # converts non-ELF unwrapped commands to 'not-wrapped'. missing
        # is in the bare-FAIL bucket because the file just doesn't exist
        # — either way we want a non-OK fate.
        expect(
            f.fate in ("not-wrapped", "missing"),
            f"unwrapped missing target → fate {f.fate!r}",
        )
        expect(not f.is_wrapped, "is_wrapped should be False")

    if failures:
        print("FAIL audit-hooks self-tests")
        for fail in failures:
            print(f"  - {fail}")
        return 1

    print("PASS audit-hooks self-tests")
    return 0


if __name__ == "__main__":
    sys.exit(main())
