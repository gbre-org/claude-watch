"""Tests for cw-theme-sync's idle gate — specifically its ability to tell
Claude Code's DIM ghost suggestion apart from text a human typed.

THE BUG THESE PIN DOWN
----------------------
Claude Code renders a suggestion inside an *empty* input box as dim (SGR 2)
ghost text:

    \x1b[39m❯\xa0\x1b[2mrun the nightly rebuild

`cw-theme-sync` captured the pane with `tmux capture-pane -p -J` — no `-e` —
which strips exactly the attribute that separates that ghost from real input.
The "input line must be empty" gate therefore read the suggestion as pending
input and never opened again while a suggestion was on screen. The daemon
went silent (waiting for idle logged nothing at all), so a wedged gate was
indistinguishable from a dead process; theme changes sat unapplied for hours.

The fix captures with `-e` and strips DIM runs before deciding whether the
input line is empty. The subtlety worth a test: SGR 2 must not be confused
with the "2" inside an extended-colour parameter list (`38;2;r;g;b`, and the
`2` that can appear as a colour index in `38;5;2`), or every ordinary
coloured run would read as a placeholder and the gate would swing open onto
text a human really is typing.

The second half of this file covers the EXTERNAL HOOKS (theme-hooks.d): the
run-parts discovery rules, the frozen argv + env contract, failure/timeout
isolation, and — most importantly — WHERE in the daemon's decision tree each
event fires. `changed` firing before the idle gate is the requirement; wiring
it after would quietly hand every external side effect the gate's unbounded
latency back.
"""

import importlib.util
import os
import sys
import time as _time
from pathlib import Path

import pytest

SCRIPT = Path(__file__).resolve().parents[1] / "cw-theme-sync"


def _load():
    """Import the extension-less script as a module."""
    spec = importlib.util.spec_from_loader(
        "cw_theme_sync",
        importlib.machinery.SourceFileLoader("cw_theme_sync", str(SCRIPT)),
    )
    mod = importlib.util.module_from_spec(spec)
    sys.modules["cw_theme_sync"] = mod
    spec.loader.exec_module(mod)
    return mod


mod = _load()


# --------------------------------------------------------------------------
# _prompt_line — the gate's reading of the input box
# --------------------------------------------------------------------------

# The exact escape shape captured from a live pane on 2026-09-01, while five
# consecutive theme reports produced not one inject attempt (suggestion text
# replaced with a generic placeholder). The box is EMPTY: the cursor sat at
# column 2, immediately after `❯\xa0`.
GHOST_LINE = "\x1b[39m❯\xa0\x1b[2mrun the nightly rebuild"


def test_dim_ghost_suggestion_reads_as_an_empty_input_line():
    assert mod._prompt_line(GHOST_LINE) == ""


def test_real_typed_input_still_reads_as_occupied():
    assert mod._prompt_line("\x1b[39m❯\xa0hello there") == "hello there"


def test_bright_input_after_a_dim_ghost_still_counts():
    # A human typing over the suggestion: the ghost is dropped, the typed
    # characters are not.
    line = "\x1b[39m❯\xa0\x1b[2mghost\x1b[22mtyped"
    assert mod._prompt_line(line) == "typed"


def test_reset_clears_dim_as_well_as_sgr22():
    line = "\x1b[39m❯\xa0\x1b[2mghost\x1b[0mtyped"
    assert mod._prompt_line(line) == "typed"


@pytest.mark.parametrize(
    "sgr",
    [
        "\x1b[38;5;2m",        # palette colour 2 — NOT dim
        "\x1b[38;2;10;20;30m",  # truecolor with a literal 2 — NOT dim
        "\x1b[48;5;2m",        # background palette colour 2 — NOT dim
        "\x1b[38;5;246m",      # the ordinary grey the TUI uses everywhere
    ],
)
def test_extended_colour_params_are_not_mistaken_for_dim(sgr):
    """A colour containing a `2` must not swallow the input line.

    If it did, the gate would report "empty" for a pane a human is typing
    into, and the non-cancelling inject (no `dd` line-clear) would glue
    `/config theme=…` onto their half-written message — the 2026-08-19
    mangling, from the other direction.
    """
    assert mod._prompt_line(f"\x1b[39m❯\xa0{sgr}typed") == "typed"


