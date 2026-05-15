#!/bin/bash
# dir-watch.sh — generic re-arming directory watcher primitive.
#
# Block until a new file matching $WATCH_PATTERN appears under $WATCH_DIR,
# invoke $WATCH_CALLBACK with the file's full path as $1, then loop back
# to watching. Never exits unless interrupted (SIGTERM / SIGINT) or the
# watch dir is unrecoverable.
#
# Contract:
#   - Required env: WATCH_DIR, WATCH_PATTERN, WATCH_CALLBACK.
#   - WATCH_DIR: absolute path to the directory to monitor.
#   - WATCH_PATTERN: bash glob pattern (NOT regex) matched against the
#                    filename (basename, not the full path). Examples:
#                      '*.json'      — anything ending in .json
#                      'v[0-9]*.md'  — v<digit>...md style
#                      '*'           — every file in the dir
#   - WATCH_CALLBACK: shell command (string). It will be invoked as:
#                       bash -c "$WATCH_CALLBACK" -- "<full-path>"
#                     so positional arg $1 inside the callback is the
#                     newly-matched file's full path.
#
# Re-arming behaviour:
#   - On startup the script drains anything already in WATCH_DIR that
#     matches the pattern (i.e. files that landed before the watcher
#     started are NOT lost).
#   - inotifywait runs in monitor mode (-m) so a single inotify handle
#     persists across many events. Each `create` / `moved_to` event
#     fires the callback exactly once for the matching filename.
#   - Fallback: when inotifywait isn't on PATH we poll the directory
#     every $POLL_INTERVAL_SECS (default 3) and emit callbacks for any
#     newly-seen filename that matches the pattern.
#
# State:
#   - "Already seen" filenames are tracked in a state file at
#     $WATCH_STATE_FILE (default /tmp/dir-watch-<sha1 of WATCH_DIR>.state).
#     Lines are bare filenames (one per line). The state file is
#     re-created if it disappears; survives across watcher restarts so
#     a quick respawn doesn't double-fire events that haven't been
#     removed from the dir.
#
# Logging:
#   - stdout: one line per fired callback in the shape
#       dir-watch: fire <basename>
#     plus the callback's own stdout (unrendered).
#   - stderr: dir-watch's own diagnostic messages. The supervisor that
#     spawned us routes both to the log path declared in the watcher's
#     .toml.
#
# Exit conditions (all non-zero are terminal — supervisor re-spawns):
#   - WATCH_DIR is not a readable directory: exit 2.
#   - WATCH_CALLBACK unset: exit 2.
#   - WATCH_PATTERN unset: exit 2.
#   - inotifywait fails AND poll fallback fails (e.g. WATCH_DIR vanishes
#     mid-flight and cannot be re-created): exit 1.
#
# The script never daemonizes; it stays foreground so the supervisor's
# process handle (Claude Code bash_id / `tmux respawn-pane`) remains
# valid and SIGTERM propagates cleanly.

set -uo pipefail

WATCH_DIR="${WATCH_DIR:-}"
WATCH_PATTERN="${WATCH_PATTERN:-}"
WATCH_CALLBACK="${WATCH_CALLBACK:-}"
POLL_INTERVAL_SECS="${POLL_INTERVAL_SECS:-3}"

# --- arg / env validation -------------------------------------------------

if [[ -z "$WATCH_DIR" ]]; then
    echo "dir-watch: WATCH_DIR is required" >&2
    exit 2
fi
if [[ -z "$WATCH_PATTERN" ]]; then
    echo "dir-watch: WATCH_PATTERN is required" >&2
    exit 2
fi
if [[ -z "$WATCH_CALLBACK" ]]; then
    echo "dir-watch: WATCH_CALLBACK is required" >&2
    exit 2
fi

# Create the watch dir if it doesn't exist yet (common case: a fresh
# container that hasn't had any cron events fire). mkdir -p is a no-op
# on an existing dir and creates parents as needed.
mkdir -p "$WATCH_DIR" 2>/dev/null || true
if [[ ! -d "$WATCH_DIR" ]] || [[ ! -r "$WATCH_DIR" ]]; then
    echo "dir-watch: WATCH_DIR=$WATCH_DIR is not a readable directory" >&2
    exit 2
fi

# --- state file -----------------------------------------------------------
#
# Hash the absolute path of WATCH_DIR so two watchers on different dirs
# don't collide on /tmp state. sha1sum is in coreutils on every Debian
# image; if it's missing for some exotic reason fall back to a base64-
# encoded digest of the path (cksum is also coreutils).

