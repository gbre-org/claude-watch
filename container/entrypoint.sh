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
#   sudo docker exec <container> cat /var/log/claude-watch/claude-watch.jsonl

set -euo pipefail

SESSION="claude-container"

# Managed-settings guard — make a bad/empty managed-settings mount resolve
# to tier-ABSENT, never a hard read error that wedges the launch.
#
# The Dockerfile no longer bakes a static symlink at
# /etc/claude-code/managed-settings.json (it used to point at the
# /mnt/host-managed-claude-config staging mount). The reason: when
# CLAUDE_HOST_MANAGED_SETTINGS_DIR is empty/unset, the example compose
# mounts /dev/null at that staging path, so the symlink traversed a
# NON-directory and resolved to ENOTDIR — a hard read error (NOT the
# dangling-ENOENT the old comment assumed). Claude Code then drops into
# the interactive "Settings Error" modal on the ENOTDIR read and BLOCKS
# on a keypress nobody presses → the detached `tmux new-session -d` pane
# wedges and the operator sees "no session to connect to".
#
# We create the symlink CONDITIONALLY here: only when the staging mount
# is a real directory holding a readable regular managed-settings.json.
# Otherwise we `rm -f` the link so the managed tier is genuinely absent
# (ENOENT, which Claude Code tolerates). The final `test -f -r` re-check
# is an unconditional belt-and-suspenders guard that ALSO catches a stale
# symlink, a directly-mounted /dev/null, or any other non-regular-file
# shape at the managed path — so a bad mount can NEVER wedge the pane.
_managed_link="/etc/claude-code/managed-settings.json"
_managed_src="/mnt/host-managed-claude-config/managed-settings.json"
if [ -d /mnt/host-managed-claude-config ] && [ -f "${_managed_src}" ] && [ -r "${_managed_src}" ]; then
    ln -sf "${_managed_src}" "${_managed_link}"
else
    rm -f "${_managed_link}"
fi
# Final unconditional guard: if the managed-settings path is anything
# other than a readable regular file (covers ENOTDIR via /dev/null mount,
# a directory, a dangling/looping symlink, a non-regular special file),
# remove it so claude launches with the managed tier cleanly absent.
if [ ! -f "${_managed_link}" ] || [ ! -r "${_managed_link}" ]; then
    rm -f "${_managed_link}"
fi
unset _managed_link _managed_src

# Reconcile the native claude install against the versions VOLUME.
#
# claude-code is installed + updated via the NATIVE managed installer (see the
# Dockerfile native-install block), which keeps versions under
# ~/.local/share/claude/versions/<ver> — a path backed by the
# claude-container-versions named volume, so in-app auto-updates PERSIST across
# `docker compose up --force-recreate`. Two pieces do NOT live on the volume
# and must be reconciled on every start, which reconcile-native-claude does:
#
#   1. Re-point the ephemeral ~/.local/bin/claude launcher at the NEWEST
#      version present in the volume. Without this, a recreate resets the
#      launcher to the image-baked version and the container ROLLS BACK even
#      though the newer auto-updated binary is still in the volume (#1158).
#   2. Pin installMethod=native in the bind-mounted ~/.claude.json (it may
#      carry a stale installMethod=global from the prior npm era), so the
#      in-app updater writes into the versions volume, not npm-global.
#
# Best-effort + fail-safe: the helper always exits 0 (missing versions dir /
# read-only or absent ~/.claude.json are graceful no-ops), so `set -euo
# pipefail` never aborts the entrypoint on it. `|| true` belt-and-suspenders.
if [ -x /usr/local/bin/reconcile-native-claude ]; then
    /usr/local/bin/reconcile-native-claude || true
fi

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

