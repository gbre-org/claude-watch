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
#
# tini is the PID 1 supervisor (installed at /usr/bin/tini by the Debian
# package). Without an explicit PID 1 init, entrypoint.sh runs as PID 1
# itself — bash inherits no zombie-reap behaviour and no kernel-level
# default signal handlers, so grandchildren orphaned by daemonising
# helpers (nohup setsid claude-watch &, mcp/MCP subprocesses, etc.) pile
# up as <defunct> entries and `docker stop` signals can race or get
# dropped entirely. tini re-parents orphaned grandchildren, reaps them
# on SIGCHLD, and forwards SIGTERM/SIGINT cleanly into the entrypoint
# script's own trap handler. The image now wires tini in via
# ENTRYPOINT ["/usr/bin/tini", "--", ...] below.
RUN apt-get update && apt-get install -y --no-install-recommends \
        tmux \
        tini \
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
#
# Also pre-create ~/.local/bin so Claude Code's "native install" self-
# updater (which writes a `claude` symlink there pointing into
# ~/.local/share/claude/versions/<ver>/) has a uid-1000-writable target
# directory ready. Without it, the updater either no-ops silently or
# leaves a partially-written entry on first run.
RUN mkdir -p /home/hndrewaall/.local/share/claude /home/hndrewaall/.local/bin \
    && chown -R 1000:1000 /home/hndrewaall/.local

# Add ~/.local/bin to PATH at the image level. Claude Code's startup
# probes for a "native install" of itself at $HOME/.local/bin/claude
# (the path the in-process self-updater writes to). When that file
# exists but $HOME/.local/bin is NOT on PATH, claude prints a yellow
# warning on every launch:
#
#   Native installation exists but ~/.local/bin is not in your PATH. Run:
#   echo 'export PATH="$HOME/.local/bin:$PATH"' >> ~/.bashrc && source ~/.bash...
#
# Once a single in-container `claude` self-update has fired (or once an
# operator's host-side `~/.local/bin/claude` becomes visible — e.g. via
# a downstream image that bind-mounts ~/.local), the warning fires on
# every subsequent claude launch and clutters the splash banner of
# every fresh tmux pane.
#
# Fix: make the container's baseline PATH match the warning's hint.
# `~/.local/bin` precedes the npm-global /usr/bin so any self-updated
# binary takes precedence over the image-baked floor — that's the
# documented native-install precedence (compose.yml volume notes
# already describe this expectation; this ENV makes it actually true).
#
# ENV PATH propagates to every child process the container ever spawns
# — entrypoint.sh, every tmux pane it opens, every `docker compose
# exec` invocation, and `cwsr`'s respawned claude — without each shell
# having to re-source /etc/profile. Source-code research conventions
# (use the path the warning suggests, don't silence the check itself):
# Claude Code's native-install probe is correct; the container's PATH
# is what was wrong.
ENV PATH=/home/hndrewaall/.local/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin

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
COPY container/claude-watch.config.toml /etc/claude-watch/config.toml

# Managed-policy CLAUDE.md — baked at the standard Linux managed-CLAUDE.md
# location (/etc/claude-code/CLAUDE.md per the Claude Code memory docs:
# https://code.claude.com/docs/en/memory#deploy-organization-wide-claude-md).
# Claude Code loads this BEFORE the user-tier ~/.claude/CLAUDE.md and the
# project-tier <cwd>/CLAUDE.md and it CANNOT be excluded by user or project
# settings — that's the "managed policy" contract.
#
# The contents describe the in-container runtime so every session starts with
# a load-bearing reminder that it's running inside the container (not on the
# host), what's bind-mounted, what hooks no-op (cross-arch Mach-O), and where
# to read more. Source of truth lives at container/baked-CLAUDE.md in the
# claude-watch repo — rebuild the image to update.
#
# IMPORTANT mount interaction: the example compose stack
# (examples/compose/docker-compose.yml) optionally bind-mounts a host-side
# managed-settings dir over /etc/claude-code/ when the operator sets
# CLAUDE_HOST_MANAGED_SETTINGS_DIR. The compose default for that env var is
# /dev/null (graceful no-op), so an unset operator's session sees the baked
# CLAUDE.md unshielded. Operators who DO set the override take responsibility
# for whatever managed-settings dir they point at — their dir replaces the
# baked one wholesale.
COPY container/baked-CLAUDE.md /etc/claude-code/CLAUDE.md

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
COPY container/claude-tools.profile.sh /etc/profile.d/claude-tools.sh
RUN chmod 0644 /etc/profile.d/claude-tools.sh \
    && chown root:root /etc/profile.d/claude-tools.sh \
    && printf '\n# Pull in claude-watch bind-mounted CLI dirs for non-login shells.\nif [ -r /etc/profile.d/claude-tools.sh ]; then\n    . /etc/profile.d/claude-tools.sh\nfi\n' >> /etc/bash.bashrc

