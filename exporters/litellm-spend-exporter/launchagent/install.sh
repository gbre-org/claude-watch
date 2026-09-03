#!/bin/bash
# Install the LiteLLM spend exporter as a macOS LaunchAgent.
#
#   ./install.sh
#
# What it does:
#   1. Creates a dedicated venv with prometheus_client (uv, corp-CA-aware).
#   2. Renders the plist template to ~/Library/LaunchAgents/ with absolute
#      paths for THIS checkout.
#   3. bootstraps + kickstarts the agent (KeepAlive -> serves :9104/metrics).
#
# Prereqs: `uv` on PATH; a gateway key at ~/.config/claude-watch/litellm-spend.key
# (a key that can read /user/info — the team key) OR `devbar auth claude` as a
# key_spend-only fallback. See README.md.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WRAPPER="$SCRIPT_DIR/litellm-spend-exporter-agent"
LABEL="com.claude-watch.litellm-spend-exporter"
PLIST_DST="$HOME/Library/LaunchAgents/$LABEL.plist"
VENV_DIR="$HOME/.local/share/claude-watch/litellm-spend-venv"
BASE_URL="${LITELLM_BASE_URL:-https://eng-ai-model-gateway.sfproxy.devx-preprod.aws-esvc1-useast2.aws.sfdc.cl}"

# Corp CA bundle so both uv (pypi) and the exporter (SF gateway) trust TLS.
CA_BUNDLE="${SSL_CERT_FILE:-/opt/homebrew/etc/ca-certificates/cert.pem}"
[ -f "$CA_BUNDLE" ] || CA_BUNDLE="/etc/ssl/cert.pem"

echo "==> creating venv at $VENV_DIR"
SSL_CERT_FILE="$CA_BUNDLE" uv venv --quiet "$VENV_DIR"
SSL_CERT_FILE="$CA_BUNDLE" uv pip install --native-tls --quiet \
    --python "$VENV_DIR/bin/python" prometheus_client

echo "==> rendering plist to $PLIST_DST"
mkdir -p "$HOME/Library/LaunchAgents" "$HOME/.config/claude-watch"
sed -e "s|@WRAPPER@|$WRAPPER|g" \
    -e "s|@HOME@|$HOME|g" \
    -e "s|@BASE_URL@|$BASE_URL|g" \
    -e "s|@CA_BUNDLE@|$CA_BUNDLE|g" \
    "$SCRIPT_DIR/$LABEL.plist.template" > "$PLIST_DST"

echo "==> (re)loading LaunchAgent"
launchctl bootout "gui/$(id -u)/$LABEL" 2>/dev/null || true
launchctl bootstrap "gui/$(id -u)" "$PLIST_DST"
launchctl kickstart -k "gui/$(id -u)/$LABEL"

echo "==> done. Verify:"
echo "    curl -s http://127.0.0.1:9104/metrics | grep '^litellm_'"
echo "    tail -f ~/Library/Logs/litellm-spend-exporter.log"
if [ ! -s "$HOME/.config/claude-watch/litellm-spend.key" ]; then
  echo "NOTE: ~/.config/claude-watch/litellm-spend.key is empty."
  echo "      Drop a gateway key that can read /user/info there for user+team"
  echo "      metrics; otherwise only litellm_key_spend_dollars populates."
fi
