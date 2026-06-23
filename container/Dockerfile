# claude-container — Phase 1b image
#
# Multi-stage build:
#   Stage 1 ("claude-watch-builder"): rust:1-bookworm. COPYs the
#     claude-watch source from the build context (the repo root the
#     wrapper invokes `docker build` with) and runs cargo build
#     --release. Output: /build/target/release/claude-watch.
#   Stage 2 (final): debian:bookworm-slim, same as Phase 0b. Copies the
#     built binary from stage 1 into /usr/local/bin/claude-watch.
#
# Builder + final share the bookworm libc to avoid glibc-mismatch on copy
# (see project doc §5 Phase 1 decision 4 / failure-mode list).
#
# Build-context shape: the claude-tmux / `docker compose build` invocations
# pass the repo root as the context (compose.yml + examples/compose/
# docker-compose.yml both use `context: ../..` / `context: ..` against
# this Dockerfile). The COPY line below absorbs whatever HEAD the
# operator's local working tree is on — the `make compose-build` /
# `claude-tmux build` flow is responsible for doing a `git pull --rebase`
# beforehand if the operator wants origin/main rather than a local
# branch. No hand-drafted SHA bump PR is required when main moves; the
# Dockerfile's Cargo layer invalidates naturally on Cargo.lock / src/
# changes.
#
# Reproducibility: a `GIT_SHA` build-arg is plumbed through to a LABEL on
# the final image. The build wrapper / Makefile target passes
# `--build-arg GIT_SHA=$(git rev-parse HEAD)` so `docker inspect IMAGE
# --format '{{ index .Config.Labels "claude_watch_sha" }}'` reports which
# revision was baked. Default empty so a bare `docker build .` (no
# wrapper) still works.

# --- Stage 1: build claude-watch from source -------------------------------
FROM rust:1-bookworm AS claude-watch-builder

WORKDIR /build

# Build-time git identity for the `claude_watch_build_info` Prometheus gauge.
#
# build.rs stamps CW_GIT_COMMIT / CW_GIT_PR into the binary. It prefers live
# git, but `.dockerignore` prunes `.git/` from this build context (see the
# repo-root .dockerignore — shipping the pack files to the daemon is wasteful),
# so live git is UNAVAILABLE here and would otherwise fall back to
# `commit="unknown"`. We therefore plumb the host-computed commit + PR in as
# build args and expose them as env vars build.rs reads as its fallback. The
# Makefile (compose-build / container-build) computes these on the host where
# git IS present; a bare `docker build` with no args cleanly degrades to
# "unknown" (same as before).
ARG CW_BUILD_COMMIT=
ARG CW_BUILD_PR=
ENV CW_BUILD_COMMIT=${CW_BUILD_COMMIT} \
    CW_BUILD_PR=${CW_BUILD_PR}

# COPY the working tree from the build context (the repo root). The
# .dockerignore at the repo root prunes target/, __pycache__/, .git/,
# editor swap files, etc. — only the source files Cargo + the rest of
# the multi-stage build actually need land in this layer. The layer is
# invalidated when any non-ignored file changes, which matches "I want
# to rebuild whenever I edit Rust source" without requiring a
# hand-drafted SHA arg per build.
COPY . /build

# claude-watch's Cargo.toml has `profile.release = { strip = true, lto = true }`,
# so the resulting binary is already stripped + LTO-optimised.
RUN cargo build --release \
    && /build/target/release/claude-watch --help >/dev/null

# --- Stage 2: runtime image ------------------------------------------------
FROM debian:bookworm-slim

