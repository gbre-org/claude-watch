#!/usr/bin/env python3
"""Tests for LOCK_TOGGLE_JS in inject-autodark.py.

The lock toggle injects a subtle top-right padlock button into ttyd's
bundled HTML. When ACTIVE it suppresses keystrokes to the terminal by
returning `false` from xterm.js's `attachCustomKeyEventHandler` hook, so
no key is written to the PTY / sent over ttyd's input WebSocket. When
inactive ttyd behaves exactly as stock. It also AUTO-ENGAGES after a
configurable idle window (TTYD_AUTOLOCK_SECONDS, default 300s, 0 = off).

We load the LOCK_TOGGLE_JS body into a Node v8 context with a tiny DOM
stub, a fake `window.term` (whose `attachCustomKeyEventHandler` captures
the veto callback), a fake wall clock (`Date.now`), and recorded
`document.addEventListener` / `setInterval` registrations, run the IIFE,
then:

  1. assert the button was created (id `cw-lock-toggle`), starts UNLOCKED
     (open-padlock glyph, aria-pressed=false, no `cw-locked` class), and
     the key-veto callback returns true (xterm processes keys normally);
  2. invoke the captured click handler → LOCKED: key-veto returns false
     (keystroke suppressed), `window.__cwTerminalLocked` flips true, the
     glyph switches to the closed padlock, aria-pressed=true, and the
     `cw-locked` class is applied (the visual cue);
  3. invoke the click handler again → back to UNLOCKED, key-veto true;
  4. drive the fake clock: activity just before the deadline resets the
     countdown, idling past it auto-locks through the SAME code path as
     the button (glyph / aria / veto / localStorage all flip), and a
     build with the window set to 0 never locks and registers no
     activity listeners at all.

The Python side (env-var resolution → the literal baked into the
injected JS) is covered by running inject-autodark.py as a subprocess
against a minimal HTML document.

Run: `python3 tests/test_lock_toggle.py` from this directory, or
`make test-ttyd-lock-toggle` from the repo root.
"""

import ast
import json
import os
import re
import subprocess
import sys
import tempfile
import unittest


HERE = os.path.dirname(os.path.abspath(__file__))
INJECT_SCRIPT = os.path.join(os.path.dirname(HERE), "inject-autodark.py")


def _extract_const(name: str):
    """Pull a module-level literal constant out of inject-autodark.py."""
    with open(INJECT_SCRIPT, "r", encoding="utf-8") as f:
        tree = ast.parse(f.read())
    for node in ast.walk(tree):
        if isinstance(node, ast.Assign):
            for target in node.targets:
                if (
                    isinstance(target, ast.Name)
                    and target.id == name
                    and isinstance(node.value, ast.Constant)
                ):
                    return node.value.value
    raise RuntimeError(f"{name} not found in inject-autodark.py")


# Back-compat alias — the JS constants are the common case.
_extract_js_const = _extract_const


def _strip_script_tags(s: str) -> str:
    s = re.sub(r"^\s*<script[^>]*>", "", s)
    s = re.sub(r"</script>\s*$", "", s)
    return s


# The build-time knob and its default, read straight from the injector so
# the tests can't drift from the source of truth.
AUTOLOCK_PLACEHOLDER = _extract_const("AUTOLOCK_PLACEHOLDER")
AUTOLOCK_ENV_VAR = _extract_const("AUTOLOCK_ENV_VAR")
DEFAULT_AUTOLOCK_SECONDS = _extract_const("DEFAULT_AUTOLOCK_SECONDS")

# Deliberate-input events the idle timer listens for. Terminal OUTPUT is
# deliberately absent: counting it would let a chatty session hold the
# lock open forever, which is the unattended case auto-lock exists for.
EXPECTED_ACTIVITY_EVENTS = {
    "keydown",
    "mousedown",
    "pointerdown",
    "touchstart",
    "wheel",
    "paste",
}


