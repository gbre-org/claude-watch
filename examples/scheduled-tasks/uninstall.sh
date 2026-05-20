#!/usr/bin/env bash
# Idempotent uninstaller for the claude-watch scheduled-task example.
#
# Removes the user-level timer/service or LaunchAgent installed by install.sh.
# Safe to run on a clean system (will be a no-op).
#
# Usage:
#     ./uninstall.sh

set -euo pipefail

NAME="claude-watch-index-refresh"
LAUNCHD_LABEL="org.gbre.claude-watch.index-refresh"

log() { printf '[uninstall] %s\n' "$*"; }

case "$(uname -s)" in
    Linux)
        if ! command -v systemctl >/dev/null 2>&1; then
            log "systemctl not present; nothing to remove."
            exit 0
        fi

        local_unit_dir="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"

        if systemctl --user list-unit-files "${NAME}.timer" 2>/dev/null | grep -q "^${NAME}.timer"; then
            log "disabling + stopping ${NAME}.timer"
            systemctl --user disable --now "${NAME}.timer" || true
        else
            log "${NAME}.timer not currently installed (skipping disable)"
        fi

        if [ -f "${local_unit_dir}/${NAME}.service" ]; then
            log "removing ${local_unit_dir}/${NAME}.service"
            rm -f "${local_unit_dir}/${NAME}.service"
        fi
        if [ -f "${local_unit_dir}/${NAME}.timer" ]; then
            log "removing ${local_unit_dir}/${NAME}.timer"
            rm -f "${local_unit_dir}/${NAME}.timer"
        fi

        log "systemctl --user daemon-reload"
        systemctl --user daemon-reload

        log "done. (logs at /var/log/journal or 'journalctl --user -u ${NAME}' remain until journal rotates.)"
        ;;

    Darwin)
        if ! command -v launchctl >/dev/null 2>&1; then
            log "launchctl not present; nothing to remove."
            exit 0
        fi

        plist_dst="${HOME}/Library/LaunchAgents/${LAUNCHD_LABEL}.plist"

        if launchctl print "gui/${UID}/${LAUNCHD_LABEL}" >/dev/null 2>&1; then
            log "booting out ${LAUNCHD_LABEL}"
            launchctl bootout "gui/${UID}" "$plist_dst" 2>/dev/null \
                || launchctl bootout "gui/${UID}/${LAUNCHD_LABEL}" 2>/dev/null \
                || true
        else
            log "${LAUNCHD_LABEL} not currently loaded (skipping bootout)"
        fi

        if [ -f "$plist_dst" ]; then
            log "removing ${plist_dst}"
            rm -f "$plist_dst"
        fi

        # Leave /tmp/${LAUNCHD_LABEL}.{out,err} alone; they're test output
        # and the user may want to inspect them after uninstall.

        log "done."
        ;;

    *)
        log "unsupported OS $(uname -s); nothing to do."
        exit 0
        ;;
esac
