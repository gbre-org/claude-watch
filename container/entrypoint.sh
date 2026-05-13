#!/usr/bin/env bash
# claude-container entrypoint — sets up a tmux session running claude.
#
# Default layout: ONE window, ONE full-screen pane running `claude`.
# Matches the `dashboard` script's documented default (docs/dashboard-layout.md):
# "no config file = claude-only single full-screen pane". Anything beyond that
# is opt-in via CLAUDE_CONTAINER_SIDEBAR=1 (which restores the prior 2-pane
# layout with claude-watch in a 25%-wide right sidebar).
#
# Sidebar mode (when CLAUDE_CONTAINER_SIDEBAR=1):
#   Pane 0 (left, ~75%): runs `claude` interactively.
#   Pane 1 (right, ~25%): runs the in-container `claude-watch` daemon,
#     observing pane 0 via in-container `tmux capture-pane`. Bare
#     `claude-watch` = daemon, same call as a host-side
#     `claude-watch.service` systemd unit. The daemon's config pins it to
#     this very tmux session so it doesn't auto-detect across other
#     sessions.
#
# Why removed-by-default: the sidebar pane was rendering as a ~10-column
# narrow strip in the ttyd browser console (q-2026-05-12-2e6c) with
# rewrapped duplicate text — visually broken at typical browser-terminal
# widths. The dashboard docs already say claude-only is the default;
# this entrypoint now matches that contract. Set CLAUDE_CONTAINER_SIDEBAR=1
# if you want the in-container daemon visible in its own pane.
#
# Env passthrough: CLAUDE_CODE_SSE_PORT, ANTHROPIC_API_KEY, plus any CLAUDE_*
# and ANTHROPIC_* vars are already in the process env (docker -e or compose
# environment:); tmux inherits them, so panes see them.
#
# Debug escape hatch: if argv[1] is set, exec it instead of launching tmux.
# Lets `docker run claude-container bash` (etc.) work for inspection. Also
# keeps the `claude-tmux bash -c "..."` validation path working unchanged.
#
# Inspect the running session from another shell on the host:
#   sudo docker exec -it <container> tmux attach -t claude-container
#   sudo docker exec <container> tmux capture-pane -t claude-container:0.1 -p
# Or peek at the structured log:
#   sudo docker exec <container> cat /tmp/claude-watch.jsonl

set -euo pipefail

SESSION="claude-container"

# Tell the in-container claude-watch where its config lives. The host's
# ~/.config/ is NOT bind-mounted (only ~/.claude, ~/repos, $PWD per the
# wrapper's "blast radius" header), so we ship a container-tailored config
# at /etc/claude-watch/config.toml via the Dockerfile and pin it explicitly
# here. The daemon also reads CLAUDE_WATCH_CONFIG by design (see
# claude-watch/src/config.rs `try_load_config`).
export CLAUDE_WATCH_CONFIG="${CLAUDE_WATCH_CONFIG:-/etc/claude-watch/config.toml}"

# Make sure the directories claude-watch wants to write to exist + are
# writable by uid 1000. State dir is under ~/.cache; logs are in /tmp.
mkdir -p "${HOME:-/home/hndrewaall}/.cache/claude-watch"

# Bring in /etc/profile.d/claude-tools.sh so the bind-mounted Python CLIs
# (session-task / claude-event / obligations) land on PATH for everything
# the entrypoint spawns -- both tmux panes inherit this PATH. Bash login
# + interactive shells started under `docker compose exec` pick the same
# fragment up via /etc/profile or /etc/bash.bashrc independently; this
# explicit source covers the entrypoint's own child processes (which are
# neither). Graceful no-op if the file is missing.
# shellcheck disable=SC1091
if [ -r /etc/profile.d/claude-tools.sh ]; then
    . /etc/profile.d/claude-tools.sh
fi