# Defensively prepend $HOME/.local/bin (Claude Code's "native install"
# location). The Dockerfile already sets ENV PATH with this dir in front,
# but a downstream image or a `docker run -e PATH=...` override could
# clobber it. Without this dir on PATH and a self-updated
# $HOME/.local/bin/claude present, every launch prints a yellow:
#
#   Native installation exists but ~/.local/bin is not in your PATH.
#
# warning. Belt-and-suspenders so the warning stays gone regardless of
# how PATH is set on the way in.
_local_bin="${HOME:-/home/hndrewaall}/.local/bin"
case ":${PATH}:" in
    *":${_local_bin}:"*) ;;
    *) export PATH="${_local_bin}:${PATH}" ;;
esac
unset _local_bin

# Symlink user utility scripts into ~/.local/bin (already on PATH above) so
# they're discoverable by tools that use shutil.which() — notably
# session-task's _pingme_notify(), which shells out to ``queue-notify`` (the
# dedicated queue Pushover path) and historically to a host-pluggable
# ``pingme`` push-notification shim. The repos/ tree is bind-mounted
# read-only; the symlinks live on the ephemeral overlay but are recreated
# every container start by this block.
#
# The ``pingme`` shim is host-pluggable (ntfy / Apprise / a homebrew script /
# etc.): point ``PINGME_SRC`` at an executable on a mounted path to surface it
# in the container as ``pingme``. Unset → no pingme symlink (queue-notify
# still handles the queue Pushover path).
#
# The botchat CLIs (botchat-send / -unread-check / -show / -history) are
# linked here too so the main loop can invoke them BARE — e.g. `botchat-send
# "..."` instead of the full `~/repos/botchat/bin/botchat-send`. They live in
# the operator's botchat repo, bind-mounted read-only under ~/repos; each CLI
# self-resolves its repo root via `Path(__file__).resolve().parent.parent`,
# so a symlink works (`.resolve()` canonicalizes through it to the real path).
# Linking into ~/.local/bin (already on PATH) rather than adding ~/bin to PATH
# keeps the change surgical — no host-native binaries (falcon, slack, devbar)
# get pulled onto the container PATH. Missing CLIs are skipped (the `-x` guard),
# so this is a no-op when the botchat repo isn't mounted.
_user_bin="${HOME:-/home/hndrewaall}/.local/bin"
for _script in \
    "${PINGME_SRC:-}" \
    "${HOME:-/home/hndrewaall}/repos/claude-watch/tools/session-task/queue-notify" \
    "${HOME:-/home/hndrewaall}/repos/botchat/bin/botchat-send" \
    "${HOME:-/home/hndrewaall}/repos/botchat/bin/botchat-unread-check" \
    "${HOME:-/home/hndrewaall}/repos/botchat/bin/botchat-show" \
    "${HOME:-/home/hndrewaall}/repos/botchat/bin/botchat-history"; do
    [ -n "$_script" ] || continue
    if [ -x "$_script" ] && [ ! -e "${_user_bin}/$(basename "$_script")" ]; then
        ln -sf "$_script" "${_user_bin}/$(basename "$_script")" 2>/dev/null || true
    fi