# exec-hook — settings.json hook safe-exec wrapper.
#
# When a user runs Claude Code inside this container with a host
# ~/.claude/settings.json that references hook binaries built for a
# different architecture (e.g. a Mac telemetry hook bind-mounted into a
# Linux container), Linux's exec() bounces them with "Exec format error"
# and Claude Code logs the failure on EVERY hook event. This shim
# inspects the target binary's magic bytes; ELF binaries are exec'd
# transparently, Mach-O / unknown formats no-op silently with a single
# stderr heads-up per target per container lifetime.
#
# Wiring: installed at /usr/local/lib/claude-hooks-shim/exec-hook AND
# /usr/local/bin/exec-hook (symlink) so settings.json hook entries can
# reference it as a bare command. The entrypoint also prepends the lib
# dir to PATH for completeness. PATH manipulation is the wiring choice
# here (over a build-time settings.json patch) because settings.json
# travels with the operator's host config — mutating it from inside the
# container would surprise the host. The shim is opt-in: the operator
# decides per hook whether to wrap their command in `exec-hook ...`.
#
# See container/hooks-shim/exec-hook for the full magic-byte table,
# dedup mechanism, and the future-scope IPC-forwarder TODO.
COPY container/hooks-shim/exec-hook /usr/local/lib/claude-hooks-shim/exec-hook
RUN chmod +x /usr/local/lib/claude-hooks-shim/exec-hook \
    && ln -s /usr/local/lib/claude-hooks-shim/exec-hook /usr/local/bin/exec-hook

# exec-hook-bridge — host-bash MCP bridge invoked by exec-hook when
# the operator opts in via CLAUDE_HOST_HOOK_BRIDGE=1. Marshals
# Mach-O / wrong-arch hook calls across the host-bash MCP server
# (mcp-host-bash at host.docker.internal:8766/mcp by default) so the
# REAL host binary runs and its exit code propagates back into the
# in-container claude session. Fail-safe: bridge errors fall back to
# the legacy silent-no-op contract so a misconfigured bridge never
# brings the session down. See container/hooks-shim/exec-hook-bridge
# for the full protocol + env-var surface.
COPY container/hooks-shim/exec-hook-bridge /usr/local/lib/claude-hooks-shim/exec-hook-bridge
RUN chmod +x /usr/local/lib/claude-hooks-shim/exec-hook-bridge \
    && ln -s /usr/local/lib/claude-hooks-shim/exec-hook-bridge /usr/local/bin/exec-hook-bridge

# generate-hooks-shim-settings — entrypoint-side companion to exec-hook.
# When CLAUDE_CONTAINER_REWRITE_HOOKS=1 is set in the container env, the
# entrypoint runs this helper to generate a container-local copy of
# ~/.claude/settings.json with every hook command wrapped in
# /usr/local/bin/exec-hook, then launches claude with `--settings
# /tmp/claude-shim/settings.json`. The output file overrides the host
# settings.json's hooks for the in-container session without mutating the
# host file. See container/hooks-shim/generate-hooks-shim-settings for the
# rewrite logic and the embedded test suite at
# container/hooks-shim/tests/generate-hooks-shim-settings.test.
COPY container/hooks-shim/generate-hooks-shim-settings /usr/local/bin/generate-hooks-shim-settings
RUN chmod +x /usr/local/bin/generate-hooks-shim-settings

# generate-project-mcp-json — sibling helper that writes a project-tier
# .mcp.json into CLAUDE_HOST_PROJECT_DIR. Claude Code v2.1.141 does NOT
# read MCP server definitions from any settings.json tier — it loads
# them from ~/.claude.json via a code path gated on the `user` tier
# being in --setting-sources. The entrypoint filters `user` out (to
# drop bare host hooks), which also suppresses MCP discovery. This
# helper sidesteps the problem by writing a standard project-tier
# .mcp.json (`<cwd>/.mcp.json`) inside the container, populated from
# ~/.claude.json mcpServers with each command wrapped in exec-hook.
# v21 workbot validation surfaced this; embedded tests at
# container/hooks-shim/tests/generate-project-mcp-json.test.
COPY container/hooks-shim/generate-project-mcp-json /usr/local/bin/generate-project-mcp-json
RUN chmod +x /usr/local/bin/generate-project-mcp-json