def test_plain_capture_without_attributes_still_parses():
    """Defensive: a capture that lost its escapes must not crash, and must
    fall back to treating everything after `❯` as input."""
    assert mod._prompt_line("❯ hello") == "hello"
    assert mod._prompt_line("❯ ") == ""


def test_no_prompt_glyph_returns_none():
    assert mod._prompt_line("  ⏵⏵ bypass permissions on\nno prompt here") is None


def test_last_prompt_line_wins():
    text = "❯\xa0stale\n──────\n\x1b[39m❯\xa0\x1b[2mghost"
    assert mod._prompt_line(text) == ""


def test_nbsp_after_the_glyph_is_stripped():
    # The prompt separator is U+00A0, not a plain space; `.strip()` must eat it
    # or an empty box would read as occupied by a single character.
    assert mod._prompt_line("\x1b[39m❯\xa0") == ""


# --------------------------------------------------------------------------
# _busy — must keep working now that captures carry escapes
# --------------------------------------------------------------------------

def test_busy_detects_esc_to_interrupt_through_sgr():
    line = "\x1b[38;5;246m⏵⏵ bypass · \x1b[39m\x1b[2mesc to interrupt\x1b[0m"
    assert mod._busy(line) is True


def test_busy_detects_queued_messages_through_sgr():
    assert mod._busy("\x1b[2mPress up to edit queued messages\x1b[0m") is True


def test_busy_detects_spinner_line_through_sgr():
    assert mod._busy("\x1b[38;5;211m✻ Thinking… (12s · 3.1k tokens)\x1b[39m") is True


def test_idle_pane_with_a_ghost_suggestion_is_not_busy():
    assert mod._busy(GHOST_LINE) is False


# --------------------------------------------------------------------------
# _capture — the flags are the fix; a regression here is silent
# --------------------------------------------------------------------------

def test_capture_asks_tmux_for_joined_lines_and_attributes(monkeypatch):
    seen = {}

    def fake_run(cmd, timeout=10):
        seen["cmd"] = cmd
        return "pane text", 0

    monkeypatch.setattr(mod, "run", fake_run)
    assert mod._capture("dashboard:0.0") == "pane text"
    # -J: without it the status bar truncates and "esc to interrupt" is lost.
    # -e: without it the dim ghost is indistinguishable from typed input.
    assert "-J" in seen["cmd"]
    assert "-e" in seen["cmd"]


def test_capture_returns_none_on_tmux_failure(monkeypatch):
    monkeypatch.setattr(mod, "run", lambda cmd, timeout=10: ("", 1))
    assert mod._capture("dashboard:0.0") is None


# --------------------------------------------------------------------------
# idle_state — a shut gate has to name itself
# --------------------------------------------------------------------------

def _pane(monkeypatch, *frames):
    """Feed `idle_state` a fixed sequence of pane captures."""
    frames = list(frames)
    monkeypatch.setattr(mod, "PROMPT_RECHECK_SECS", 0.0)
    monkeypatch.setattr(
        mod, "_capture", lambda pane: frames.pop(0) if frames else frames_last(frames)
    )


def frames_last(frames):  # pragma: no cover - guard for over-consumption
    raise AssertionError("_capture called more times than the test provided frames")


def test_idle_state_opens_for_a_ghost_only_prompt(monkeypatch):
    _pane(monkeypatch, GHOST_LINE, GHOST_LINE)
    assert mod.idle_state("p") == (True, "idle")


def test_idle_state_names_a_busy_pane(monkeypatch):
    _pane(monkeypatch, "esc to interrupt\n❯\xa0")
    idle, reason = mod.idle_state("p")
    assert idle is False
    assert "busy" in reason


def test_idle_state_names_occupied_input(monkeypatch):
    _pane(monkeypatch, "\x1b[39m❯\xa0half a sentence")
    idle, reason = mod.idle_state("p")
    assert idle is False
    assert "half a sentence" in reason


def test_idle_state_names_a_missing_prompt(monkeypatch):
    _pane(monkeypatch, "just some scrollback\nno box at all")
    idle, reason = mod.idle_state("p")
    assert idle is False
    assert "no prompt" in reason


def test_idle_state_names_an_uncapturable_pane(monkeypatch):
    _pane(monkeypatch, None)
    idle, reason = mod.idle_state("p")
    assert idle is False
    assert "cannot capture" in reason


