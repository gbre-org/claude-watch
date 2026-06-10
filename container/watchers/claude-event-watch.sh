#!/bin/bash
# claude-event-watch.sh — thin launcher for the baked claude-event-watch
# binary at /usr/local/bin/claude-event-watch. This wrapper exists so
# the /opt/claude-container/watchers/ convention (*.sh + *.toml pair) is
# satisfied, and so the /start-watchers skill has a stable launcher path.
#
# The real implementation lives at tools/watchers/claude-event-watch and
# is COPY'd into /usr/local/bin/ by the Dockerfile.
exec /usr/local/bin/claude-event-watch "$@"
