#!/usr/bin/env python3
"""Tests for PASTE_EVENT_HANDLER_JS in inject-autodark.py.

The handler is loaded into a Node v8 context with a tiny DOM stub +
a fake `window.term` (so sendToTerminal succeeds), then exercised
with three synthetic paste events covering:

  1. text-only clipboard (`types = ['text/plain']`) — handler must
     NOT call preventDefault / stopImmediatePropagation and must NOT
     hit the upload endpoint.
  2. image-only clipboard (`types = ['image/png']`) — handler must
     preventDefault, attempt the async clipboard.read() + upload, and
     fire \x16.
  3. mixed clipboard (`types = ['image/png', 'text/plain']`) — image
     path wins, same as case 2.

The test stubs navigator.clipboard, fetch, and document.addEventListener
just enough to capture the handler registered by the IIFE, then invokes
it directly with the synthetic events.

Run: `python3 tests/test_paste_handler.py` from this directory, or
`make test-ttyd-paste-handler` from the repo root.
"""

import ast
import json
import os
import re
import subprocess
import sys
import unittest


HERE = os.path.dirname(os.path.abspath(__file__))
INJECT_SCRIPT = os.path.join(os.path.dirname(HERE), "inject-autodark.py")


def _extract_paste_handler_js() -> str:
    """Pull PASTE_EVENT_HANDLER_JS out of inject-autodark.py.

    We parse the file as Python AST and grab the value of the
    PASTE_EVENT_HANDLER_JS module-level assignment. That string still
    contains the `<script>` tags and the doubly-escaped `\\\\x16` byte
    that gets unescaped to `\\x16` when injected into the HTML (since
    the surrounding f-strings aren't involved here — this is a plain
    string literal — the `\\\\x16` in source is `\\x16` at runtime, and
    that's what the inline script sees).
    """
    with open(INJECT_SCRIPT, "r", encoding="utf-8") as f:
        tree = ast.parse(f.read())
    for node in ast.walk(tree):
        if isinstance(node, ast.Assign):
            for target in node.targets:
                if (
                    isinstance(target, ast.Name)
                    and target.id == "PASTE_EVENT_HANDLER_JS"
                    and isinstance(node.value, ast.Constant)
                ):
                    return node.value.value
    raise RuntimeError("PASTE_EVENT_HANDLER_JS not found in inject-autodark.py")


def _strip_script_tags(s: str) -> str:
    # Remove the <script id="..."> and </script> wrappers so we can
    # eval the body directly in node.
    s = re.sub(r"^\s*<script[^>]*>", "", s)
    s = re.sub(r"</script>\s*$", "", s)
    return s