done
unset _user_bin _script

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
# /usr/local/bin/exec-hook, and tell the in-container claude to load it as
# the user-tier settings file. The host file is never touched.
#
# Wiring: `claude --setting-sources project,local --settings <shim-path>`.
#
# - `--setting-sources project,local` filters the bind-mounted host
#   `~/.claude/settings.json` (the "user" tier) OUT of Claude Code's
#   settings cascade. Without this, the bare host hook commands would
#   STILL load and fire alongside the wrapped ones (additive merge), and
#   the bare ones would STILL hit "Exec format error" on every hook
#   event — exactly the symptom the v19 workbot validation surfaced.
# - `--settings <shim-path>` then loads our rewritten file as an
#   additional settings source. Because the user tier is filtered out,
#   the shim's wrapped hooks (plus its env / permissions passthrough,
#   preserved by generate-hooks-shim-settings) effectively REPLACE the
#   host user tier inside the container, while leaving the host file
#   untouched on disk.
# - Project (`<cwd>/.claude/settings.json`) and local
#   (`<cwd>/.claude/settings.local.json`) tiers continue to load
#   normally so per-repo overrides still work.
#
# Claude Code's default `settingSources` is `["user","project","local"]`
# (verified in the binary, ~/.local/share/claude/versions/2.1.141: the
# default literal lives next to `kh5=["user","project","local"]`). Passing
# `--setting-sources project,local` drops the user tier explicitly; Claude
# Code logs "userSettings source is disabled (--setting-sources)" on
# unrelated retention-cleanup paths so the suppression is internally
# observable.
#
# Default OFF so existing operators see no behaviour change. Mac-host
# operators flip the flag in their .env / docker-compose.override.yml.
#
# CLAUDE_SHIM_PATTERNS — operator-tunable glob list that narrows which
# hook + MCP commands get wrapped with /usr/local/bin/exec-hook. When
# unset / empty (the default), every command is wrapped (preserves the
# pre-PR-#147 behaviour exactly). Set to a colon-separated list of
# globs to wrap only matching commands; first whitespace-separated
# token of each command is matched against each glob (fnmatch.fnmatchcase).
# Example: CLAUDE_SHIM_PATTERNS='/Users/*/.local/bin/*:/Users/*/.local/pkgs/*/bin/*'
# Both generate-hooks-shim-settings and generate-project-mcp-json honor
# this env var natively (they each have a `--shim-patterns` flag that
# defaults to $CLAUDE_SHIM_PATTERNS), so we don't need to plumb the
# value through explicitly here — it's read directly from the
# inherited environment. The two `--shim-patterns "$CLAUDE_SHIM_PATTERNS"`
# lines below make that contract visible to anyone scanning the
# entrypoint without having to grep into the python helpers.
# CLAUDE_CONTAINER_OBLIGATIONS — opt-in obligations gate hook installation.
#
# When "1" (the default), the entrypoint runs generate-hooks-shim-settings
# REGARDLESS of CLAUDE_CONTAINER_REWRITE_HOOKS so the rewritten settings
# include the canonical obligations gate hooks (pre-agent-queue-gate,
# pre-tool-obligations-gate, post-tool-obligations-update,
# post-tool-mark-attachment-read). When "0", the obligations gate is not
# wired; useful for container builds where the operator wants the
# container to be a "raw" Claude Code sandbox without the host's
# guardrails. The hooks themselves default-open when the `obligations`
# CLI is missing, so the worst case is a NO-OP per hook fire.
CLAUDE_CONTAINER_OBLIGATIONS="${CLAUDE_CONTAINER_OBLIGATIONS:-1}"
CLAUDE_SHIM_SETTINGS_PATH=""
# CLAUDE_SHIM_FILTER_USER tracks whether the eventual claude invocation
# should pass `--setting-sources project,local` (filter the host user
# tier OUT) or load the shim ADDITIVELY (let the host user tier load
# normally + layer the obligations gates on top). REWRITE_HOOKS=1 wants
# the filter (the shim contains the wrapped host hooks; user-tier
# would re-add the unwrapped ones); obligations-only wants additive
# (host hooks load via user tier, shim only adds the obligations gates).
CLAUDE_SHIM_FILTER_USER=""
export CLAUDE_SHIM_FILTER_USER
if [ "${CLAUDE_CONTAINER_REWRITE_HOOKS:-0}" = "1" ]; then
    CLAUDE_SHIM_SETTINGS_PATH="${CLAUDE_SHIM_SETTINGS_PATH:-/tmp/claude-shim/settings.json}"
    # Project-tier settings file. When an operator bind-mounts host
    # ~/.claude at the project-cwd path (so the cwd's .claude symlink
    # resolves + project-tier slash commands load), that host
    # settings.json ALSO becomes Claude Code's PROJECT tier, read RAW
    # from <cwd>/.claude/settings.json. A poison `apiKeyHelper` there
    # (a host-only Mach-O helper) bypasses the user-tier sanitizer and
    # 127s on every inference. We pass the project-tier path to the
    # generator so it can neutralize that helper by writing an
    # empty-string apiKeyHelper into the shim (flagSettings tier, which
    # outranks projectSettings) -- no host-file mutation, no mount.
    # Strictly gated inside the generator on a detected NON-runnable
    # project helper, so operators without the leak see no change.
    _project_settings=""
    if [ -n "${CLAUDE_HOST_PROJECT_DIR:-}" ] \
            && [ -f "${CLAUDE_HOST_PROJECT_DIR}/.claude/settings.json" ]; then
        _project_settings="${CLAUDE_HOST_PROJECT_DIR}/.claude/settings.json"
    fi
    /usr/local/bin/generate-hooks-shim-settings \
        --input "${HOME:-/home/hndrewaall}/.claude/settings.json" \
        --output "$CLAUDE_SHIM_SETTINGS_PATH" \
        --shim-patterns "${CLAUDE_SHIM_PATTERNS:-}" \
        --neutralize-project-apikeyhelper "$_project_settings" \
        --inject-obligations "$CLAUDE_CONTAINER_OBLIGATIONS" || true
    unset _project_settings
    CLAUDE_SHIM_FILTER_USER=1
