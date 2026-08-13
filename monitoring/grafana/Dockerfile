# Grafana with a Solarized dark/light theme baked in, plus a small set of
# usability patches. See README.md in this directory for the "why" of each one.
#
# HOW THIS WORKS, AND WHY IT IS A BUILD-TIME PATCH
# ------------------------------------------------
# Grafana OSS has no supported way to replace the palette of the built-in
# themes, so this image rewrites the compiled CSS/JS bundles at build time and
# additionally injects a stylesheet at runtime (belt-and-suspenders: Emotion
# generates class names that change between releases, so the stylesheet catches
# what a colour sed misses, and vice-versa).
#
# That is only repeatable because the bundles are content-hashed but STABLE
# within a release, which is why the FROM line is pinned exactly. Treat a
# version bump as a real change: rebuild, and expect one or two of the patches
# below to need re-derivation. Each patch that CAN fail silently ends in an
# explicit grep guard that fails the build instead — that is deliberate and the
# most important property of this file. A no-oping sed produces a green build
# and a silently-reverted patch, which is far worse than a red one.
FROM grafana/grafana:13.1.3

# App title shown in the browser tab and on the login page. Leave at the
# default to keep stock Grafana wording; set it to rebrand:
#   docker build --build-arg GF_APP_TITLE="My Metrics" .
ARG GF_APP_TITLE=Grafana

# Remove the "Powered by" footer that Grafana renders in kiosk mode and in the
# embedded solo-panel view. Off by default in this example; the mechanism is
# documented because it is one of the more fragile patches here and it is easy
# to get subtly wrong. Check Grafana's trademark/attribution guidance before
# enabling it on anything you publish.
ARG STRIP_POWERED_BY=false

# Inject the Solarized stylesheet. The <link> is added below WITHOUT a version
# query string; the final RUN step in this file stamps a content hash onto it,
# because the *.css sed steps mutate this file in place and the hash has to
# describe the bytes actually served.
COPY solarized.css /usr/share/grafana/public/build/solarized.css

USER root