# Harness JS: stub DOM + window.term + wall clock, load the IIFE, capture
# the button's click handler and xterm's veto callback, then snapshot
# state before / after two toggles and across the idle scenarios.
HARNESS_TEMPLATE = r"""
'use strict';

// --- fake wall clock ------------------------------------------------
// The auto-lock compares Date.now() against a last-activity stamp, so
// the harness drives time directly instead of sleeping.
var __now = 1600000000000;
Date.now = function() { return __now; };
function advance(ms) { __now += ms; }

// --- element stub ---------------------------------------------------
function makeEl() {
    var classes = {};
    return {
        id: '', type: '', textContent: '', title: '',
        _attrs: {},
        _listeners: {},
        setAttribute: function(k, v) { this._attrs[k] = v; },
        getAttribute: function(k) { return this._attrs[k]; },
        addEventListener: function(n, fn) { this._listeners[n] = fn; },
        classList: {
            add: function(c) { classes[c] = true; },
            remove: function(c) { delete classes[c]; },
            contains: function(c) { return !!classes[c]; },
        },
    };
}

var appendedButton = null;

// --- DOM stub -------------------------------------------------------
// document-level listeners are recorded so the test can assert WHICH
// events count as activity, and fire them synthetically.
var docListeners = {};
var docListenerOrder = [];
global.document = {
    readyState: 'complete',
    addEventListener: function(n, fn) {
        if (!docListeners[n]) { docListenerOrder.push(n); }
        docListeners[n] = fn;
    },
    createElement: function(tag) { return makeEl(); },
    body: {
        appendChild: function(el) { appendedButton = el; },
    },
};

// --- window.term stub: capture the veto callback -------------------
// `element` is the xterm `.xterm` container the pointer/mouse guard binds
// its capture-phase listeners to; makeEl records them in `_listeners` so
// the test can fire a synthetic pointer event and inspect the guard.
var keyVeto = null;
var termElement = makeEl();
global.window = {
    term: {
        attachCustomKeyEventHandler: function(fn) { keyVeto = fn; },
        element: termElement,
    },
    // Drives the `?autolock=<seconds>` per-tab override.
    location: { search: '__LOCATION_SEARCH__' },
};

// --- localStorage stub: backs the persistence path -----------------
// Pre-seeded with __PRESEED__ (a JSON object) so we can exercise both
// the fresh-load (empty) and restore-from-storage (pre-set) cases.
var __lsStore = __PRESEED__;
global.window.localStorage = {
    getItem: function(k) {
        return Object.prototype.hasOwnProperty.call(__lsStore, k)
            ? __lsStore[k] : null;
    },
    setItem: function(k, v) { __lsStore[k] = String(v); },
    removeItem: function(k) { delete __lsStore[k]; },
};

// Timers are recorded rather than run: the init poll is idempotent and
// the idle tick is what the auto-lock scenarios drive by hand.
var intervals = [];
global.setInterval = function(fn, ms) {
    intervals.push(fn);
    return intervals.length;
};
global.clearInterval = function() {};
function runTicks() {
    for (var i = 0; i < intervals.length; i++) { intervals[i](); }
}

// --- Load the IIFE --------------------------------------------------
__HANDLER_BODY__

if (!appendedButton) {
    console.error(JSON.stringify({ error: 'lock button not created' }));
    process.exit(2);
}
if (typeof keyVeto !== 'function') {
    console.error(JSON.stringify({ error: 'key veto handler not attached' }));
    process.exit(2);
}

function snapshot() {
    return {
        buttonId: appendedButton.id,
        ariaPressed: appendedButton.getAttribute('aria-pressed'),
        ariaLabel: appendedButton.getAttribute('aria-label'),
        textContent: appendedButton.textContent,
        title: appendedButton.title,
        hasLockedClass: appendedButton.classList.contains('cw-locked'),
        lockedFlag: global.window.__cwTerminalLocked,
        // keyVeto returns TRUE when xterm should process the key (unlocked)
        // and FALSE when the key must be suppressed (locked).
        keyProcessed: keyVeto({ key: 'a' }),
        // Persisted value in the localStorage stub ('1'/'0'/null).
        stored: global.window.localStorage.getItem('cw-terminal-locked'),
    };
}

var out = {
    initial: null, afterLock: null, afterUnlock: null,
    // Idle window this page resolved to (build-time default, possibly
    // overridden by ?autolock=).
    autoLockSeconds: global.window.__cwAutoLockSeconds,
    // Which document events were wired as activity signals.
    activityEvents: docListenerOrder,
};
out.initial = snapshot();

var click = appendedButton._listeners['click'];
if (typeof click !== 'function') {
    console.error(JSON.stringify({ error: 'click handler not registered' }));
    process.exit(2);
}
var fakeEvt = {
    preventDefault: function() {},
    stopPropagation: function() {},
};

click(fakeEvt);
out.afterLock = snapshot();

click(fakeEvt);
out.afterUnlock = snapshot();

// --- auto-lock scenarios --------------------------------------------
// Deadline in ms for whatever window this page resolved to (0 = the
// auto-lock is disabled, and every step below must leave it unlocked).
var DEADLINE_MS = (out.autoLockSeconds || 0) * 1000;

// (a) activity one second shy of the deadline restarts the countdown,
//     so a second near-deadline idle stretch STILL does not lock.
advance(Math.max(DEADLINE_MS - 1000, 0));
runTicks();
out.beforeDeadline = snapshot();
var keyActivity = docListeners['keydown'];
if (keyActivity) { keyActivity({ type: 'keydown' }); }
advance(Math.max(DEADLINE_MS - 1000, 0));
runTicks();
out.afterActivityReset = snapshot();

// (b) idling past the deadline with no activity auto-locks.
advance(DEADLINE_MS + 1000);
runTicks();
out.afterIdle = snapshot();

// (c) an hour of further idling: proves a disabled build never locks,
//     and that an already-locked one is not re-locked on top of itself.
advance(3600 * 1000);
runTicks();
out.afterLongIdle = snapshot();

// --- pointer / mouse guard ------------------------------------------
// The lock must ALSO swallow pointer input (clicks/taps, wheel
// scrollback, mouse-tracking escape sequences, touch) — otherwise a
// locked terminal still forwards them to the live session. The guard
// binds capture-phase listeners to the terminal element; here we fire
// each synthetically and record whether it was swallowed. Driven from a
// deterministic UNLOCKED baseline so the outcome is independent of the
// build config that ran the toggle cycle above.
var POINTER_GUARD_EVENTS = [
    'mousedown', 'mouseup', 'mousemove', 'click', 'dblclick',
    'contextmenu', 'wheel',
    'pointerdown', 'pointerup', 'pointermove',
    'touchstart', 'touchmove', 'touchend'
];
function firePointer(name) {
    var fn = termElement._listeners[name];
    if (typeof fn !== 'function') { return { installed: false }; }
    var prevented = false, stopped = false;
    fn({
        type: name,
        cancelable: true,
        preventDefault: function() { prevented = true; },
        stopImmediatePropagation: function() { stopped = true; },
    });
    return { installed: true, prevented: prevented, stopped: stopped };
}
function firePointerAll() {
    var r = {};
    for (var i = 0; i < POINTER_GUARD_EVENTS.length; i++) {
        r[POINTER_GUARD_EVENTS[i]] = firePointer(POINTER_GUARD_EVENTS[i]);
    }
    return r;
}
// Force a known UNLOCKED baseline (the toggle cycle / auto-lock may have
// left it either way depending on config), then measure both states.
if (global.window.__cwTerminalLocked) { click(fakeEvt); }
out.pointerUnlocked = firePointerAll();
click(fakeEvt);   // engage the lock
out.pointerLocked = firePointerAll();
out.pointerGuardEvents = POINTER_GUARD_EVENTS;

process.stdout.write(JSON.stringify(out) + '\n');
process.exit(0);
"""

