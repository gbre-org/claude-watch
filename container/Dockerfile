# claude-container — Phase 1b image
#
# Multi-stage build:
#   Stage 1 ("claude-watch-builder"): rust:1-bookworm. Clones the public
#     claude-watch source, pins to a known-good commit, runs cargo build
#     --release. Output: /build/target/release/claude-watch.
#   Stage 2 (final): debian:bookworm-slim, same as Phase 0b. Copies the
#     built binary from stage 1 into /usr/local/bin/claude-watch.
#
# Builder + final share the bookworm libc to avoid glibc-mismatch on copy
# (see project doc §5 Phase 1 decision 4 / failure-mode list).
#
# Pinned claude-watch revision: ccd1599bb9d05ebe36703488a22ba62b321ff6f2
# (origin/main as of 2026-05-11; bump deliberately in future phases).

# --- Stage 1: build claude-watch from source -------------------------------
FROM rust:1-bookworm AS claude-watch-builder

ARG CLAUDE_WATCH_REPO=https://github.com/hndrewaall/claude-watch.git
ARG CLAUDE_WATCH_REF=ccd1599bb9d05ebe36703488a22ba62b321ff6f2

WORKDIR /build

# Shallow-clone for build speed, then check out the pinned commit. The
# fetch-depth=1 + explicit fetch-by-sha pattern works against GitHub's
# git protocol v2 (uploadpack.allowReachableSHA1InWant defaults on).
RUN git init . \
    && git remote add origin "${CLAUDE_WATCH_REPO}" \
    && git fetch --depth 1 origin "${CLAUDE_WATCH_REF}" \
    && git checkout FETCH_HEAD

# claude-watch's Cargo.toml has `profile.release = { strip = true, lto = true }`,
# so the resulting binary is already stripped + LTO-optimised.
RUN cargo build --release \
    && /build/target/release/claude-watch --help >/dev/null

# --- Stage 2: runtime image ------------------------------------------------
FROM debian:bookworm-slim

# Non-interactive apt and consistent locale for a tmux-driven UI.
ENV DEBIAN_FRONTEND=noninteractive \
    LANG=C.UTF-8 \
    LC_ALL=C.UTF-8

