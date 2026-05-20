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
#   4. Injects a keydown handler (PASTE_INTERCEPT_JS) that maps the
#      browser-native Cmd+V / Ctrl+V to a raw \x16 byte sent into the
#      xterm.js terminal — Claude Code's chat:imagePaste keybinding.
#   5. Injects a floating "Paste image" button (PASTE_IMAGE_BUTTON_JS)
#      that calls navigator.clipboard.read(), POSTs the resulting PNG
#      blob to the clipboard-upload sidecar at /clipboard-upload, and
#      on a 200 response fires \x16 to trigger chat:imagePaste. The
#      sidecar (separate service, see examples/compose/clipboard-upload/)
#      atomically writes the PNG to a named volume that the
#      claude-container's xclip shim reads. This is the browser-only
#      complement to workbot's Mac-side clipboard-bridge daemon.
#
# Output is written in place: index.html is overwritten.

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

# JS: intercept Cmd+V (Mac) / Ctrl+V (non-Mac) and send the raw Ctrl+V
# byte (\x16) to the terminal instead of triggering the browser's native
# paste. Claude Code uses Ctrl+V as its `chat:imagePaste` keybinding; in
# a browser-based terminal (ttyd), the browser intercepts Cmd+V/Ctrl+V
# for clipboard paste before xterm.js ever sees the keystroke. This
# handler bridges that gap so the raw byte reaches the terminal app.
#
# Users who want text paste can use Ctrl+Shift+V (ttyd's default text-
# paste keybinding), right-click context menu, or the browser's Edit >
# Paste menu. The trade-off is intentional: image paste into Claude Code
# is the primary use case for this terminal.
PASTE_INTERCEPT_JS = """<script id="paste-intercept-injected">
(function() {
    'use strict';

    // Send a raw byte string to the terminal via ttyd's xterm.js instance.
    // ttyd wires term.onData -> ws.send('0' + data), so triggering the
    // terminal's data event sends the byte over the WebSocket to the PTY.
    function sendToTerminal(data) {
        var t = window.term;
        if (!t) return false;
        // xterm.js v5: _core.coreService.triggerDataEvent fires onData.
        // This is the same path xterm.js uses internally when processing
        // keyboard input.
        try {
            if (t._core && t._core.coreService &&
                typeof t._core.coreService.triggerDataEvent === 'function') {
                t._core.coreService.triggerDataEvent(data);
                return true;
            }
        } catch (e) { /* fall through */ }
        // Fallback: older xterm.js v5 builds expose _onData on _core.
        try {
            if (t._core && t._core._onData &&
                typeof t._core._onData.fire === 'function') {
                t._core._onData.fire(data);
                return true;
            }
        } catch (e) { /* fall through */ }
        return false;
    }

    var isMac = /Mac|iPhone|iPad|iPod/.test(navigator.platform);

    document.addEventListener('keydown', function(e) {
        // Cmd+V on Mac, Ctrl+V on non-Mac (without Shift -- Ctrl+Shift+V
        // is the standard ttyd text-paste keybinding and must pass through).
        var isPaste = isMac
            ? (e.metaKey && !e.ctrlKey && !e.shiftKey && e.key === 'v')
            : (e.ctrlKey && !e.metaKey && !e.shiftKey && e.key === 'v');
        if (isPaste) {
            e.preventDefault();
            e.stopPropagation();
            sendToTerminal('\\x16');
        }
    }, true);  // useCapture=true to fire before xterm.js's own handler
})();
</script>
"""

