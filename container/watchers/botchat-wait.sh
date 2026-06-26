#!/bin/bash
# botchat-wait.sh — thin launcher for the botchat repo's shipped inbound
# watcher (bin/botchat-wait), bind-mounted into the container via ~/repos.
#
# The LOGIC lives in the botchat repo (block on the write-sentinel, surface NEW
# unread sender=andrew messages once via a surfaced-cursor, print the
# `=== N new botchat message(s) ===` block + the RESTART banner, and EXIT —
# no-consume: it never marks read/acked, the main loop does that via
# `botchat-send --mark-read <ids> --ack <ids>`). This wrapper only points the
# script at the running botchat container's HTTP API + the data dir and execs
# it. See botchat DEPLOY.md §8.
#
# HTTP MODE (no psycopg in this container). The botchat store migrated
# SQLite->Postgres, so bin/botchat-wait's DB path imports `psycopg`, which is
# NOT installed in claude-container (and the host venv is the wrong arch —
# macOS-aarch64 can't run in this Linux container). Rather than install a DB
# driver into the container, we point the watcher at the COMPOSE botchat
# container's HTTP API via BOTCHAT_API_BASE: bin/botchat-wait then fetches the
# unread andrew messages over `GET /api/messages?sender=andrew&unread=1`
# (urllib, Python stdlib only — psycopg / botchat.store are never imported in
# HTTP mode) and keeps its writable state (the `botchat.surfaced` cursor +
# blocks on `botchat.sentinel`) on the data dir, which rides the SEPARATE
# read-write ~/repos/botchat-data bind-mount — the same dir the botchat
# container's BOTCHAT_DATA_DIR=/data writes its sentinel/attachments to, so
# inotify-on-sentinel still wakes the watcher the instant a message lands.
#
# The botchat app is published on the host at :8111 (compose `botchat` service,
# container :8000); from this container it is reachable at
# host.docker.internal:8111. Both BOTCHAT_API_BASE and BOTCHAT_DATA_DIR stay
# overridable (${VAR:-...}) so tests/smoke runs can repoint them.
#
# NOTE: reactions (Andrew's emoji on workbot messages) are NOT surfaced in HTTP
# mode — the botchat HTTP API has no list-andrew-reactions endpoint (that is a
# watcher-internal store query). Inbound MESSAGES — the critical human->bot
# path — are surfaced fully. If reaction surfacing becomes load-bearing, add an
# API endpoint in botchat + extend bin/botchat-wait's HTTP path.
export BOTCHAT_API_BASE="${BOTCHAT_API_BASE:-http://host.docker.internal:8111}"
export BOTCHAT_DATA_DIR="${BOTCHAT_DATA_DIR:-/home/hndrewaall/repos/botchat-data}"
exec /home/hndrewaall/repos/botchat/bin/botchat-wait "$@"
