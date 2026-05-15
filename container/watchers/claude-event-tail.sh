#!/bin/bash
# claude-event-tail.sh — in-container watcher that surfaces claude-event
# JSON files dropped into ~/claude-events/ to the in-container session.
#
# Shape: thin wrapper around `lib/dir-watch.sh`. The primitive handles
# inotify / poll / re-arm / state. This script defines what an event
# "fire" looks like: read the JSON, print a one-liner, append the
# compact JSON to the consumed ring buffer, delete the source file.
#
# Default to stdout-as-event-channel (matches the host's
# claude-event-watch shape) — the supervising session reads it via the
# log path declared in claude-event-tail.toml.
#
# Env vars (forwarded to the callback via export, since `bash -c` runs
# a fresh shell):
#   CLAUDE_EVENT_QUEUE      default ~/claude-events
#   CLAUDE_EVENT_LOG_DIR    default ~/.config/claude-events
#
# Lifecycle:
#   - This script execs lib/dir-watch.sh, which runs foreground forever.
#   - On terminal failure (lib/dir-watch.sh exit) the supervisor honours
#     this watcher's restart_policy = "always" (see .toml).

set -uo pipefail

# Resolve baked location of the dir-watch primitive. Default to the
# baked container path; tests can override with DIR_WATCH_LIB.
DIR_WATCH_LIB="${DIR_WATCH_LIB:-/etc/claude-code/watchers/lib/dir-watch.sh}"

CLAUDE_EVENT_QUEUE="${CLAUDE_EVENT_QUEUE:-$HOME/claude-events}"
CLAUDE_EVENT_LOG_DIR="${CLAUDE_EVENT_LOG_DIR:-$HOME/.config/claude-events}"
CLAUDE_EVENT_LOG_FILE="$CLAUDE_EVENT_LOG_DIR/consumed.jsonl"

mkdir -p "$CLAUDE_EVENT_QUEUE" "$CLAUDE_EVENT_LOG_DIR"

export CLAUDE_EVENT_QUEUE CLAUDE_EVENT_LOG_DIR CLAUDE_EVENT_LOG_FILE

# The callback receives the matched file's full path as $1. It:
#   1. Parses the JSON (delegated to python3 — jq isn't guaranteed).
#   2. Prints one line to stdout in the
#        EVENT[<source>/<tag>] <message-first-60-chars…>
#      shape (matches host's claude-event-watch output).
#   3. Appends the compact JSON form to consumed.jsonl.
#   4. Deletes the source file so the watcher doesn't refire on a
#      subsequent inotify glitch / supervisor restart.
#
# Wrapped in single-quoted heredoc so the body is a literal string
# stored in $CALLBACK — `bash -c "$WATCH_CALLBACK" -- <path>` will run
# this with $1 = the new file's path.
read -r -d '' CALLBACK <<'CALLBACK_EOF' || true
src="$1"
log="${CLAUDE_EVENT_LOG_FILE:-$HOME/.config/claude-events/consumed.jsonl}"
mkdir -p "$(dirname "$log")"

python3 - "$src" "$log" <<'PYEOF'
import json, os, sys
src, log = sys.argv[1], sys.argv[2]
try:
    with open(src, "r", encoding="utf-8", errors="replace") as fp:
        ev = json.load(fp)
except Exception as e:
    print(f"EVENT[malformed/unknown] {os.path.basename(src)} ({e})")
    sys.exit(0)

source = str(ev.get("source", "unknown"))
tag = str(ev.get("tag", "untagged"))
message = str(ev.get("message", ""))
message = " ".join(message.split())
if len(message) > 60:
    message = message[:60] + "…"
print(f"EVENT[{source}/{tag}] {message}")

line = json.dumps(ev, separators=(",", ":"), ensure_ascii=False)
data = (line + "\n").encode("utf-8")
fd = os.open(log, os.O_WRONLY | os.O_APPEND | os.O_CREAT, 0o644)
try:
    os.write(fd, data)
finally:
    os.close(fd)
PYEOF

# Best-effort delete; if it fails the state file prevents refire.
rm -f "$src" 2>/dev/null || true
CALLBACK_EOF

export WATCH_DIR="$CLAUDE_EVENT_QUEUE"
export WATCH_PATTERN='*.json'
export WATCH_CALLBACK="$CALLBACK"

exec "$DIR_WATCH_LIB"
