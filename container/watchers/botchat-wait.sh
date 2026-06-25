#!/bin/bash
# botchat-wait.sh — thin launcher for the botchat repo's shipped inbound
# watcher (bin/botchat-wait), bind-mounted into the container via ~/repos.
#
# The LOGIC lives in the botchat repo (block on the DB write-sentinel,
# surface NEW unread sender=andrew messages once via a surfaced-cursor,
# print the `=== N new botchat message(s) ===` block + the RESTART banner,
# and EXIT — no-consume: it never marks read/acked, the main loop does that
# via `botchat-send --mark-read <ids> --ack <ids>`). This wrapper only points
# the script at the in-container DB path and execs it. See botchat DEPLOY.md §8.
#
# No new bind-mount is required: the script (~/repos/botchat/bin/botchat-wait)
# rides the existing read-only ~/repos mount, and its writable state (the
# `<db>.surfaced` cursor + `<db>.sentinel`) lives under ~/repos/botchat-data,
# which is a SEPARATE read-write bind-mount.
#
# BOTCHAT_DB stays overridable (${BOTCHAT_DB:-...}) so tests/smoke runs can
# repoint it at a tmp DB. The script's own default (/data/botchat.db) does NOT
# exist in this container, which is why this launcher exports the real path.
export BOTCHAT_DB="${BOTCHAT_DB:-/home/hndrewaall/repos/botchat-data/botchat.db}"
exec /home/hndrewaall/repos/botchat/bin/botchat-wait "$@"
