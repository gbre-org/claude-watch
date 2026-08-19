#!/usr/bin/env python3
"""Tests for LOCK_TOGGLE_JS in inject-autodark.py.

The lock toggle injects a subtle top-right padlock button into ttyd's
bundled HTML. When ACTIVE it suppresses keystrokes to the terminal by
returning `false` from xterm.js's `attachCustomKeyEventHandler` hook, so
no key is written to the PTY / sent over ttyd's input WebSocket. When
inactive ttyd behaves exactly as stock.

We load the LOCK_TOGGLE_JS body into a Node v8 context with a tiny DOM
stub + a fake `window.term` (whose `attachCustomKeyEventHandler` captures
the veto callback), run the IIFE, then:

  1. assert the button was created (id `cw-lock-toggle`), starts UNLOCKED
     (open-padlock glyph, aria-pressed=false, no `cw-locked` class), and
     the key-veto callback returns true (xterm processes keys normally);
  2. invoke the captured click handler → LOCKED: key-veto returns false
     (keystroke suppressed), `window.__cwTerminalLocked` flips true, the
     glyph switches to the closed padlock, aria-pressed=true, and the
     `cw-locked` class is applied (the visual cue);
  3. invoke the click handler again → back to UNLOCKED, key-veto true.

Run: `python3 tests/test_lock_toggle.py` from this directory, or
`make test-ttyd-lock-toggle` from the repo root.
"""

import ast
import json
import os
import re
import subprocess
import unittest


HERE = os.path.dirname(os.path.abspath(__file__))
INJECT_SCRIPT = os.path.join(os.path.dirname(HERE), "inject-autodark.py")


def _extract_js_const(name: str) -> str:
    """Pull a module-level string constant out of inject-autodark.py."""
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


def _strip_script_tags(s: str) -> str:
    s = re.sub(r"^\s*<script[^>]*>", "", s)
    s = re.sub(r"</script>\s*$", "", s)
    return s


# Harness JS: stub DOM + window.term, load the IIFE, capture the button's
# click handler and xterm's veto callback, then snapshot state before /
# after two toggles.
HARNESS_TEMPLATE = r"""
'use strict';

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
global.document = {
    readyState: 'complete',
    addEventListener: function() {},
    createElement: function(tag) { return makeEl(); },
    body: {
        appendChild: function(el) { appendedButton = el; },
    },
};

// --- window.term stub: capture the veto callback -------------------
var keyVeto = null;
global.window = {
    term: {
        attachCustomKeyEventHandler: function(fn) { keyVeto = fn; },
    },
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

// Poll is a no-op in the harness (term is present at load, so init()
// runs synchronously and attaches everything on first call).
global.setInterval = function() { return 0; };
global.clearInterval = function() {};

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

var out = { initial: null, afterLock: null, afterUnlock: null };
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

process.stdout.write(JSON.stringify(out) + '\n');
process.exit(0);
"""

# Open / closed padlock glyphs the JS emits (surrogate pairs decoded to
# the actual astral code points for comparison against the JSON output).
UNLOCK_GLYPH = "\U0001F513"  # open padlock
LOCK_GLYPH = "\U0001F512"    # closed padlock


def _run_node_harness(preseed: dict | None = None) -> dict:
    js = _strip_script_tags(_extract_js_const("LOCK_TOGGLE_JS"))
    harness = HARNESS_TEMPLATE.replace("__HANDLER_BODY__", js)
    harness = harness.replace("__PRESEED__", json.dumps(preseed or {}))
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


if __name__ == "__main__":
    unittest.main(verbosity=2)