dir_hash() {
    local h
    if command -v sha1sum >/dev/null 2>&1; then
        h=$(printf '%s' "$1" | sha1sum | awk '{print $1}')
    else
        h=$(printf '%s' "$1" | cksum | awk '{print $1}')
    fi
    printf '%s' "$h"
}

STATE_FILE="${WATCH_STATE_FILE:-/tmp/dir-watch-$(dir_hash "$WATCH_DIR").state}"
mkdir -p "$(dirname "$STATE_FILE")" 2>/dev/null || true
touch "$STATE_FILE" 2>/dev/null || {
    echo "dir-watch: cannot create state file at $STATE_FILE" >&2
    exit 2
}

echo "dir-watch: watching $WATCH_DIR for $WATCH_PATTERN (state=$STATE_FILE)" >&2

# Has-fired check: O(N) grep against the state file. Good enough for
# the small N expected (~tens of events between supervisor restarts).
already_seen() {
    local name="$1"
    grep -Fxq "$name" "$STATE_FILE" 2>/dev/null
}

mark_seen() {
    local name="$1"
    # O_APPEND is atomic for line-sized writes on POSIX.
    printf '%s\n' "$name" >> "$STATE_FILE"
}

# --- callback dispatch ----------------------------------------------------

fire_callback() {
    local full_path="$1"
    local name
    name=$(basename "$full_path")

    # Skip if file vanished between detection and dispatch (e.g. a
    # peer process consumed it). Don't mark-seen here — if the file
    # comes back with the same name we want to refire.
    if [[ ! -e "$full_path" ]]; then
        return 0
    fi

    if already_seen "$name"; then
        return 0
    fi

    mark_seen "$name"
    echo "dir-watch: fire $name"
    # Invoke the callback via bash -c so $WATCH_CALLBACK can be any
    # shell snippet ("foo $1 bar", "/path/to/script", etc.). The `--`
    # is the $0 slot, then "$full_path" is $1 inside the callback.
    bash -c "$WATCH_CALLBACK" -- "$full_path" || {
        local rc=$?
        echo "dir-watch: callback exited $rc for $name (continuing)" >&2
    }
}

# --- drain pass: pick up anything already pending -------------------------

drain_pending() {
    # nullglob so empty matches expand to nothing instead of the
    # literal pattern. shopt is local to this function via subshell.
    (
        shopt -s nullglob
        # WATCH_PATTERN is a bash glob. We deliberately do NOT quote
        # the glob so it expands.
        for f in "$WATCH_DIR"/$WATCH_PATTERN; do
            [[ -f "$f" ]] || continue
            fire_callback "$f"
        done
    )
}

drain_pending

# --- main loop: inotifywait monitor mode, or poll fallback ----------------

if command -v inotifywait >/dev/null 2>&1; then
    # -m: monitor mode (don't exit after first event)
    # -q: quiet (suppress the startup banner)
    # -e create -e moved_to: react to new files (atomic-rename emitters
    #     use moved_to, simple writers use create)
    # --format '%f': just the basename, one per line on stdout
    # On WATCH_DIR vanishing inotifywait exits non-zero; we fall back
    # to the poll loop in that case so the watcher self-heals.
    echo "dir-watch: using inotifywait" >&2
    while :; do
        # Read each event line from inotifywait's stdout. The pipe
        # keeps inotifywait alive across many events; the inner read
        # blocks until the next event line arrives.
        while IFS= read -r name; do
            [[ -z "$name" ]] && continue
            # Match against the bash glob WATCH_PATTERN. Use the
            # extglob-safe `[[ ... == pattern ]]` form (unquoted RHS).
            # shellcheck disable=SC2053
            if [[ "$name" == $WATCH_PATTERN ]]; then
                fire_callback "$WATCH_DIR/$name"
            fi
        done < <(
            inotifywait \
                -m -q \
                -e create -e moved_to \
                --format '%f' \
                "$WATCH_DIR" 2>/dev/null
        )
        # inotifywait exited (dir went away, fd exhaustion, etc.).
        # Sleep briefly + retry; if WATCH_DIR is back we'll re-arm.
        echo "dir-watch: inotifywait exited — retrying in 5s" >&2
        sleep 5
        if [[ ! -d "$WATCH_DIR" ]]; then
            mkdir -p "$WATCH_DIR" 2>/dev/null || true
        fi
        # Drain anything that landed during the gap.
        drain_pending
    done
else
    echo "dir-watch: inotifywait not found, polling every ${POLL_INTERVAL_SECS}s" >&2
    while :; do
        sleep "$POLL_INTERVAL_SECS"
        drain_pending
    done
fi