# Open / closed padlock glyphs the JS emits (surrogate pairs decoded to
# the actual astral code points for comparison against the JSON output).
UNLOCK_GLYPH = "\U0001F513"  # open padlock
LOCK_GLYPH = "\U0001F512"    # closed padlock


def _run_node_harness(
    preseed: dict | None = None,
    autolock: int = DEFAULT_AUTOLOCK_SECONDS,
    search: str = "",
) -> dict:
    js = _strip_script_tags(_extract_js_const("LOCK_TOGGLE_JS"))
    # Same substitution the injector performs at build time.
    js = js.replace(AUTOLOCK_PLACEHOLDER, str(autolock))
    harness = HARNESS_TEMPLATE.replace("__HANDLER_BODY__", js)
    harness = harness.replace("__PRESEED__", json.dumps(preseed or {}))
    harness = harness.replace("__LOCATION_SEARCH__", search)
    proc = subprocess.run(
        ["node", "-e", harness],
        capture_output=True,
        text=True,
        timeout=30,
    )
    if proc.returncode != 0:
        raise RuntimeError(
            f"node harness failed (rc={proc.returncode}):\n"
            f"STDOUT:\n{proc.stdout}\nSTDERR:\n{proc.stderr}"
        )
    return json.loads(proc.stdout.strip().splitlines()[-1])


class LockToggleTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.out = _run_node_harness()

    def test_button_created_with_expected_ids(self):
        i = self.out["initial"]
        self.assertEqual(i["buttonId"], "cw-lock-toggle")
        self.assertEqual(i["ariaLabel"], "Toggle terminal input lock")

    def test_starts_unlocked_and_passes_keys(self):
        i = self.out["initial"]
        self.assertEqual(i["ariaPressed"], "false")
        self.assertFalse(i["hasLockedClass"])
        self.assertEqual(i["textContent"], UNLOCK_GLYPH)
        # Unlocked → xterm processes the key (veto returns true).
        self.assertTrue(i["keyProcessed"])
        # Flag is defined and false up front.
        self.assertEqual(i["lockedFlag"], False)

    def test_click_locks_and_suppresses_keys(self):
        a = self.out["afterLock"]
        self.assertEqual(a["ariaPressed"], "true")
        self.assertTrue(a["hasLockedClass"], "locked visual cue missing")
        self.assertEqual(a["textContent"], LOCK_GLYPH)
        # Locked → veto returns FALSE so xterm drops the key (no PTY write).
        self.assertFalse(a["keyProcessed"])
        # Shared flag flips so the paste handler also suppresses paste.
        self.assertTrue(a["lockedFlag"])

    def test_second_click_unlocks(self):
        u = self.out["afterUnlock"]
        self.assertEqual(u["ariaPressed"], "false")
        self.assertFalse(u["hasLockedClass"])
        self.assertEqual(u["textContent"], UNLOCK_GLYPH)
        self.assertTrue(u["keyProcessed"])
        self.assertFalse(u["lockedFlag"])

    def test_toggle_persists_state_to_localstorage(self):
        # Fresh load with empty storage writes nothing until the first
        # toggle; each toggle then persists the new state so it survives
        # a page reload.
        self.assertIsNone(self.out["initial"]["stored"])
        self.assertEqual(self.out["afterLock"]["stored"], "1")
        self.assertEqual(self.out["afterUnlock"]["stored"], "0")


