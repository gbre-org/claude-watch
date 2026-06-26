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
# The durable store is PostgreSQL now (botchat SQLite->Postgres migration). The
# watcher connects via BOTCHAT_DSN (provided by the container environment — it
# carries the DB password, so it is NEVER hardcoded here) and keeps its writable
# state (the `botchat.surfaced` cursor + `botchat.sentinel`) on the data dir,
# which rides the SEPARATE read-write ~/repos/botchat-data bind-mount.
#
# BOTCHAT_DATA_DIR stays overridable (${BOTCHAT_DATA_DIR:-...}) so tests/smoke
# runs can repoint it; it must match the dir the writers (the web container +
# botchat-send) touch. The script's own default (/data) may not be mounted in
# this container, which is why this launcher exports the real path.
export BOTCHAT_DATA_DIR="${BOTCHAT_DATA_DIR:-/home/hndrewaall/repos/botchat-data}"
exec /home/hndrewaall/repos/botchat/bin/botchat-wait "$@"