def test_idle_state_catches_a_human_typing_during_the_recheck(monkeypatch):
    _pane(monkeypatch, GHOST_LINE, "\x1b[39m❯\xa0now typing")
    idle, reason = mod.idle_state("p")
    assert idle is False
    assert "recheck" in reason


def test_is_idle_wraps_idle_state(monkeypatch):
    _pane(monkeypatch, GHOST_LINE, GHOST_LINE)
    assert mod.is_idle("p") is True


# ==========================================================================
# External hooks (theme-hooks.d)
#
# The one hard property: a hook can NEVER break the inject path. Everything
# below either pins a piece of the published contract (argv shape, env,
# discovery rules, ordering) or pins that a misbehaving hook is survivable.
# ==========================================================================

def _hooks_dir(tmp_path, monkeypatch):
    d = tmp_path / "theme-hooks.d"
    d.mkdir()
    monkeypatch.setattr(mod, "HOOKS_DIR", str(d))
    return d


def _hook(d, name, body="#!/bin/sh\nexit 0\n", mode=0o755):
    p = d / name
    p.write_text(body)
    p.chmod(mode)
    return p


@pytest.fixture
def logged(monkeypatch):
    """Capture the daemon's log lines instead of writing them anywhere."""
    lines = []
    monkeypatch.setattr(mod, "log", lines.append)
    return lines


# --------------------------------------------------------------------------
# discover_hooks — run-parts conventions
# --------------------------------------------------------------------------

def test_missing_hooks_dir_is_empty_not_an_error(monkeypatch, tmp_path):
    monkeypatch.setattr(mod, "HOOKS_DIR", str(tmp_path / "nope"))
    assert mod.discover_hooks() == []


def test_only_executable_files_are_discovered(monkeypatch, tmp_path):
    d = _hooks_dir(tmp_path, monkeypatch)
    _hook(d, "10-yes")
    _hook(d, "20-no", mode=0o644)  # chmod -x is the disable switch
    assert [h.name for h in mod.discover_hooks()] == ["10-yes"]


@pytest.mark.parametrize(
    "name",
    [
        "10-kiosk.sh",     # any dot at all
        "20-notify.bak",
        "30-old.disabled",
        "40-editor~",      # editor backup
        ".hidden",
        "50 spaced",
    ],
)
def test_names_outside_the_run_parts_charset_are_skipped(
    monkeypatch, tmp_path, name
):
    """The name filter is what stops a backup copy firing behind your back."""
    d = _hooks_dir(tmp_path, monkeypatch)
    _hook(d, name)
    assert mod.discover_hooks() == []


def test_subdirectories_are_skipped_not_recursed(monkeypatch, tmp_path):
    d = _hooks_dir(tmp_path, monkeypatch)
    sub = d / "20-nested"
    sub.mkdir()
    _hook(sub, "10-inner")
    _hook(d, "10-top")
    assert [h.name for h in mod.discover_hooks()] == ["10-top"]


def test_symlink_to_an_executable_is_included(monkeypatch, tmp_path):
    d = _hooks_dir(tmp_path, monkeypatch)
    target = tmp_path / "real-tool"
    target.write_text("#!/bin/sh\nexit 0\n")
    target.chmod(0o755)
    (d / "10-link").symlink_to(target)
    assert [h.name for h in mod.discover_hooks()] == ["10-link"]


def test_discovery_order_is_byte_not_numeric(monkeypatch, tmp_path):
    """`10-a` before `2-c`: the contract says BYTE order, like run-parts.

    Pinned explicitly because "sorted by NN prefix" reads as numeric to
    everyone who has not been bitten by it, and a hook pair whose relative
    order matters would silently swap.
    """
    d = _hooks_dir(tmp_path, monkeypatch)
    for name in ("20-b", "2-c", "10-a"):
        _hook(d, name)
    assert [h.name for h in mod.discover_hooks()] == ["10-a", "2-c", "20-b"]


# --------------------------------------------------------------------------
# Invocation contract — argv stays at two args forever; the rest is env
# --------------------------------------------------------------------------

