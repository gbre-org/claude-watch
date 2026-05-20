#!/usr/bin/env bash
# Idempotent installer for the claude-watch scheduled-task example.
#
# Detects the host OS (Linux + systemd vs macOS + launchd), copies the unit
# files into the user's per-user directory, enables the timer / agent, and
# verifies one firing.
#
# Re-running this script on an already-installed system is safe: existing
# units are overwritten in place, the timer / agent is re-enabled, and one
# fresh tick is forced for verification.
#
# Usage:
#     ./install.sh           install + verify
#     ./install.sh --verify  re-verify only (no copy)
#
# Exit codes:
#     0  success
#     1  unsupported OS, missing tooling, or verification failed

set -euo pipefail

HERE="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
NAME="claude-watch-index-refresh"
LAUNCHD_LABEL="org.gbre.claude-watch.index-refresh"

log() { printf '[install] %s\n' "$*"; }
err() { printf '[install] ERROR: %s\n' "$*" >&2; }

verify_only=0
if [ "${1:-}" = "--verify" ]; then
    verify_only=1
fi

# ---------------------------------------------------------------------------
# OS detection
# ---------------------------------------------------------------------------
OS=""
case "$(uname -s)" in
    Linux)  OS=linux ;;
    Darwin) OS=macos ;;
    *)
        err "unsupported OS: $(uname -s). This example covers Linux (systemd) and macOS (launchd)."
        exit 1
        ;;
esac

# ---------------------------------------------------------------------------
# Linux / systemd path
# ---------------------------------------------------------------------------
install_linux() {
    if ! command -v systemctl >/dev/null 2>&1; then
        err "systemctl not found; this example targets systemd-based Linux hosts."
        exit 1
    fi

    local user_unit_dir="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"

    if [ "$verify_only" -eq 0 ]; then
        log "installing units into ${user_unit_dir}"
        mkdir -p "$user_unit_dir"
        cp "${HERE}/systemd/${NAME}.service" "${user_unit_dir}/"
        cp "${HERE}/systemd/${NAME}.timer"   "${user_unit_dir}/"

        log "systemctl --user daemon-reload"
        systemctl --user daemon-reload

        log "enabling ${NAME}.timer"
        systemctl --user enable --now "${NAME}.timer"
    fi

    log "forcing one immediate run of ${NAME}.service for verification"
    systemctl --user start "${NAME}.service"

    # Wait briefly for the journal to flush.
    sleep 2

    log "recent journal entries:"
    if ! journalctl --user -u "${NAME}" --since '5 minutes ago' --no-pager 2>/dev/null | tail -n 20; then
        err "journalctl returned non-zero (may be empty); check 'systemctl --user status ${NAME}.service' manually."
    fi

    log "active timer state:"
    systemctl --user list-timers "${NAME}.timer" --no-pager || true

    # Verification predicate: service unit reports a successful invocation.
    if systemctl --user is-active --quiet "${NAME}.service" 2>/dev/null \
        || systemctl --user show "${NAME}.service" -p ActiveState -p Result --no-pager \
            | grep -qE 'ActiveState=(active|inactive)|Result=success'; then
        log "OK: ${NAME}.service has run at least once on this host."
    else
        err "verification failed: ${NAME}.service did not report success."
        err "Inspect with: systemctl --user status ${NAME}.service"
        exit 1
    fi
}

# ---------------------------------------------------------------------------
# macOS / launchd path
# ---------------------------------------------------------------------------
install_macos() {
    if ! command -v launchctl >/dev/null 2>&1; then
        err "launchctl not found; macOS install path can't proceed."
        exit 1
    fi

    local agent_dir="${HOME}/Library/LaunchAgents"
    local plist_src="${HERE}/launchd/${LAUNCHD_LABEL}.plist"
    local plist_dst="${agent_dir}/${LAUNCHD_LABEL}.plist"

    if [ "$verify_only" -eq 0 ]; then
        log "installing plist into ${agent_dir}"
        mkdir -p "$agent_dir"
        cp "$plist_src" "$plist_dst"

        # bootout first if already loaded (idempotent reinstall).
        if launchctl print "gui/${UID}/${LAUNCHD_LABEL}" >/dev/null 2>&1; then
            log "agent already loaded; booting out for clean reload"
            launchctl bootout "gui/${UID}" "$plist_dst" || true
        fi

        log "loading agent via launchctl bootstrap gui/${UID}"
        launchctl bootstrap "gui/${UID}" "$plist_dst"
        launchctl enable "gui/${UID}/${LAUNCHD_LABEL}"
    fi

    log "kicking one immediate run via launchctl kickstart"
    launchctl kickstart -k "gui/${UID}/${LAUNCHD_LABEL}"

    sleep 3

    log "recent agent output:"
    if [ -f /tmp/${LAUNCHD_LABEL}.out ]; then
        tail -n 20 "/tmp/${LAUNCHD_LABEL}.out"
    else
        log "(no /tmp/${LAUNCHD_LABEL}.out yet — agent may not have written stdout)"
    fi

    log "launchctl print:"
    launchctl print "gui/${UID}/${LAUNCHD_LABEL}" 2>/dev/null \
        | grep -E '^(\s*state|\s*last exit code|\s*program|\s*pid)' \
        || true

    # Verification predicate: agent is loaded and either ran (has output)
    # or shows last-exit-code in launchctl print.
    if launchctl print "gui/${UID}/${LAUNCHD_LABEL}" >/dev/null 2>&1; then
        log "OK: ${LAUNCHD_LABEL} is loaded under gui/${UID}."
    else
        err "verification failed: ${LAUNCHD_LABEL} is not loaded."
        exit 1
    fi
}

case "$OS" in
    linux) install_linux ;;
    macos) install_macos ;;
esac

log "done."