# Pull outstanding Alpine security fixes. Grafana's base image lags its distro
# packages between releases, so this closes host-layer CVEs without waiting for
# an upstream tag. It does NOT cover CVEs compiled into the Grafana Go binary
# itself — re-check those against the base image on every version bump.
RUN apk update \
    && apk upgrade --no-cache \
    && rm -rf /var/cache/apk/*

# python3 for the rehash step below. Separate layer so it caches independently
# of the bundle mutations.
RUN apk add --no-cache python3

# Snapshot the SHA-256 of every *.js bundle BEFORE any mutation.
# rehash-mutated-bundles.py (last step before the runtime script injection)
# diffs against this manifest to find which bundles we actually changed.
COPY pre-sed-snapshot.py /usr/local/bin/pre-sed-snapshot.py
RUN python3 /usr/local/bin/pre-sed-snapshot.py > /tmp/pre-sed-hashes.json

# Load the stylesheet from index.html.
RUN sed -i 's|</head>|<link rel="stylesheet" href="public/build/solarized.css"></head>|' \
        /usr/share/grafana/public/views/index.html && \
    grep -q 'public/build/solarized.css' /usr/share/grafana/public/views/index.html

# Optional rebrand of the app title.
#
# Grafana 13 moved user-visible strings into locale bundles
# (public/locales/<lang>/grafana.json) AND kept hardcoded English fallbacks
# inside the JS bundles, so BOTH surfaces need patching — the locale files are
# fetched asynchronously, and a browser that has the old copy cached will
# happily re-render the old string over a correctly-patched bundle.
#
# The `this.LoginTitle=` form is a pre-13 shape, kept because it costs nothing
# and reappears occasionally.
#
# Skipped entirely (and no guard) when GF_APP_TITLE is the stock value, since
# every one of these seds would be an identity rewrite.
RUN set -eu; \
    if [ "$GF_APP_TITLE" != "Grafana" ]; then \
        sed -i "s|this.AppTitle=\"Grafana\"|this.AppTitle=\"${GF_APP_TITLE}\"|g" \
            /usr/share/grafana/public/build/*.js; \
        sed -i "s|this.LoginTitle=\"Welcome to Grafana\"|this.LoginTitle=\"${GF_APP_TITLE}\"|g" \
            /usr/share/grafana/public/build/*.js; \
        find /usr/share/grafana/public/build/ -name '*.js' -exec sed -i \
            -e "s|\"welcome-to-grafana\":\"Welcome to Grafana\"|\"welcome-to-grafana\":\"${GF_APP_TITLE}\"|g" \
            -e "s|children:\"Welcome to Grafana\"|children:\"${GF_APP_TITLE}\"|g" \
            -e "s|heading:\"Welcome to Grafana\"|heading:\"${GF_APP_TITLE}\"|g" \
            {} +; \
        find /usr/share/grafana/public/locales/ -name 'grafana.json' -exec sed -i \
            -e "s|\"welcome-to-grafana\": \"Welcome to Grafana\"|\"welcome-to-grafana\": \"${GF_APP_TITLE}\"|g" \
            {} +; \
        if ! grep -rqF "$GF_APP_TITLE" /usr/share/grafana/public/build/*.js; then \
            echo "ERROR: app-title patch matched nothing — Grafana's minified form may have changed" >&2; \
            exit 1; \
        fi; \
        echo "Applied app title: $GF_APP_TITLE"; \
    else \
        echo "GF_APP_TITLE is stock; skipping title patch"; \
    fi

# Dark theme: rewrite Grafana's dark greys to the Solarized dark palette in the
# compiled CSS.
#
#   #111217 canvas          -> #002b36 base03
#   #181b1f background      -> #073642 base02
#   #22252b secondary       -> #073642 base02
#   #2c3235                 -> #0a4050
#   #18181a layer0          -> #002b36 base03
#   #212124 layer1          -> #073642 base02
#   #2a2a2f layer2          -> #0a4050
#   #34343b border1         -> #586e75 base01
#   #64646b border2         -> #657b83 base00
#   #282b31 (13.x layer2)   -> #0a4050
#
# The trailing entries are older/alternate greys that Grafana still emits in
# places; they are harmless when absent.
RUN find /usr/share/grafana/public/build/ -name '*.css' -exec sed -i \
    -e 's/#111217/#002b36/g' \
    -e 's/#181b1f/#073642/g' \
    -e 's/#22252b/#073642/g' \
    -e 's/#2c3235/#0a4050/g' \
    -e 's/#18181a/#002b36/g' \
    -e 's/#212124/#073642/g' \
    -e 's/#2a2a2f/#0a4050/g' \
    -e 's/#34343b/#586e75/g' \
    -e 's/#64646b/#657b83/g' \
    -e 's/#141414/#002b36/g' \
    -e 's/#161719/#002b36/g' \
    -e 's/#1f1f20/#073642/g' \
    -e 's/#222426/#073642/g' \
    -e 's/#262628/#073642/g' \
    -e 's/#292a2d/#073642/g' \
    -e 's/#2f2f32/#0a4050/g' \
    -e 's/#343436/#586e75/g' \
    -e 's/#424345/#586e75/g' \
    -e 's/#414350/#586e75/g' \
    -e 's/#404357/#586e75/g' \
    -e 's/#303032/#0a4050/g' \
    -e 's/#282b31/#0a4050/g' \
    {} +

# Light theme: same idea against the Solarized light palette.
#
#   #f4f5f5 canvas          -> #fdf6e3 base3
#   #24292e text            -> #657b83 base00
#   #d8d9da gray4           -> #93a1a1 base1
#   #f4f5f8 gray6           -> #fdf6e3 base3
#   #fbfbfb gray7           -> #fdf6e3 base3
#   #ececec gray5           -> #eee8d5 base2
#   #e4e7e7 lightBorder1    -> #93a1a1 base1
RUN find /usr/share/grafana/public/build/ -name 'grafana.light.*.css' -exec sed -i \
    -e 's/#f4f5f5/#fdf6e3/g' \
    -e 's/#24292e/#657b83/g' \
    -e 's/#d8d9da/#93a1a1/g' \
    -e 's/#f4f5f8/#fdf6e3/g' \
    -e 's/#fbfbfb/#fdf6e3/g' \
    -e 's/#ececec/#eee8d5/g' \
    -e 's/#e4e7e7/#93a1a1/g' \
    -e 's/#181a1b/#657b83/g' \
    {} +

# The same palettes again, in the JS bundles. Grafana ships the theme objects in
# JS as well as CSS (panel chrome, chart axes, tooltips and anything that reads
# theme.colors.* at runtime come from here, not the stylesheet), so a CSS-only
# patch leaves half the UI stock.
RUN find /usr/share/grafana/public/build/ -name '*.js' -exec sed -i \
    -e 's/#111217/#002b36/g' \
    -e 's/#181b1f/#073642/g' \
    -e 's/#22252b/#073642/g' \
    -e 's/#2c3235/#0a4050/g' \
    -e 's/#18181a/#002b36/g' \
    -e 's/#212124/#073642/g' \
    -e 's/#2a2a2f/#0a4050/g' \
    -e 's/#34343b/#586e75/g' \
    -e 's/#64646b/#657b83/g' \
    -e 's/#141414/#002b36/g' \
    -e 's/#161719/#002b36/g' \
    -e 's/#1f1f20/#073642/g' \
    -e 's/#222426/#073642/g' \
    -e 's/#262628/#073642/g' \
    -e 's/#292a2d/#073642/g' \
    -e 's/#2f2f32/#0a4050/g' \
    -e 's/#343436/#586e75/g' \
    -e 's/#424345/#586e75/g' \
    -e 's/#414350/#586e75/g' \
    -e 's/#404357/#586e75/g' \
    -e 's/#303032/#0a4050/g' \
    -e 's/#f4f5f5/#fdf6e3/g' \
    -e 's/#d0d1d3/#93a1a1/g' \
    -e 's/#e4e7e7/#93a1a1/g' \
    -e 's/#d8d9da/#93a1a1/g' \
    -e 's/#ececec/#eee8d5/g' \
    -e 's/#f4f5f8/#fdf6e3/g' \
    -e 's/#fbfbfb/#fdf6e3/g' \
    -e 's/#282b31/#0a4050/g' \
    -e 's/#181a1b/#657b83/g' \
    -e 's/blackBase="36, 41, 46"/blackBase="101, 123, 131"/g' \
    -e 's/primary:r.white/primary:"#eee8d5"/g' \
    {} +

# Optional custom branding (favicon / loading logo / sidebar mark).
#
# This directory ships EMPTY of images on purpose — the example keeps stock
# Grafana branding. Drop `grafana_icon.svg` and/or `fav32.png` into branding/
# and rebuild to use your own; nothing below runs otherwise.
#
# Two things make this more than a file copy:
#
#  * Grafana 13 serves branding from public/build/img/ (11.x used public/img/),
#    so both locations are written for any code path still on the old one.
#  * The sidebar mark is loaded by webpack as a content-hashed URL,
#    static/img/grafana_icon.<hash>.svg. Overwriting the file in place is not
#    enough: those assets are served with a one-year max-age, so a browser that
#    cached the stock icon at that exact URL keeps it essentially forever — the
#    filename never changed, so there is nothing to revalidate against. The step
#    below therefore renames the asset to a sentinel we control and rewrites
#    every JS reference to it, which makes it a cold cache key everywhere. Bump
#    BRAND_SENTINEL on any later branding change to re-bust.
#
# Deliberately NOT touched: grafana_text_logo_{dark,light}.*.svg. Those are the
# wide ~4:1 wordmark SVGs used in the dashboard header. Substituting a square
# icon there collapses the entire nav row on narrow viewports (title,
# breadcrumbs, user menu and share button all vanish) — measured, not theorised.
# Rebranding the wordmark needs a matching wide-aspect asset, not this one.
ARG BRAND_SENTINEL=custom001
COPY branding/ /tmp/branding/
RUN set -eu; \
    SVG=/tmp/branding/grafana_icon.svg; \
    PNG=/tmp/branding/fav32.png; \
    if [ ! -f "$SVG" ] && [ ! -f "$PNG" ]; then \
        echo "No custom branding supplied; keeping stock Grafana branding"; \
    else \
        if [ -f "$PNG" ]; then \
            for dest in /usr/share/grafana/public/img/fav32.png \
                        /usr/share/grafana/public/img/apple-touch-icon.png \
                        /usr/share/grafana/public/img/grafana_icon.png \
                        /usr/share/grafana/public/build/img/fav32.png \
                        /usr/share/grafana/public/build/img/apple-touch-icon.png; do \
                cp "$PNG" "$dest"; \
            done; \
        fi; \
        if [ -f "$SVG" ]; then \
            cp "$SVG" /usr/share/grafana/public/img/grafana_icon.svg; \
            cp "$SVG" /usr/share/grafana/public/build/img/grafana_icon.svg; \
            STATIC=/usr/share/grafana/public/build/static/img; \
            MATCHES="$(find "$STATIC" -maxdepth 1 -name 'grafana_icon.*.svg' -print)"; \
            if [ -z "$MATCHES" ]; then \
                echo "ERROR: no $STATIC/grafana_icon.*.svg — Grafana may have renamed the sidebar asset" >&2; \
                exit 1; \
            fi; \
            echo "$MATCHES" | while read -r f; do \
                base="$(basename "$f")"; \
                new_name="grafana_icon.${BRAND_SENTINEL}.svg"; \
                echo "Replacing hashed sidebar logo: $base -> $new_name"; \
                cp "$SVG" "$STATIC/$new_name"; \
                if [ "$base" != "$new_name" ]; then rm "$f"; fi; \
                find /usr/share/grafana/public/build/ -maxdepth 1 -name '*.js' -exec \
                    sed -i "s|$base|$new_name|g" {} +; \
            done; \
        fi; \
    fi; \
    rm -rf /tmp/branding

# Optional: strip the "Powered by" footer (see the STRIP_POWERED_BY ARG above).
#
# The footer appears in the embedded solo-panel view and in the kiosk-mode
# dashboard footer. Both carry an English fallback hardcoded in the JS bundle
# AND a locale-file entry, so both are patched — but patching strings alone is
# not sufficient, because Grafana fetches locale JSON asynchronously with an
# hour of cache lifetime, so a browser can re-render the old text over a fresh
# bundle for up to an hour after a rebuild.
#
# The robust half is therefore a code patch, not a string patch: the footer
# component computes a "is this kiosk mode" boolean and then has an early
# return. We widen that early return to include the kiosk boolean, so kiosk mode
# returns null no matter what i18n produces.
#
# NAME-AGNOSTIC ON PURPOSE — do not hardcode the minified identifiers. They
# differ between bundles WITHIN A SINGLE BUILD, let alone between releases, so
# no literal pattern can cover them all. That is exactly how this broke once: a
# literal guard written against one release matched zero files in the next and,
# having nothing to match, no-oped silently. Build green, footer back. Hence the
# grep guard at the end; keep it.
#
# Two busybox-sed constraints this expression is shaped around. Both cost a
# debugging cycle; do not "simplify" them away:
#   1. NO BACKREFERENCES IN THE PATTERN. busybox `sed -E` silently fails to
#      match `\1` on the search side (replacement side is fine). That is why the
#      kiosk variable is captured twice instead of asserting the names match.
#   2. DELIMITER IS '#', AND LITERAL PIPES ARE WRITTEN '[|]'. With '|' as the
#      delimiter, busybox un-escapes `\|` back into ERE alternation; the regex
#      degenerates to "empty|empty|…", matches the empty string at offset 0, and
#      PREPENDS the replacement to the file — corrupting the bundle rather than
#      failing.
#
# If the footer returns after a version bump, re-derive with:
#   docker run --rm <image> grep -oE '.{140}footerHide.{140}' \
#     /usr/share/grafana/public/build/*.js
RUN set -eu; \
    if [ "$STRIP_POWERED_BY" = "true" ]; then \
        find /usr/share/grafana/public/locales/ -name 'grafana.json' -exec sed -i \
            -e 's|"footerPoweredBy": "Powered by"|"footerPoweredBy": ""|g' \
            -e 's|"powered-by": "Powered by"|"powered-by": ""|g' \
            {} +; \
        find /usr/share/grafana/public/build/ -name '*.js' -exec sed -i \
            -e 's|"dashboard.kiosk.footerPoweredBy","Powered by"|"dashboard.kiosk.footerPoweredBy",""|g' \
            -e 's|footerText:"Powered by"|footerText:""|g' \
            -e 's|children:"Powered by"}|children:""}|g' \
            -e 's|"footerPoweredBy":"Powered by"|"footerPoweredBy":""|g' \
            -e 's|"powered-by":"Powered by"|"powered-by":""|g' \
            {} +; \
        find /usr/share/grafana/public/build/ -name '*.js' -exec sed -i -E \
            -e 's#([A-Za-z0-9_$]+)=([A-Za-z0-9_$]+)==="kiosk";if\(([A-Za-z0-9_$]+)[|][|]!([A-Za-z0-9_$]+)&&([A-Za-z0-9_$]+)\.footerHide\)return null#\1=\2==="kiosk";if(\3||\4||!\4\&\&\5.footerHide)return null#g' \
            {} +; \
        if [ "$(grep -lE '==="kiosk";if\([A-Za-z0-9_$]+[|][|][A-Za-z0-9_$]+[|][|]!' /usr/share/grafana/public/build/*.js 2>/dev/null | wc -l)" -lt 1 ]; then \
            echo "ERROR: kiosk-footer short-circuit injection produced no matches — Grafana's minified form may have changed" >&2; \
            exit 1; \
        fi; \
        echo "Stripped the kiosk/solo-panel footer"; \
    else \
        echo "STRIP_POWERED_BY is false; leaving the footer alone"; \
    fi

# Restore the `d k` kiosk-toggle keyboard shortcut on dashboard pages.
#
# Upstream issue: https://github.com/grafana/grafana/issues/96944
# Grafana 13's scenes-based dashboard has its own keyboard-shortcut module which
# does not include the `d k` binding; the legacy binding lives in a setup
# function that only the old render path ever called, so the shortcut is
# effectively gone.
#
# Fix: inject `d k` into the legacy keybinding service's
# clearAndInitGlobalBindings, which IS still called on every route change,
# including scene-rendered dashboards. That service is a long-lived singleton
# with the chrome service already wired in, so the binding works regardless of
# which render path drew the dashboard.
#
# Again NAME-AGNOSTIC. A previous version hardcoded the minified module alias
# for the theme-toggle import; a point release renamed it and moved the chunk,
# and the patch matched nothing. What is actually stable, and what the regex
# anchors on, is: the string literal "c r" (the theme-cycle shortcut), its (!0)
# argument, and the immediately-following method boundary. Anchoring on the
# preceding bind also proves it is an expression statement, so appending
# ",this.bind(...)" stays syntactically valid.
RUN find /usr/share/grafana/public/build/ -maxdepth 1 -name '*.js' -exec sed -i -E \
        -e 's|(this\.bind\("c r",\(\)=>\(0,[A-Za-z0-9_$]+\.[A-Za-z0-9_$]+\)\(!0\)\))\}bindGlobalEsc|\1,this.bind("d k",()=>this.chromeService.onToggleKioskMode())}bindGlobalEsc|g' \
        {} + && \
    if [ "$(grep -l 'this.bind("d k",()=>this.chromeService.onToggleKioskMode())' /usr/share/grafana/public/build/*.js 2>/dev/null | wc -l)" -lt 1 ]; then \
        echo "ERROR: 'd k' kiosk-toggle injection produced no matches — Grafana's minified form may have changed" >&2; \
        exit 1; \
    fi

# Re-hash and rename every *.js bundle the steps above mutated, then rewrite all
# cross-bundle references to the old filenames.
#
# This is the step that makes everything above safe to ship more than once.
# Webpack's cache contract is "the filename contains the content hash, so the
# URL is immutable" — which is what justifies the one-year max-age those assets
# are served with. Editing a bundle in place violates that contract: the content
# changed, the URL did not, and every browser that already has the old bundle
# keeps it. Rehashing restores the invariant.
#
# Idempotent: if nothing was mutated, nothing is renamed.
COPY rehash-mutated-bundles.py /usr/local/bin/rehash-mutated-bundles.py
RUN python3 /usr/local/bin/rehash-mutated-bundles.py

# Mobile kiosk toggle button — a floating control that restores the one-tap
# kiosk entry Grafana 11 had in the dashboard toolbar. Grafana 13 removed that
# button; kiosk is now two taps through the profile menu, and on a phone there
# is no way to LEAVE kiosk at all, because exiting is bound to Esc.
#
# Deliberately a standalone script rather than a bundle patch: adding a real
# React ToolbarButton would mean synthesizing a new element inside minified code
# (jsx factory alias, component module reference, chrome-service handle), all of
# which webpack re-mints per release. Every other patch in this file only
# rewrites an expression that already exists. This script touches no minified
# identifier — it rides the `?kiosk` URL parameter plus the `d k` binding
# injected above, and falls back to a URL toggle if that binding ever goes away.
#
# Copied AFTER the rehash step on purpose: it must not be seen by the pre-sed
# hash snapshot, the colour seds, or the rename pass.
COPY kiosk-button.js /usr/share/grafana/public/build/kiosk-button.js

# Inject the script with a CONTENT hash in its src, for the same reason spelled
# out for solarized.css below. The nonce placeholder is emitted for
# forward-compatibility: it is inert while CSP is off, and stays valid if
# content_security_policy is ever enabled.
RUN set -eu; \
    JS=/usr/share/grafana/public/build/kiosk-button.js; \
    INDEX=/usr/share/grafana/public/views/index.html; \
    if [ ! -s "$JS" ]; then \
        echo "ERROR: kiosk-button.js missing or empty at $JS" >&2; exit 1; \
    fi; \
    SUM="$(sha256sum "$JS" | cut -c1-16)"; \
    sed -i "s|</body>|  <script nonce=\"[[.Nonce]]\" src=\"public/build/kiosk-button.js?v=${SUM}\" type=\"text/javascript\" defer></script>\n  </body>|" "$INDEX"; \
    if [ "$(grep -c "kiosk-button.js?v=${SUM}" "$INDEX")" -ne 1 ]; then \
        echo "ERROR: kiosk-button.js script tag not injected exactly once into $INDEX" >&2; \
        exit 1; \
    fi; \
    echo "Injected kiosk-button.js?v=${SUM}"

# Cache-bust solarized.css by stamping a CONTENT hash into its href.
#
# Unlike the webpack bundles (content-hashed filenames, kept correct by the
# rehash step above), this stylesheet lives at a FIXED path that Grafana serves
# with a one-year max-age. Browsers, and any CDN in front of the deployment, pin
# the first copy they ever see and never revalidate — so a theme edit can be
# invisible on the surface people actually use, with no purge available if the
# CDN hostname is not a zone you control. This is not hypothetical; it happened,
# with an edge serving a months-old stylesheet straight through a user's hard
# refresh (CDNs generally ignore client no-cache on a HIT).
#
# Making the href content-addressed fixes it permanently: edit the theme, the
# hash changes, the URL changes, every cache is cold. The long max-age then
# becomes correct rather than a trap.
#
# The hash MUST be computed HERE, after the *.css sed steps mutated the file in
# place, so it describes the bytes actually served — which also means changing a
# sed RULE busts the cache, not just changing the source file. Content-derived
# and not a build timestamp, on purpose: an unchanged theme keeps its URL, so
# rebuilds do not needlessly evict a valid cache entry.
#
# index.html itself is served no-store, which is what makes this
# self-propagating: a new href reaches the browser on the very next page load.
RUN set -eu; \
    CSS=/usr/share/grafana/public/build/solarized.css; \
    INDEX=/usr/share/grafana/public/views/index.html; \
    SUM="$(sha256sum "$CSS" | cut -c1-16)"; \
    sed -i "s|href=\"public/build/solarized.css\"|href=\"public/build/solarized.css?v=${SUM}\"|" "$INDEX"; \
    if ! grep -q "public/build/solarized.css?v=${SUM}" "$INDEX"; then \
        echo "ERROR: failed to stamp solarized.css content hash into index.html" >&2; \
        exit 1; \
    fi; \
    echo "Stamped solarized.css?v=${SUM}"

USER grafana