DUMP_HOOK = """#!/bin/sh
{
  echo "argc=$#"
  echo "arg1=$1"
  echo "arg2=$2"
  echo "cwd=$(pwd)"
  echo "event=$CW_THEME_EVENT"
  echo "new=$CW_THEME_NEW"
  echo "old=$CW_THEME_OLD"
  echo "reason=$CW_THEME_REASON"
  echo "source=$CW_THEME_SOURCE_FILE"
  echo "ts=$CW_THEME_TIMESTAMP"
  echo "iso=$CW_THEME_TIMESTAMP_ISO"
  echo "pane=${CW_THEME_PANE-<unset>}"
  echo "hook=$CW_THEME_HOOK"
  echo "stdin=$(cat)"
} > "$HOOK_DUMP"
"""


def _dump(path):
    return dict(
        ln.split("=", 1) for ln in path.read_text().splitlines() if "=" in ln
    )


def test_applied_hook_gets_the_full_documented_contract(
    monkeypatch, tmp_path, logged
):
    d = _hooks_dir(tmp_path, monkeypatch)
    _hook(d, "10-dump", DUMP_HOOK)
    out = tmp_path / "dump.txt"
    monkeypatch.setenv("HOOK_DUMP", str(out))
    monkeypatch.setattr(mod, "THEME_FILE", "/host-clipboard/theme")

    mod.run_hooks(
        "applied", "dark", old="light", reason="change",
        pane="claude-container:0.0",
    )

    got = _dump(out)
    # argv is frozen at TWO positional args. Growing it is exactly the
    # compatibility trap this contract exists to avoid.
    assert got["argc"] == "2"
    assert got["arg1"] == "applied"
    assert got["arg2"] == "dark"
    assert got["cwd"] == "/"
    assert got["event"] == "applied"
    assert got["new"] == "dark"
    assert got["old"] == "light"
    assert got["reason"] == "change"
    assert got["source"] == "/host-clipboard/theme"
    assert got["ts"].isdigit()
    assert got["iso"].startswith("20") and "T" in got["iso"]
    assert got["pane"] == "claude-container:0.0"
    assert got["hook"] == "10-dump"
    # stdin is /dev/null, so `cat` returns immediately with nothing.
    assert got["stdin"] == ""


def test_changed_hook_has_no_pane_even_when_the_daemon_was_given_one(
    monkeypatch, tmp_path, logged
):
    """CW_THEME_PANE is `applied`-only, and that must hold for the ABSENCE.

    CW_THEME_PANE is also one of the daemon's own config knobs, so a naive
    "inherit the environment" would leak it into a `changed` fire and invite
    hooks to trust a pane that has nothing to do with that event.
    """
    d = _hooks_dir(tmp_path, monkeypatch)
    _hook(d, "10-dump", DUMP_HOOK)
    out = tmp_path / "dump.txt"
    monkeypatch.setenv("HOOK_DUMP", str(out))
    monkeypatch.setenv("CW_THEME_PANE", "configured:0.0")

    mod.run_hooks("changed", "light", old=None, reason="startup")

    got = _dump(out)
    assert got["pane"] == "<unset>"
    assert got["old"] == ""       # empty on the first observation
    assert got["reason"] == "startup"


def test_hook_environment_inherits_the_daemon_environment(
    monkeypatch, tmp_path, logged
):
    d = _hooks_dir(tmp_path, monkeypatch)
    out = tmp_path / "dump.txt"
    _hook(d, "10-inherit", f'#!/bin/sh\necho "$SOME_SITE_VAR" > "{out}"\n')
    monkeypatch.setenv("SOME_SITE_VAR", "kept")
    mod.run_hooks("changed", "dark")
    assert out.read_text().strip() == "kept"


# --------------------------------------------------------------------------
# Failure isolation — the hard property
# --------------------------------------------------------------------------

def test_a_failing_hook_is_swallowed_and_does_not_stop_the_next(
    monkeypatch, tmp_path, logged
):
    d = _hooks_dir(tmp_path, monkeypatch)
    marker = tmp_path / "ran"
    _hook(d, "10-boom", "#!/bin/sh\necho 'curl: (7) refused' >&2\nexit 1\n")
    _hook(d, "20-after", f'#!/bin/sh\ntouch "{marker}"\n')

    mod.run_hooks("changed", "dark")  # must not raise

    assert marker.exists(), "a failing hook must not abort the sequence"
    joined = "\n".join(logged)
    assert "hook 10-boom (changed dark): FAILED rc=1" in joined
    assert "curl: (7) refused" in joined
    assert "hook 20-after (changed dark): ok in" in joined