# Floating "Paste image" button + click handler. Reads an image from
# the browser clipboard via the async Clipboard API, uploads it as raw
# PNG bytes to the clipboard-upload sidecar (sibling service that
# atomically writes /host-clipboard/clipboard.png on a named volume
# shared with the claude-container), and then triggers the same raw
# Ctrl+V byte (\x16) that the keydown intercept above uses so Claude
# Code's `chat:imagePaste` action fires inside the terminal.
#
# Phase A (paste-event handler that intercepts Cmd+V/Ctrl+V image data
# directly) is intentionally deferred to a follow-up. This button is
# the lowest-friction primitive: user clicks once, grants the
# permission prompt once per origin, and from then on the click acts
# as an explicit "send my clipboard image to Claude" action.
#
# Why a button at all (when the keydown intercept already exists):
#   1. The keydown intercept fires the *byte* \x16 but does NOT upload
#      anything — it relies on the claude-container being able to read
#      a real image from /host-clipboard/clipboard.png that some
#      separate channel put there (e.g. workbot's Mac-local
#      clipboard-bridge writing the file via AppleScript). For
#      browser-only operators (no Mac bridge running), the file is
#      stale or absent, and Claude Code's xclip shim reads the wrong
#      bytes.
#   2. navigator.clipboard.read() requires a user gesture in every
#      major browser. Wiring it to a global keydown listener would
#      either prompt-spam on every Ctrl+V or silently fail; a
#      dedicated button is the cleanest UX.
#
# Wire shape (matches the clipboard-upload sidecar from the companion
# PR): POST /clipboard-upload with Content-Type: image/png and the raw
# PNG bytes as the body. 200 = stored on the named volume, anything
# else = surface to the toast.
PASTE_IMAGE_BUTTON_JS = """<style id="paste-image-button-injected-style">
#cw-paste-image-btn {
    position: fixed;
    bottom: 16px;
    right: 16px;
    z-index: 9999;
    padding: 8px 14px;
    font: 13px/1.2 -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
    background: rgba(38, 139, 210, 0.85); /* Solarized blue */
    color: #fdf6e3;
    border: 1px solid rgba(7, 54, 66, 0.4);
    border-radius: 6px;
    cursor: pointer;
    box-shadow: 0 2px 6px rgba(0, 0, 0, 0.25);
    opacity: 0.55;
    transition: opacity 0.15s ease;
    user-select: none;
}
#cw-paste-image-btn:hover, #cw-paste-image-btn:focus {
    opacity: 1.0;
    outline: none;
}
#cw-paste-image-btn:active {
    transform: translateY(1px);
}
#cw-paste-image-btn[disabled] {
    cursor: progress;
    opacity: 0.4;
}
#cw-paste-image-toast {
    position: fixed;
    bottom: 60px;
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
<script id="paste-image-button-injected">
(function() {
    'use strict';

    var UPLOAD_URL = '/clipboard-upload';
    var TOAST_MS = 2800;

    function ensureToast() {
        var t = document.getElementById('cw-paste-image-toast');
        if (t) return t;
        t = document.createElement('div');
        t.id = 'cw-paste-image-toast';
        document.body.appendChild(t);
        return t;
    }

    var toastTimer = null;
    function showToast(msg, isError) {
        var t = ensureToast();
        t.textContent = msg;
        t.classList.toggle('error', !!isError);
        t.classList.add('visible');
        if (toastTimer) { clearTimeout(toastTimer); }
        toastTimer = setTimeout(function() {
            t.classList.remove('visible');
        }, TOAST_MS);
    }

    // Mirrors PASTE_INTERCEPT_JS's sendToTerminal — kept inline so the
    // two scripts don't share globals (each injection block is its own
    // IIFE). xterm.js v5 path first, v4 / older-v5 fallback after.
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

    async function readClipboardImage() {
        if (!navigator.clipboard || !navigator.clipboard.read) {
            throw new Error('Clipboard API unavailable (needs HTTPS or localhost)');
        }
        var items = await navigator.clipboard.read();
        for (var i = 0; i < items.length; i++) {
            var item = items[i];
            for (var j = 0; j < item.types.length; j++) {
                var type = item.types[j];
                if (type.indexOf('image/') === 0) {
                    var blob = await item.getType(type);
                    // Normalize to image/png since the sidecar validates
                    // PNG magic bytes. If the clipboard has a non-PNG
                    // image (e.g. image/jpeg from a screenshot tool),
                    // the upload will fail with 415 and the toast will
                    // surface that — fine for v1; transcoding to PNG in
                    // the browser is a Phase A concern.
                    return { blob: blob, type: type };
                }
            }
        }
        return null;
    }

    async function onPasteClick(ev) {
        if (ev) { ev.preventDefault(); }
        var btn = document.getElementById('cw-paste-image-btn');
        if (btn) { btn.setAttribute('disabled', 'disabled'); }
        try {
            var found = await readClipboardImage();
            if (!found) {
                showToast('No image in clipboard', true);
                return;
            }
            var resp = await uploadBlob(found.blob);
            if (!resp.ok) {
                var detail = '';
                try {
                    var body = await resp.json();
                    if (body && body.error) { detail = ': ' + body.error; }
                } catch (e) { /* non-JSON body, fall through */ }
                showToast('Upload failed: ' + resp.status + detail, true);
                return;
            }
            // Sidecar wrote /host-clipboard/clipboard.png atomically.
            // Now fire Ctrl-V so Claude Code's chat:imagePaste action
            // runs xclip inside the container, which reads the file
            // the sidecar just wrote and base64-encodes it into the
            // current prompt.
            var sent = sendToTerminal('\\x16');
            if (!sent) {
                showToast('Uploaded, but terminal not ready', true);
                return;
            }
            showToast('Image pasted');
        } catch (err) {
            // NotAllowedError = permission denied / no user gesture.
            // DataError = clipboard contained something we can't read.
            // Network errors land here too (fetch rejects).
            var msg = (err && err.message) ? err.message : String(err);
            showToast('Error: ' + msg, true);
        } finally {
            if (btn) { btn.removeAttribute('disabled'); }
        }
    }

    function mountButton() {
        if (document.getElementById('cw-paste-image-btn')) return;
        var btn = document.createElement('button');
        btn.id = 'cw-paste-image-btn';
        btn.type = 'button';
        btn.title = 'Read an image from the browser clipboard and send it to Claude Code';
        btn.textContent = 'Paste image';
        btn.addEventListener('click', onPasteClick);
        document.body.appendChild(btn);
    }

    if (document.readyState === 'loading') {
        document.addEventListener('DOMContentLoaded', mountButton);
    } else {
        mountButton();
    }
})();
</script>
"""