class LockPointerGuardTests(unittest.TestCase):
    """The lock swallows POINTER input too, not just keystrokes.

    attachCustomKeyEventHandler only vetoes KEY events; mouse / wheel /
    touch are a separate path xterm.js turns into selection, focus,
    scrollback, and mouse-tracking escape sequences bound for the live
    session. The guard binds capture-phase listeners on the terminal
    element that swallow the full pointer set while locked and pass through
    untouched while unlocked.
    """

    # Every event the guard must cover — the full mouse/wheel set plus the
    # unified-pointer and touch equivalents (the kiosk client is touch).
    EXPECTED_POINTER_EVENTS = {
        "mousedown", "mouseup", "mousemove", "click", "dblclick",
        "contextmenu", "wheel",
        "pointerdown", "pointerup", "pointermove",
        "touchstart", "touchmove", "touchend",
    }

    @classmethod
    def setUpClass(cls):
        cls.out = _run_node_harness()

    def test_guard_covers_the_full_pointer_set(self):
        self.assertEqual(
            set(self.out["pointerGuardEvents"]), self.EXPECTED_POINTER_EVENTS
        )

    def test_guard_is_installed_on_the_terminal_element(self):
        # A listener must exist for every guarded event — a missing one is
        # a silent leak of that event type to the terminal.
        for name, r in self.out["pointerLocked"].items():
            self.assertTrue(r["installed"], f"{name} guard not installed")

    def test_locked_swallows_all_pointer_events(self):
        # Locked → each event is stopped (never reaches xterm) and, when
        # cancelable, its default is prevented.
        for name, r in self.out["pointerLocked"].items():
            self.assertTrue(r["stopped"], f"{name} not stopped while locked")
            self.assertTrue(
                r["prevented"], f"{name} default not prevented while locked"
            )

    def test_unlocked_is_a_pure_passthrough(self):
        # Unlocked → stock ttyd behavior: the guard touches nothing, so no
        # stopPropagation and no preventDefault on any pointer event.
        for name, r in self.out["pointerUnlocked"].items():
            self.assertTrue(r["installed"], f"{name} guard not installed")
            self.assertFalse(
                r["stopped"], f"{name} stopped while UNLOCKED (regression)"
            )
            self.assertFalse(
                r["prevented"],
                f"{name} prevented while UNLOCKED (regression)",
            )


class LockToggleAutoLockTests(unittest.TestCase):
    """Default (300s) build: the lock engages itself after idle time."""

    @classmethod
    def setUpClass(cls):
        cls.out = _run_node_harness()

    def test_default_window_is_the_documented_default(self):
        self.assertEqual(self.out["autoLockSeconds"], 300)
        self.assertEqual(DEFAULT_AUTOLOCK_SECONDS, 300)

    def test_activity_wiring_uses_deliberate_input_events(self):
        wired = set(self.out["activityEvents"])
        self.assertEqual(wired, EXPECTED_ACTIVITY_EVENTS)
        # Terminal OUTPUT must never count as activity — a chatty session
        # would otherwise never auto-lock.
        for never in ("message", "data", "render", "mousemove"):
            self.assertNotIn(never, wired)

    def test_unlocked_tooltip_advertises_the_idle_window(self):
        self.assertIn("auto-locks after 5 min idle",
                      self.out["initial"]["title"])

    def test_activity_before_the_deadline_resets_the_countdown(self):
        # One second shy of the deadline: still unlocked.
        self.assertFalse(self.out["beforeDeadline"]["hasLockedClass"])
        # A keystroke restamps the clock, so another near-deadline idle
        # stretch still does not trip the lock.
        r = self.out["afterActivityReset"]
        self.assertFalse(r["hasLockedClass"], "idle timer was not reset")
        self.assertTrue(r["keyProcessed"])
        self.assertFalse(r["lockedFlag"])

    def test_idle_past_the_deadline_auto_locks(self):
        a = self.out["afterIdle"]
        self.assertEqual(a["ariaPressed"], "true")
        self.assertTrue(a["hasLockedClass"])
        self.assertEqual(a["textContent"], LOCK_GLYPH)
        # Same suppression as a manual lock: keys vetoed, paste flag set.
        self.assertFalse(a["keyProcessed"])
        self.assertTrue(a["lockedFlag"])
        self.assertIn("AUTO-LOCKED", a["title"])

    def test_auto_lock_persists_like_a_manual_lock(self):
        # Written through the same setLocked() path, so a reload while
        # auto-locked comes back locked.
        self.assertEqual(self.out["afterIdle"]["stored"], "1")

    def test_further_idling_does_not_disturb_an_engaged_lock(self):
        long_idle = self.out["afterLongIdle"]
        self.assertTrue(long_idle["hasLockedClass"])
        self.assertEqual(long_idle["stored"], "1")