def test_a_hanging_hook_is_killed_at_the_timeout_and_the_rest_still_run(
    monkeypatch, tmp_path, logged
):
    """Bounded worst case.

    `sleep` is a CHILD of the hook shell, so this also pins that the kill goes
    to the process GROUP: killing only the shell leaves the child alive AND
    still holding the stdout pipe, which turns a bounded timeout into an
    unbounded wait.
    """
    d = _hooks_dir(tmp_path, monkeypatch)
    marker = tmp_path / "ran"
    monkeypatch.setattr(mod, "HOOK_TIMEOUT_SECS", 0.3)
    _hook(d, "10-slow", "#!/bin/sh\nsleep 30\n")
    _hook(d, "20-after", f'#!/bin/sh\ntouch "{marker}"\n')

    started = _time.monotonic()
    mod.run_hooks("applied", "light", pane="p")
    elapsed = _time.monotonic() - started

    assert elapsed < 10, f"the timeout was not bounded ({elapsed:.1f}s)"
    assert marker.exists()
    assert "hook 10-slow (applied light): TIMEOUT after 0.3s; killed" in logged


def test_a_hook_that_cannot_exec_is_reported_not_raised(
    monkeypatch, tmp_path, logged
):
    d = _hooks_dir(tmp_path, monkeypatch)
    # Executable bit set, but the interpreter does not exist.
    _hook(d, "10-broken", "#!/nonexistent/interpreter\n")
    mod.run_hooks("changed", "dark")
    assert any("10-broken" in ln and "FAILED" in ln for ln in logged)


def test_run_hooks_never_raises_even_if_discovery_explodes(monkeypatch, logged):
    def boom():
        raise RuntimeError("kaboom")

    monkeypatch.setattr(mod, "discover_hooks", boom)
    mod.run_hooks("changed", "dark")  # must not propagate
    assert any("hook runner error" in ln for ln in logged)


def test_hook_output_is_truncated_in_the_log(monkeypatch, tmp_path, logged):
    d = _hooks_dir(tmp_path, monkeypatch)
    _hook(
        d, "10-chatty",
        "#!/bin/sh\nfor i in 1 2 3 4 5 6 7 8; do echo L$i; done\n",
    )
    mod.run_hooks("changed", "dark")
    line = next(ln for ln in logged if "10-chatty" in ln)
    assert "L5" in line and "L6" not in line
    assert "[truncated]" in line


# --------------------------------------------------------------------------
# Forward compatibility — a hook must exit 0 on an event it does not know
# --------------------------------------------------------------------------

def test_an_unrecognised_event_is_passed_through_verbatim(
    monkeypatch, tmp_path, logged
):
    """Adding a third event later (`failed`, say) must be non-breaking.

    The runner does not police the event name, and a contract-abiding hook
    exits 0 on one it does not recognise — so it logs `ok`, not `FAILED`.
    """
    d = _hooks_dir(tmp_path, monkeypatch)
    out = tmp_path / "seen"
    _hook(
        d, "10-guarded",
        "#!/bin/sh\n"
        'case "$1" in\n'
        f'  changed) echo changed > "{out}" ;;\n'
        "  *) exit 0 ;;\n"   # the documented forward-compat rule
        "esac\n",
    )
    mod.run_hooks("failed", "dark")
    assert not out.exists()
    assert any("10-guarded (failed dark): ok in" in ln for ln in logged)


# --------------------------------------------------------------------------
# Re-discovery — installing a hook must not need a restart
# --------------------------------------------------------------------------

def test_hooks_are_rediscovered_on_every_fire(monkeypatch, tmp_path, logged):
    d = _hooks_dir(tmp_path, monkeypatch)
    mod.run_hooks("changed", "dark")
    assert logged == []

    marker = tmp_path / "late"
    _hook(d, "10-late", f'#!/bin/sh\ntouch "{marker}"\n')
    mod.run_hooks("changed", "light")
    assert marker.exists()


# --------------------------------------------------------------------------
# Off by default, by absence — the regression that keeps this feature free
# --------------------------------------------------------------------------

