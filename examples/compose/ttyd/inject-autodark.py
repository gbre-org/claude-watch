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
    injected = (
        CSS + JS + PASTE_INTERCEPT_JS + PASTE_TOAST_STYLE
        + PASTE_EVENT_HANDLER_JS + marker
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
    patched = inject(html)
    with open(out_path, "w", encoding="utf-8") as f:
        f.write(patched)
    sys.stderr.write(
        f"inject-autodark.py: wrote {len(patched)} bytes to {out_path} "
        f"(input was {len(html)} bytes)\n"
    )
    # Sanity-check: our marker classes are present in the output.
    # The floating "Paste image" button was removed 2026-05-20 — Cmd+V
    # via PASTE_EVENT_HANDLER_JS is the sole image-paste path now —
    # so `paste-image-button-injected` / `cw-paste-image-btn` are
    # explicitly NOT in this list. The toast surface keeps its
    # `cw-paste-image-toast` id (used by PASTE_EVENT_HANDLER_JS).
    for needle in ("autodark-injected", "prefers-color-scheme",
                   "paste-intercept-injected",
                   "paste-toast-injected-style",
                   "cw-paste-image-toast",
                   "paste-event-handler-injected"):
        if needle not in patched:
            sys.stderr.write(
                f"inject-autodark.py: missing '{needle}' in output — abort\n"
            )
            return 1
    # And reverse-check: removed markers MUST be absent. Catches
    # accidental partial reverts in code review.
    for absent in ("paste-image-button-injected", "cw-paste-image-btn"):
        if absent in patched:
            sys.stderr.write(
                f"inject-autodark.py: removed marker '{absent}' still "
                f"present in output — abort\n"
            )
            return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