# Obligations gate hook scripts — bake the canonical PreToolUse +
# PostToolUse hook scripts from tools/hooks/ into /usr/local/bin/ so
# the in-container claude can install the obligations gate even when
# the host's settings.json has no obligations hook entries (or the
# user tier is filtered out via --setting-sources project,local).
#
# Wiring shape: generate-hooks-shim-settings INJECTS these hooks into
# the rewritten settings.json when CLAUDE_CONTAINER_OBLIGATIONS=1
# (default ON). The hooks resolve via $PATH so $BIN_DIR/obligations
# (bind-mounted from ~/repos/claude-watch/tools/obligations/obligations
# via the claude-tools.profile.sh) is the live CLI source. When the
# bind-mount is absent (stripped-down `docker run` without
# ~/repos/claude-watch), the hooks default-open per their built-in
# fail-safe (CLI-missing → empty allow).
#
# Hooks installed:
#   - pre-agent-queue-gate-hook            (PreToolUse Agent matcher)
#   - pre-tool-obligations-gate-hook       (PreToolUse * matcher)
#   - post-tool-obligations-update-hook    (PostToolUse * matcher)
#   - post-tool-mark-attachment-read-hook  (PostToolUse Read matcher)
#
# Each script is the canonical source of truth from tools/hooks/ in
# this repo (the same scripts `make install` symlinks into
# ~/bin/ on the host). Baking them at the container's
# /usr/local/bin/ ensures the obligations gate fires inside the
# container regardless of host state.
COPY tools/hooks/pre-agent-queue-gate-hook /usr/local/bin/pre-agent-queue-gate-hook
COPY tools/hooks/pre-tool-obligations-gate-hook /usr/local/bin/pre-tool-obligations-gate-hook
COPY tools/hooks/post-tool-obligations-update-hook /usr/local/bin/post-tool-obligations-update-hook
COPY tools/hooks/post-tool-mark-attachment-read-hook /usr/local/bin/post-tool-mark-attachment-read-hook
RUN chmod +x /usr/local/bin/pre-agent-queue-gate-hook \
            /usr/local/bin/pre-tool-obligations-gate-hook \
            /usr/local/bin/post-tool-obligations-update-hook \
            /usr/local/bin/post-tool-mark-attachment-read-hook

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
COPY container/bin/trust-workspace.py /usr/local/bin/trust-workspace
RUN chmod +x /usr/local/bin/trust-workspace

# audit-hooks — per-hook fate audit for the host's bind-mounted
# ~/.claude/settings.json. Walks every command, reports ok-elf /
# ok-script / silent-no-op / not-wrapped / missing / builtin. Useful
# for operators verifying that corp telemetry hooks land where they
# expect (typical: a macOS Mach-O hook bind-mounted into the Linux
# container reports `silent-no-op` because exec-hook intercepts; the
# operator can then decide to ship a Linux build, bridge via
# host-bash MCP, or accept that telemetry doesn't fire in the
# container). The script is observability ONLY — it does not mutate
# settings.json and does not fire the hooks. Embedded `--test` mode
# runs the self-tests; integrated into container/tests/audit-hooks.test
# for the make-test loop.
COPY container/bin/audit-hooks.py /usr/local/bin/audit-hooks
RUN chmod +x /usr/local/bin/audit-hooks

# cwsr — in-container self-restart for the claude CLI. Upgrades
# @anthropic-ai/claude-code via npm-global and uses `tmux respawn-pane
# -k` to roll the inner claude process WITHOUT requiring a host-side
# `docker compose restart`. Operator workflow: a new claude version
# ships; from inside the in-container tmux session (or from another
# container shell via `docker compose exec`), run `cwsr` and the inner
# pane swaps to the new version while the tmux session, MCP state, and
# named-volume claude versions/ dir all survive.
COPY container/bin/cwsr /usr/local/bin/cwsr
RUN chmod +x /usr/local/bin/cwsr

