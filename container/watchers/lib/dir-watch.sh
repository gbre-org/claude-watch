#!/bin/bash
# dir-watch.sh — generic re-arming directory watcher primitive.
#
# Block until a matching lifecycle event happens under $WATCH_DIR,
# invoke $WATCH_CALLBACK with the file's full path as $1 and an action
# label as $2, then loop back to watching. Never exits unless
# interrupted (SIGTERM / SIGINT) or the watch dir is unrecoverable.
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
#                       bash -c "$WATCH_CALLBACK" -- "<full-path>" "<action>"
#                     so $1 is the file's full path and $2 is the
#                     action label (see WATCH_EVENTS below).
#
# Optional env:
#   - WATCH_EVENTS: space-separated list of lifecycle events to fire on.
#                   Default: "created" (backwards-compat — pre-events
#                   callers see no behaviour change). Allowed values:
#                     created    — new file appeared (inotify CREATE /
#                                  MOVED_TO; poll: filename not in prior
#                                  snapshot). Includes "moved-in".
#                     modified   — existing file's content changed
#                                  (inotify CLOSE_WRITE; poll: same
#                                  filename + newer mtime). NOTE: not
#                                  fired for newly-created files — only
#                                  for subsequent modifications.
#                     deleted    — file removed or renamed out of the
#                                  dir (inotify DELETE / MOVED_FROM;
#                                  poll: filename gone from glob).
#                     moved-in   — alias for `created` (same inotify
#                                  event, same poll semantics).
#                     moved-out  — alias for `deleted`.
#                     all        — shorthand for all of the above.
#   - POLL_INTERVAL_SECS: fallback poll interval when inotifywait isn't
#                         on PATH (default 3s).
#   - WATCH_STATE_FILE: override the state-file path (default
#                       /tmp/dir-watch-<sha1 of WATCH_DIR>.state).
#
# Action labels passed to the callback as $2:
#   - "created"   — new file or renamed-in
#   - "modified"  — content changed
#   - "deleted"   — removed or renamed-out
#
# Re-arming behaviour:
#   - On startup the script drains anything already in WATCH_DIR that
#     matches the pattern (i.e. files that landed before the watcher
#     started are NOT lost — fired as "created").
#   - inotifywait runs in monitor mode (-m) so a single inotify handle
#     persists across many events. The -e set is built dynamically from
#     WATCH_EVENTS.
#   - Fallback: when inotifywait isn't on PATH we poll the directory
#     every $POLL_INTERVAL_SECS (default 3) and diff against the
#     previous snapshot to emit created / modified / deleted callbacks.
#
# State file format:
#   - Tab-separated lines: <filename>\t<mtime>\t<inode>. Used by both
#     the inotify path (to suppress refire on the same (name, mtime,
#     inode) tuple after a restart) and the poll fallback (to detect
#     created / modified / deleted diffs across polls).
#   - On upgrade from the pre-events format (plain filenames, no tabs)
#     the state file is rewritten to the new format from the current
#     dir snapshot. No callbacks fire for files that were already in
#     the old state file — quiet, controlled migration.
#   - mtime/inode are read via GNU stat (`stat -c '%Y' / '%i'`). The
#     baked image is Debian-based, so GNU stat is guaranteed. Other
#     platforms (e.g. macOS) would need the BSD `-f '%m' / '%i'` form.
#
# Logging:
#   - stdout: one line per fired callback in the shape
#       dir-watch: fire <action> <basename>
#     plus the callback's own stdout (unrendered).
#   - stderr: dir-watch's own diagnostic messages. The supervisor that
#     spawned us routes both to the log path declared in the watcher's
#     .toml.
#
# Exit conditions (all non-zero are terminal — supervisor re-spawns):
#   - WATCH_DIR is not a readable directory: exit 2.
#   - WATCH_CALLBACK unset: exit 2.
#   - WATCH_PATTERN unset: exit 2.
#   - WATCH_EVENTS contains an unrecognised token: exit 2.
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
WATCH_EVENTS="${WATCH_EVENTS:-created}"
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

# Parse WATCH_EVENTS into action flags: WANT_CREATED / WANT_MODIFIED /
# WANT_DELETED. moved-in is folded into WANT_CREATED, moved-out into
# WANT_DELETED. "all" sets everything.
WANT_CREATED=0
WANT_MODIFIED=0
WANT_DELETED=0
for tok in $WATCH_EVENTS; do
    case "$tok" in
        all)
            WANT_CREATED=1
            WANT_MODIFIED=1
            WANT_DELETED=1
            ;;
        created|moved-in)
            WANT_CREATED=1
            ;;
        modified)
            WANT_MODIFIED=1
            ;;
        deleted|moved-out)
            WANT_DELETED=1
            ;;
        *)
            echo "dir-watch: unknown WATCH_EVENTS token: $tok" >&2
            echo "dir-watch: allowed: created modified deleted moved-in moved-out all" >&2
            exit 2
            ;;
    esac
