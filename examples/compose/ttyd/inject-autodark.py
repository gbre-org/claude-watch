#!/usr/bin/env python3
# Inject autodark CSS + xterm.js theme-swap JS into ttyd's bundled HTML.
#
# Why: ttyd 1.7.7 ships a single self-contained index.html (~730 KB) that
# inlines all CSS + the xterm.js JS bundle. The default body background
# is white. xterm.js itself respects the theme= ttyd command-line option,
# but the page chrome (everything outside the terminal renderer
# rectangle) stays white. macOS Safari in system dark mode then shows a
# white frame around a dark terminal — visually broken.
#
# This script:
#   1. Reads the upstream HTML (captured at build time via a one-shot
#      ttyd run + wget — see Dockerfile).
#   2. Injects a <style> block that uses @media (prefers-color-scheme)
#      to recolor the body / html background using the Solarized palette
#      (Ethan Schoonover's public-domain base03 / base3). This handles
#      the page chrome around the xterm.js terminal — margins, scrollbar
#      gutter, area visible during initial load.
#   3. Injects a <script> (applyAutodarkTheme) that ALSO flips the
#      xterm.js Terminal instance's theme (window.term.options.theme)
#      to match the system color-scheme. This is required because the
#      xterm.js canvas renderer paints its OWN background color over
#      the body chrome — a CSS-only flip leaves the visible terminal
#      area unchanged even though getComputedStyle on the body reports
#      the right color (this was the v7 bug caught by the workbot
#      browser-side probe). The script:
#        - reads prefers-color-scheme on initial load,
#        - reapplies on a setInterval poll because ttyd's WebSocket
#          sends a SET_PREFERENCES message AFTER the initial connect
#          that overwrites whatever theme the page set (without the
#          reapply, the terminal flashes to its compile-time default
#          a second after load),
#        - listens for matchMedia change events so live OS theme flips
#          propagate without a page reload.
#   4. Injects a keydown handler (PASTE_INTERCEPT_JS) that stops
#      propagation on Cmd+V / Ctrl+V so xterm.js's own keydown handler
#      doesn't double-fire alongside our paste event listener. It does
#      NOT preventDefault — the browser's default action (firing the
#      `paste` event) must run so step 5's handler gets clipboard data.
#   5. Injects a document-level `paste` event listener
#      (PASTE_EVENT_HANDLER_JS) that branches synchronously on
#      `e.clipboardData.types`:
#        - If ANY image/* MIME is present (image/png, image/jpeg,
#          image/webp, image/gif, …), preventDefault +
#          stopImmediatePropagation IMMEDIATELY (sync), then asynchronously
#          read the image via navigator.clipboard.read() (paste keystroke
#          satisfies the user-gesture requirement; the sync .items path
#          is unreliable for macOS Cmd+Shift+4 screenshots), POST the
#          blob to the clipboard-upload sidecar at /clipboard-upload,
#          and on a 200 response fire \x16 (chat:imagePaste keybinding)
#          so the in-container xclip shim reads the freshly-written
#          PNG. Toast for success / failure.
#        - If NO image MIME is present, the handler returns immediately
#          WITHOUT preventDefault — text falls through to xterm.js's
#          native paste handling, which streams the bytes into the PTY.
#          This is what makes Cmd+V work for both images AND text in
#          one keybinding (Andrew, 2026-05-20).
#      Ctrl+Shift+V remains the xterm.js Clipboard-addon default text
#      paste; this Cmd+V unification means it's now redundant for text,
#      but kept available as a fallback.
#
# Output is written in place: index.html is overwritten.

import os
import re
import sys

# Solarized palette (Ethan Schoonover, public domain).
# base03 = darkest background (dark mode); base3 = lightest (light mode).
DARK_BG = "#002b36"   # base03
DARK_FG = "#93a1a1"   # base1
LIGHT_BG = "#fdf6e3"  # base3
LIGHT_FG = "#586e75"  # base01

# CSS: drives the page chrome (everything outside xterm.js's canvas).
# We default the page to dark and let prefers-color-scheme: light flip
# it. xterm.js paints its own region using the theme= JSON; this CSS
# only controls the area around it.
CSS = f"""<style id="autodark-injected">
/* claude-ttyd autodark: matches page chrome to system color-scheme.
 * The xterm.js renderer paints its own rectangle; this CSS is for the
 * area outside (window background visible during initial load, around
 * the canvas while resizing, between rows, etc.). */
html, body {{
    background-color: {DARK_BG};
    color: {DARK_FG};
    margin: 0;
    padding: 0;
}}
@media (prefers-color-scheme: light) {{
    html, body {{
        background-color: {LIGHT_BG};
        color: {LIGHT_FG};
    }}
}}
</style>
"""

# Solarized-light xterm.js theme as a JS object literal. The Dockerfile
# already passes a -t theme=… for Solarized-dark via ttyd CLI flags;
# this script wires up the LIGHT side and the runtime swap, since
# ttyd's -t flag only supports one theme.
LIGHT_THEME_JSON = """{
    background:"#fdf6e3",foreground:"#586e75",cursor:"#586e75",
    cursorAccent:"#fdf6e3",
    selectionBackground:"#eee8d5",selectionForeground:"#073642",
    selectionInactiveBackground:"#eee8d5",
    black:"#073642",red:"#dc322f",green:"#859900",yellow:"#b58900",
    blue:"#268bd2",magenta:"#d33682",cyan:"#2aa198",white:"#eee8d5",
    brightBlack:"#002b36",brightRed:"#cb4b16",brightGreen:"#586e75",
    brightYellow:"#657b83",brightBlue:"#839496",brightMagenta:"#6c71c4",
    brightCyan:"#93a1a1",brightWhite:"#fdf6e3"
}"""

