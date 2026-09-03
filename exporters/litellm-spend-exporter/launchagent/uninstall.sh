#!/bin/bash
# Dismantle the LiteLLM spend exporter LaunchAgent.
set -euo pipefail
LABEL="com.claude-watch.litellm-spend-exporter"
PLIST="$HOME/Library/LaunchAgents/$LABEL.plist"
launchctl bootout "gui/$(id -u)/$LABEL" 2>/dev/null || true
rm -f "$PLIST"
echo "removed $PLIST and unloaded $LABEL"
echo "(venv at ~/.local/share/claude-watch/litellm-spend-venv left in place;"
echo " rm -rf it to fully clean up)"