elif [ "$CLAUDE_CONTAINER_OBLIGATIONS" = "1" ]; then
    # Obligations gate without the cross-arch hook rewrite. Generate a
    # MINIMAL shim that contains ONLY the obligations gate hooks; the
    # host's user-tier settings.json continues to load normally and the
    # shim layers additively on top. No `--setting-sources project,local`
    # filter — that would drop the host's other hooks (claude-watch
    # hook-fire, inject-signal-context-hook, etc.) along with the
    # cross-arch ones. This path is the right shape for Linux hosts
    # (no Mach-O pain) that still want the gates wired.
    CLAUDE_SHIM_SETTINGS_PATH="${CLAUDE_SHIM_SETTINGS_PATH:-/tmp/claude-shim/settings.json}"
    /usr/local/bin/generate-hooks-shim-settings \
        --input "${HOME:-/home/hndrewaall}/.claude/settings.json" \
        --output "$CLAUDE_SHIM_SETTINGS_PATH" \
        --inject-obligations 1 \
        --obligations-only || true
fi
# If the helper didn't produce an output file (e.g. input missing AND
# obligations injection disabled, or unparseable input), clear the var
# so we don't pass a broken --settings path to claude.
if [ -n "$CLAUDE_SHIM_SETTINGS_PATH" ] && [ ! -f "$CLAUDE_SHIM_SETTINGS_PATH" ]; then
    CLAUDE_SHIM_SETTINGS_PATH=""
fi

# Seed default-bundled obligations rows when the gate is enabled. The
# seeder is idempotent: existing rows tagged `[default-seed]` are
# skipped, so re-running the entrypoint never duplicates rows.
# Currently seeds the `subagent_queue_item_running` row which enforces
# "every subagent tool call must correspond to a running queue item"
# continuously throughout the subagent's lifetime (the existing
# pre-agent-queue-gate-hook enforces it only at SPAWN time; this row +
# the post-tool-agent-arm-hook enforce it AFTER spawn). Best-effort:
# obligations-init exits 0 on any internal failure so a missing
# obligations CLI / broken state file never blocks container start.
# CLAUDE_OBLIGATIONS_MANIFEST_DIR — the in-container path obligations-init
# scans for USER-MANAGED obligation manifests (one *.json row spec per file),
# applied idempotently AFTER the baked default-seed rows. This is the
# bind-mount target of the operator's private $CLAUDE_HOST_OBLIGATIONS_DIR
# (see examples/compose/docker-compose.yml). It lets operator-specific
# obligations (e.g. the AskUserQuestion presence-gate) be DECLARATIVE private
# config that auto-applies on EVERY container start — no per-session
# `register-*` script to forget, and nothing operator-specific baked into the
# shared public image. Defaults to the documented mount path; obligations-init
# itself no-ops cleanly when the dir is absent / empty / a non-directory
# (e.g. an unset operator leaves the mount off entirely), so this is harmless
# on a stripped-down deployment.
CLAUDE_OBLIGATIONS_MANIFEST_DIR="${CLAUDE_OBLIGATIONS_MANIFEST_DIR:-/mnt/host-obligations-config}"
export CLAUDE_OBLIGATIONS_MANIFEST_DIR
if [ "$CLAUDE_CONTAINER_OBLIGATIONS" = "1" ] \
        && [ -x /usr/local/bin/obligations-init ]; then
    /usr/local/bin/obligations-init -v || true
