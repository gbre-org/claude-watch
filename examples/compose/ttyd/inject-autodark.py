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
    injected = CSS + JS + PASTE_INTERCEPT_JS + marker
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
                   "paste-intercept-injected"):
        if needle not in patched:
            sys.stderr.write(
                f"inject-autodark.py: missing '{needle}' in output — abort\n"
            )
            return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