DARK_THEME_JSON = """{
    background:"#002b36",foreground:"#93a1a1",cursor:"#93a1a1",
    cursorAccent:"#002b36",
    selectionBackground:"#073642",selectionForeground:"#eee8d5",
    selectionInactiveBackground:"#073642",
    black:"#073642",red:"#dc322f",green:"#859900",yellow:"#b58900",
    blue:"#268bd2",magenta:"#d33682",cyan:"#2aa198",white:"#eee8d5",
    brightBlack:"#002b36",brightRed:"#cb4b16",brightGreen:"#586e75",
    brightYellow:"#657b83",brightBlue:"#839496",brightMagenta:"#6c71c4",
    brightCyan:"#93a1a1",brightWhite:"#fdf6e3"
}"""

# JS: walks the page for the xterm.js Terminal instance and reapplies
# the theme matching prefers-color-scheme. Runs on:
#   1. initial DOMContentLoaded (catches first paint),
#   2. setInterval(2s) — race-condition reapply (see comment below),
#   3. matchMedia change listener — instant swap if the user toggles
#      system dark mode while the tab is open.
#
# RACE NOTE: ttyd's WebSocket sends a SET_PREFERENCES message AFTER
# the initial connect handshake. The xterm.js client merges that into
# its options and re-paints, OVERWRITING whatever theme we set on
# initial load. The setInterval poll defends against that — every
# couple seconds we re-stamp the correct theme. Cost is negligible
# (one object assignment + a single repaint trigger).
# This mirrors a historical fix used in the maintainer's homelab
# nginx sub_filter injection for the same upstream xterm.js race.
JS = f"""<script id="autodark-injected">
(function() {{
    'use strict';
    var SOLARIZED_LIGHT = {LIGHT_THEME_JSON};
    var SOLARIZED_DARK = {DARK_THEME_JSON};

    function preferredTheme() {{
        try {{
            return window.matchMedia('(prefers-color-scheme: light)').matches
                ? SOLARIZED_LIGHT : SOLARIZED_DARK;
        }} catch (e) {{ return SOLARIZED_DARK; }}
    }}

    function findTerm() {{
        // ttyd 1.7.7's bundled JS contains the literal assignment
        // `window.term = t` where `t` is the xterm.js Terminal
        // instance (verified by grepping the served HTML). That's the
        // canonical accessor; we use it directly rather than trying
        // to dig through `.xterm` DOM nodes (the prod bundle does NOT
        // stash the instance on the DOM element).
        //
        // Both xterm.js v4 (setOption) and v5 (options.theme setter)
        // are supported by applyAutodarkTheme below — DO NOT gate this lookup
        // on either API existing, because v5 dropped setOption and an
        // early gate would return null on every tick.
        if (window.term) return window.term;
        return null;
    }}

    function applyAutodarkTheme() {{
        var theme = preferredTheme();
        // Body chrome (visible during initial load, around the
        // canvas while resizing, and on margin/scrollbar regions).
        try {{
            document.body.style.backgroundColor = theme.background;
            document.documentElement.style.backgroundColor = theme.background;
        }} catch (e) {{ /* DOM may not be ready yet */ }}
        // xterm.js canvas — without this, the terminal renderer
        // paints its own background OVER the body chrome regardless
        // of CSS, so a CSS-only flip leaves the visible terminal
        // area unchanged. THIS is what was missing in the original
        // injection: the CSS rule was firing (workbot confirmed
        // getComputedStyle returned base3) but the canvas covered
        // it.
        var t = findTerm();
        if (!t) return false;
        try {{
            // xterm.js v5: options.theme setter triggers a repaint.
            // v4 fallback: setOption('theme', …). Try v5 first since
            // ttyd 1.7.7 bundles v5.x; setOption was removed in v5.
            if (t.options) {{
                t.options.theme = theme;
            }} else if (typeof t.setOption === 'function') {{
                t.setOption('theme', theme);
            }}
            return true;
        }} catch (e) {{ return false; }}
    }}

    // 1. Initial paint — apply as soon as the DOM has the xterm node.
    if (document.readyState === 'loading') {{
        document.addEventListener('DOMContentLoaded', applyAutodarkTheme);
    }} else {{
        applyAutodarkTheme();
    }}

    // 2. Race-condition reapply. ttyd's WS SET_PREFERENCES arrives
    //    after WS open and overwrites our theme; poll every 2s to
    //    restamp. Negligible cost; survives all xterm.js version skews.
    setInterval(applyAutodarkTheme, 2000);

    // 3. Live swap when the user toggles system dark mode.
    try {{
        var mql = window.matchMedia('(prefers-color-scheme: light)');
        var onChange = function() {{ applyAutodarkTheme(); }};
        if (mql.addEventListener) {{
            mql.addEventListener('change', onChange);
        }} else if (mql.addListener) {{
            mql.addListener(onChange);
        }}
    }} catch (e) {{ /* noop */ }}
}})();
</script>
"""