done

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

echo "dir-watch: watching $WATCH_DIR for $WATCH_PATTERN (events=$WATCH_EVENTS, state=$STATE_FILE)" >&2

# --- state file helpers ---------------------------------------------------
#
# State lines: <filename>\t<mtime>\t<inode>. mtime/inode of 0 sentinel
# means "we know the filename was seen but didn't capture metadata"
# (used for the legacy-format migration path).

# Returns the stat tuple "<mtime>\t<inode>" for a path, or "0\t0" if
# the file vanished between glob + stat.
stat_tuple() {
    local f="$1"
    local mtime inode
    mtime=$(stat -c '%Y' "$f" 2>/dev/null) || mtime=0
    inode=$(stat -c '%i' "$f" 2>/dev/null) || inode=0
    printf '%s\t%s' "$mtime" "$inode"
}

# Look up a filename in the state file. Echoes the matched line
# (sans trailing newline) or empty string on miss.
state_lookup() {
    local name="$1"
    # Use awk with a tab field separator so we match the filename
    # exactly, not a substring.
    awk -F'\t' -v n="$name" '$1 == n { print; exit }' "$STATE_FILE" 2>/dev/null
}

# Remove all rows for a filename from the state file. Atomic-ish: write
# to .tmp then rename. mv is atomic on POSIX when src and dst are on
# the same filesystem (true here — both under $STATE_FILE's dir).
state_remove() {
    local name="$1"
    local tmp="$STATE_FILE.tmp.$$"
    awk -F'\t' -v n="$name" '$1 != n' "$STATE_FILE" > "$tmp" 2>/dev/null && \
        mv "$tmp" "$STATE_FILE" 2>/dev/null
    rm -f "$tmp" 2>/dev/null || true
}

# Append a (name, mtime, inode) row. Caller is responsible for removing
# any prior row for the same name first (state_remove).
state_append() {
    local name="$1"
    local mtime="$2"
    local inode="$3"
    printf '%s\t%s\t%s\n' "$name" "$mtime" "$inode" >> "$STATE_FILE"
}

# Replace any prior row for `name` with the current (mtime, inode).
state_upsert() {
    local name="$1"
    local mtime="$2"
    local inode="$3"
    state_remove "$name"
    state_append "$name" "$mtime" "$inode"
}

# Detect legacy state-file format (plain filenames, no tab). If any
# non-empty line lacks a tab, treat the whole file as legacy and
# rewrite it from the current dir snapshot. No callbacks fire for
# files in the legacy state — quiet migration.
maybe_migrate_state() {
    if [[ ! -s "$STATE_FILE" ]]; then
        return 0
    fi
    if grep -qP '\t' "$STATE_FILE" 2>/dev/null; then
        return 0
    fi
    echo "dir-watch: migrating state file from pre-events format" >&2
    local tmp="$STATE_FILE.migrate.$$"
    : > "$tmp"
    # Read each legacy filename, look it up in the dir, capture
    # its current (mtime, inode). Files no longer present get
    # mtime=0/inode=0 so they're treated as "known-seen, no metadata".
    while IFS= read -r legacy_name || [[ -n "$legacy_name" ]]; do
        [[ -z "$legacy_name" ]] && continue
        if [[ -e "$WATCH_DIR/$legacy_name" ]]; then
            local tup
            tup=$(stat_tuple "$WATCH_DIR/$legacy_name")
            printf '%s\t%s\n' "$legacy_name" "$tup" >> "$tmp"
        else
            printf '%s\t0\t0\n' "$legacy_name" >> "$tmp"
        fi
    done < "$STATE_FILE"
    mv "$tmp" "$STATE_FILE" 2>/dev/null || rm -f "$tmp"
}

maybe_migrate_state

# --- callback dispatch ----------------------------------------------------

fire_callback() {
    local full_path="$1"
    local action="$2"
    local name
    name=$(basename "$full_path")

    echo "dir-watch: fire $action $name"
    # Invoke the callback via bash -c so $WATCH_CALLBACK can be any
    # shell snippet ("foo $1 bar", "/path/to/script", etc.). The `--`
    # is the $0 slot, then "$full_path" is $1 and "$action" is $2.
    bash -c "$WATCH_CALLBACK" -- "$full_path" "$action" || {
        local rc=$?
        echo "dir-watch: callback exited $rc for $name ($action; continuing)" >&2
    }
}