# Prepend the hooks-shim dir to PATH so bare `exec-hook ...` resolves to
# the safe-exec wrapper without depending on /usr/local/bin's relative
# position. Operators reference the shim from ~/.claude/settings.json
# hook entries (see /usr/local/lib/claude-hooks-shim/exec-hook header
# for the why + usage). PATH manipulation here (rather than mutating
# settings.json at build time) keeps the host-side config untouched —
# settings.json travels with the operator's host install. The shim is a
# strict wrapper: ELF targets pass through transparently, only non-ELF
# formats no-op.
case ":${PATH}:" in
    *":/usr/local/lib/claude-hooks-shim:"*) ;;
    *) export PATH="/usr/local/lib/claude-hooks-shim:${PATH}" ;;
esac

# CLAUDE_CONTAINER_REWRITE_HOOKS — opt-in entrypoint-side hook wrapper.
#
# When the host is non-Linux (typical: a Mac laptop bind-mounting
# ~/.claude/settings.json into a Linux container) AND that settings.json
# references host-native hook binaries by absolute path, Linux's exec()
# bounces them with "Exec format error" on every hook event. PR #135 ships
# /usr/local/bin/exec-hook (the magic-byte sniffer) but its wiring is
# opt-in per hook: the operator has to hand-edit each command in
# settings.json to wrap it. On a Mac host that edit would mutate the
# host's live settings.json, which is hostile.
#
# Instead: when CLAUDE_CONTAINER_REWRITE_HOOKS=1, generate a CONTAINER-LOCAL
# copy of settings.json with every hook command wrapped in
# /usr/local/bin/exec-hook, and tell the in-container claude to merge it
# via `--settings <path>` (additive on top of the user/project/local
# cascade). The host file is never touched.
#
# Default OFF so existing operators see no behaviour change. Mac-host
# operators flip the flag in their .env / docker-compose.override.yml.
CLAUDE_SHIM_SETTINGS_PATH=""
if [ "${CLAUDE_CONTAINER_REWRITE_HOOKS:-0}" = "1" ]; then
    CLAUDE_SHIM_SETTINGS_PATH="${CLAUDE_SHIM_SETTINGS_PATH:-/tmp/claude-shim/settings.json}"
    # Best-effort generation; the helper is fail-safe (returns 0 even
    # on parse errors and logs to stderr).
    /usr/local/bin/generate-hooks-shim-settings \
        --input "${HOME:-/home/hndrewaall}/.claude/settings.json" \
        --output "$CLAUDE_SHIM_SETTINGS_PATH" || true
    # If the helper didn't produce an output file (input missing,
    # unparseable), clear the var so we don't pass a broken --settings
    # path to claude.
    if [ ! -f "$CLAUDE_SHIM_SETTINGS_PATH" ]; then
        CLAUDE_SHIM_SETTINGS_PATH=""
    fi
fi
export CLAUDE_SHIM_SETTINGS_PATH

# Pre-trust the in-container workspace so Claude Code skips its first-launch
# "Quick safety check: Is this a project you created or one you trust?"
# prompt. The trust state lives at projects[<path>].hasTrustDialogAccepted
# in ~/.claude.json (which is bind-mounted rw from the host operator's
# ${HOME}/.claude.json in the example compose stack). The merge preserves
# every other project entry already in the file; see container/bin/
# trust-workspace.py for the full safety / idempotency contract.
#
# WORKSPACE defaults to /workspace (matches the Dockerfile WORKDIR and the
# example compose claude-tmux working_dir). Override with $WORKSPACE if a
# downstream image lands claude in a different cwd.
#
# Graceful no-op when ~/.claude.json is missing, unparseable, or the
# bind-mount is read-only — the trust prompt would just resurface on first
# launch in that case, which is the same UX as a fresh upstream image. We
# wrap in `|| true` defensively even though the script never exits non-zero
# on recoverable errors, because `set -euo pipefail` is in effect.
if [ -x /usr/local/bin/trust-workspace ]; then
    /usr/local/bin/trust-workspace "${WORKSPACE:-/workspace}" || true
    # Also pre-trust the host-project-dir cwd when CLAUDE_HOST_PROJECT_DIR
    # is the active WORKDIR (claude-tmux bind-mounted that path at the
    # same absolute path inside the container). Without this, the in-
    # container claude would still see the trust prompt on first launch
    # at the project path even though /workspace is already trusted.
    if [ -n "${CLAUDE_HOST_PROJECT_DIR:-}" ] && [ -d "$CLAUDE_HOST_PROJECT_DIR" ]; then
        /usr/local/bin/trust-workspace "$CLAUDE_HOST_PROJECT_DIR" || true
    fi