# JS: stop propagation on Cmd+V (Mac) / Ctrl+V (non-Mac) keydown so
# xterm.js's own keydown handler doesn't run alongside our paste
# listener. We do NOT preventDefault — the browser's default action is
# to fire the subsequent `paste` event, which is exactly what we need
# PASTE_EVENT_HANDLER_JS to receive so it can branch on
# `e.clipboardData.types`. Killing the keydown's default would also
# suppress the paste event on some Safari / Chromium builds, breaking
# both image AND text paste.
#
# Why no \x16 here:
#   Previous revisions sent \x16 synchronously on keydown. That fires
#   BEFORE the paste-event handler's async navigator.clipboard.read()
#   resolves and uploads the PNG, so the in-container xclip shim races
#   against the upload and reads stale bytes from a previous paste (or
#   no bytes at all). The paste-event handler is the SOLE source of
#   \x16 and only fires AFTER the upload completes.
#
# Cmd+V is now unified — image-containing clipboards go through the
# async upload path; text-only clipboards fall through to xterm.js's
# native paste so the bytes stream into the PTY directly. See the
# PASTE_EVENT_HANDLER_JS comment for the branching logic.
PASTE_INTERCEPT_JS = """<script id="paste-intercept-injected">
(function() {
    'use strict';

    var isMac = /Mac|iPhone|iPad|iPod/.test(navigator.platform);

    // Suppress the browser's default Cmd+V / Ctrl+V handling. We do
    // NOT preventDefault on the paste event itself here — that's the
    // job of PASTE_EVENT_HANDLER_JS, which needs the paste event to
    // fire so it can read clipboardData / navigator.clipboard.read().
    //
    // useCapture=true fires before xterm.js's own keydown handler.
    document.addEventListener('keydown', function(e) {
        var keyIsV = (e.key === 'v' || e.key === 'V' || e.code === 'KeyV');
        var isPaste = isMac
            ? (e.metaKey && !e.ctrlKey && !e.shiftKey && keyIsV)
            : (e.ctrlKey && !e.metaKey && !e.shiftKey && keyIsV);
        if (isPaste) {
            // stopPropagation only — do NOT preventDefault. Calling
            // preventDefault on keydown for Cmd+V in some Safari /
            // Chromium builds also suppresses the paste event, which
            // breaks the async upload path. Letting the keydown's
            // default action proceed is fine because xterm.js's
            // textarea overlay is empty / hidden; the user-visible
            // effect is purely the paste event firing.
            e.stopPropagation();
        }
    }, true);  // useCapture=true to fire before xterm.js's own handler
})();
</script>
"""

# Toast styles. Previously bundled with the floating "Paste image"
# button (PASTE_IMAGE_BUTTON_JS) — the button is gone (Andrew, 2026-05-20:
# Cmd+V is the only supported path now) but the toast is still surfaced
# by PASTE_EVENT_HANDLER_JS for success / upload-failure feedback. The
# id `cw-paste-image-toast` is unchanged so the styling carries over.
PASTE_TOAST_STYLE = """<style id="paste-toast-injected-style">
#cw-paste-image-toast {
    position: fixed;
    bottom: 16px;
    right: 16px;
    z-index: 9999;
    padding: 8px 12px;
    font: 12px/1.3 -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
    background: rgba(7, 54, 66, 0.92);
    color: #eee8d5;
    border-radius: 4px;
    max-width: 320px;
    box-shadow: 0 2px 8px rgba(0, 0, 0, 0.3);
    opacity: 0;
    transition: opacity 0.2s ease;
    pointer-events: none;
}
#cw-paste-image-toast.visible { opacity: 1; }
#cw-paste-image-toast.error { background: rgba(220, 50, 47, 0.92); }
</style>
"""