# self-clear — fresh-context-injection helper that the in-container
# claude-watch daemon shells out to when its policy module decides to
# /clear the wedged pane. The script (Python 3) sends Escape+dd+i+text
# keystrokes to the in-container tmux pane via `tmux send-keys` and
# polls for the post-/clear prompt to come back idle.
#
# Baked into the image at /usr/local/bin/self-clear so the daemon's
# `Command::new("self-clear")` invocation in
# claude-watch/src/policy.rs (spawn_immediate_clear /
# spawn_deferred_clear) resolves regardless of whether the operator
# bind-mounts host ~/bin into the container. The example compose stack
# DOES bind-mount ~/bin and the host symlink at ~/bin/self-clear ->
# tools/watchers/self-clear would normally win /usr/local/bin
# precedence, but operator-stripped containers (which don't bind-mount
# ~/bin) need the baked copy as the floor so the wedged-pane recovery
# path doesn't silently fail with "command not found". The script is
# self-contained Python (no external dependencies beyond stdlib + tmux
# + claude-watch on PATH).
#
# Source-of-truth lives at container/bin/self-clear; the host-side
# tools/watchers/self-clear is a symlink into this file so both the
# host's `~/bin/self-clear -> tools/watchers/self-clear` chain and the
# container build (whose context is container/) end up running the
# same bytes.
COPY container/bin/self-clear /usr/local/bin/self-clear
RUN chmod +x /usr/local/bin/self-clear

# Baked skills / agents / watchers — three top-level dirs in the repo
# (container/skills/, container/agents/, container/watchers/) that ship
# in-container slash commands, custom agent prompts, and background
# watcher launchers respectively. See each dir's README.md for the
# convention, the on-disk schema (watchers), and the add-a-new-entry
# walkthrough.
#
# Wiring shape:
#
#   container/skills/   -> /etc/claude-code/skills/   (canonical bake path,
#                                                       documented for inspection)
#                       -> /etc/claude-code/plugin/commands/
#                                                      (where Claude Code's
#                                                       plugin loader reads
#                                                       slash commands)
#   container/agents/   -> /etc/claude-code/agents/   (canonical bake path)
#                       -> /etc/claude-code/plugin/agents/
#                                                      (plugin loader reads
#                                                       custom agents here)
#   container/watchers/ -> /etc/claude-code/watchers/ (probed by the
#                                                       /start-watchers skill;
#                                                       NOT a plugin loader path)
#   container/plugin/.claude-plugin/plugin.json
#                       -> /etc/claude-code/plugin/.claude-plugin/plugin.json
#                          (manifest the plugin loader requires; sets the
#                           plugin namespace to `claude-container`, so
#                           skills land as /claude-container:<name>)
#
# entrypoint.sh appends `--plugin-dir /etc/claude-code/plugin` to the
# claude argv so the plugin loader picks up the baked commands/agents at
# session start. The two-target COPY (skills -> {skills,plugin/commands};
# agents -> {agents,plugin/agents}) keeps the canonical paths
# inspectable AND the plugin-loader paths populated, without forcing the
# operator to think about the plugin layout. Watchers do NOT land in the
# plugin dir because they aren't slash commands or agents — they're
# shell scripts the agent runs via the Bash tool with run_in_background.
COPY container/skills/ /etc/claude-code/skills/
COPY container/skills/ /etc/claude-code/plugin/commands/
COPY container/agents/ /etc/claude-code/agents/
COPY container/agents/ /etc/claude-code/plugin/agents/
COPY container/watchers/ /etc/claude-code/watchers/
COPY container/plugin/.claude-plugin/plugin.json /etc/claude-code/plugin/.claude-plugin/plugin.json

# Entrypoint script — tmux session bootstrap + env passthrough + signal handling.
# Phase 1e (q-2026-05-11-8a96): pane 1 now runs the real claude-watch daemon
# instead of the Phase 0 placeholder bash banner. Pane 1 keeps itself open with
# a shell on daemon failure so errors surface on `tmux attach`.
COPY container/entrypoint.sh /usr/local/bin/claude-tmux-entrypoint
RUN chmod +x /usr/local/bin/claude-tmux-entrypoint

# Drop privileges before runtime — root in a bind-mounted layout breaks host perms.
USER hndrewaall
WORKDIR /workspace

# tini as PID 1: forwards SIGTERM/SIGINT to the entrypoint script's
# `trap cleanup TERM INT` handler, reaps zombie grandchildren (the
# nohup setsid claude-watch daemon spawned by the entrypoint, MCP
# subprocesses, anything else that orphans), and exits with the
# entrypoint's status. See the apt-get layer above for the why.
# Future-tier alternatives (s6-overlay, process-compose) live in
# container/README.md "Future-tier supervision".
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/claude-tmux-entrypoint"]