def inject(html: str) -> str:
    """Inject CSS + JS into the <head> of ttyd's bundled HTML.

    ttyd 1.7.7 ships a one-line minified HTML — the `<head>` open and
    close tags are present but everything is on a single line. We
    splice our content RIGHT BEFORE </head> so it loads after ttyd's
    own <style>/<link> definitions and wins on the cascade.
    """
    marker = "</head>"
    if marker not in html:
        # Defensive: if upstream HTML structure ever changes, fail
        # loudly so the build catches it instead of silently shipping
        # a no-op injection.
        raise SystemExit(
            "inject-autodark.py: '</head>' marker not found in input HTML"
        )
    injected = CSS + JS + PASTE_INTERCEPT_JS + PASTE_IMAGE_BUTTON_JS + marker
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
    patched = inject(html)
    with open(out_path, "w", encoding="utf-8") as f:
        f.write(patched)
    sys.stderr.write(
        f"inject-autodark.py: wrote {len(patched)} bytes to {out_path} "
        f"(input was {len(html)} bytes)\n"
    )
    # Sanity-check: our marker classes are present in the output.
    for needle in ("autodark-injected", "prefers-color-scheme",
                   "paste-intercept-injected",
                   "paste-image-button-injected",
                   "cw-paste-image-btn"):
        if needle not in patched:
            sys.stderr.write(
                f"inject-autodark.py: missing '{needle}' in output — abort\n"
            )
            return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