fi

# MCP server json generation runs ONLY when CLAUDE_CONTAINER_REWRITE_HOOKS=1
# (operator opts in to dropping the user-tier settings, which is what
# suppresses the ~/.claude.json MCP discovery path and necessitates the
# project-tier .mcp.json fix). The obligations gate path above does NOT
# require dropping the user tier, so we keep the MCP json generation
# scoped to the rewrite-hooks opt-in.
if [ "${CLAUDE_CONTAINER_REWRITE_HOOKS:-0}" = "1" ]; then
    # MCP server definitions live in ~/.claude.json (where `claude mcp
    # add ...` writes by default) and load via a code path that's
    # gated on the `user` tier being in --setting-sources. Since we
    # filter `user` out below (only when CLAUDE_CONTAINER_REWRITE_HOOKS=1),
    # the `~/.claude.json` MCP discovery path is suppressed and Claude
    # Code reports "No MCP servers configured" inside the container.
    # v21 workbot validation confirmed: PR #145's attempt to inject
    # `mcpServers` into the shim settings.json had zero effect — Claude
    # Code doesn't read MCP definitions from any settings.json tier.
    #
    # Fix: write a project-tier `.mcp.json` inside CLAUDE_HOST_PROJECT_DIR.
    # Project tier IS in `--setting-sources project,local`, and
    # `.mcp.json` is Claude Code's standard project-level MCP config
    # file. (These project servers are AUTO-APPROVED via the shim's
    # `enableAllProjectMcpServers: true` — written by
    # generate-hooks-shim-settings above unless CLAUDE_MCP_AUTOAPPROVE=0 —
    # so they connect instead of showing ⏸ "Pending approval".)
    # The helper wraps each server's `command` with
    # /usr/local/bin/exec-hook so cross-arch host binaries (Mac
    # Mach-O, etc.) silently no-op instead of spamming "Exec format
    # error" on each invocation.
    #
    # No-op when CLAUDE_HOST_PROJECT_DIR is unset (default WORKDIR
    # /workspace doesn't get a .mcp.json — operators without a host
    # project dir get the existing pre-PR behavior of no MCP servers).
    if [ -n "${CLAUDE_HOST_PROJECT_DIR:-}" ] && [ -d "$CLAUDE_HOST_PROJECT_DIR" ]; then
        # CLAUDE_MCP_HTTP_BRIDGE — colon-separated `name=url` pairs.
        # Cross-arch MCP servers (e.g. macOS Mach-O like the
        # a corp host-mcp-server) can't exec inside the Linux
        # container; exec-hook silent-no-ops them, which surfaces in
        # /mcp as "Failed to reconnect: ENOENT". When the operator
        # runs a host-side HTTP→stdio adapter for those binaries
        # (mcp-proxy / mcphost / hand-rolled) they can rewrite the
        # in-container entry to Claude Code's native HTTP transport
        # by setting this env var. The helper consumes it natively
        # (reads from CLAUDE_MCP_HTTP_BRIDGE when --http-bridge isn't
        # passed), but the explicit flag here makes the contract
        # visible to anyone scanning the entrypoint without grepping
        # into the helper.
        /usr/local/bin/generate-project-mcp-json \
            --mcp-input "${HOME:-/home/hndrewaall}/.claude.json" \
            --output-dir "$CLAUDE_HOST_PROJECT_DIR" \
            --shim-patterns "${CLAUDE_SHIM_PATTERNS:-}" \
            --http-bridge "${CLAUDE_MCP_HTTP_BRIDGE:-}" || true
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
    # Best-effort cleanup before the script exits. process-compose (as
    # PID 1) handles the real signal-forwarding + child-reaping
    # contract — it sends SIGTERM to every supervised process on
    # SIGTERM, then SIGKILL after a grace period. The trap here covers
    # the narrow case where this entrypoint script is run OUTSIDE
    # process-compose (debug shell, validation harness) and we want a
    # clean tmux teardown.
    if tmux has-session -t "$SESSION" 2>/dev/null; then
        tmux kill-session -t "$SESSION" || true
    fi
    exit 0
}
trap cleanup TERM INT

