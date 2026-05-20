# ttyd web-console for the claude-container tmux session.
#
# Built FROM debian:bookworm-slim so the in-container tmux client
# version matches the tmux server running in the claude-container
# sibling service exactly. Both use the bookworm `tmux` apt package
# (currently 3.3a). This matters because the tmux wire protocol is
# strict about client/server version match — attaching a tmux 3.6
# client (Alpine 3.23's package) to a tmux 3.3a server (Debian
# bookworm's package) fails with the misleading error
# "open terminal failed: not a terminal" instead of a version-skew
# message. See tmux issue #4356 for the upstream confirmation.
#
# We still copy the prebuilt static ttyd binary out of upstream
# tsl0922/ttyd:1.7.7 (the C backend hasn't changed for our use case
# and the binary is statically linked so it works fine on bookworm).
# But the frontend HTML — which is what xterm.js, the websocket
# client, and the reconnect logic live in — is rebuilt from our
# ttyd fork at https://github.com/hndrewaall/ttyd. That fork carries
# an auto-reconnect-with-exponential-backoff + visibilitychange
# handler patch that keeps the browser session resilient to laptop
# sleep / VPN flap / brief network drops (the upstream behaviour is
# "Press Enter to Reconnect" after any non-1000 close or any single
# socket error, which is unfriendly for a long-lived in-browser
# console). Upstream PR: https://github.com/tsl0922/ttyd/pull/1536.
# Once that merges we'll swap the fork pin back to tsl0922/ttyd:main.
#
# Pin a concrete fork commit for reproducibility. Bump when the fork
# advances.
FROM tsl0922/ttyd:1.7.7 AS upstream

# -------------------------------------------------------------------
# Stage: build the ttyd frontend (xterm.js + reconnect/visibility
# patch) from our fork.
#
# ttyd serves a single self-contained index.html (~730 KB, all CSS +
# JS inlined) — `yarn build` runs webpack to produce dist/index.html
# and then `npx gulp inline` rolls every asset into a single
# dist/inline.html that we hand to ttyd via `-I` at runtime. This
# bypasses the C-binary-embedded copy entirely, so we can ship our
# fork's frontend on top of any upstream-prebuilt binary.
# -------------------------------------------------------------------
FROM node:20-bookworm-slim AS frontend-builder
ARG TTYD_FORK_REF=c2a942d4486236bd8c9e3d4d62f3818fc4589eb4
RUN apt-get update && apt-get install -y --no-install-recommends \
        git \
        ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && corepack enable
WORKDIR /build
RUN git clone --depth 50 https://github.com/hndrewaall/ttyd.git ttyd \
    && cd ttyd \
    && git checkout ${TTYD_FORK_REF}
WORKDIR /build/ttyd/html
RUN yarn install --immutable
RUN yarn build && npx gulp inline
# /build/ttyd/html/dist/inline.html is the single-file self-contained
# bundle we hand to ttyd via `-I` in the runtime stage below.

# -------------------------------------------------------------------
# Stage: apply autodark CSS + theme-swap JS to the fork-built HTML.
#
# Same injection pipeline as before — the patched HTML adds:
#   - A @media (prefers-color-scheme) <style> block painting body /
#     html background Solarized base03 (dark) or base3 (light) so the
#     page chrome around xterm.js matches the system color-scheme.
#   - A <script> that flips xterm.js's runtime theme to Solarized
#     dark/light matching prefers-color-scheme, with a setInterval
#     reapply to defend against ttyd's post-connect SET_PREFERENCES
#     overwrite (full race details in inject-autodark.py).
#
# Inputs/outputs are now files on disk rather than a live HTTP fetch
# from a backgrounded ttyd instance (the fork-build above gives us
# the canonical HTML directly).
# -------------------------------------------------------------------
FROM debian:bookworm-slim AS html-builder
RUN apt-get update && apt-get install -y --no-install-recommends \
        python3 \
    && rm -rf /var/lib/apt/lists/*
COPY inject-autodark.py /tmp/inject-autodark.py
COPY --from=frontend-builder /build/ttyd/html/dist/inline.html /tmp/index.upstream.html
RUN set -eux; \
    python3 /tmp/inject-autodark.py /tmp/index.upstream.html /tmp/index.patched.html; \
    grep -q 'prefers-color-scheme' /tmp/index.patched.html; \
    grep -q 'autodark-injected' /tmp/index.patched.html; \
    grep -q 'paste-intercept-injected' /tmp/index.patched.html; \
    grep -q 'paste-image-button-injected' /tmp/index.patched.html; \
    grep -q 'cw-paste-image-btn' /tmp/index.patched.html; \
    grep -q 'paste-event-handler-injected' /tmp/index.patched.html

# -------------------------------------------------------------------
# Runtime stage.
# -------------------------------------------------------------------
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
        tmux \
        bash \
        ncurses-term \
        tini \
        ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=upstream /usr/bin/ttyd /usr/bin/ttyd
COPY --from=html-builder /tmp/index.patched.html /usr/local/share/ttyd/index.html

ENTRYPOINT ["/usr/bin/tini", "--"]