# Document-level `paste` event handler.
#
# Cmd+V (Mac) / Ctrl+V (non-Mac) MUST work for both images AND text in
# a single keybinding (Andrew, 2026-05-20). The handler branches
# SYNCHRONOUSLY on `e.clipboardData.types`:
#
#   - `types` includes an `image/*` MIME → preventDefault +
#     stopImmediatePropagation immediately (sync), then async read the
#     image via navigator.clipboard.read() (paste keystroke is a
#     transient user activation, per HTML spec; no permission prompt),
#     POST blob to /clipboard-upload, fire \x16 on 200.
#   - `types` does NOT include any `image/*` MIME → return without
#     preventDefault. xterm.js's native paste flow then runs and
#     streams the text into the PTY.
#
# Why a sync types check rather than always going async:
#   The sync `.types` array IS reliable across Chrome / Safari /
#   Firefox — it reflects which MIMEs the browser populated on the
#   ClipboardEvent's DataTransfer. What's unreliable is the SYNC
#   retrieval of the image bytes via `e.clipboardData.items[i].getAsFile()`
#   (in particular for macOS Cmd+Shift+4 screenshots, where Chrome /
#   Safari occasionally surface an empty items list even though
#   `.types` includes `image/png`). The fix is to use `.types` for the
#   sync decision and `navigator.clipboard.read()` for the async byte
#   retrieval.
#
# Why an async clipboard read in a paste handler (and not a fresh
# button gesture):
#   navigator.clipboard.read() needs a "transient user activation"
#   (HTML spec). A paste event qualifies — the spec explicitly lists
#   `paste` keystrokes as activation triggers. Verified in Chrome 122 /
#   Safari 17 / Firefox 124 on macOS: the async read resolves without
#   a permission prompt when invoked from inside a paste event
#   listener.
#
# Race elimination:
#   PASTE_INTERCEPT_JS now sends NO bytes; this handler is the SOLE
#   source of \x16, and we only fire after the upload completes.
#   Back-to-back pastes are guarded by the `inFlight` flag.
PASTE_EVENT_HANDLER_JS = """<script id="paste-event-handler-injected">
(function() {
    'use strict';

    var UPLOAD_URL = '/clipboard-upload';
    var TOAST_MS = 2800;
    // Flip to true (or wire to a ?cw-paste-debug=1 query param) when
    // diagnosing paste failures; logs every step of the async pipeline.
    var DEBUG = false;

    function dbg() {
        if (!DEBUG) return;
        try { console.log.apply(console, ['[cw-paste]'].concat([].slice.call(arguments))); }
        catch (e) { /* noop */ }
    }

    function ensureToast() {
        var t = document.getElementById('cw-paste-image-toast');
        if (t) return t;
        t = document.createElement('div');
        t.id = 'cw-paste-image-toast';
        if (document.body) {
            document.body.appendChild(t);
        }
        return t;
    }

    var toastTimer = null;
    function showToast(msg, isError) {
        var t = ensureToast();
        if (!t) return;
        t.textContent = msg;
        t.classList.toggle('error', !!isError);
        t.classList.add('visible');
        if (toastTimer) { clearTimeout(toastTimer); }
        toastTimer = setTimeout(function() {
            t.classList.remove('visible');
        }, TOAST_MS);
    }

    // ttyd wires term.onData -> ws.send('0' + data), so triggering the
    // terminal's data event sends the byte over the WebSocket to the
    // PTY. xterm.js v5 path first, v4 / older-v5 fallback after.
    function sendToTerminal(data) {
        var t = window.term;
        if (!t) return false;
        try {
            if (t._core && t._core.coreService &&
                typeof t._core.coreService.triggerDataEvent === 'function') {
                t._core.coreService.triggerDataEvent(data);
                return true;
            }
        } catch (e) { /* fall through */ }
        try {
            if (t._core && t._core._onData &&
                typeof t._core._onData.fire === 'function') {
                t._core._onData.fire(data);
                return true;
            }
        } catch (e) { /* fall through */ }
        return false;
    }

    function uploadBlob(blob) {
        return fetch(UPLOAD_URL, {
            method: 'POST',
            headers: { 'Content-Type': 'image/png' },
            body: blob,
        });
    }

    // Read the first image/* ClipboardItem from the ASYNC Clipboard
    // API. The paste keystroke that triggered our event satisfies the
    // user-gesture requirement, so no permission prompt fires.
    //
    // Returns a Blob or null.
    async function readAsyncClipboardImage() {
        if (!navigator.clipboard || !navigator.clipboard.read) {
            dbg('navigator.clipboard.read unavailable');
            return null;
        }
        var items;
        try {
            items = await navigator.clipboard.read();
        } catch (err) {
            dbg('clipboard.read rejected', err);
            // NotAllowedError = no user gesture (shouldn't happen in
            // a paste handler) or permission denied. DataError = item
            // unreadable. Re-raise so the caller can toast.
            throw err;
        }
        dbg('async clipboard items:', items.length);
        for (var i = 0; i < items.length; i++) {
            var item = items[i];
            for (var j = 0; j < item.types.length; j++) {
                var type = item.types[j];
                dbg('  item[' + i + '].type[' + j + '] =', type);
                if (type.indexOf('image/') === 0) {
                    var blob = await item.getType(type);
                    dbg('  got blob, size=' + blob.size + ' type=' + blob.type);
                    return blob;
                }
            }
        }
        return null;
    }

    var inFlight = false;

    // SYNC sniff: does this ClipboardEvent's DataTransfer advertise any
    // image MIME type? `e.clipboardData.types` is a DOMStringList /
    // Array of MIME strings populated synchronously when the event
    // fires — checking it is fast and side-effect-free, and reliable
    // across Chrome / Safari / Firefox. The unreliable bit is the
    // SYNC item retrieval (`e.clipboardData.items[i].getAsFile()`),
    // not the .types list itself.
    function clipboardHasImage(e) {
        if (!e || !e.clipboardData) return false;
        var types = e.clipboardData.types;
        if (!types) return false;
        // `types` may be a DOMStringList (Safari) or Array (Chrome /
        // Firefox); `Array.from` normalises both to an Array.
        var arr = Array.from(types);
        for (var i = 0; i < arr.length; i++) {
            if (typeof arr[i] === 'string' && arr[i].indexOf('image/') === 0) {
                return true;
            }
        }
        return false;
    }

    async function onPaste(e) {
        dbg('paste event fired, types=', e.clipboardData && e.clipboardData.types);

        // Lock guard: when the terminal is locked (lock-toggle-injected
        // sets window.__cwTerminalLocked), suppress ALL paste — image
        // AND text — so no clipboard content reaches the live session.
        // attachCustomKeyEventHandler only vetoes KEY events, not the
        // browser's separate `paste` event, so the guard is enforced
        // here too. Block native handling immediately (sync).
        if (window.__cwTerminalLocked) {
            e.preventDefault();
            e.stopImmediatePropagation();
            return;
        }

        // SYNC branch: only intercept when an image MIME is advertised.
        // Text-only clipboards fall through to xterm.js's native paste
        // (which streams the bytes into the PTY) — this is what makes
        // Cmd+V work for BOTH images and text in one keybinding.
        if (!clipboardHasImage(e)) {
            dbg('no image MIME in types, letting native paste through');
            return;
        }

        // Image present — we own this paste. Block native handling
        // immediately (sync, before any await) so xterm.js doesn't
        // also try to paste anything.
        e.preventDefault();
        e.stopImmediatePropagation();

        if (inFlight) {
            showToast('Paste already in progress', true);
            return;
        }
        inFlight = true;

        try {
            var blob;
            try {
                blob = await readAsyncClipboardImage();
            } catch (err) {
                var msg = (err && err.message) ? err.message : String(err);
                showToast('Clipboard read error: ' + msg, true);
                return;
            }
            if (!blob) {
                // `.types` advertised an image but the async read came
                // back empty. Rare; treat as a soft failure with toast.
                dbg('types advertised image but async read returned no blob');
                showToast('Clipboard image unreadable', true);
                return;
            }

            dbg('uploading blob, size=' + blob.size);
            var resp = await uploadBlob(blob);
            if (!resp.ok) {
                var detail = '';
                try {
                    var body = await resp.json();
                    if (body && body.error) { detail = ': ' + body.error; }
                } catch (err) { /* non-JSON body */ }
                showToast('Upload failed: ' + resp.status + detail, true);
                return;
            }
            dbg('upload ok, firing \\\\x16');
            // Sidecar wrote /host-clipboard/clipboard.png atomically.
            // Now fire \\x16 (chat:imagePaste) — the in-container xclip
            // shim reads the file the sidecar just wrote and base64-
            // encodes it into the current Claude Code prompt.
            var sent = sendToTerminal('\\x16');
            if (!sent) {
                showToast('Uploaded, but terminal not ready', true);
                return;
            }
            showToast('Image pasted');
        } finally {
            inFlight = false;
        }
    }

    // useCapture=true so we run BEFORE any other paste listener on the
    // page (xterm.js attaches its own paste handler for the textarea
    // overlay it uses for IME / clipboard input; we want first dibs on
    // image data).
    document.addEventListener('paste', onPaste, true);
})();
</script>
"""