# Debug / one-shot exec path. Skips tmux + process-compose entirely —
# `claude-tmux bash`, `claude-tmux bash -c "..."`, and any other
# argv-passed command path bypasses both. Important for the Phase 0e
# validation pattern (non-interactive `claude --print` via
# `claude-tmux bash -c "..."`).
if [ "$#" -gt 0 ]; then
    exec "$@"
fi

# Pre-create the log dirs used by the process-compose-supervised
# services. Logs live under the FHS /var/log/claude-watch/ tree (the
# Dockerfile pre-creates it uid-1000-owned) so operators can
# `docker compose exec <c> tail -f /var/log/claude-watch/...` to
# inspect. Belt-and-suspenders against downstream image overrides.
mkdir -p /var/log/claude-watch/watchers /var/run/claude 2>/dev/null || true
: > /var/log/claude-watch/watchers/supervisor.log 2>/dev/null || true
: > /var/log/claude-watch/claude-watch.jsonl 2>/dev/null || true
: > /var/log/claude-watch/claude-watch.log 2>/dev/null || true
: > /var/log/claude-watch/cron.log 2>/dev/null || true

# Propagate container env vars into /etc/cron.d/00-env so cron jobs
# (which run in a clean environment) inherit CLAUDE_EVENT_QUEUE, HOME,
# and PATH. Without this, cron jobs calling `claude-event` write events
# to the default $HOME/claude-events instead of the bind-mounted host
# directory. The file sorts first in /etc/cron.d/ so its env-var lines
# apply to all subsequent entries (cron processes /etc/cron.d/ in sorted
# order and env-var lines apply to entries in the same file + later files).
#
# /etc/cron.d/ files must be root:root mode 0644 (cron rejects others).
# The Dockerfile grants passwordless sudo for `tee /etc/cron.d/00-env`.
if [ -n "${CLAUDE_EVENT_QUEUE:-}" ]; then
    printf '# /etc/cron.d/00-env — generated by entrypoint from container env vars.\n# Do not edit; regenerated on every container start.\n%s\n%s\n%s\n' \
        "CLAUDE_EVENT_QUEUE=${CLAUDE_EVENT_QUEUE}" \
        "HOME=${HOME:-/home/hndrewaall}" \
        "PATH=${PATH}" \
        | sudo -n tee /etc/cron.d/00-env > /dev/null 2>&1 \
    && sudo -n chmod 0644 /etc/cron.d/00-env 2>/dev/null || true
fi

