#!/usr/bin/env bash
# Fresh-laptop bootstrap helper for examples/compose/.
#
# Idempotent. Safe to re-run. Side effects:
#   1. Checks for docker, git, gh; warns (does not fail) if any missing.
#   2. Clones https://github.com/gbre-org/eichi as a sibling of this
#      repo if no sibling clone exists.
#   3. Seeds examples/compose/.env from .env.example if .env doesn't
#      exist yet — left for the operator to fill in ANTHROPIC_API_KEY.
#   4. Prints next-step guidance.
#
# Usage:
#   bash examples/compose/bootstrap.sh
# Or via the top-level Makefile:
#   make bootstrap

set -u

HERE="$(cd "$(dirname "$0")" && pwd)"
COMPOSE_DIR="$HERE"
CW_ROOT="$(cd "$HERE/../.." && pwd)"
SIBLING_ROOT="$(dirname "$CW_ROOT")"
EICHI_DIR="$SIBLING_ROOT/eichi"

ok()    { printf '  [ok]   %s\n' "$*"; }
warn()  { printf '  [warn] %s\n' "$*"; }
info()  { printf '  [info] %s\n' "$*"; }

echo "claude-watch examples/compose bootstrap"
echo

echo "Prerequisites"
for tool in docker git; do
    if command -v "$tool" >/dev/null 2>&1; then
        ok "$tool found ($(command -v "$tool"))"
    else
        warn "$tool not found on PATH — install before \`docker compose up\`"
    fi
done

# Compose v2 lives under `docker compose` (subcommand), not the legacy
# `docker-compose` binary.
if docker compose version >/dev/null 2>&1; then
    ok "docker compose v2 plugin available"
else
    warn "docker compose v2 plugin missing — install docker-compose-plugin"
fi

if command -v gh >/dev/null 2>&1; then
    ok "gh CLI found (optional, only needed for PR workflows)"
else
    info "gh CLI not installed (optional)"
fi

echo
echo "Sibling eichi clone"
if [ -d "$EICHI_DIR/.git" ]; then
    ok "$EICHI_DIR exists"
else
    info "Cloning https://github.com/gbre-org/eichi into $EICHI_DIR"
    if git clone https://github.com/gbre-org/eichi.git "$EICHI_DIR"; then
        ok "Clone complete"
    else
        warn "git clone failed — clone $EICHI_DIR manually before docker compose up"
    fi
fi

echo
echo "Environment file"
ENV_FILE="$COMPOSE_DIR/.env"
ENV_EXAMPLE="$COMPOSE_DIR/.env.example"
if [ -f "$ENV_FILE" ]; then
    ok ".env already exists at $ENV_FILE (left untouched)"
elif [ -f "$ENV_EXAMPLE" ]; then
    cp "$ENV_EXAMPLE" "$ENV_FILE"
    ok "Created $ENV_FILE from .env.example"
    info "Fill in ANTHROPIC_API_KEY before docker compose up"
else
    warn ".env.example missing — expected at $ENV_EXAMPLE"
fi

echo
echo "session-task queue state"
# Belt-and-suspenders with queue-minisite's ENOENT graceful-empty-state
# (the UI renders an empty queue when this file is missing). Seeding it
# explicitly here ALSO makes the host-side `session-task` CLI happy on a
# fresh laptop where neither `claude-watch` nor `session-task` has been
# run yet, since the read-time lock + parent-mkdir path only runs when
# someone first invokes a queue subcommand.
SESSION_DIR="$HOME/.config/session"
QUEUE_JSON="$SESSION_DIR/queue.json"
mkdir -p "$SESSION_DIR"
if [ -f "$QUEUE_JSON" ]; then
    ok "existing queue.json at $QUEUE_JSON (not modified)"
else
    # Canonical empty shape -- mirrors `_queue_empty()` in
    # tools/session-task/session-task. schema_version is the on-disk
    # marker (currently 2); items + locked_scopes are the two top-level
    # collections every queue operation indexes into.
    cat > "$QUEUE_JSON" <<'JSON'
{
  "schema_version": 2,
  "items": [],
  "locked_scopes": {}
}
JSON
    ok "seeded empty queue.json at $QUEUE_JSON"
fi

echo
echo "Next steps"
echo "  1. Edit $ENV_FILE (set ANTHROPIC_API_KEY if you want the claude-container service to talk to the API)."
echo "  2. (Optional) bootstrap an eichi index on the host:"
echo "       cd $EICHI_DIR && uv venv --python 3.11 && uv pip install -e ."
echo "       eichi index ~/Documents/notes   # or any corpus you want searchable"
echo "  3. From $COMPOSE_DIR run:"
echo "       docker compose up"
echo "  4. Open http://localhost:8000/ (queue) and http://localhost:8001/ (search)."
echo