def test_no_hooks_dir_spawns_nothing_and_logs_nothing(
    monkeypatch, tmp_path, logged
):
    monkeypatch.setattr(mod, "HOOKS_DIR", str(tmp_path / "absent"))

    def no_subprocesses(*a, **kw):  # pragma: no cover - must never run
        raise AssertionError("a hook subprocess was spawned with no hooks dir")

    monkeypatch.setattr(mod.subprocess, "Popen", no_subprocesses)
    mod.run_hooks("changed", "dark")
    mod.run_hooks("applied", "dark", pane="p")
    assert logged == []


def test_an_empty_hooks_dir_is_equally_silent(monkeypatch, tmp_path, logged):
    _hooks_dir(tmp_path, monkeypatch)
    mod.run_hooks("changed", "dark")
    assert logged == []


# --------------------------------------------------------------------------
# --status surfaces the dir + what was discovered in it
# --------------------------------------------------------------------------

def test_status_reports_hooks_dir_and_the_discovered_list(
    monkeypatch, tmp_path, capsys
):
    d = _hooks_dir(tmp_path, monkeypatch)
    _hook(d, "10-a")
    _hook(d, "20-b", mode=0o644)  # lost its +x: must NOT be listed
    monkeypatch.setattr(mod, "resolve_pane", lambda: "p")
    monkeypatch.setattr(mod, "idle_state", lambda pane: (True, "idle"))
    monkeypatch.setattr(mod, "read_theme", lambda: "dark")
    monkeypatch.setattr(mod, "read_settings_theme", lambda: "dark")

    mod.print_status()

    out = capsys.readouterr().out
    assert f"hooks_dir     : {d}" in out
    assert "hooks         : 10-a" in out
    assert "20-b" not in out


# ==========================================================================
# Wiring — WHERE the two events fire in the daemon's decision tree.
#
# `changed` before the idle gate is the requirement; wiring it after is the
# regression that would quietly reintroduce hours of latency on every
# external side effect.
# ==========================================================================

class _StopLoop(Exception):
    """Break out of the daemon's `while True` from a patched sleep."""


class _Ticker:
    """A `time` stand-in that aborts the poll loop after N sleeps."""

    def __init__(self, limit):
        self.limit = limit
        self.calls = 0

    def __getattr__(self, name):
        return getattr(_time, name)

    def sleep(self, _secs):
        self.calls += 1
        if self.calls >= self.limit:
            raise _StopLoop()


def _daemon(monkeypatch, tmp_path, theme="dark", idle=True, inject=True,
            sleeps=2):
    """Arm `main()`'s daemon path with recorded hook fires.

    Returns the list a fake `run_hooks` appends
    `(event, new, old, reason, pane)` tuples to.
    """
    theme_file = tmp_path / "theme"
    theme_file.write_text(theme)
    monkeypatch.setattr(mod, "THEME_FILE", str(theme_file))
    monkeypatch.setattr(mod, "LOG_FILE", str(tmp_path / "log"))
    monkeypatch.setattr(mod, "POLL_SECS", 0.0)
    monkeypatch.setattr(mod, "IDLE_POLL_SECS", 0.0)
    monkeypatch.setattr(mod, "resolve_pane", lambda: "claude-container:0.0")
    monkeypatch.setattr(
        mod, "idle_state",
        lambda pane: (True, "idle") if idle else (False, "pane busy"),
    )
    monkeypatch.setattr(mod, "inject_theme", lambda pane, t: inject)
    monkeypatch.setattr(mod, "log", lambda msg: None)
    monkeypatch.setattr(mod, "time", _Ticker(sleeps))
    monkeypatch.setattr(sys, "argv", ["cw-theme-sync"])

    fires = []
    monkeypatch.setattr(
        mod, "run_hooks",
        lambda event, new, old=None, reason="change", pane=None: fires.append(
            (event, new, old, reason, pane)
        ),
    )
    return fires


def _run_daemon():
    with pytest.raises(_StopLoop):
        mod.main()


def test_changed_fires_even_when_the_pane_never_goes_idle(
    monkeypatch, tmp_path
):
    """THE requirement. The idle gate can stay shut for hours, and an
    external side effect must not inherit that latency."""
    fires = _daemon(monkeypatch, tmp_path, theme="dark", idle=False)
    _run_daemon()
    assert ("changed", "dark", None, "startup", None) in fires
    assert not [f for f in fires if f[0] == "applied"]