# Handle a "created" event: only fire if the filename isn't already
# in state. Records the file's metadata so a later poll/inotify event
# for the same file can distinguish "modified" from "first seen".
#
# Note: re-using a basename (drop, delete, re-drop) without an
# intervening `deleted` handling does NOT re-fire — the state row
# survives. This matches pre-events behaviour and guards against
# double-deliveries when a peer process recreates a file under the
# same name.
handle_created() {
    local full_path="$1"
    local name
    name=$(basename "$full_path")

    if [[ ! -e "$full_path" ]]; then
        # Vanished between detection and dispatch. Don't record state.
        return 0
    fi
    local tup
    tup=$(stat_tuple "$full_path")
    local mtime inode
    mtime=${tup%%$'\t'*}
    inode=${tup##*$'\t'}

    if [[ -n "$(state_lookup "$name")" ]]; then
        # Already known — refresh metadata so a later modify event
        # has the up-to-date baseline, but don't fire.
        state_upsert "$name" "$mtime" "$inode"
        return 0
    fi
    state_upsert "$name" "$mtime" "$inode"
    if (( WANT_CREATED )); then
        fire_callback "$full_path" "created"
    fi
}

# Handle a "modified" event. Only fires if the file was already known
# in the state file (i.e. a prior created event registered it). For
# files that appear out of nowhere as modify events (e.g. a writer
# that created the file before the watcher armed), treat as created.
handle_modified() {
    local full_path="$1"
    local name
    name=$(basename "$full_path")

    if [[ ! -e "$full_path" ]]; then
        return 0
    fi
    local prior
    prior=$(state_lookup "$name")
    local tup
    tup=$(stat_tuple "$full_path")
    local mtime inode
    mtime=${tup%%$'\t'*}
    inode=${tup##*$'\t'}

    if [[ -z "$prior" ]]; then
        # No prior record — treat as a create.
        state_upsert "$name" "$mtime" "$inode"
        if (( WANT_CREATED )); then
            fire_callback "$full_path" "created"
        fi
        return 0
    fi

    # Skip if the file's mtime hasn't advanced since the prior record.
    # This dedupes the CREATE → CLOSE_WRITE sequence inotify emits for
    # every new file: handle_created already recorded mtime, and the
    # subsequent close_write would otherwise double-fire as modified.
    # On a real edit (`echo new > file` later), mtime advances and we
    # fire as expected.
    local prior_mtime=${prior#*$'\t'}
    prior_mtime=${prior_mtime%$'\t'*}
    if [[ "$mtime" == "$prior_mtime" ]]; then
        return 0
    fi

    # Record updated metadata regardless of fire decision.
    state_upsert "$name" "$mtime" "$inode"
    if (( WANT_MODIFIED )); then
        fire_callback "$full_path" "modified"
    fi
}

# Handle a "deleted" event. Only fires if the file was known.
handle_deleted() {
    local full_path="$1"
    local name
    name=$(basename "$full_path")

    local prior
    prior=$(state_lookup "$name")
    if [[ -z "$prior" ]]; then
        # Unknown — nothing to forget, nothing to fire.
        return 0
    fi
    state_remove "$name"
    if (( WANT_DELETED )); then
        fire_callback "$full_path" "deleted"
    fi
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
            handle_created "$f"
        done
    )
}

drain_pending

# --- main loop: inotifywait monitor mode, or poll fallback ----------------

# Optional override: WATCH_DISABLE_INOTIFY=1 forces the poll fallback
# even when inotifywait is on PATH. Used by tests; production callers
# leave this unset.
if [[ -z "${WATCH_DISABLE_INOTIFY:-}" ]] && command -v inotifywait >/dev/null 2>&1; then
    # Build the -e list dynamically from WANT_CREATED / WANT_MODIFIED /
    # WANT_DELETED. We always want create + moved_to even if the user
    # only asked for "modified" or "deleted" — without them we can't
    # update state for files that appear after startup, so a later
    # modify/delete would look like an out-of-nowhere event. handle_*
    # below silently no-ops for unwanted actions.
    INOTIFY_EVENTS=(-e create -e moved_to)
    if (( WANT_MODIFIED )); then
        INOTIFY_EVENTS+=(-e close_write)
    fi
    if (( WANT_DELETED )); then
        INOTIFY_EVENTS+=(-e delete -e moved_from)
    fi
    echo "dir-watch: using inotifywait (events=${INOTIFY_EVENTS[*]})" >&2
    while :; do
        # Read each event line from inotifywait's stdout. The pipe
        # keeps inotifywait alive across many events; the inner read
        # blocks until the next event line arrives. Format is
        # "<filename> <event-flags>" — event-flags can be a single
        # token (CREATE) or a comma-separated set (CREATE,ISDIR).
        while IFS=' ' read -r name flags; do
            [[ -z "$name" ]] && continue
            # Match against the bash glob WATCH_PATTERN. Use the
            # extglob-safe `[[ ... == pattern ]]` form (unquoted RHS).
            # shellcheck disable=SC2053
            if [[ "$name" != $WATCH_PATTERN ]]; then
                continue
            fi
            # Skip directories — we only care about files. inotify
            # encodes "is a directory" in the flag set as ISDIR.
            if [[ "$flags" == *ISDIR* ]]; then
                continue
            fi
            local_path="$WATCH_DIR/$name"
            case "$flags" in
                *CREATE*|*MOVED_TO*)
                    handle_created "$local_path"
                    ;;
                *CLOSE_WRITE*)
                    handle_modified "$local_path"
                    ;;
                *DELETE*|*MOVED_FROM*)
                    handle_deleted "$local_path"
                    ;;
            esac
        done < <(
            inotifywait \
                -m -q \
                "${INOTIFY_EVENTS[@]}" \
                --format '%f %e' \
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
        # Build the current dir snapshot keyed by filename. We compare
        # against the state file to detect created / modified /
        # deleted.
        declare -A CURRENT_MTIME=()
        declare -A CURRENT_INODE=()
        (
            shopt -s nullglob
            for f in "$WATCH_DIR"/$WATCH_PATTERN; do
                [[ -f "$f" ]] || continue
                bn=$(basename "$f")
                mtime=$(stat -c '%Y' "$f" 2>/dev/null) || continue
                inode=$(stat -c '%i' "$f" 2>/dev/null) || continue
                printf '%s\t%s\t%s\n' "$bn" "$mtime" "$inode"
            done
        ) > "$STATE_FILE.snap.$$" || true

        # Read current snapshot into associative arrays.
        while IFS=$'\t' read -r bn mtime inode || [[ -n "$bn" ]]; do
            [[ -z "$bn" ]] && continue
            CURRENT_MTIME["$bn"]="$mtime"
            CURRENT_INODE["$bn"]="$inode"
        done < "$STATE_FILE.snap.$$"

        # Read prior state into associative arrays.
        declare -A PRIOR_MTIME=()
        declare -A PRIOR_INODE=()
        while IFS=$'\t' read -r bn mtime inode || [[ -n "$bn" ]]; do
            [[ -z "$bn" ]] && continue
            PRIOR_MTIME["$bn"]="${mtime:-0}"
            PRIOR_INODE["$bn"]="${inode:-0}"
        done < "$STATE_FILE"

        # Diff: created / modified.
        for bn in "${!CURRENT_MTIME[@]}"; do
            cur_mtime="${CURRENT_MTIME[$bn]}"
            cur_inode="${CURRENT_INODE[$bn]}"
            if [[ -z "${PRIOR_MTIME[$bn]+x}" ]]; then
                # New filename.
                handle_created "$WATCH_DIR/$bn"
            else
                prior_mtime="${PRIOR_MTIME[$bn]}"
                prior_inode="${PRIOR_INODE[$bn]}"
                # Inode change with a real prior inode = atomic rename
                # over the same name. Only treat as delete+create
                # when the consumer subscribed to delete events;
                # otherwise the swap is invisible (matches the
                # pre-events behaviour where state is keyed by name).
                if [[ "$cur_inode" != "$prior_inode" ]] && [[ "$prior_inode" != "0" ]] && (( WANT_DELETED )); then
                    handle_deleted "$WATCH_DIR/$bn"
                    handle_created "$WATCH_DIR/$bn"
                elif [[ "$cur_mtime" != "$prior_mtime" ]]; then
                    handle_modified "$WATCH_DIR/$bn"
                fi
            fi
        done

        # Diff: deleted. Only trip the handler if the consumer asked
        # for delete events — otherwise leave state in place so a
        # later same-name re-create stays deduped (matches the
        # pre-events "name-keyed state" contract).
        if (( WANT_DELETED )); then
            for bn in "${!PRIOR_MTIME[@]}"; do
                if [[ -z "${CURRENT_MTIME[$bn]+x}" ]]; then
                    handle_deleted "$WATCH_DIR/$bn"
                fi
            done
        fi

        rm -f "$STATE_FILE.snap.$$" 2>/dev/null || true
        unset CURRENT_MTIME CURRENT_INODE PRIOR_MTIME PRIOR_INODE
    done
fi