class LockToggleAutoLockDisabledTests(unittest.TestCase):
    """A build with the window set to 0 never auto-locks."""

    @classmethod
    def setUpClass(cls):
        cls.out = _run_node_harness(autolock=0)

    def test_window_is_zero(self):
        self.assertEqual(self.out["autoLockSeconds"], 0)

    def test_no_activity_listeners_are_registered(self):
        # Disabled means no timer AND no listeners — zero overhead, not
        # a timer that fires and does nothing.
        self.assertEqual(self.out["activityEvents"], [])

    def test_never_locks_no_matter_how_long_idle(self):
        for phase in ("beforeDeadline", "afterActivityReset",
                      "afterIdle", "afterLongIdle"):
            snap = self.out[phase]
            self.assertFalse(snap["hasLockedClass"], f"{phase} auto-locked")
            self.assertTrue(snap["keyProcessed"], f"{phase} suppressed keys")

    def test_manual_toggle_still_works(self):
        # Disabling auto-lock must not disable the padlock button.
        self.assertTrue(self.out["afterLock"]["hasLockedClass"])
        self.assertEqual(self.out["afterLock"]["stored"], "1")
        self.assertFalse(self.out["afterUnlock"]["hasLockedClass"])
        # And the tooltip drops the idle hint entirely.
        self.assertNotIn("auto-locks", self.out["initial"]["title"])


class LockToggleQueryOverrideTests(unittest.TestCase):
    """`?autolock=<seconds>` overrides the baked-in window per tab."""

    @classmethod
    def setUpClass(cls):
        cls.out = _run_node_harness(autolock=300, search="?autolock=45")

    def test_query_param_wins_over_the_build_default(self):
        self.assertEqual(self.out["autoLockSeconds"], 45)
        self.assertIn("auto-locks after 45 s idle",
                      self.out["initial"]["title"])

    def test_locks_on_the_overridden_deadline(self):
        self.assertFalse(self.out["afterActivityReset"]["hasLockedClass"])
        self.assertTrue(self.out["afterIdle"]["hasLockedClass"])
        self.assertFalse(self.out["afterIdle"]["keyProcessed"])


class LockToggleQueryDisableTests(unittest.TestCase):
    """`?autolock=0` disables auto-lock for one tab without a rebuild."""

    @classmethod
    def setUpClass(cls):
        cls.out = _run_node_harness(autolock=300, search="?autolock=0")

    def test_override_to_zero_disables(self):
        self.assertEqual(self.out["autoLockSeconds"], 0)
        self.assertEqual(self.out["activityEvents"], [])

    def test_never_auto_locks(self):
        self.assertFalse(self.out["afterIdle"]["hasLockedClass"])
        self.assertFalse(self.out["afterLongIdle"]["hasLockedClass"])