def test_startup_fires_changed_with_an_empty_old_value(monkeypatch, tmp_path):
    fires = _daemon(monkeypatch, tmp_path, theme="light", idle=False)
    _run_daemon()
    assert fires[0] == ("changed", "light", None, "startup", None)


def test_applied_fires_only_after_a_verified_inject(monkeypatch, tmp_path):
    fires = _daemon(monkeypatch, tmp_path, theme="dark", idle=True, inject=True)
    _run_daemon()
    assert [f[0] for f in fires] == ["changed", "applied"]
    assert fires[1] == (
        "applied", "dark", None, "startup", "claude-container:0.0",
    )


def test_applied_does_not_fire_on_a_failed_inject(monkeypatch, tmp_path):
    fires = _daemon(
        monkeypatch, tmp_path, theme="dark", idle=True, inject=False,
    )
    _run_daemon()
    assert [f[0] for f in fires] == ["changed"]


def test_applied_does_not_fire_on_the_giving_up_path(monkeypatch, tmp_path):
    """GIVING UP marks the theme applied to stop hammering the prompt line.
    That bookkeeping must not masquerade as a real apply."""
    fires = _daemon(
        monkeypatch, tmp_path, theme="dark", idle=True, inject=False,
        sleeps=12,
    )
    monkeypatch.setattr(mod, "MAX_VERIFY_FAILURES", 1)
    monkeypatch.setattr(mod, "RETRY_BACKOFF_SECS", (0.0,))
    _run_daemon()
    assert [f[0] for f in fires] == ["changed"]


def test_a_same_value_rewrite_fires_nothing(monkeypatch, tmp_path):
    """The daemon already coalesces and is idempotent; hooks inherit that."""
    fires = _daemon(
        monkeypatch, tmp_path, theme="dark", idle=False, sleeps=6,
    )
    theme_file = Path(mod.THEME_FILE)
    real_getmtime = os.path.getmtime
    bumped = {"n": 0}

    def rewriting_getmtime(path):
        # The browser re-POSTs the SAME value on every page load: a fresh
        # mtime, an unchanged value.
        bumped["n"] += 1
        theme_file.write_text("dark")
        return real_getmtime(path) + bumped["n"]

    monkeypatch.setattr(os.path, "getmtime", rewriting_getmtime)
    _run_daemon()
    assert [f[0] for f in fires] == ["changed"], "only the first read changed"


def test_an_unrecognised_file_value_fires_nothing(monkeypatch, tmp_path):
    fires = _daemon(monkeypatch, tmp_path, theme="chartreuse", idle=False)
    _run_daemon()
    assert fires == []


def test_force_fires_applied_with_reason_force(monkeypatch, tmp_path):
    theme_file = tmp_path / "theme"
    theme_file.write_text("light")
    monkeypatch.setattr(mod, "THEME_FILE", str(theme_file))
    monkeypatch.setattr(mod, "resolve_pane", lambda: "p")
    monkeypatch.setattr(mod, "idle_state", lambda pane: (True, "idle"))
    monkeypatch.setattr(mod, "inject_theme", lambda pane, t: True)
    monkeypatch.setattr(mod, "log", lambda msg: None)
    fires = []
    monkeypatch.setattr(
        mod, "run_hooks",
        lambda event, new, old=None, reason="change", pane=None: fires.append(
            (event, new, old, reason, pane)
        ),
    )
    assert mod.force_once() == 0
    # `changed` must NOT fire: --force re-asserts, it does not change anything.
    assert fires == [("applied", "light", None, "force", "p")]


def test_force_fires_nothing_when_the_inject_fails(monkeypatch, tmp_path):
    theme_file = tmp_path / "theme"
    theme_file.write_text("light")
    monkeypatch.setattr(mod, "THEME_FILE", str(theme_file))
    monkeypatch.setattr(mod, "resolve_pane", lambda: "p")
    monkeypatch.setattr(mod, "idle_state", lambda pane: (True, "idle"))
    monkeypatch.setattr(mod, "inject_theme", lambda pane, t: False)
    monkeypatch.setattr(mod, "log", lambda msg: None)
    fires = []
    monkeypatch.setattr(mod, "run_hooks", lambda *a, **kw: fires.append(a))
    assert mod.force_once() == 3
    assert fires == []