# JS: report the browser colour-scheme to the server so Claude Code's TUI
# theme can follow it. The autodark swap above recolours the *terminal*
# (xterm.js). This block additionally POSTs "dark"/"light" to the
# clipboard-upload sidecar's /theme-report endpoint (q- dynamic-theme),
# which writes it to a shared file; the in-container cw-theme-sync daemon
# then injects `/config theme=<x>` so the CLAUDE-CODE TUI theme (not just
# the terminal colours) tracks the browser. Fires on initial load and on
# every prefers-color-scheme change; only POSTs on an actual change, and
# re-arms on a failed POST so a transient error self-heals on the next flip.
THEME_REPORT_JS = """<script id="theme-report-injected">
(function() {
    'use strict';
    var REPORT_URL = '/theme-report';
    var last = null;
    var timer = null;

    function current() {
        try {
            return window.matchMedia('(prefers-color-scheme: dark)').matches
                ? 'dark' : 'light';
        } catch (e) { return 'dark'; }
    }

    function report() {
        var t = current();
        if (t === last) return;   // debounce: only report real changes
        last = t;
        try {
            fetch(REPORT_URL, {
                method: 'POST',
                headers: { 'Content-Type': 'application/json' },
                body: JSON.stringify({ theme: t }),
                keepalive: true
            }).then(function(resp) {
                if (!resp || !resp.ok) { last = null; }  // re-arm on failure
            }).catch(function() { last = null; });
        } catch (e) { last = null; }
    }

    // Coalesce bursts (e.g. an OS toggle firing multiple mql events).
    function schedule() {
        if (timer) { clearTimeout(timer); }
        timer = setTimeout(report, 300);
    }

    if (document.readyState === 'loading') {
        document.addEventListener('DOMContentLoaded', schedule);
    } else {
        schedule();
    }

    try {
        var mql = window.matchMedia('(prefers-color-scheme: dark)');
        var onChange = function() { schedule(); };
        if (mql.addEventListener) {
            mql.addEventListener('change', onChange);
        } else if (mql.addListener) {
            mql.addListener(onChange);
        }
    } catch (e) { /* noop */ }
})();
</script>
"""


# Lock-toggle CSS. A subtle, transparent padlock button pinned to the
# TOP-RIGHT corner of the page chrome (mirrors the kiosk-mode toggle in
# the operator's custom Grafana: near-invisible at rest, brightens on
# hover, and follows the system color-scheme so it reads on both the
# Solarized-dark and -light chrome painted by the autodark CSS above).
#
# At rest it's ~0.28 opacity so it doesn't distract from the terminal.
# When the lock is ACTIVE the button gets the `.cw-locked` class: full
# opacity + a Solarized-orange tint + a subtle ring, so "input is
# suppressed" is unmistakable at a glance. Colours are chosen from the
# same Solarized palette the rest of this file uses.
# --- Auto-lock configuration -----------------------------------------
#
# The lock toggle also AUTO-ENGAGES after a period of operator inactivity
# (Andrew, 2026-08-22). Rationale: the manual padlock only helps when the
# operator remembers to click it; the common failure it guards against —
# walking away from a browser tab pointed at a live claude/tmux session
# and letting a cat / child / colleague / stray keystroke reach the PTY —
# is exactly the case where nobody is around to press the button.
#
# The idle window is configured at IMAGE BUILD time via the
# TTYD_AUTOLOCK_SECONDS env var (the html-builder stage of the Dockerfile
# passes it through; docker-compose.yml exposes it as a build arg). The
# resolved integer is substituted into the injected JS in place of
# AUTOLOCK_PLACEHOLDER, so the shipped index.html carries a literal
# `var AUTO_LOCK_SECONDS = <n>;`. 0 disables auto-lock entirely and leaves
# the manual toggle as the only path.
#
# Why build-time rather than a runtime env var: the injection pipeline
# bakes a single static index.html into the image (ttyd serves it via
# `-I`), so there is no server-side template render at request time to
# read a runtime env var from. The injected JS additionally honours a
# per-tab `?autolock=<seconds>` query param so an operator can override
# (or disable, with `?autolock=0`) without a rebuild.
AUTOLOCK_ENV_VAR = "TTYD_AUTOLOCK_SECONDS"
DEFAULT_AUTOLOCK_SECONDS = 300
AUTOLOCK_PLACEHOLDER = "__CW_AUTOLOCK_SECONDS__"


def resolve_autolock_seconds(env=None) -> int:
    """Resolve the auto-lock idle window (seconds) from the environment.

    Unset / empty -> DEFAULT_AUTOLOCK_SECONDS (300). `0` disables
    auto-lock. Anything that is not a non-negative integer is a BUILD
    FAILURE rather than a silent fallback: a typo'd
    TTYD_AUTOLOCK_SECONDS that quietly reverted to the default would ship
    an image whose lock behaviour differs from what the operator asked
    for, and nothing downstream would notice.
    """
    source = os.environ if env is None else env
    raw = source.get(AUTOLOCK_ENV_VAR)
    if raw is None or str(raw).strip() == "":
        return DEFAULT_AUTOLOCK_SECONDS
    text = str(raw).strip()
    try:
        seconds = int(text)
    except ValueError:
        raise SystemExit(
            f"inject-autodark.py: {AUTOLOCK_ENV_VAR} must be a "
            f"non-negative integer number of seconds (0 disables), "
            f"got {text!r}"
        )
    if seconds < 0:
        raise SystemExit(
            f"inject-autodark.py: {AUTOLOCK_ENV_VAR} must be >= 0 "
            f"(0 disables), got {seconds}"
        )
    return seconds


