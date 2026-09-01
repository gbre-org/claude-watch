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
"""

import importlib.util
import sys
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