# Harness JS: injects DOM/clipboard/fetch stubs, loads the handler IIFE
# (which registers `paste` via document.addEventListener), then captures
# the registered handler and invokes it with each synthetic event.
HARNESS_TEMPLATE = r"""
'use strict';

const results = { calls: [] };

// --- DOM stubs ------------------------------------------------------
let registeredPasteHandler = null;
const documentStub = {
    addEventListener: function(name, fn /*, capture */) {
        if (name === 'paste') registeredPasteHandler = fn;
    },
    body: {
        appendChild: function() {},
    },
    getElementById: function() { return null; },
    createElement: function() {
        return {
            id: '', textContent: '',
            classList: { toggle: function(){}, add: function(){}, remove: function(){} },
        };
    },
    readyState: 'complete',
};
global.document = documentStub;

// --- Clipboard / fetch stubs ---------------------------------------
let imageBlobToServe = null;  // controlled per-test via __setImageBlob

const clipboardStub = {
    read: async function() {
        if (!imageBlobToServe) return [];
        results.calls.push('clipboard.read');
        return [{
            types: ['image/png'],
            getType: async function(t) {
                results.calls.push('clipboard.getType:' + t);
                return imageBlobToServe;
            },
        }];
    },
};
// Node 22+ has `navigator` as a non-writable getter on globalThis;
// override it via defineProperty so the handler sees our stub.
Object.defineProperty(global, 'navigator', {
    value: { clipboard: clipboardStub },
    writable: true,
    configurable: true,
});

global.fetch = async function(url, opts) {
    results.calls.push('fetch:' + url);
    return {
        ok: true,
        status: 200,
        json: async function() { return {}; },
    };
};

// --- window.term stub so sendToTerminal succeeds -------------------
global.window = {
    term: {
        _core: {
            coreService: {
                triggerDataEvent: function(data) {
                    results.calls.push('term.data:' + JSON.stringify(data));
                },
            },
        },
    },
    matchMedia: function() {
        return { matches: false, addEventListener: function(){}, addListener: function(){} };
    },
};
global.setTimeout = function(fn /*, ms */) { return 0; };
global.clearTimeout = function() {};
global.console.log = function() {};  // silence dbg() output

// --- Array.from polyfill is built into Node, nothing to do ---------

// --- Load the handler IIFE -----------------------------------------
__HANDLER_BODY__

if (!registeredPasteHandler) {
    console.error(JSON.stringify({ error: 'paste handler not registered' }));
    process.exit(2);
}

// --- Synthetic paste-event factory ---------------------------------
function makeEvent(types) {
    const ev = {
        clipboardData: {
            types: types,
        },
        preventDefaultCalled: false,
        stopImmediatePropagationCalled: false,
        stopPropagationCalled: false,
        preventDefault: function() { this.preventDefaultCalled = true; },
        stopImmediatePropagation: function() { this.stopImmediatePropagationCalled = true; },
        stopPropagation: function() { this.stopPropagationCalled = true; },
    };
    return ev;
}

async function runCase(label, types, blob) {
    imageBlobToServe = blob;
    results.calls = [];
    const ev = makeEvent(types);
    await registeredPasteHandler(ev);
    return {
        label: label,
        preventDefaultCalled: ev.preventDefaultCalled,
        stopImmediatePropagationCalled: ev.stopImmediatePropagationCalled,
        stopPropagationCalled: ev.stopPropagationCalled,
        calls: results.calls.slice(),
    };
}

(async function() {
    const out = [];
    out.push(await runCase('text-only', ['text/plain'], null));
    out.push(await runCase('image-only', ['image/png'], { size: 1024, type: 'image/png' }));
    out.push(await runCase('mixed-image-and-text',
                           ['image/png', 'text/plain'],
                           { size: 1024, type: 'image/png' }));
    out.push(await runCase('image-jpeg', ['image/jpeg'],
                           { size: 2048, type: 'image/jpeg' }));
    out.push(await runCase('empty-types', [], null));
    process.stdout.write(JSON.stringify(out) + '\n');
    process.exit(0);
})().catch(function(err) {
    console.error('harness error:', err && err.stack || err);
    process.exit(3);
});
"""


def _run_node_harness() -> list:
    js = _strip_script_tags(_extract_paste_handler_js())
    harness = HARNESS_TEMPLATE.replace("__HANDLER_BODY__", js)
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


class PasteHandlerTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.results = _run_node_harness()
        cls.by_label = {r["label"]: r for r in cls.results}

    def test_text_only_does_not_intercept(self):
        r = self.by_label["text-only"]
        self.assertFalse(
            r["preventDefaultCalled"],
            "text-only paste must NOT preventDefault — native paste must flow",
        )
        self.assertFalse(r["stopImmediatePropagationCalled"])
        # No upload, no clipboard.read, no \x16
        for c in r["calls"]:
            self.assertFalse(
                c.startswith("fetch:") or c.startswith("clipboard.read")
                or c.startswith("term.data:"),
                f"unexpected side-effect on text-only paste: {c}",
            )

    def test_image_only_intercepts_and_uploads(self):
        r = self.by_label["image-only"]
        self.assertTrue(r["preventDefaultCalled"])
        self.assertTrue(r["stopImmediatePropagationCalled"])
        self.assertIn("clipboard.read", r["calls"])
        self.assertTrue(
            any(c.startswith("fetch:/clipboard-upload") for c in r["calls"]),
            f"expected POST to /clipboard-upload, got: {r['calls']}",
        )
        self.assertIn(
            'term.data:"\\u0016"', r["calls"],
            f"expected \\x16 byte fired to terminal, got: {r['calls']}",
        )

    def test_mixed_image_and_text_takes_image_path(self):
        r = self.by_label["mixed-image-and-text"]
        self.assertTrue(r["preventDefaultCalled"])
        self.assertTrue(r["stopImmediatePropagationCalled"])
        self.assertIn("clipboard.read", r["calls"])
        self.assertTrue(
            any(c.startswith("fetch:/clipboard-upload") for c in r["calls"])
        )
        self.assertIn('term.data:"\\u0016"', r["calls"])

    def test_image_jpeg_intercepts(self):
        # image/jpeg also matches `image/*` — covers non-PNG MIMEs.
        r = self.by_label["image-jpeg"]
        self.assertTrue(r["preventDefaultCalled"])
        self.assertTrue(r["stopImmediatePropagationCalled"])

    def test_empty_types_does_not_intercept(self):
        r = self.by_label["empty-types"]
        self.assertFalse(r["preventDefaultCalled"])
        self.assertFalse(r["stopImmediatePropagationCalled"])


if __name__ == "__main__":
    unittest.main(verbosity=2)