LOCK_TOGGLE_STYLE = """<style id="lock-toggle-injected-style">
#cw-lock-toggle {
    position: fixed;
    top: 6px;
    right: 8px;
    z-index: 10000;
    width: 26px;
    height: 26px;
    display: flex;
    align-items: center;
    justify-content: center;
    padding: 0;
    margin: 0;
    border: 1px solid transparent;
    border-radius: 5px;
    background: transparent;
    color: #93a1a1;            /* base1 — reads on the dark chrome */
    font-size: 14px;
    line-height: 1;
    cursor: pointer;
    opacity: 0.28;             /* subtle / near-transparent at rest */
    transition: opacity 0.15s ease, background-color 0.15s ease,
                color 0.15s ease, border-color 0.15s ease;
    -webkit-user-select: none;
    user-select: none;
}
#cw-lock-toggle:hover,
#cw-lock-toggle:focus-visible {
    opacity: 0.85;
    outline: none;
}
/* Active lock: unmistakable visual cue that keystrokes are ignored. */
#cw-lock-toggle.cw-locked {
    opacity: 0.95;
    color: #cb4b16;                          /* solarized orange */
    border-color: rgba(203, 75, 22, 0.5);
    background-color: rgba(203, 75, 22, 0.12);
}
@media (prefers-color-scheme: light) {
    #cw-lock-toggle { color: #586e75; }      /* base01 on light chrome */
    #cw-lock-toggle.cw-locked { color: #cb4b16; }
}
</style>
"""