# Base toolchain: tmux for the entrypoint UI, build-essential for native npm deps,
# python3 because claude-watch and many host tools shell out to python,
# vim for in-container emergency edits, curl/ca-certs for NodeSource bootstrap.
# Host-specific shell-history integrations are intentionally omitted —
# layer them on via a downstream image if you need them.
RUN apt-get update && apt-get install -y --no-install-recommends \
        tmux \
        git \
        curl \
        ca-certificates \
        build-essential \
        python3 \
        python3-pip \
        vim \
        gnupg \
        procps \
    && rm -rf /var/lib/apt/lists/*

# Node.js via NodeSource (LTS 20.x) — Claude Code CLI requires Node >= 20.
# Bookworm's apt nodejs is too old. In Phase 1+ this install path is replaced
# by a bind-mount of the host's npm-global tree, so keep this layer minimal.
RUN curl -fsSL https://deb.nodesource.com/setup_20.x | bash - \
    && apt-get install -y --no-install-recommends nodejs \
    && rm -rf /var/lib/apt/lists/*

# Create hndrewaall user with uid/gid 1000 — MUST match host hndrewaall so
# bind-mounts (~/.claude, ~/repos, $PWD) round-trip without root-owned files.
RUN groupadd --gid 1000 hndrewaall \
    && useradd --uid 1000 --gid 1000 --shell /bin/bash --create-home hndrewaall

# Pre-create the named-volume mountpoint with the right ownership BEFORE
# the volume is attached at runtime. When docker initializes a fresh named
# volume on a directory that already exists in the image, it COPIES the
# directory's permissions + ownership into the new volume. Without this
# step, the volume defaults to root:root mode 0755 and the in-container
# hndrewaall (uid 1000) cannot write into it — auto-updates fail with
# Permission denied. Phase 1d (q-2026-05-11-c0b9). Empirical proof: the
# first wired-up test of the named volume failed with EACCES on `touch`
# until this pre-creation step landed.
RUN mkdir -p /home/hndrewaall/.local/share/claude \
    && chown -R 1000:1000 /home/hndrewaall/.local

# Pre-create the tmux default socket dir for uid 1000 with the
# permissions tmux requires (owner-writable, not world-writable).
# This is the path tmux uses on the default socket when the server
# is started by uid 1000. examples/compose/docker-compose.yml mounts
# a named volume here so a sibling ttyd service can attach to the
# same in-container session; mounting on a path that already exists
# in the image with correct ownership propagates those perms into
# the fresh named volume on first use (Docker's bind-mount copy-up
# behaviour). Without this, the volume directory is root-owned 0755
# and tmux refuses to start with "directory has unsafe permissions".
RUN install -d -m 0700 -o 1000 -g 1000 /tmp/tmux-1000

# Install Claude Code CLI globally. Package name confirmed via `npm view`
# 2026-05-11. In Phase 1+ this is replaced by a bind-mount of host npm-global,
# so prefer installing into a path a bind-mount can shadow cleanly.
RUN npm install -g @anthropic-ai/claude-code

# Bake the claude-watch Rust binary in from stage 1. Glibc compatibility is
# guaranteed because the builder is `rust:1-bookworm` and this stage is
# `debian:bookworm-slim` — same Debian release, same libc.
# claude-watch has no `--version` flag; we verify by exercising `--help`,
# which exits 0 only if the binary loads and clap parses its CLI.
COPY --from=claude-watch-builder /build/target/release/claude-watch /usr/local/bin/claude-watch
RUN claude-watch --help >/dev/null

# Container-tailored claude-watch config — pins the daemon to the in-container
# tmux session `claude-container:0.0` (the claude pane created by
# entrypoint.sh), points all writes at uid-1000-writable paths (/tmp/, ~/.cache),
# and disables host-only features (watcher_monitor, auto_update, reauth, hybrid,
# task_watch) that have no in-container backing yet (Phase 2 work). The host's
# ~/.config/claude-watch/config.toml is NOT bind-mounted by the wrapper, so
# without this file the daemon would FATAL on "no config file found". Phase 1e.
COPY claude-watch.config.toml /etc/claude-watch/config.toml

# Bake CLAUDE_WATCH_CONFIG into the image-level env (Phase 1.5 fix #3,
# q-2026-05-11-d7c0). entrypoint.sh exports it for its own children (pane 0/1
# under tmux), but `docker exec <container> claude-watch ...` spawns a fresh
# shell that inherits ONLY image-level ENV — not the entrypoint's runtime
# export. Without this, ad-hoc `docker exec` invocations of claude-watch fall
# back to the daemon's "no config file found" path and either FATAL or load
# wrong defaults. Phase 1f §8 "Bug #3" in the project doc. The entrypoint's
# `export CLAUDE_WATCH_CONFIG=${CLAUDE_WATCH_CONFIG:-...}` line is now a
# defensive no-op (the env var is already set) but kept for clarity.
ENV CLAUDE_WATCH_CONFIG=/etc/claude-watch/config.toml

# /etc/profile.d/claude-tools.sh — discoverability for bind-mounted CLIs.
#
# The container bakes ONLY the Claude Code CLI and the claude-watch Rust
# binary. The Python CLIs that ship in the claude-watch source tree
# (session-task, claude-event, obligations) are NOT installed into the
# image — they live next to their source and are discoverable on PATH
# only when the operator bind-mounts `~/repos/claude-watch` into the
# container (as the example compose does).
#
# This profile-d fragment looks for those tool dirs at login / new-shell
# time and prepends them to PATH if present. Graceful no-op when the
# bind-mount is absent (e.g. on a stripped-down `docker run`).
#
# Login shells source /etc/profile -> /etc/profile.d/*.sh; interactive
# non-login shells (e.g. `docker compose exec <c> bash`) source
# /etc/bash.bashrc, so we also append a one-liner there that pulls this
# same fragment in.
COPY claude-tools.profile.sh /etc/profile.d/claude-tools.sh
RUN chmod 0644 /etc/profile.d/claude-tools.sh \
    && chown root:root /etc/profile.d/claude-tools.sh \
    && printf '\n# Pull in claude-watch bind-mounted CLI dirs for non-login shells.\nif [ -r /etc/profile.d/claude-tools.sh ]; then\n    . /etc/profile.d/claude-tools.sh\nfi\n' >> /etc/bash.bashrc

# trust-workspace — pre-seeds projects[<workspace>].hasTrustDialogAccepted=true
# in ~/.claude.json so the in-container claude binary skips the first-launch
# "Quick safety check" trust dialog. Invoked by entrypoint.sh before tmux
# launches claude; safe to invoke ad-hoc later (e.g. via
# `docker compose exec claude-container trust-workspace /custom/path`) if a
# downstream image runs in a different cwd. The script is a graceful no-op
# when the config file is missing or the bind-mount is read-only, so a
# stripped-down `docker run` without ~/.claude.json mounted still works
# (operator sees the trust prompt once and dismisses it manually). See
# container/bin/trust-workspace.py for the full safety contract and the
# embedded `--test` mode (11 unit tests, including the Linux file-bind-mount
# EBUSY fallback path).
COPY bin/trust-workspace.py /usr/local/bin/trust-workspace
RUN chmod +x /usr/local/bin/trust-workspace

# Entrypoint script — tmux session bootstrap + env passthrough + signal handling.
# Phase 1e (q-2026-05-11-8a96): pane 1 now runs the real claude-watch daemon
# instead of the Phase 0 placeholder bash banner. Pane 1 keeps itself open with
# a shell on daemon failure so errors surface on `tmux attach`.
COPY entrypoint.sh /usr/local/bin/claude-tmux-entrypoint
RUN chmod +x /usr/local/bin/claude-tmux-entrypoint

# Drop privileges before runtime — root in a bind-mounted layout breaks host perms.
USER hndrewaall
WORKDIR /workspace

ENTRYPOINT ["/usr/local/bin/claude-tmux-entrypoint"]