# Reproducibility tag — the build wrapper / Makefile target passes
# --build-arg GIT_SHA=$(git rev-parse HEAD) so `docker inspect IMAGE
# --format '{{ index .Config.Labels "claude_watch_sha" }}'` reports the
# exact local revision baked into this image. Default empty so a bare
# `docker build .` (no wrapper, no SHA passed) still produces a working
# image — the LABEL just reads as an empty string in that case.
ARG GIT_SHA=
LABEL claude_watch_sha="${GIT_SHA}"

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
# Service supervision (PID 1 init) is handled by process-compose, a
# single static Go binary installed below from upstream. We don't ship
# tini anymore — process-compose handles SIGCHLD reaping and
# SIGTERM/SIGINT forwarding to its supervised children, and provides
# the declarative restart-on-failure / depends_on contract that tini
# (a single-process supervisor) couldn't. See
# /opt/claude-container/process-compose.yml for the service declarations
# and container/README.md "PID 1 supervisor" for the migration
# rationale.
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
        jq \
        iproute2 \
        procps \
        inotify-tools \
        cron \
        sudo \
    && rm -rf /var/lib/apt/lists/*

# Passwordless-sudo for uid 1000 to launch the cron daemon. The
# entrypoint runs as uid 1000 (per the image-level USER directive
# below) but cron must run as root to read /etc/cron.d/, manage
# /var/spool/cron/crontabs/, and setuid() into the user named in
# each crontab line. Granting passwordless sudo for ONLY the cron
# binary keeps the blast radius minimal: the in-container claude /
# operator can't sudo arbitrary commands, only start the cron
# daemon (which the entrypoint already does at boot). The sudoers
# entry uses NOPASSWD because the entrypoint is non-interactive and
# nothing in the container has a hndrewaall password set anyway.
#
# Why not "drop USER, exec as root, gosu to uid 1000 at the end":
# the rest of the image / entrypoint / spawned watchers + daemons
# is built assuming the entrypoint and every child are uid 1000
# (claude-watch daemon, watcher supervisor, MCP subprocesses,
# tmux session, npm-global). A USER flip would require auditing
# every spawn site for re-entrancy as the wrong uid. The narrow
# sudo-for-cron carve-out is the minimum-change shape that lets
# cron start without disturbing any existing uid-1000 invariant.
# CRON_NOFORK tag + the `Defaults!CRON_NOFORK !pam_session, !pam_setcred`
# line are the crux of clean container teardown (and therefore of
# self-redeploy). By DEFAULT, Debian's sudo links the sudoers policy
# plugin against PAM with pam_session + pam_setcred enabled. Per sudo(8)
# "Process model": the sudoers plugin defines a close function whenever
# pam_session or pam_setcred is enabled, and "if the policy plugin does
# not define a close function, sudo will execute the command directly
# instead of calling fork(2) first." So with the defaults, `sudo -n
# /usr/sbin/cron -f` does NOT exec cron — it FORKS it and stays alive as
# a monitor. process-compose then tracks the `sudo` wrapper PID, not the
# root cron child. On `docker stop` / `up --force-recreate`,
# process-compose SIGKILLs the sudo wrapper, which cannot forward
# SIGKILL, so the forked ROOT cron survives, pins the container netns +
# the shared tmux-socket named volume, and the freshly-recreated
# container wedges (a full `docker compose down` papered over the leak by
# tearing the whole project + netns down).
#
# Disabling pam_session + pam_setcred for the tagged cron command means
# the sudoers plugin defines NO close function, so sudo `execve()`s cron
# DIRECTLY — the process process-compose launches and signals IS the
# cron daemon. SIGTERM/SIGKILL reach cron with no orphaned wrapper, so
# nothing survives to pin the netns/socket and force-recreate is clean.
#
# `Defaults!"/usr/sbin/cron -f -L 15"` scopes the override to the exact
# argv the supervisor uses; the bare `/usr/sbin/cron` Cmnd_Alias keeps
# the legacy NOPASSWD grant for any other invocation.
RUN printf '%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n' \
        'Cmnd_Alias CRON_NOFORK = /usr/sbin/cron -f -L 15' \
        'Defaults!CRON_NOFORK !pam_session, !pam_setcred, !use_pty' \
        'hndrewaall ALL=(root) NOPASSWD: CRON_NOFORK' \
        'hndrewaall ALL=(root) NOPASSWD: /bin/kill' \
        'hndrewaall ALL=(root) NOPASSWD: /usr/bin/tee /etc/cron.d/00-env' \
        'hndrewaall ALL=(root) NOPASSWD: /bin/chmod 0644 /etc/cron.d/00-env' \
        'hndrewaall ALL=(root) NOPASSWD: /usr/bin/tee /etc/cron.d/private-*' \
        'hndrewaall ALL=(root) NOPASSWD: /bin/chmod 0644 /etc/cron.d/private-*' \
        > /etc/sudoers.d/hndrewaall-cron \
    && chmod 0440 /etc/sudoers.d/hndrewaall-cron \
    && visudo -cf /etc/sudoers.d/hndrewaall-cron

# Passwordless-sudo for uid 1000 to run apt / dpkg at RUNTIME. There is no
# blanket "no-sudo" policy inside this container: the host-side aversion to
# sudo is purely about macOS Touch ID / fingerprint prompts, which do NOT
# exist in the Linux container. So baking a NOPASSWD carve-out for the
# package-manager binaries lets an in-session `sudo apt-get install <pkg>`
# work non-interactively (no password, no prompt) — handy when a one-off
# tool is missing from the baked image and the operator doesn't want to
# rebuild + redeploy just to try it. Scoped to the apt/dpkg binaries only,
# mirroring the cron carve-out's minimal-blast-radius shape. The package
# DB / installed tools do NOT persist across `docker compose up
# --force-recreate` (no named volume backs /usr or /var/lib/dpkg), so a
# tool that proves durably useful should still be added to the apt layer
# above and the image rebuilt — this carve-out is for runtime convenience,
# not a persistence mechanism.
RUN printf '%s\n' \
        'hndrewaall ALL=(root) NOPASSWD: /usr/bin/apt-get, /usr/bin/apt, /usr/bin/dpkg' \
        > /etc/sudoers.d/hndrewaall-apt \
    && chmod 0440 /etc/sudoers.d/hndrewaall-apt \
    && visudo -cf /etc/sudoers.d/hndrewaall-apt

# cron — Debian's vixie-cron daemon for in-container recurring jobs.
#
# The image bakes a /etc/cron.d/cw-default entry containing the two
# default-public cron lines every claude-watch deployment needs:
#   - active-agents JSON state writer (consumed by queue-minisite,
#     work-queue-exporter, cron-queue-check).
#   - claude-watch metrics emit.
# Both run as uid 1000 (hndrewaall) and write to /var/lib/claude-watch/
# (the named-volume-backed state dir) and /tmp/ respectively.
#
# Operator-side jobs (host-specific PR-watch loops, custom event
# emitters that reference absolute paths to host-side scripts, etc.)
# DO NOT belong in the baked default — every operator's wiring is
# different. The /etc/cron.d/private path is reserved for operator
# bind-mounts. compose.yml has a commented-out volume declaration
# showing the canonical override shape:
#
#   services:
#     claude-container:
#       volumes:
#         - "/host/path/to/private-cron.d/:/etc/cron.d/private/:ro"
#
# The supervisor starts cron at container boot regardless of whether
# /etc/cron.d/private is populated — an empty dir is a no-op (zero
# jobs to run from there) but the baked /etc/cron.d/cw-default
# entries still fire. Jobs run as whichever user the crontab entry
# specifies (the cron daemon runs as root in the container's PID
# namespace so it can setuid() into the user named in each line).
#
# Entries that drop claude-event JSON into the spool (via the
# in-container `claude-event` CLI, which honors CLAUDE_EVENT_QUEUE)
# flow to the in-container claude-event-watch watcher automatically —
# the cron daemon, the spool dir, and the watcher all live in the
# same container so no cross-process IPC is needed.

# Bake the default cron entries. /etc/cron.d/ files must be owned by
# root and mode 0644 (cron rejects world-writable or non-root-owned
# files in /etc/cron.d/). The COPY chowns to root:root by default;
# the trailing chmod 0644 belt-and-suspenders against operator-side
# umask drift.
COPY container/cron.d/cw-default /etc/cron.d/cw-default
RUN chmod 0644 /etc/cron.d/cw-default \
    && chown root:root /etc/cron.d/cw-default

# Reserve /etc/cron.d/private as the operator-bind-mount slot. The dir
# exists with mode 0755 + root:root ownership so a `:ro` bind-mount
# over it works without docker auto-creating a fresh root-owned dir
# with wrong perms on first launch (same auto-creation gotcha
# /home/hndrewaall/.config got hit by). Empty by default — operators
# drop their per-host cron.d files into the bind-mounted path. cron's
# default /etc/cron.d/ behaviour reads the directory recursively;
# entries inside /etc/cron.d/private/ are picked up just like baked
# entries.
RUN mkdir -p /etc/cron.d/private \
    && chmod 0755 /etc/cron.d/private \
    && chown root:root /etc/cron.d/private

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

# process-compose — service supervisor (replaces tini at PID 1).
#
# Escalation past the README's "tini sufficient until three services"
# threshold rule: claude-watch daemon + crond are the long-running
# services living alongside the foreground tmux session. tini reaps
# zombies + forwards signals but doesn't
# model dependencies, restart on failure, or centralize logs.
# process-compose declares those service contracts in
# /opt/claude-container/process-compose.yml and runs them with restart
# policies + depends_on ordering.
#
# Static binary release from upstream (single-binary Go); no apt
# package available. Pinned by version + sha-checked via the upstream
# `process-compose_checksums.txt`. Linux amd64 + arm64 supported via
# runtime `uname -m` dispatch — same pattern the VS Code CLI install
# above uses.
#
# Source-of-truth URL:
#   https://github.com/F1bonacc1/process-compose/releases/download/v1.110.0/process-compose_linux_<arch>.tar.gz
ARG PROCESS_COMPOSE_VERSION=v1.110.0
RUN case "$(uname -m)" in \
        x86_64)  PC_ARCH=amd64 ;; \
        aarch64) PC_ARCH=arm64 ;; \
        *) echo "unsupported arch for process-compose: $(uname -m)" >&2; exit 1 ;; \
    esac \
    && curl -fsSL "https://github.com/F1bonacc1/process-compose/releases/download/${PROCESS_COMPOSE_VERSION}/process-compose_linux_${PC_ARCH}.tar.gz" \
        -o /tmp/process-compose.tar.gz \
    && tar -xzf /tmp/process-compose.tar.gz -C /usr/local/bin/ process-compose \
    && chmod +x /usr/local/bin/process-compose \
    && rm -f /tmp/process-compose.tar.gz \
    && /usr/local/bin/process-compose version >/dev/null

# Pre-create /var/lib/claude-watch with uid-1000 ownership. This is the
# canonical in-container state dir for claude-watch — active-agents.json
# (the per-minute JSON state file the active-agents writer cron emits),
# plus any other daemon state that needs to survive container recreate,
# lands here. compose.yml mounts a backing volume (named volume by
# default, or a host bind-mount when CW_STATE_PATH is set) at this path.
#
# Same Docker bind-mount-perm-inheritance rationale as
# /home/hndrewaall/.local above: when docker initializes a fresh named
# volume on a directory that already exists in the image, it copies the
# directory's ownership + perms into the new volume. Pre-creating uid
# 1000 means the in-container writer cron (running as uid 1000 via the
# baked /etc/cron.d/cw-default crontab) can write to the path on first
# launch without an out-of-band `chown`. When the operator overrides
# with CW_STATE_PATH=/host/path, docker bind-mounts the host dir
# wholesale and the image-side ownership is irrelevant — the operator
# is responsible for making the host path uid-1000 writable.
RUN mkdir -p /var/lib/claude-watch /var/lib/claude-watch/cron-disabled \
    && chown 1000:1000 /var/lib/claude-watch /var/lib/claude-watch/cron-disabled \
    && chmod 0755 /var/lib/claude-watch /var/lib/claude-watch/cron-disabled

# Pre-create /var/run/claude/workload-state owned by uid 1000 so
# `workload run` can write state files without needing root or
# runtime mkdir. Same pattern as /var/lib/claude-watch above.
RUN mkdir -p /var/run/claude/workload-state \
    && chown 1000:1000 /var/run/claude /var/run/claude/workload-state \
    && chmod 0755 /var/run/claude /var/run/claude/workload-state

# Pre-create /var/log/claude-watch (+ the watchers/ subdir) owned by
# uid 1000 so the daemon, the cw-claude-watch-launch wrapper, and the
# session watchers can write their logs (claude-watch.jsonl/.log,
# cron.log, watchers/*.log) without needing root or runtime mkdir.
# Same pattern as /var/lib/claude-watch and /var/run/claude above.
RUN mkdir -p /var/log/claude-watch/watchers \
    && chown -R 1000:1000 /var/log/claude-watch \
    && chmod -R 0755 /var/log/claude-watch

# Pre-create ~/.config/ owned by uid 1000 so features that write
# persistent state to a subdirectory of ~/.config/ (claude-event-watch
# consumed.jsonl, claude-watch-ack pending-alerts.json, future state
# files) can `mkdir -p ~/.config/<feature>/` + write without sudo.
#
# Why this is needed: Docker's bind-mount auto-creation walks the target
# path and creates any missing parent directories as root:root mode 0755.
# The example compose stack bind-mounts ~/.config/session (and may
# optionally mount ~/.config/eichi, ~/.config/gh, etc.). On first
# container start the bind-mount's target parent `/home/hndrewaall/.config`
# does NOT exist in the image, so Docker creates it root-owned BEFORE the
# entrypoint runs. The entrypoint runs as uid 1000 (non-root, per the
# image-level USER directive below) and cannot fix the ownership without
# escalating to root. Result: every feature that wants to write a new
# subdir under ~/.config/ has to be paired with a one-shot
# `docker exec -u root chown` on first deploy — observed in v57
# (claude-events) and v59 (claude-watch-ack) workbot validation rounds.
#
# Fix shape (same pattern the ~/.local pre-creation above uses): create
# ~/.config/ in the image with uid 1000 ownership. When Docker walks the
# bind-mount target path next, it finds the pre-existing parent and uses
# it as-is rather than auto-creating a fresh root-owned one. Mode 0755
# matches the standard XDG_CONFIG_HOME (~/.config/) convention; 0700
# isn't required (no secrets live at this layer — per-feature subdirs
# choose their own modes).
RUN mkdir -p /home/hndrewaall/.config \
    && chown 1000:1000 /home/hndrewaall/.config \
    && chmod 0755 /home/hndrewaall/.config

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
# `~/.npm-global/bin` is also on PATH right after `~/.local/bin`. The
# Dockerfile installs @anthropic-ai/claude-code into this uid-1000-owned
# npm prefix (see the npm-install RUN block below for the autoupdate-v2
# rationale), so the baked claude entry point lives at
# $HOME/.npm-global/bin/claude. The image also ships a /usr/bin/claude
# symlink pointing at the same file (chowned -h to uid 1000) so
# `docker compose exec claude-container claude ...` works even when the
# operator's $PATH doesn't include $HOME/.npm-global/bin (e.g.
# stripped-down shells). Within the entrypoint and tmux panes the
# PATH-baked $HOME/.npm-global/bin/claude wins because it comes first.
#
# ENV PATH propagates to every child process the container ever spawns
# — entrypoint.sh, every tmux pane it opens, every `docker compose
# exec` invocation, and `cwsr`'s respawned claude — without each shell
# having to re-source /etc/profile. Source-code research conventions
# (use the path the warning suggests, don't silence the check itself):
# Claude Code's native-install probe is correct; the container's PATH
# is what was wrong.
ENV PATH=/home/hndrewaall/.local/bin:/home/hndrewaall/.npm-global/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin

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

# Install Claude Code CLI globally as uid 1000 with a uid-1000-writable
# npm prefix. Package name confirmed via `npm view` 2026-05-11.
#
# Why this shape (autoupdate-v2, 2026-05-15):
#
# The previous shape `npm install -g` as root → chown the install tree +
# the /usr/bin/claude symlink to uid 1000 (PR #204) — sat at the boundary
# of what works because Claude Code's in-process auto-updater calls `npm
# install -g @anthropic-ai/claude-code@latest` as the interactive user
# (uid 1000), and npm in that invocation honors its CONFIGURED prefix,
# not the historical install location. With the image's npm using its
# stock default of `/usr/local` as the prefix, every auto-update attempt
# tried to write to `/usr/local/lib/node_modules/` (root-owned) and
# failed with:
#
#   Insufficient permissions to install update
#   Try running with sudo or fix npm permissions
#
# (workbot v68 validation 2026-05-15). Chowning the original install
# tree didn't help — npm wasn't trying to write there, it was trying to
# write to its configured prefix.
#
# Fix: configure npm's prefix to a uid-1000-owned path under $HOME and
# run the install as uid 1000 from the start. Both build-time AND
# self-update-time installs land in the same place under the user's home
# directory; no chown gymnastics, no /usr/lib vs /usr/local split, no
# privilege-asymmetric writes. This is the canonical "unprivileged
# npm-global" pattern documented at
# https://docs.npmjs.com/resolving-eacces-permissions-errors-when-installing-packages-globally.
#
# The /usr/bin/claude symlink stays — it's a stable entry point for
# `docker compose exec claude-container claude ...` invocations that
# don't pre-resolve $HOME's npm-global bindir on PATH. Chown -h on it
# (PR #204) stays for the same reason: npm install upgrades unlink + relink
# the symlink, so it has to be uid-1000-owned. The symlink now points
# into the npm-global prefix under $HOME instead of /usr/lib/.
#
# Pre-create $HOME/.npm-global/{lib,bin} as uid 1000 so npm doesn't have
# to mkdir them at install time (some npm versions race on parent dir
# perms when the prefix path doesn't exist yet). The `npm config set
# prefix` writes to /home/hndrewaall/.npmrc (the per-user npmrc), which
# is uid-1000-owned.
#
# `su - hndrewaall -c '...'` runs the whole install pipeline as uid 1000
# with a login shell (so $HOME, $PATH etc. are set up correctly) without
# permanently switching the Dockerfile's USER context — subsequent COPY
# operations stay simple. The trailing `ln -sf` + `chown -h` recreate
# the /usr/bin/claude entry point (was previously the npm-emitted symlink
# into /usr/lib/; now an explicit ln to $HOME/.npm-global/bin/claude).
# Chown -h flips the symlink's own ownership to uid 1000 so future
# in-process `npm install -g` upgrades (which unlink+relink the symlink)
# don't fail with EACCES on a root-owned symlink in /usr/bin/. PR #204's
# rationale stands; only the target of the symlink changed.
RUN install -d -o 1000 -g 1000 /home/hndrewaall/.npm-global \
                                /home/hndrewaall/.npm-global/lib \
                                /home/hndrewaall/.npm-global/bin \
    && su - hndrewaall -c 'npm config set prefix /home/hndrewaall/.npm-global \
                            && npm install -g @anthropic-ai/claude-code' \
    && ln -sf /home/hndrewaall/.npm-global/bin/claude /usr/bin/claude \
    && chown -h 1000:1000 /usr/bin/claude

# Bake the claude-watch Rust binary in from stage 1. Glibc compatibility is
# guaranteed because the builder is `rust:1-bookworm` and this stage is
# `debian:bookworm-slim` — same Debian release, same libc.
# claude-watch has no `--version` flag; we verify by exercising `--help`,
# which exits 0 only if the binary loads and clap parses its CLI.
COPY --from=claude-watch-builder /build/target/release/claude-watch /usr/local/bin/claude-watch
RUN claude-watch --help >/dev/null

# VS Code CLI (`code` / `code tunnel`) — Microsoft's standalone CLI binary.
#
# Baked so the operator can run `code <file>` / `code .` / `code tunnel`
# inside the in-container shell and have a working VS Code window open
# without first installing anything host-side OR inside the container.
#
# Path B trade-off (chosen 2026-05-14, q-2026-05-14-ee13): we bake the
# BINARY at image build time and the operator authenticates
# interactively the first time they run `code tunnel` post-deploy. The
# tunnel may not be the operator's currently-active VS Code window —
# `code tunnel` creates its own session and clicks open in whichever VS
# Code instance the operator first authenticated with. Path A
# (`devcontainer.json` reattach) was rejected because it'd break the
# current cw / ttyd / `docker compose exec` model. See
# container/README.md "VS Code CLI" + examples/compose/README.md
# "VS Code tunnel auth bootstrap" for the full operator workflow.
#
# Source-of-truth URL: `https://update.code.visualstudio.com/latest/cli-linux-${ARCH}/stable`
# redirects to the current pinned `vscode_cli_linux_${ARCH}_cli.tar.gz`
# build on `vscode.download.prss.microsoft.com`. The tarball contains a
# single `code` binary at the root. Verified 2026-05-14: code 1.120.0
# (commit 0958016b...) tarball ~11MB, extracted binary ~30MB. The
# documented Alpine pattern at https://code.visualstudio.com/docs/remote/tunnels
# uses the parallel `cli-alpine-x64` URL — this is the linux-x64
# equivalent.
#
# Arch detection via `uname -m` so the same Dockerfile builds cleanly on
# both x86_64 (cli-linux-x64) and aarch64 (cli-linux-arm64) hosts. We
# prefer runtime `uname` over buildx's `TARGETARCH` so the build works
# under plain `docker build` too (no buildx requirement). Microsoft
# publishes the linux-arm64 tarball at the parallel URL — verified
# 2026-05-15 via HEAD redirect to vscode_cli_linux_arm64_cli.tar.gz.
#
# `code --version` at the end is a smoke-test guard: it exits 0 only if
# the binary loads + the clap CLI parses, mirroring the claude-watch
# `--help` guard above. No runtime auth is required for `--version` —
# auth only happens when the operator runs `code tunnel` interactively
# post-deploy.
RUN case "$(uname -m)" in \
        x86_64)  VSCODE_ARCH=x64 ;; \
        aarch64) VSCODE_ARCH=arm64 ;; \
        *) echo "unsupported arch: $(uname -m)" >&2; exit 1 ;; \
    esac \
    && curl -fsSL "https://update.code.visualstudio.com/latest/cli-linux-${VSCODE_ARCH}/stable" -o /tmp/vscode_cli.tar.gz \
    && tar -xzf /tmp/vscode_cli.tar.gz -C /usr/local/bin/ code \
    && chmod +x /usr/local/bin/code \
    && rm -f /tmp/vscode_cli.tar.gz \
    && /usr/local/bin/code --version >/dev/null

# Container-tailored claude-watch config — pins the daemon to the in-container
# tmux session `claude-container:0.0` (the claude pane created by
# entrypoint.sh), points all writes at uid-1000-writable paths (/tmp/, ~/.cache),
# and disables host-only features (auto_update, reauth, hybrid, task_watch)
# that have no in-container backing yet (Phase 2 work). The host's
# ~/.config/claude-watch/config.toml is NOT bind-mounted by the wrapper, so
# without this file the daemon would FATAL on "no config file found". Phase 1e.
COPY container/claude-watch.config.toml /etc/claude-watch/config.toml

# Baked watchers.conf — the daemon's watcher_monitor list. Watchers are
# session-scoped run_in_background tasks (block-print-exit contract):
# the in-container session launches each watcher and restarts it on
# exit. The daemon's watcher_monitor reads this pipe-separated file and
# fires `[CLAUDE-WATCH] WATCHER(S) DOWN` alerts via tmux-inject if the
# watcher's pgrep pattern is missing for several consecutive checks
# (i.e. the session failed to restart it). The session restarts in
# seconds; the daemon alerts the main loop if that has actually failed.
#
# Path /opt/claude-container/watchmen/watchers.conf mirrors the host's
# ~/.config/watchmen/watchers.conf convention (the "watchmen" subdir
# survives from the legacy shell-script supervisor's name). The
# container's claude-watch.config.toml points watcher_monitor.watchers_config
# at this exact path.
COPY container/watchers.conf /opt/claude-container/watchmen/watchers.conf

# watcher-ctl: multicall alias of claude-watch. The daemon's
# WATCHER(S) DOWN alert prompt injects `watcher-ctl run <name>` into
# the main loop's pane (the canonical cardinal-rule restart shape).
# Without this symlink the main loop hits "command not found" when it
# tries to follow the alert.
RUN ln -s /usr/local/bin/claude-watch /usr/local/bin/watcher-ctl

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
# /etc/claude-code/ is RESERVED for Claude Code's managed-config tier on
# Linux. The image keeps exactly two entries there:
#
#   CLAUDE.md             — this baked managed-policy memory file.
#   managed-settings.json — a symlink into the host-managed-settings
#                           staging mount (see the RUN ln -s below).
#
# Everything claude-watch-specific (skills, agents, watchers, plugin,
# process-compose.yml, doc mirror, watchmen config) is baked under
# /opt/claude-container/ instead, so a host managed-settings bind-mount
# can never shadow — or be shadowed by — container runtime files.
#
# Mount interaction: the example compose stack
# (examples/compose/docker-compose.yml) bind-mounts the operator's host
# managed-settings dir (CLAUDE_HOST_MANAGED_SETTINGS_DIR; Linux default
# /etc/claude-code, macOS /Library/Application Support/ClaudeCode) read-only
# at the fixed staging path /mnt/host-managed-claude-config. The symlink
# below makes the host's managed-settings.json visible at the documented
# Linux path the in-container claude actually reads, REGARDLESS of where
# the host OS keeps the file. When the env var is unset the compose mounts
# /dev/null at the staging path and the symlink dangles — Claude Code
# treats the managed-settings tier as absent. The host's own managed
# CLAUDE.md (if any) is intentionally NOT propagated: the baked
# container-runtime CLAUDE.md owns that slot.
COPY container/baked-CLAUDE.md /etc/claude-code/CLAUDE.md
RUN ln -s /mnt/host-managed-claude-config/managed-settings.json \
        /etc/claude-code/managed-settings.json

# Bake the docs the managed CLAUDE.md links to so its cross-references
# resolve to LOCAL paths in-container instead of raw-GitHub URLs.
# baked-CLAUDE.md lands at /etc/claude-code/CLAUDE.md and links to its
# sibling docs by ABSOLUTE /opt/claude-container/<repo-relative-path>
# targets (the managed dir itself stays doc-free — see the reservation
# note above). We mirror the repo layout under /opt/claude-container/
# for exactly the subtrees baked-CLAUDE.md reaches into:
#
#   docs/                          -> the concept + per-subsystem docs
#                                     (event-hierarchy, watchers, adding-watchers, …)
#   README.md                      -> top-level README (Alerting hierarchy)
#   container/README.md            -> blast-radius / entrypoint reference
#   container/hooks-shim/exec-hook -> the exec-hook shim baked-CLAUDE.md cites
#   container/{skills,agents,watchers}
#                                  -> the baked slash-command / agent / watcher dirs
#   examples/compose/{README.md,docker-compose.yml,bin}
#                                  -> the fresh-laptop developer stack
#
# scripts/check-doc-links.py (run in CI) maps /opt/claude-container/-prefixed
# links back to repo-relative paths and guarantees each resolves to a path
# that exists in the repo, so this COPY set stays in sync with the doc's
# links: a new link to an un-baked path fails CI.
COPY docs/ /opt/claude-container/docs/
COPY README.md /opt/claude-container/README.md
COPY container/README.md /opt/claude-container/container/README.md
COPY container/hooks-shim/exec-hook /opt/claude-container/container/hooks-shim/exec-hook
COPY container/skills/ /opt/claude-container/container/skills/
COPY container/agents/ /opt/claude-container/container/agents/
COPY container/watchers/ /opt/claude-container/container/watchers/
COPY examples/compose/README.md /opt/claude-container/examples/compose/README.md
COPY examples/compose/docker-compose.yml /opt/claude-container/examples/compose/docker-compose.yml
COPY examples/compose/bin/ /opt/claude-container/examples/compose/bin/

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

# Bake WATCHERS_CONFIG into the image-level env so `watcher-ctl run <name>`
# (multicall alias of claude-watch) finds the baked watchers.conf instead
# of falling through to the default $HOME/.config/watchmen/watchers.conf,
# which isn't bind-mounted in the container layout. The daemon's
# [watcher_monitor].watchers_config TOML key handles the daemon's own
# read path; this env var covers the CLI path the main loop invokes when
# the WATCHER(S) DOWN alert prompt tells it to run `watcher-ctl run
# claude-event-watch`. src/watcher.rs config_path() honors WATCHERS_CONFIG.
ENV WATCHERS_CONFIG=/opt/claude-container/watchmen/watchers.conf

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

# claude-mcp-settings-shim — make bare `claude mcp ...` report the TRUTH.
# The interactive session launches with `--setting-sources project,local
# --settings $CLAUDE_SHIM_SETTINGS_PATH`, whose shim settings.json carries
# `enableAllProjectMcpServers: true` (auto-approving the project-tier
# .mcp.json servers). But a SEPARATE bare `claude mcp list` (the
# session-start checklist, the operator, the session's own Bash tool) is a
# FRESH claude process that does NOT inherit those flags — it defaults to
# `--setting-sources user,project,local`, never loads the shim, and so
# reports the project .mcp.json servers as "Pending approval" even though
# the live session has them Connected. That stale/alarming display is the
# entire long-standing "MCP servers stuck Pending approval" bug.
#
# This wrapper sits FIRST on PATH (the entrypoint prepends
# /usr/local/lib/claude-hooks-shim ahead of the real claude in
# ~/.npm-global/bin) as a `claude` shim. For `mcp` subcommands it injects
# the SAME setting-sources + settings flags the session uses, so
# `claude mcp list` reports the session's real (Connected) state. Every
# other subcommand execs the real claude verbatim. Surgical: only `mcp`,
# only when the caller hasn't already supplied the flags.
COPY container/hooks-shim/claude-mcp-settings-shim /usr/local/lib/claude-hooks-shim/claude-mcp-settings-shim
RUN chmod +x /usr/local/lib/claude-hooks-shim/claude-mcp-settings-shim \
    && ln -s /usr/local/lib/claude-hooks-shim/claude-mcp-settings-shim /usr/local/lib/claude-hooks-shim/claude

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
#   - pre-agent-background-required-hook   (PreToolUse Agent matcher)
#   - pre-tool-obligations-gate-hook       (PreToolUse * matcher)
#   - post-tool-obligations-update-hook    (PostToolUse * matcher)
#   - post-tool-mark-attachment-read-hook  (PostToolUse Read matcher)
#   - post-tool-agent-arm-hook             (PostToolUse Agent matcher)
#
# Each script is the canonical source of truth from tools/hooks/ in
# this repo (the same scripts `make install` symlinks into
# ~/bin/ on the host). Baking them at the container's
# /usr/local/bin/ ensures the obligations gate fires inside the
# container regardless of host state.
COPY tools/hooks/pre-agent-queue-gate-hook /usr/local/bin/pre-agent-queue-gate-hook
COPY tools/hooks/pre-agent-background-required-hook /usr/local/bin/pre-agent-background-required-hook
COPY tools/hooks/pre-tool-obligations-gate-hook /usr/local/bin/pre-tool-obligations-gate-hook
COPY tools/hooks/post-tool-obligations-update-hook /usr/local/bin/post-tool-obligations-update-hook
COPY tools/hooks/post-tool-mark-attachment-read-hook /usr/local/bin/post-tool-mark-attachment-read-hook
# post-tool-agent-arm-hook -- writes agent_id -> queue_id bindings to
# ~/.config/claude/agent-queue-bindings.json on every successful async-
# launched Agent spawn. Companion to the `subagent_queue_item_running`
# obligations predicate, which enforces "every subagent tool call must
# correspond to a running queue item" AFTER spawn (the existing
# pre-agent-queue-gate-hook enforces it at SPAWN time; this one
# continues enforcement throughout the subagent's lifetime by binding
# the spawned agent_id to its q-id and letting the predicate read the
# binding on every PreToolUse).
COPY tools/hooks/post-tool-agent-arm-hook /usr/local/bin/post-tool-agent-arm-hook
# claude-watch alert gate -- PreToolUse hook that DENIES every non-exempt
# tool call while any `[CLAUDE-WATCH]` alert is pending in
# ~/.config/claude-watch/pending-alerts.json. Companion UserPromptSubmit
# hook auto-records `[CLAUDE-WATCH]` injects from the tmux pane into the
# state file; companion `claude-watch-ack` CLI clears the gate.
# Together they turn the soft baked-CLAUDE.md `claude-watch alerts`
# cardinal into a hard tool-layer gate.
COPY tools/hooks/pre-tool-claude-watch-alert-gate-hook /usr/local/bin/pre-tool-claude-watch-alert-gate-hook
COPY tools/hooks/user-prompt-claude-watch-alert-record-hook /usr/local/bin/user-prompt-claude-watch-alert-record-hook
# claude-watch-ack CLI -- manages the pending-alerts state file, the
# only mechanism (besides the audited bypass env-var) to clear the
# alert gate.
COPY tools/claude-watch-ack/claude-watch-ack /usr/local/bin/claude-watch-ack
# Dispatcher gate -- PreToolUse hook that DENIES the (N+1)th
# consecutive non-exempt non-Agent tool call (default threshold N=3).
# Spawning an Agent (or running `claude-watch-dispatch reset`) zeros
# the counter. Together with the companion CLI, this turns the soft
# baked-CLAUDE.md `dispatcher, not worker` cardinal into a hard
# tool-layer gate.
COPY tools/hooks/pre-tool-dispatch-gate-hook /usr/local/bin/pre-tool-dispatch-gate-hook
# claude-watch-dispatch CLI -- inspect counter, reset, set threshold,
# list effective exempt patterns. The CLI itself is exempt from the
# gate so introspection always works.
COPY tools/claude-watch-dispatch/claude-watch-dispatch /usr/local/bin/claude-watch-dispatch
# Agent communication CLIs -- async host -> subagent inbox protocol.
#
#   - obligations: registry that backs every gate hook in /usr/local/bin/.
#     Previously bind-mount-only via the claude-tools.profile.sh; baking
#     it makes the obligations gate operational even when ~/repos/claude-
#     watch isn't bind-mounted (e.g. stripped-down `docker run` without
#     the host repo). The bind-mounted source still wins on PATH (via
#     the profile prepend) when present, so dev iteration stays live.
#
#   - agent-msg: subagent inbox CLI (send / inbox / ack / arm / disarm /
#     gc / gc-dead). The main loop calls `agent-msg send <agent-id>
#     "<message>"` and the spawned subagent's PreToolUse obligations
#     gate (the existing pre-tool-obligations-gate-hook above) DENIES
#     every non-ack tool call until the subagent runs `agent-msg ack`
#     against its inbox. `arm` registers the gate-mode obligation via
#     the `obligations` CLI; without `obligations` baked, agent-msg
#     would only work in containers that bind-mount ~/repos/claude-
#     watch. Baking both keeps the channel operational by default.
#
#   - agent-tail: reads a subagent's JSONL transcript at
#     ~/.claude/projects/<slug>/<session>/subagents/agent-<id>.jsonl.
#     Companion to `claude-watch active-agents` for inspecting a
#     running subagent's tool history. Includes `--list` mode that
#     enumerates active subagent transcripts and `--follow` for live
#     streaming.
COPY tools/obligations/obligations /usr/local/bin/obligations
# shell_ast.py -- dependency-free shell-structure parser imported by both
# the obligations CLI (no_pipe_pattern predicate) and the
# pre-tool-obligations-gate-hook (watcher-ctl bare gate) to make pattern
# matching AST-aware: a forbidden pipe/operator that appears inside a
# quoted argument or heredoc body no longer false-positive-denies. Both
# importers fall back to the previous raw string-match if this module is
# absent, so baking it alongside obligations keeps the AST behavior in the
# container. Placed flat in /usr/local/bin/ so `obligations` (same dir)
# and the hook (own-dir import candidate) both resolve it.
COPY tools/obligations/shell_ast.py /usr/local/bin/shell_ast.py
# obligations-init -- idempotent seed for the default-bundled obligations
# rows. Invoked from the container entrypoint after the obligations CLI
# is available on PATH. Currently seeds the
# `subagent_queue_item_running` row (continuous subagent queue-discipline
# enforcement). Best-effort: any failure logs to stderr but exits 0 so
# entrypoint keeps going.
COPY tools/obligations/obligations-init /usr/local/bin/obligations-init
COPY tools/agent-msg/agent-msg /usr/local/bin/agent-msg
COPY tools/agent-tail/agent-tail /usr/local/bin/agent-tail
RUN chmod +x /usr/local/bin/pre-agent-queue-gate-hook \
            /usr/local/bin/pre-agent-background-required-hook \
            /usr/local/bin/pre-tool-obligations-gate-hook \
            /usr/local/bin/post-tool-obligations-update-hook \
            /usr/local/bin/post-tool-mark-attachment-read-hook \
            /usr/local/bin/post-tool-agent-arm-hook \
            /usr/local/bin/pre-tool-claude-watch-alert-gate-hook \
            /usr/local/bin/user-prompt-claude-watch-alert-record-hook \
            /usr/local/bin/claude-watch-ack \
            /usr/local/bin/pre-tool-dispatch-gate-hook \
            /usr/local/bin/claude-watch-dispatch \
            /usr/local/bin/obligations \
            /usr/local/bin/obligations-init \
            /usr/local/bin/agent-msg \
            /usr/local/bin/agent-tail

# eval-no-grep-watcher-output — obligations evaluator script that DENIES
# Bash commands which pipe task/watcher output files through grep/sed/awk/
# head/tail. Ensures the main loop reads ALL events unfiltered. Seeded as
# a default obligation row by obligations-init (evaluator predicate,
# decision_mode=exit_code, stdin_field=tool_input.command).
COPY container/bin/eval-no-grep-watcher-output /usr/local/bin/eval-no-grep-watcher-output
RUN chmod +x /usr/local/bin/eval-no-grep-watcher-output

# eval-event-must-act — obligations evaluator (q-2026-05-21-856d
# refinement) that escalates over N consecutive non-exempt Bash tool
# calls while actionable events remain un-acked in pending-actions.json.
# Default N=3 (override via $EVENT_MUST_ACT_N); under-threshold calls
# ALLOW + bump the counter, threshold-reached calls DENY. Each event-ack
# transaction resets the counter to 0 so the LLM gets a fresh N-call
# grace window after every ack. Default-open on missing/corrupt state.
# Signal-tagged events are filtered upstream by event-ack ingest and
# never feed into pending-actions.json; the Signal-DM ack-gate lives
# on a separate obligation row (signal-wait-* watcher + signal-send
# ack-gate path, NOT migrated to this gate per Andrew 2026-05-21).
COPY tools/event-must-act/eval-event-must-act /usr/local/bin/eval-event-must-act
RUN chmod +x /usr/local/bin/eval-event-must-act

# eval-agent-ack-pending — obligations evaluator that blocks Bash tool
# calls while completed agents await acknowledgment by the main loop.
# When a subagent delivers a task-notification, the main loop must
# register it (agent-ack register <queue-id>), process the output, call
# session-task queue done/abandon, then clear it (agent-ack done <queue-id>).
# Default N=2 (override via $AGENT_ACK_N): denies after N consecutive
# non-exempt calls while pending-ack entries exist. Default-open on
# missing/corrupt state.
COPY container/bin/eval-agent-ack-pending /usr/local/bin/eval-agent-ack-pending
RUN chmod +x /usr/local/bin/eval-agent-ack-pending

# eval-queue-ready-unspawned — obligations evaluator that blocks Bash tool
# calls when session-task queue has items with status=pending and
# ready_now=true. Forces the main loop to spawn agents for ready work
# before doing anything else. Default-open on errors.
COPY container/bin/eval-queue-ready-unspawned /usr/local/bin/eval-queue-ready-unspawned
RUN chmod +x /usr/local/bin/eval-queue-ready-unspawned

# agent-ack — CLI to manage the agent completion acknowledgment state.
# Subcommands: register, done, list, clear, status. Manages the state
# file at ~/.config/claude/agent-ack-pending.json used by the
# eval-agent-ack-pending evaluator.
COPY container/bin/agent-ack /usr/local/bin/agent-ack
RUN chmod +x /usr/local/bin/agent-ack

# event-ack — CLI to manage the four-tier claude-event response surface.
# Manages ~/.config/claude-events/{pending-actions,tool-call-counter,
# ambient-context}.json with flock-protected transactions. Subcommands:
#   - ingest: classify (via event-classify) + route to pending/ambient
#   - add/ack/list/clear: legacy pending-actions surface (v1-compat)
#   - drain-ambient (hook-internal): pop ambient queue for UserPrompt inject
#   - reset-counter/bump-counter: N-counter knobs
# Signal-tagged events are excluded by classification (the Signal-DM
# ack-gate stays on its existing per-thread obligation path).
COPY tools/event-must-act/event-ack /usr/local/bin/event-ack
RUN chmod +x /usr/local/bin/event-ack

# event-classify — data-driven source/tag -> tier classifier (ambient
# / actionable / excluded). Owns the CLASSIFICATIONS table that maps
# every known event source to its tier; new sources are added by
# appending to the table. Used by event-ack ingest + by the main loop
# (event-classify --source X --tag Y) when manually triaging events.
COPY tools/event-must-act/event-classify /usr/local/bin/event-classify
RUN chmod +x /usr/local/bin/event-classify

# user-prompt-ambient-inject-hook — UserPromptSubmit hook that drains
# the ambient event queue on every prompt and emits the queued events
# as additional context for the LLM's next turn. Non-blocking, no
# gate. Companion to the actionable tier (event_must_act obligation,
# seeded by obligations-init). Together they implement the four-tier
# event model -- ambient (this hook), actionable (must-act gate),
# signal-distinct (NOT touched by this refactor; stays on signal-wait-
# watcher + signal-send ack-gate), unknown (defaults to ambient).
COPY tools/event-must-act/user-prompt-ambient-inject-hook /usr/local/bin/user-prompt-ambient-inject-hook
RUN chmod +x /usr/local/bin/user-prompt-ambient-inject-hook

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

COPY container/bin/mcp-reconnect /usr/local/bin/mcp-reconnect
RUN chmod +x /usr/local/bin/mcp-reconnect

# xclip — cross-host clipboard image-paste shim.
#
# The bundled `claude` binary's TUI image-paste code path shells out to
# `xclip -selection clipboard -t TARGETS -o` (to enumerate clipboard
# MIME types) and then `xclip -selection clipboard -t image/png -o > $tmp`
# (to read the PNG bytes). Inside this container on a headless host
# there is NO display server reachable from xclip — the real
# /usr/bin/xclip fails with "Can't open display" and image paste
# silently no-ops.
#
# This shim, baked at /usr/local/bin/xclip (which beats /usr/bin/xclip
# in PATH order from the ENV PATH line above), interprets the
# specific argv shapes claude emits and reads the bytes from a
# bind-mounted host directory (/host-clipboard/ by default; override
# via XCLIP_BRIDGE_DIR). An operator-side daemon on the Mac (see
# examples/compose/bin/clipboard-bridge-daemon + the launchd plist
# under examples/compose/launchd/) watches the Mac clipboard and
# rsync's PNG bytes to the path that's bind-mounted here. When no
# bridge directory is mounted (no operator setup), the shim returns
# "empty clipboard" gracefully — claude's image-paste path treats
# that as "nothing to paste" rather than crashing.
#
# Empirical history (q-2026-05-17-a0ff): the prior investigation
# concluded "no xclip-shim path works for this layout." Andrew pushed
# back ("we should at least be able to shim xclip"), and the empirical
# re-investigation found that the file-watch bridge approach DOES work
# end-to-end. The shim has 11 embedded tests; run them at any time
# inside the container with `python3 /usr/local/bin/xclip --self-test`.
#
# Opt-out (debugging / containers that actually have a real X server):
#   XCLIP_SHIM=disabled  → the shim execs /usr/bin/xclip directly.
#
# Real xclip is NOT installed in the base image (the shim does the
# whole job for the canonical claude invocations); a downstream image
# that needs the real binary for non-claude callers can apt-get
# install xclip and set XCLIP_SHIM=disabled.
COPY container/bin/xclip /usr/local/bin/xclip
RUN chmod +x /usr/local/bin/xclip

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

# cw-watcher-health-check — cron-driven dead-watcher detector.
# Checks for unconsumed events in the queue dir older than 2 min;
# if any exist, injects a [CLAUDE-WATCH] alert into the tmux pane
# via send-keys (bypasses the event system, which is broken if
# events are accumulating). Fired by the */2 cron entry in cw-default.
COPY tools/event-must-act/cw-watcher-health-check /usr/local/bin/cw-watcher-health-check
RUN chmod +x /usr/local/bin/cw-watcher-health-check