# Lock-toggle JS. Builds the top-right padlock button and wires the
# actual keystroke suppression.
#
# Suppression mechanism: xterm.js exposes
# `term.attachCustomKeyEventHandler(fn)` — the OFFICIAL hook for vetoing
# key events. Returning `false` tells xterm to NOT process the key, so
# it is never written to the PTY nor sent over ttyd's input WebSocket
# (ttyd wires `term.onData -> ws.send('0'+data)`; a key xterm never
# processes produces no onData). We attach ONE handler that returns
# `!locked`, so:
#   - unlocked → returns true → xterm behaves exactly as stock ttyd.
#   - locked   → returns false → every keystroke is swallowed at the
#     terminal layer.
# This is deliberately terminal-scoped rather than a document-level
# capture-phase keydown trap: it leaves browser-native shortcuts
# (Cmd/Ctrl+R reload, devtools, Cmd+C copy of a selection, tab switch)
# untouched — only input destined for the live claude/tmux session is
# guarded. It also degrades safely: if `window.term` isn't ready on
# first paint we retry on a poll (ttyd creates `term` asynchronously
# after the WS connects, same race the autodark reapply defends).
#
# The lock state is also mirrored onto `window.__cwTerminalLocked` so
# the paste handler above can suppress clipboard paste (image AND text)
# while locked — `attachCustomKeyEventHandler` only vetoes key events,
# not the browser's separate `paste` event, so the paste path needs its
# own check for the guard to be complete.
#
# Persistence: the lock state is stored in localStorage under
# `cw-terminal-locked` ('1' locked / '0' unlocked). It's read back on
# page load to seed the initial state (button glyph, aria-pressed, the
# `.cw-locked` class, `window.__cwTerminalLocked`, and the key-veto), so
# a locked terminal survives a browser refresh / reconnect instead of
# silently reverting to unlocked. First load (no stored value) defaults
# to unlocked; localStorage access is wrapped in try/catch so private
# mode / disabled storage degrades to in-memory-only rather than breaking
# the toggle.
#
# Auto-lock: the toggle also engages itself after AUTO_LOCK_SECONDS of
# operator INACTIVITY (default 300, substituted from TTYD_AUTOLOCK_SECONDS
# at build time — see resolve_autolock_seconds above; 0 disables). Activity
# is a capture-phase listener on a fixed set of deliberate-input events
# (keydown / mousedown / pointerdown / touchstart / wheel / paste) that
# stamps a wall-clock timestamp; a 1 s poll compares Date.now() against it
# and calls the SAME setLocked() the button does, so an auto-lock is
# indistinguishable downstream — same glyph, same aria-pressed, same
# key-veto, same localStorage write (a reload while auto-locked comes back
# locked). Terminal OUTPUT is deliberately excluded from the activity set:
# counting it would let a chatty session (build log, `tail -f`, streaming
# tokens) hold the lock open forever, which is exactly the unattended case
# this guards. The tick is a no-op while already locked, so a restored or
# manual lock is never re-locked on top of itself, and an explicit unlock
# restamps the activity clock so the operator gets the full window back.
LOCK_TOGGLE_JS = """<script id="lock-toggle-injected">
(function() {
    'use strict';

    // Persist the lock state across page reloads in localStorage. Without
    // this the lock is a per-page in-memory flag, so every browser refresh
    // / laptop-sleep-driven ttyd reconnect / accidental reload silently
    // drops back to UNLOCKED — surprising for a safety guard whose whole
    // point is "don't let stray keystrokes reach the live session". With
    // persistence a locked terminal STAYS locked until explicitly unlocked.
    var STORAGE_KEY = 'cw-terminal-locked';

    // Idle window (seconds) before the lock auto-engages. Substituted at
    // image build time from TTYD_AUTOLOCK_SECONDS (default 300); 0
    // disables auto-lock and leaves the manual toggle as the only path.
    var AUTO_LOCK_SECONDS = __CW_AUTOLOCK_SECONDS__;

    // How often the idle deadline is checked. 1 s is coarse enough to be
    // free and fine enough that the lock lands within a second of the
    // configured deadline. Deliberately a POLL against a wall-clock
    // timestamp rather than a setTimeout that every keystroke clears and
    // re-arms: a suspended laptop freezes timers, so a machine that
    // sleeps through the deadline would wake UNLOCKED. Comparing
    // Date.now() against the last-activity stamp locks correctly the
    // instant the tab wakes up.
    var IDLE_TICK_MS = 1000;

    // Per-tab runtime override: `?autolock=<seconds>` on the ttyd URL
    // beats the build-time default (`?autolock=0` disables it for this
    // tab). The image bakes a single static index.html, so without this
    // the only way to change the window would be an image rebuild.
    function readAutoLockOverride() {
        try {
            var search = (window.location && window.location.search) || '';
            var m = /[?&]autolock=(\\d+)/.exec(search);
            if (m) { return parseInt(m[1], 10); }
        } catch (e) { /* noop */ }
        return null;
    }

    var override = readAutoLockOverride();
    if (override !== null && isFinite(override) && override >= 0) {
        AUTO_LOCK_SECONDS = override;
    }
    if (!isFinite(AUTO_LOCK_SECONDS) || AUTO_LOCK_SECONDS < 0) {
        AUTO_LOCK_SECONDS = 0;
    }
    var autoLockEnabled = AUTO_LOCK_SECONDS > 0;
    // Exposed for debugging / tests: the window this page actually used.
    window.__cwAutoLockSeconds = AUTO_LOCK_SECONDS;

    function readStoredLocked() {
        // First load (nothing stored) → getItem returns null → false
        // (unlocked), the safe / stock default. Wrapped in try/catch
        // because Safari private mode and disabled-storage configs THROW
        // on any localStorage access rather than returning null.
        try {
            return window.localStorage.getItem(STORAGE_KEY) === '1';
        } catch (e) { return false; }
    }

    function writeStoredLocked(v) {
        // Degrade to in-memory-only if storage is unavailable (private
        // mode / quota / disabled): the lock still works for this session,
        // it just won't survive a reload. Never let a storage error break
        // the toggle itself.
        try {
            window.localStorage.setItem(STORAGE_KEY, v ? '1' : '0');
        } catch (e) { /* noop */ }
    }

    // Seed from persisted state so a reload restores the prior lock. An
    // AUTO lock persists through exactly the same path as a manual one,
    // so a reload while auto-locked comes back locked; and because the
    // idle tick is a no-op while `locked` is already true, a restored
    // lock is never re-locked on top of itself.
    var locked = readStoredLocked();

    // What engaged the CURRENT lock — 'user' (button) or 'auto' (idle
    // deadline). Drives the tooltip only; null while unlocked, and null
    // for a state restored from a previous page load (we don't persist
    // the source, only the lock itself).
    var lockSource = null;

    // Shared flag other injected handlers (the paste handler) read to
    // suppress input while the terminal is locked. Defined up front AND
    // seeded from storage so a paste that races button creation still
    // sees the correct restored value.
    window.__cwTerminalLocked = locked;

    var LOCK_ICON = '\\uD83D\\uDD12';    // closed padlock
    var UNLOCK_ICON = '\\uD83D\\uDD13';  // open padlock

    var btn = null;

    function humanIdle() {
        return (AUTO_LOCK_SECONDS % 60 === 0)
            ? (AUTO_LOCK_SECONDS / 60) + ' min'
            : AUTO_LOCK_SECONDS + ' s';
    }

    function render() {
        if (!btn) return;
        btn.textContent = locked ? LOCK_ICON : UNLOCK_ICON;
        btn.setAttribute('aria-pressed', locked ? 'true' : 'false');
        if (locked && lockSource === 'auto') {
            btn.title = 'Terminal input AUTO-LOCKED after ' + humanIdle()
                + ' idle — keystrokes are ignored. Click to unlock.';
        } else if (locked) {
            btn.title = 'Terminal input LOCKED — keystrokes are ignored. '
                + 'Click to unlock.';
        } else if (autoLockEnabled) {
            btn.title = 'Lock terminal input (ignore keystrokes) — '
                + 'auto-locks after ' + humanIdle() + ' idle';
        } else {
            btn.title = 'Lock terminal input (ignore keystrokes)';
        }
        if (locked) { btn.classList.add('cw-locked'); }
        else { btn.classList.remove('cw-locked'); }
    }

    // ---- idle tracking -------------------------------------------------
    // Only DELIBERATE operator input counts as activity. Terminal OUTPUT
    // is deliberately NOT a signal: a chatty session (a build log, a
    // `tail -f`, Claude Code streaming tokens) would otherwise hold the
    // lock open forever, which is precisely the unattended case the
    // auto-lock exists for. Passive pointer MOVEMENT is excluded for the
    // same reason — a nudged desk is not presence.
    var ACTIVITY_EVENTS = [
        'keydown',      // typing
        'mousedown',    // click / drag-select
        'pointerdown',  // pen / unified pointer
        'touchstart',   // tablet + phone, the main non-keyboard client
        'wheel',        // scrollback
        'paste'         // Cmd+V, whose image path preventDefaults early
    ];

    var lastActivityAt = Date.now();

    function markActivity() { lastActivityAt = Date.now(); }

    function wireActivityListeners() {
        // Capture phase so activity still registers when xterm.js (or our
        // own paste interceptor) stops propagation on its handlers.
        for (var i = 0; i < ACTIVITY_EVENTS.length; i++) {
            document.addEventListener(ACTIVITY_EVENTS[i], markActivity, true);
        }
    }

    function idleTick() {
        if (!autoLockEnabled) return;
        if (locked) return;   // already locked — nothing to auto-engage
        if (Date.now() - lastActivityAt >= AUTO_LOCK_SECONDS * 1000) {
            setLocked(true, 'auto');
        }
    }

    function setLocked(v, source) {
        locked = !!v;
        lockSource = locked ? (source || 'user') : null;
        window.__cwTerminalLocked = locked;
        writeStoredLocked(locked);   // persist so the state survives reload
        // An explicit unlock restarts the idle countdown from NOW so the
        // operator gets the full window back — without this, unlocking
        // after a long idle would re-lock on the very next tick.
        if (!locked) { markActivity(); }
        render();
    }

    function ensureButton() {
        if (btn || !document.body) return;
        btn = document.createElement('button');
        btn.id = 'cw-lock-toggle';
        btn.type = 'button';
        btn.setAttribute('aria-label', 'Toggle terminal input lock');
        btn.addEventListener('click', function(e) {
            if (e && e.preventDefault) { e.preventDefault(); }
            if (e && e.stopPropagation) { e.stopPropagation(); }
            setLocked(!locked, 'user');
        });
        // Keep a click on the toggle from bubbling into xterm.js focus /
        // selection handling.
        btn.addEventListener('mousedown', function(e) {
            if (e && e.stopPropagation) { e.stopPropagation(); }
        });
        document.body.appendChild(btn);
        render();
    }

    var handlerAttached = false;
    function attachKeyGuard() {
        var t = window.term;
        if (!t || handlerAttached) return;
        if (typeof t.attachCustomKeyEventHandler === 'function') {
            // false => xterm ignores the key (no PTY write / no WS send)
            // while locked; true => normal processing.
            t.attachCustomKeyEventHandler(function() { return !locked; });
            handlerAttached = true;
        }
    }

    function init() {
        ensureButton();
        attachKeyGuard();
    }

    if (document.readyState === 'loading') {
        document.addEventListener('DOMContentLoaded', init);
    } else {
        init();
    }

    // ttyd builds window.term asynchronously after the WS connects, so
    // it may be absent on first paint. Poll until BOTH the button is
    // mounted and the key guard is attached, then stop (mirrors the
    // autodark reapply poll; negligible cost).
    var iv = setInterval(function() {
        init();
        if (btn && handlerAttached) { clearInterval(iv); }
    }, 1000);

    // Auto-lock wiring. Skipped entirely when disabled (0) so an opted-out
    // build registers no listeners and no timer at all.
    if (autoLockEnabled) {
        wireActivityListeners();
        setInterval(idleTick, IDLE_TICK_MS);
    }
})();
</script>
"""