fi

cleanup() {
    # Killing the tmux session terminates both panes' child processes
    # (pane 0's claude, pane 1's claude-watch daemon) via SIGHUP from the
    # tmux server. This is the right shape for SIGTERM/SIGINT delivered to
    # PID 1 (this script) from `docker stop` / wrapper signal forwarding.
    if tmux has-session -t "$SESSION" 2>/dev/null; then
        tmux kill-session -t "$SESSION" || true
    fi
    exit 0
}
trap cleanup TERM INT

# Debug / one-shot exec path. Skips tmux entirely — `claude-tmux bash`,
# `claude-tmux bash -c "..."`, and any other argv-passed command path
# bypasses both panes. Important for the Phase 0e validation pattern
# (non-interactive `claude --print` via `claude-tmux bash -c "..."`).
if [ "$#" -gt 0 ]; then
    exec "$@"
fi

# Build the tmux session. Start detached so we can configure panes before
# attaching, then attach at the end (blocks until session ends).
#
# Pane 0 uses `exec claude` (Phase 1.5 fix #2, q-2026-05-11-d7c0): bash replaces
# itself with the claude binary, so tmux's `#{pane_current_command}` reports
# `claude` rather than `bash`. claude-watch's status command uses
# `pane_current_command == "claude"` as its primary pane-discovery filter
# (claude-watch/src/status.rs); the prior wrapper `claude; echo ...; read`
# kept bash as PID 1 of the pane, so the filter never matched and pane
# discovery silently no-op'd. Trade-off: the pane now closes immediately when
# claude exits (no "press Enter to close pane" UX). Acceptable for Phase 1.5;
# Phase 2+ can revisit if users need post-exit inspection. See Phase 1f §8
# "Bug #2" in the project doc.
# Build the claude invocation. When the rewritten settings file exists
# (CLAUDE_CONTAINER_REWRITE_HOOKS=1 path above), pass it via --settings so
# Claude Code merges the wrapped hook commands on top of the user/project
# cascade. Otherwise launch claude bare to preserve the existing default.
CLAUDE_CMD="exec claude"
if [ -n "${CLAUDE_SHIM_SETTINGS_PATH:-}" ]; then
    CLAUDE_CMD="exec claude --settings ${CLAUDE_SHIM_SETTINGS_PATH}"
fi

tmux new-session -d -s "$SESSION" -x 200 -y 50 \
    "$CLAUDE_CMD"

# Optional sidebar: when CLAUDE_CONTAINER_SIDEBAR=1, split off a 25%-wide
# right pane running the in-container claude-watch daemon. Bare
# `claude-watch` with no subcommand runs the daemon (same invocation as
# the host's systemd unit). If the binary fails to start (config parse
# error, missing tmux server, etc.) we surface stderr inline AND keep the
# pane open with a shell prompt so the failure is visible on
# `tmux attach` instead of leaving a closed pane and an empty session.
#
# Default-off because the sidebar renders as a too-narrow strip in
# typical browser terminals (the ttyd web console), and the dashboard
# docs already say claude-only is the default single-pane shape.
if [ "${CLAUDE_CONTAINER_SIDEBAR:-0}" = "1" ]; then
    tmux split-window -h -t "$SESSION:0" -p 25 \
        "echo '[pane 1] starting claude-watch (config=$CLAUDE_WATCH_CONFIG)'; \
         claude-watch 2>&1 || { ec=\$?; \
            echo; echo '[pane 1] claude-watch exited with code '\$ec; \
            echo '[pane 1] dropping to shell so you can inspect; exit to close'; \
            exec bash; }"
fi

# Focus the main claude pane.
tmux select-pane -t "$SESSION:0.0"

# Attach. Blocks until the session exits.
exec tmux attach-session -t "$SESSION"