# Copy bind-mounted private cron entries to a root-owned location.
# /etc/cron.d/private/ is bind-mounted from the host with uid 1000 ownership,
# but cron requires root:root 0644. We copy (not symlink) so ownership is correct.
if [ -d /etc/cron.d/private ] && ls /etc/cron.d/private/* >/dev/null 2>&1; then
    for f in /etc/cron.d/private/*; do
        dest="/etc/cron.d/private-$(basename "$f")"
        sudo -n tee "$dest" < "$f" > /dev/null 2>&1
        sudo -n chmod 0644 "$dest" 2>/dev/null
    done
fi

# CLAUDE_CMD-block contract for new appenders: keep every conditional
# `if [...]; then ... fi` immediately back-to-back (no comments between
# blocks; blank lines also break the consecutive-if regex used by
# container/tests/entrypoint-claude-cmd.test). Document each block's
# purpose with a one-line trailing comment INSIDE the `if` body, OR
# keep a block-level comment WITHIN the `if`. The block-extraction
# regex in the test file matches greedily through the final `fi\n`
# only as long as `if`s are consecutive.
#
# The CLAUDE_CMD construction is now consumed by
# /usr/local/bin/cw-tmux-bootstrap (which process-compose runs as the
# setup-tmux-session oneshot — see /opt/claude-container/process-compose.yml).
# The conditional blocks remain here so the test parser still has a
# canonical source to inspect; bootstrap re-runs the same logic against
# the same env vars at session-creation time.
CLAUDE_CMD="exec claude"
if [ -n "${CLAUDE_SHIM_SETTINGS_PATH:-}" ] && [ -n "${CLAUDE_SHIM_FILTER_USER:-}" ]; then
    CLAUDE_CMD="exec claude --setting-sources project,local --settings ${CLAUDE_SHIM_SETTINGS_PATH}"
fi
if [ -n "${CLAUDE_SHIM_SETTINGS_PATH:-}" ] && [ -z "${CLAUDE_SHIM_FILTER_USER:-}" ]; then
    CLAUDE_CMD="exec claude --settings ${CLAUDE_SHIM_SETTINGS_PATH}"
fi
if [ -d /opt/claude-container/plugin/.claude-plugin ]; then
    CLAUDE_CMD="$CLAUDE_CMD --plugin-dir /opt/claude-container/plugin"
fi
if [ "${CLAUDE_DANGEROUSLY_SKIP_PERMISSIONS:-}" = "1" ]; then
    CLAUDE_CMD="$CLAUDE_CMD --dangerously-skip-permissions"
fi
if [ -n "${CLAUDE_AUTO_CONTINUE:-}" ]; then
    _auto_continue_val="${CLAUDE_AUTO_CONTINUE_PROMPT:-The claude-container process was just (re)created. Run your session-start checklist, start event watchers, then check session-task for pending work.}"
    _auto_continue_quoted="'${_auto_continue_val//\'/\'\\\'\'}'"
    CLAUDE_CMD="$CLAUDE_CMD --continue $_auto_continue_quoted"
fi
export CLAUDE_CMD

# Propagate the per-service opt-out env vars into process-compose's
# environment. process-compose interpolates ${VAR:-default} in its
# config (see /opt/claude-container/process-compose.yml `disabled:` keys),
# so flipping these flags here disables individual supervised
# processes without modifying the baked config.
#
# Legacy env-var aliases — historical entrypoint code used
# CLAUDE_CONTAINER_DAEMON=0 / CLAUDE_CONTAINER_CRON=0 to skip a daemon
# spawn. process-compose.yml uses the *_DISABLED suffix
# (yes-is-disabled semantics). The mapping below preserves the old
# contract.
if [ "${CLAUDE_CONTAINER_DAEMON:-1}" = "0" ]; then
    export CLAUDE_CONTAINER_DAEMON_DISABLED=true
fi
if [ "${CLAUDE_CONTAINER_CRON:-1}" = "0" ]; then
    export CLAUDE_CONTAINER_CRON_DISABLED=true
fi

# Hand off to process-compose. From here forward process-compose is
# the supervisor — it spawns:
#   1. cw-tmux-bootstrap (oneshot — creates tmux session detached)
#   2. tmux-attach (foreground TTY — what the operator interacts with)
#   3. claude-watch daemon (long-running, restart on failure)
#   4. crond (long-running, restart on failure)
#
# Flags:
#   --config <path>    — service declarations.
#   --tui=false        — no curses UI; logs go to stdout/stderr where
#                        docker captures them.
#   --no-server        — disable the HTTP/gRPC API server (we don't
#                        use the management API; saves a port + a
#                        background goroutine).
#   --keep-tui=false   — redundant with --tui=false on newer versions
#                        but kept for forward-compat.
#
# `exec` replaces this script's process with process-compose so
# signals delivered to PID 1 reach process-compose directly. The
# `cleanup` trap above continues to fire if we're invoked OUTSIDE
# process-compose (debug path).
exec /usr/local/bin/process-compose up \
    --config /opt/claude-container/process-compose.yml \
    --tui=false \
    --no-server