def lock_toggle_js(seconds: int) -> str:
    """LOCK_TOGGLE_JS with the auto-lock idle window substituted in.

    Kept separate from the constant (rather than making LOCK_TOGGLE_JS an
    f-string) so the template stays a plain module-level string literal —
    the tests lift it out of the source with `ast` and run it under Node,
    which needs a real `ast.Constant` to read.
    """
    return LOCK_TOGGLE_JS.replace(AUTOLOCK_PLACEHOLDER, str(int(seconds)))


def inject(html: str, autolock_seconds: int | None = None) -> str:
    """Inject CSS + JS into the <head> of ttyd's bundled HTML.

    ttyd 1.7.7 ships a one-line minified HTML — the `<head>` open and
    close tags are present but everything is on a single line. We
    splice our content RIGHT BEFORE </head> so it loads after ttyd's
    own <style>/<link> definitions and wins on the cascade.

    `autolock_seconds` is the idle window baked into the lock toggle;
    None resolves it from TTYD_AUTOLOCK_SECONDS (default 300, 0 = off).
    """
    if autolock_seconds is None:
        autolock_seconds = resolve_autolock_seconds()
    marker = "</head>"
    if marker not in html:
        # Defensive: if upstream HTML structure ever changes, fail
        # loudly so the build catches it instead of silently shipping
        # a no-op injection.
        raise SystemExit(
            "inject-autodark.py: '</head>' marker not found in input HTML"
        )
    injected = (
        CSS + JS + THEME_REPORT_JS + PASTE_INTERCEPT_JS + PASTE_TOAST_STYLE
        + PASTE_EVENT_HANDLER_JS + LOCK_TOGGLE_STYLE
        + lock_toggle_js(autolock_seconds) + marker
    )
    # Replace only the FIRST occurrence (xterm.js's inline JS may
    # mention the string '</head>' inside a quoted literal further
    # down).
    return html.replace(marker, injected, 1)


def main() -> int:
    if len(sys.argv) != 3:
        sys.stderr.write(
            "usage: inject-autodark.py <input.html> <output.html>\n"
        )
        return 2
    in_path, out_path = sys.argv[1], sys.argv[2]
    with open(in_path, "r", encoding="utf-8") as f:
        html = f.read()
    autolock_seconds = resolve_autolock_seconds()
    patched = inject(html, autolock_seconds)
    with open(out_path, "w", encoding="utf-8") as f:
        f.write(patched)
    sys.stderr.write(
        f"inject-autodark.py: wrote {len(patched)} bytes to {out_path} "
        f"(input was {len(html)} bytes; auto-lock "
        + (f"{autolock_seconds}s" if autolock_seconds else "DISABLED")
        + ")\n"
    )
    # Sanity-check: our marker classes are present in the output.
    # The floating "Paste image" button was removed 2026-05-20 — Cmd+V
    # via PASTE_EVENT_HANDLER_JS is the sole image-paste path now —
    # so `paste-image-button-injected` / `cw-paste-image-btn` are
    # explicitly NOT in this list. The toast surface keeps its
    # `cw-paste-image-toast` id (used by PASTE_EVENT_HANDLER_JS).
    for needle in ("autodark-injected", "prefers-color-scheme",
                   "theme-report-injected",
                   "paste-intercept-injected",
                   "paste-toast-injected-style",
                   "cw-paste-image-toast",
                   "paste-event-handler-injected",
                   "lock-toggle-injected-style",
                   "lock-toggle-injected",
                   "cw-lock-toggle",
                   # The auto-lock window really was substituted (a
                   # surviving placeholder is a JS syntax error that
                   # would take the whole toggle down at runtime).
                   f"var AUTO_LOCK_SECONDS = {autolock_seconds};"):
        if needle not in patched:
            sys.stderr.write(
                f"inject-autodark.py: missing '{needle}' in output — abort\n"
            )
            return 1
    # And reverse-check: removed markers MUST be absent. Catches
    # accidental partial reverts in code review.
    for absent in ("paste-image-button-injected", "cw-paste-image-btn",
                   AUTOLOCK_PLACEHOLDER):
        if absent in patched:
            sys.stderr.write(
                f"inject-autodark.py: removed marker '{absent}' still "
                f"present in output — abort\n"
            )
            return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