class LockToggleRestoreTests(unittest.TestCase):
    """Loading with a persisted locked state restores the lock on init."""

    @classmethod
    def setUpClass(cls):
        # localStorage already says locked ('1') — simulates a reload
        # after the operator locked the terminal in a prior page load.
        cls.out = _run_node_harness({"cw-terminal-locked": "1"})

    def test_restores_locked_on_load(self):
        i = self.out["initial"]
        # Button reflects the restored locked state immediately on load.
        self.assertEqual(i["ariaPressed"], "true")
        self.assertTrue(i["hasLockedClass"], "restored lock cue missing")
        self.assertEqual(i["textContent"], LOCK_GLYPH)
        # The key guard is seeded locked, so keystrokes are suppressed
        # from the very first paint (veto returns false).
        self.assertFalse(i["keyProcessed"])
        # Shared flag the paste handler reads is restored too.
        self.assertTrue(i["lockedFlag"])
        self.assertEqual(i["stored"], "1")

    def test_click_unlocks_and_persists(self):
        # Toggling from the restored locked state unlocks and persists '0'.
        a = self.out["afterLock"]
        self.assertEqual(a["ariaPressed"], "false")
        self.assertFalse(a["hasLockedClass"])
        self.assertEqual(a["textContent"], UNLOCK_GLYPH)
        self.assertTrue(a["keyProcessed"])
        self.assertFalse(a["lockedFlag"])
        self.assertEqual(a["stored"], "0")

    def test_idle_ticks_do_not_disturb_a_standing_manual_lock(self):
        # This run ends its click cycle LOCKED (restored locked → unlock
        # → lock), so every idle phase must leave that manual lock exactly
        # as it is: no double-lock, no relabelling it as an auto-lock.
        for phase in ("afterActivityReset", "afterIdle", "afterLongIdle"):
            snap = self.out[phase]
            self.assertTrue(snap["hasLockedClass"], phase)
            self.assertEqual(snap["stored"], "1", phase)
            self.assertNotIn("AUTO-LOCKED", snap["title"], phase)


MINIMAL_HTML = "<html><head><title>t</title></head><body></body></html>\n"


def _run_injector(env_value: str | None) -> tuple[subprocess.CompletedProcess,
                                                  str]:
    """Run inject-autodark.py end-to-end with TTYD_AUTOLOCK_SECONDS set."""
    env = dict(os.environ)
    env.pop(AUTOLOCK_ENV_VAR, None)
    if env_value is not None:
        env[AUTOLOCK_ENV_VAR] = env_value
    with tempfile.TemporaryDirectory() as td:
        src = os.path.join(td, "in.html")
        dst = os.path.join(td, "out.html")
        with open(src, "w", encoding="utf-8") as f:
            f.write(MINIMAL_HTML)
        proc = subprocess.run(
            [sys.executable, INJECT_SCRIPT, src, dst],
            capture_output=True,
            text=True,
            timeout=60,
            env=env,
        )
        out = ""
        if os.path.exists(dst):
            with open(dst, "r", encoding="utf-8") as f:
                out = f.read()
    return proc, out


class InjectorAutoLockConfigTests(unittest.TestCase):
    """TTYD_AUTOLOCK_SECONDS → the literal baked into the injected JS."""

    def test_default_is_300_seconds(self):
        proc, out = _run_injector(None)
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertIn("var AUTO_LOCK_SECONDS = 300;", out)

    def test_env_var_overrides_the_default(self):
        proc, out = _run_injector("900")
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertIn("var AUTO_LOCK_SECONDS = 900;", out)
        self.assertNotIn("var AUTO_LOCK_SECONDS = 300;", out)

    def test_zero_disables_but_keeps_the_manual_toggle(self):
        proc, out = _run_injector("0")
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertIn("var AUTO_LOCK_SECONDS = 0;", out)
        # The padlock button itself must still ship.
        self.assertIn("cw-lock-toggle", out)
        self.assertIn("lock-toggle-injected", out)

    def test_empty_value_falls_back_to_the_default(self):
        proc, out = _run_injector("")
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertIn("var AUTO_LOCK_SECONDS = 300;", out)

    def test_non_integer_value_fails_the_build(self):
        proc, _ = _run_injector("abc")
        self.assertNotEqual(proc.returncode, 0)
        self.assertIn(AUTOLOCK_ENV_VAR, proc.stderr)

    def test_negative_value_fails_the_build(self):
        proc, _ = _run_injector("-5")
        self.assertNotEqual(proc.returncode, 0)
        self.assertIn(AUTOLOCK_ENV_VAR, proc.stderr)

    def test_placeholder_never_survives_into_the_output(self):
        # A surviving placeholder is a JS syntax error that would take
        # the whole toggle down at runtime.
        proc, out = _run_injector("120")
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertNotIn(AUTOLOCK_PLACEHOLDER, out)


if __name__ == "__main__":
    unittest.main(verbosity=2)