# cw-cron-run + cw-cron-toggle — toggle baked cron.d entries without sudo.
# cw-cron-run is the flag-file exec-wrapper prefixed in front of every
# cw-default cron command; if a job's flag file exists under the
# uid-1000-writable /var/lib/claude-watch/cron-disabled it skips the job
# (exit 0), else it `exec`s the real command. cw-cron-toggle is the
# operator CLI that flips those flag files. No sudoers change required.
# See docs/cron-toggle.md.
COPY container/bin/cw-cron-run /usr/local/bin/cw-cron-run
RUN chmod +x /usr/local/bin/cw-cron-run
COPY container/bin/cw-cron-toggle /usr/local/bin/cw-cron-toggle
RUN chmod +x /usr/local/bin/cw-cron-toggle

# claude-event-watch — the canonical block-print-exit event watcher.
# Session runs this as a `run_in_background` task; it blocks on inotifywait,
# prints pending events, and exits. Baked so it's on PATH without depending
# on the ~/repos bind-mount.
COPY tools/watchers/claude-event-watch /usr/local/bin/claude-event-watch
RUN chmod +x /usr/local/bin/claude-event-watch

# Baked skills / agents / watchers — three top-level dirs in the repo
# (container/skills/, container/agents/, container/watchers/) that ship
# in-container slash commands, custom agent prompts, and background
# watcher launchers respectively. See each dir's README.md for the
# convention, the on-disk schema (watchers), and the add-a-new-entry
# walkthrough.
#
# Wiring shape:
#
#   container/skills/   -> /opt/claude-container/skills/   (canonical bake path,
#                                                       documented for inspection)
#                       -> /opt/claude-container/plugin/commands/
#                                                      (where Claude Code's
#                                                       plugin loader reads
#                                                       slash commands)
#   container/agents/   -> /opt/claude-container/agents/   (canonical bake path)
#                       -> /opt/claude-container/plugin/agents/
#                                                      (plugin loader reads
#                                                       custom agents here)
#   container/watchers/ -> /opt/claude-container/watchers/ (probed by the
#                                                       /start-watchers skill;
#                                                       NOT a plugin loader path)
#   container/plugin/.claude-plugin/plugin.json
#                       -> /opt/claude-container/plugin/.claude-plugin/plugin.json
#                          (manifest the plugin loader requires; sets the
#                           plugin namespace to `claude-container`, so
#                           skills land as /claude-container:<name>)
#
# entrypoint.sh appends `--plugin-dir /opt/claude-container/plugin` to the
# claude argv so the plugin loader picks up the baked commands/agents at
# session start. The two-target COPY (skills -> {skills,plugin/commands};
# agents -> {agents,plugin/agents}) keeps the canonical paths
# inspectable AND the plugin-loader paths populated, without forcing the
# operator to think about the plugin layout. Watchers do NOT land in the
# plugin dir because they aren't slash commands or agents — they're
# shell scripts the agent runs via the Bash tool with run_in_background.
COPY container/skills/ /opt/claude-container/skills/
COPY container/skills/ /opt/claude-container/plugin/commands/
COPY container/agents/ /opt/claude-container/agents/
COPY container/agents/ /opt/claude-container/plugin/agents/
COPY container/watchers/ /opt/claude-container/watchers/
# Defensive chmod +x on the baked watcher scripts. The recursive COPY
# above preserves source-side perms, but a future refactor that swaps
# the COPY for a different mechanism (or an upstream tar layer that
# drops the execute bit) would silently break /start-watchers without
# the explicit chmod here. Mirrors the same defence-in-depth pattern
# every other baked-script COPY uses (exec-hook, cwsr, self-clear,
# etc.).
RUN chmod +x /opt/claude-container/watchers/claude-event-watch.sh
COPY container/plugin/.claude-plugin/plugin.json /opt/claude-container/plugin/.claude-plugin/plugin.json

# process-compose service config + helper launchers. The supervisor
# runs as PID 1 (see ENTRYPOINT below) and reads
# /opt/claude-container/process-compose.yml at startup to drive the
# in-container service set: tmux session bootstrap (oneshot), tmux
# attach (foreground TTY), claude-watch daemon (long-running), crond
# (long-running). Each long-running process has an
# `availability: restart: on_failure` policy so a crash triggers a
# restart instead of a dead container.
#
# cw-tmux-bootstrap creates the in-container tmux session in detached
# mode (extracted from entrypoint.sh's tmux session-creation logic).
# cw-claude-watch-launch wraps the claude-watch invocation with the
# CLAUDE_WATCH_CONTAINER_MODE=1 env + log-file plumbing.
COPY container/process-compose.yml /opt/claude-container/process-compose.yml
COPY container/bin/cw-tmux-bootstrap /usr/local/bin/cw-tmux-bootstrap
COPY container/bin/cw-claude-watch-launch /usr/local/bin/cw-claude-watch-launch
RUN chmod +x /usr/local/bin/cw-tmux-bootstrap \
            /usr/local/bin/cw-claude-watch-launch

# Entrypoint script — env setup + hooks-shim generation + trust-workspace
# pre-seeding. Replaces the prior tmux-session-creation responsibilities
# (which now live in /usr/local/bin/cw-tmux-bootstrap, invoked by
# process-compose as the setup-tmux-session oneshot). After setup, the
# entrypoint execs process-compose with the baked config; process-compose
# is PID 1 from there forward.
COPY container/entrypoint.sh /usr/local/bin/claude-tmux-entrypoint
RUN chmod +x /usr/local/bin/claude-tmux-entrypoint

# Drop privileges before runtime — root in a bind-mounted layout breaks host perms.
USER hndrewaall
WORKDIR /workspace

# process-compose as PID 1 (escalation past tini's threshold). The
# binary forwards SIGTERM/SIGINT to all supervised processes, reaps
# zombies, and exits when all foreground processes complete.
#
# The entrypoint script is the FIRST process under process-compose's
# supervision — it runs setup (PATH, hooks-shim, trust-workspace,
# obligations-init) then execs `process-compose up` with the baked
# /opt/claude-container/process-compose.yml. process-compose then
# supervises the tmux session + daemons per the declared restart
# policies.
#
# Why not s6-overlay: process-compose's mental model maps 1:1 onto
# docker-compose.yml (depends_on, restart policies, health checks),
# which is the model operators already use for the outer compose
# stack. s6-overlay's /etc/cont-init.d + /etc/services.d shape is
# strictly more flexible but has a steeper learning curve; we'll
# revisit if process-compose hits a gap.
ENTRYPOINT ["/usr/local/bin/claude-tmux-entrypoint"]
