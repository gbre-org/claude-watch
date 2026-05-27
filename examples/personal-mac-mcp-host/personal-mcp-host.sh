#!/usr/bin/env bash
# personal-mcp-host.sh — boot the MCP server AND/OR the reverse SSH tunnel.
#
# What this does
#
# Brings up the operator's on-demand "remote-access" MCP server in two
# pieces:
#
#   1. mcp-host-bash --port $MCP_LOCAL_PORT
#      Reuses the existing launcher under
#      examples/compose/bin/mcp-host-bash, which spawns mcp-proxy +
#      cli-mcp-server with the cw-profile allow-list, optional
#      mcp-proxy-auth-shim bearer auth, and logs to the standard
#      ~/.local/state/claude-container/mcp-host-bash.log path. We do
#      not duplicate that surface — operators who've already set up
#      the compose stack are already configured for it.
#
#   2. ssh -N -R $REMOTE_PORT:127.0.0.1:$MCP_LOCAL_PORT ... $REMOTE_USER@$REMOTE_HOST
#      A reverse-forward SSH tunnel: the MacBook dials out to
#      $REMOTE_HOST and asks sshd to bind $REMOTE_PORT on the remote's
#      loopback, forwarding any connection back to the MacBook's
#      $MCP_LOCAL_PORT.
#
#      The remote-side Claude Code dials its own localhost:$REMOTE_PORT
#      and reaches the MacBook's MCP server through the SSH-encrypted
#      pipe. No inbound TCP port on the MacBook, no relay server, no
#      NAT punch-through.
#
# Two operating modes
#
#   Bundled (default): start BOTH pieces. The wrapper owns the MCP
#   server's lifecycle and the tunnel's. Simplest shape for operators
#   who only want one unit to manage.
#
#   Tunnel-only (--tunnel-only / PERSONAL_MCP_TUNNEL_ONLY=1): start ONLY
#   the reverse SSH tunnel. Assumes mcp-host-bash is ALREADY listening
#   on 127.0.0.1:$MCP_LOCAL_PORT — typically because it runs always-on
#   under the compose-stack LaunchAgent
#   (examples/compose/launchd/org.gbre.claude-watch.mcp-host-bash.plist,
#   RunAtLoad=true). The recommended split: MCP host always up locally
#   at boot, remote access granted on-demand by starting the tunnel
#   only. In this mode the wrapper does NOT launch mcp-host-bash and
#   does NOT run the listener probe (the MCP server's lifecycle is not
#   ours to manage).
#
# Lifecycle (bundled, default)
#
#   1. Source sibling .env file. Refuse to start if missing.
#   2. Resolve mcp-host-bash binary path. Refuse if not executable.
#   3. Start mcp-host-bash --port $MCP_LOCAL_PORT in the background;
#      capture pid.
#   4. Poll-wait for 127.0.0.1:$MCP_LOCAL_PORT to enter LISTEN (same
#      probe pattern as mcp-host-bash's wait_for_listener). Fail-fast
#      if the launcher exits before binding.
#   5. Start ssh -N -R ... in the background; capture pid.
#   6. Poll-wait on both children. If either dies, take the other
#      down and exit non-zero so launchd's KeepAlive can respawn the
#      whole thing.
#   7. SIGTERM / SIGINT trap: forward to both children, then exit.
#
# Lifecycle (tunnel-only)
#
#   1. Source sibling .env file. Refuse to start if missing.
#   2. Skip the mcp-host-bash resolve + launch + listener probe
#      entirely.
#   3. Start ssh -N -R ... in the foreground (exec); when it dies,
#      launchd's KeepAlive can respawn the tunnel.
#   4. SIGTERM / SIGINT trap: forward to the ssh child, then exit.
#
# Usage
#
#   personal-mcp-host.sh                  # bundled: foreground (Ctrl-C to stop)
#   personal-mcp-host.sh --tunnel-only    # tunnel only (MCP already up locally)
#   personal-mcp-host.sh --print-cmd      # print planned argv + exit 0
#   personal-mcp-host.sh --help           # this help
#
# Env vars consumed from sibling .env (required)
#
#   REMOTE_HOST       remote host the MacBook dials out to. DNS name or IP.
#   REMOTE_USER       remote SSH user.
#   REMOTE_PORT       port the tunnel binds on $REMOTE_HOST's loopback.
#   MCP_LOCAL_PORT    port mcp-host-bash binds on the MacBook's loopback.
#   SSH_KEY_PATH      private SSH key the tunnel uses (recommend a dedicated key).
#
# Optional env vars
#
#   MCP_HOST_BASH_BIN          override path to the mcp-host-bash launcher.
#                              Default: ../compose/bin/mcp-host-bash relative
#                              to this script.
#   MCP_HOST_BASH_BEARER       shared-secret bearer token (recommended). Forwarded
#                              to mcp-host-bash, which fronts mcp-proxy with the
#                              auth shim. Generate once:
#                                head -c 32 /dev/urandom | base64
#   CW_PROFILE                 trust profile for mcp-host-bash. Default `corp-dev`
#                              (read-y floor). Set `corp-dev-trusted` to widen.
#   ALLOWED_DIR                fence run_command to this dir. Default: $HOME.
#   ALLOW_SHELL_OPERATORS      let run_command chain pipes / &&. Default false.
#   PERSONAL_MCP_TUNNEL_ONLY   set to 1 (or pass --tunnel-only) to start ONLY
#                              the reverse SSH tunnel, skipping the
#                              mcp-host-bash launch + listener probe. Use when
#                              mcp-host-bash is already running locally (e.g.
#                              the always-on compose-stack LaunchAgent). The
#                              --tunnel-only flag and this env var are
#                              equivalent; either enables the mode.
#   PERSONAL_MCP_DISABLED      soft kill switch — script exits 0 immediately.
#                              Pair with launchd's KeepAlive to leave the unit
#                              registered without actually running mcp-host-bash
#                              and the tunnel.
#   PERSONAL_MCP_SSH_EXTRA     extra space-separated `ssh -o KEY=VALUE` opts
#                              appended to the tunnel's argv. For one-off
#                              tuning (proxy jump, lower keep-alive cadence)
#                              without editing this script.
#
# Exit codes
#   0   normal shutdown (or --help / --print-cmd / PERSONAL_MCP_DISABLED)
#   1   missing mcp-host-bash binary, or child died before binding, or
#       child died during steady-state and we tore the other one down.
#   2   bad flag / missing .env / missing required key in .env

set -euo pipefail

# -----------------------------------------------------------------------------
# Argv parsing
# -----------------------------------------------------------------------------

usage() {
    sed -n '2,/^set -euo/p' "$0" | sed -e 's/^# \{0,1\}//' -e '/^set -euo/d'
}

PRINT_CMD=0
# Tunnel-only mode: seed from the env var so PERSONAL_MCP_TUNNEL_ONLY=1
# and --tunnel-only are equivalent. The flag (if passed) wins.
TUNNEL_ONLY=0
if [ "${PERSONAL_MCP_TUNNEL_ONLY:-0}" = "1" ]; then
    TUNNEL_ONLY=1
fi
while [ "$#" -gt 0 ]; do
    case "$1" in
        --help|-h)
            usage
            exit 0
            ;;
        --tunnel-only)
            # Start ONLY the reverse SSH tunnel; skip the mcp-host-bash
            # launch + listener probe. For when mcp-host-bash is already
            # running locally (e.g. the always-on compose-stack
            # LaunchAgent). Equivalent to PERSONAL_MCP_TUNNEL_ONLY=1.
            TUNNEL_ONLY=1
            shift
            ;;
        --print-cmd)
            # Test-only: build the planned ssh argv but print it
            # (one-per-line) instead of executing. Also skips the
            # mcp-host-bash launch + listener probe so the test runs
            # on hosts that don't have mcp-proxy / cli-mcp-server
            # installed. In tunnel-only mode the MCP_HOST_BASH_BIN:
            # block is omitted (the wrapper does not manage the MCP
            # server's lifecycle).
            PRINT_CMD=1
            shift
            ;;
        *)
            printf 'personal-mcp-host: unknown argument %q\n' "$1" >&2
            echo 'See --help for usage.' >&2
            exit 2
            ;;
    esac
done

# -----------------------------------------------------------------------------
# Soft kill switch
# -----------------------------------------------------------------------------

if [ "${PERSONAL_MCP_DISABLED:-0}" = "1" ] && [ "$PRINT_CMD" = "0" ]; then
    echo "personal-mcp-host: PERSONAL_MCP_DISABLED=1 — refusing to start. Unset to enable." >&2
    exit 0
fi

# -----------------------------------------------------------------------------
# Load .env (sibling file)
# -----------------------------------------------------------------------------

script_dir="$(cd "$(dirname "$0")" && pwd)"
env_file="${PERSONAL_MCP_ENV_FILE:-${script_dir}/.env}"

if [ ! -f "$env_file" ]; then
    cat >&2 <<EOF
personal-mcp-host: missing .env at $env_file

Copy the template and fill in your own values:

    cp ${script_dir}/.env.example ${script_dir}/.env
    \$EDITOR ${script_dir}/.env

See README.md for the full operator walkthrough.
EOF
    exit 2
fi

# shellcheck disable=SC1090
. "$env_file"

# Validate required keys.
: "${REMOTE_HOST:?REMOTE_HOST not set in $env_file}"
: "${REMOTE_USER:?REMOTE_USER not set in $env_file}"
: "${REMOTE_PORT:?REMOTE_PORT not set in $env_file}"
: "${MCP_LOCAL_PORT:?MCP_LOCAL_PORT not set in $env_file}"
: "${SSH_KEY_PATH:?SSH_KEY_PATH not set in $env_file}"

# Resolve mcp-host-bash. Default: sibling repo path relative to this script.
MCP_HOST_BASH_BIN="${MCP_HOST_BASH_BIN:-${script_dir}/../compose/bin/mcp-host-bash}"

# Export config the mcp-host-bash child reads from its env. The launcher
# itself sources ~/.config/claude-container/mcp-host-bash.env too —
# operators who already have their cw-profile + allow-list dialed in
# there can leave these unset in the sibling .env.
export MCP_HOST_BASH_BIND="127.0.0.1"
if [ -n "${MCP_HOST_BASH_BEARER:-}" ]; then
    export MCP_HOST_BASH_BEARER
fi
if [ -n "${CW_PROFILE:-}" ]; then
    export CW_PROFILE
fi
if [ -n "${ALLOWED_DIR:-}" ]; then
    export ALLOWED_DIR
fi
if [ -n "${ALLOW_SHELL_OPERATORS:-}" ]; then
    export ALLOW_SHELL_OPERATORS
fi

# -----------------------------------------------------------------------------
# Build the ssh argv
#
# Notable opts:
#   -N                          no remote command — just hold the tunnel.
#   -R REMOTE_PORT:127.0.0.1:LOCAL
#                               bind REMOTE_PORT on remote's loopback,
#                               forward to LOCAL on this side.
#   ExitOnForwardFailure=yes    fail loud rather than silently sit
#                               connected if the remote bind fails (port
#                               in use, key revoked, sshd policy reject).
#   ServerAliveInterval=30
#   ServerAliveCountMax=3       detect a dead remote / dead network within
#                               ~90s and exit so launchd respawns.
#   BatchMode=yes               refuse to prompt for a password — the
#                               dedicated key MUST work non-interactively.
#   StrictHostKeyChecking=accept-new
#                               pin the remote's host key on first
#                               connect; refuse if it later changes.
#                               (The README walks through pre-populating
#                               known_hosts via ssh-keyscan for operators
#                               who want to defeat first-connect MITM too.)
# -----------------------------------------------------------------------------

ssh_argv=(
    ssh
    -N
    -R "${REMOTE_PORT}:127.0.0.1:${MCP_LOCAL_PORT}"
    -o ExitOnForwardFailure=yes
    -o ServerAliveInterval=30
    -o ServerAliveCountMax=3
    -o BatchMode=yes
    -o StrictHostKeyChecking=accept-new
    -i "$SSH_KEY_PATH"
)

# Optional operator-supplied extras. Split on whitespace — operators
# pass these as `PERSONAL_MCP_SSH_EXTRA="-o ProxyJump=bastion -o
# ServerAliveInterval=15"` in their .env. We don't quote each token
# because the operator can't pass values containing spaces this way
# anyway (ssh's -o syntax is KEY=VALUE without whitespace).
if [ -n "${PERSONAL_MCP_SSH_EXTRA:-}" ]; then
    # shellcheck disable=SC2206
    extra_opts=( ${PERSONAL_MCP_SSH_EXTRA} )
    ssh_argv+=( "${extra_opts[@]}" )
fi

ssh_argv+=( "${REMOTE_USER}@${REMOTE_HOST}" )

if [ "$PRINT_CMD" = "1" ]; then
    # Print mode: argv one-per-line for the test suite.
    #
    # Bundled (default): two blocks —
    #   MCP_HOST_BASH_BIN:  the resolved launcher path + --port arg
    #   SSH:                the ssh tunnel argv
    #
    # Tunnel-only: ONLY the SSH: block. The wrapper does not launch
    # mcp-host-bash in this mode, so emitting an MCP_HOST_BASH_BIN:
    # block would misrepresent what runs.
    if [ "$TUNNEL_ONLY" = "0" ]; then
        echo "MCP_HOST_BASH_BIN:"
        echo "$MCP_HOST_BASH_BIN"
        echo "--port"
        echo "$MCP_LOCAL_PORT"
        echo
    fi
    echo "SSH:"
    printf '%s\n' "${ssh_argv[@]}"
    exit 0
fi

# -----------------------------------------------------------------------------
# Pre-flight: the mcp-host-bash launcher must be executable.
#
# Skipped in tunnel-only mode — the wrapper does not launch the MCP
# server there (it is assumed already up locally), so the launcher
# binary need not even be present.
# -----------------------------------------------------------------------------

if [ "$TUNNEL_ONLY" = "0" ] && [ ! -x "$MCP_HOST_BASH_BIN" ]; then
    cat >&2 <<EOF
personal-mcp-host: mcp-host-bash not found / not executable: $MCP_HOST_BASH_BIN

Set MCP_HOST_BASH_BIN in $env_file to the absolute path of your
checkout's examples/compose/bin/mcp-host-bash, or run the bundled
installer once to populate the static binaries it depends on:

    ../compose/bin/install-host-deps

If your mcp-host-bash launcher lives outside this checkout, point
MCP_HOST_BASH_BIN at it directly.
EOF
    exit 1
fi

# Pre-flight: ssh on PATH.
if ! command -v ssh >/dev/null 2>&1; then
    echo "personal-mcp-host: ssh not found on PATH" >&2
    exit 1
fi

# Pre-flight: SSH key readable.
if [ ! -r "$SSH_KEY_PATH" ]; then
    echo "personal-mcp-host: SSH key not readable: $SSH_KEY_PATH" >&2
    echo "personal-mcp-host: check SSH_KEY_PATH in $env_file" >&2
    exit 1
fi

# -----------------------------------------------------------------------------
# Trap + cleanup
# -----------------------------------------------------------------------------

mcp_pid=""
ssh_pid=""
cleanup_exit_code=0
shutting_down=0

cleanup() {
    # Take down ssh first — that's the network-facing piece. If the
    # remote is mid-handshake we don't want to leave a half-open
    # tunnel while mcp-host-bash flushes its log.
    for pid in "$ssh_pid" "$mcp_pid"; do
        if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
            kill -TERM "$pid" 2>/dev/null || true
        fi
    done
    sleep 0.5
    for pid in "$ssh_pid" "$mcp_pid"; do
        if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
            kill -KILL "$pid" 2>/dev/null || true
        fi
    done
    exit "$cleanup_exit_code"
}
trap 'shutting_down=1; cleanup' TERM INT

# -----------------------------------------------------------------------------
# Listener probe — same shape as examples/compose/bin/mcp-host-bash's
# wait_for_listener.
#
# Returns:
#   0   port is in LISTEN, TCP connect succeeded
#   1   timed out without a successful connect
#   2   child mcp-host-bash exited before binding
#   3   shutdown trap fired mid-poll (TERM/INT)
# -----------------------------------------------------------------------------

wait_for_listener() {
    local host=$1 port=$2 timeout=$3
    local deadline
    deadline=$(( $(date +%s) + timeout ))
    while [ "$(date +%s)" -lt "$deadline" ]; do
        if [ "$shutting_down" = "1" ]; then
            return 3
        fi
        if ! kill -0 "$mcp_pid" 2>/dev/null; then
            return 2
        fi
        if python3 -c "
import socket, sys
s = socket.socket()
s.settimeout(0.3)
try:
    s.connect(('$host', $port))
    s.close()
except OSError:
    sys.exit(1)
" 2>/dev/null; then
            sleep 0.2
            if [ "$shutting_down" = "1" ]; then
                return 3
            fi
            if ! kill -0 "$mcp_pid" 2>/dev/null; then
                return 2
            fi
            return 0
        fi
        sleep 0.2
    done
    return 1
}

# -----------------------------------------------------------------------------
# Banner
# -----------------------------------------------------------------------------

{
    if [ "$TUNNEL_ONLY" = "1" ]; then
        echo "personal-mcp-host: starting (tunnel-only)"
    else
        echo "personal-mcp-host: starting"
    fi
    echo "  MCP_LOCAL_PORT:        $MCP_LOCAL_PORT"
    echo "  REMOTE:                ${REMOTE_USER}@${REMOTE_HOST}:${REMOTE_PORT}"
    echo "  SSH_KEY_PATH:          $SSH_KEY_PATH"
    if [ -n "${MCP_HOST_BASH_BEARER:-}" ]; then
        echo "  bearer auth:           ENABLED"
    else
        echo "  bearer auth:           DISABLED (MCP_HOST_BASH_BEARER unset)"
        echo "  NOTE:                  the SSH tunnel encrypts the wire, but anyone"
        echo "                         else on the remote's loopback can dial the MCP"
        echo "                         server. Set MCP_HOST_BASH_BEARER for"
        echo "                         defense-in-depth."
    fi
    echo "  CW_PROFILE:            ${CW_PROFILE:-<unset; mcp-host-bash default applies>}"
    if [ "$TUNNEL_ONLY" = "1" ]; then
        echo "  launcher:              <tunnel-only; mcp-host-bash assumed already running>"
    else
        echo "  launcher:              $MCP_HOST_BASH_BIN"
    fi
    if [ -n "${PERSONAL_MCP_SSH_EXTRA:-}" ]; then
        echo "  SSH extras:            $PERSONAL_MCP_SSH_EXTRA"
    fi
    echo
    echo "Ctrl-C to stop."
    echo
} >&2

# -----------------------------------------------------------------------------
# Tunnel-only: skip the mcp-host-bash launch + listener probe entirely.
# Just open the reverse SSH tunnel and hold it. mcp-host-bash is assumed
# already listening on 127.0.0.1:$MCP_LOCAL_PORT (e.g. the always-on
# compose-stack LaunchAgent). If the tunnel dies, exit non-zero so
# launchd's KeepAlive respawns it.
# -----------------------------------------------------------------------------

if [ "$TUNNEL_ONLY" = "1" ]; then
    "${ssh_argv[@]}" &
    ssh_pid=$!
    while kill -0 "$ssh_pid" 2>/dev/null; do
        sleep 1
    done
    cleanup_exit_code=1
    cleanup
fi

# -----------------------------------------------------------------------------
# Start mcp-host-bash
# -----------------------------------------------------------------------------

"$MCP_HOST_BASH_BIN" --port "$MCP_LOCAL_PORT" &
mcp_pid=$!

# Wait for the loopback port to bind before opening the tunnel.
probe_rc=0
wait_for_listener 127.0.0.1 "$MCP_LOCAL_PORT" 15 || probe_rc=$?
case "$probe_rc" in
    0)
        echo "personal-mcp-host: mcp-host-bash listening on 127.0.0.1:$MCP_LOCAL_PORT" >&2
        ;;
    2)
        cat >&2 <<EOF
personal-mcp-host: FATAL: mcp-host-bash exited before binding 127.0.0.1:$MCP_LOCAL_PORT.
       Common causes:
         - install-host-deps was never run (mcp-proxy / cli-mcp-server
           missing from PATH).
         - $MCP_LOCAL_PORT already owned by a stale prior instance —
           lsof -nP -iTCP:$MCP_LOCAL_PORT -sTCP:LISTEN
         - bad operator config under
           ~/.config/claude-container/mcp-host-bash.env
       Check the launcher's stderr above for the underlying error.
EOF
        cleanup_exit_code=1
        cleanup
        ;;
    3)
        exit 1
        ;;
    *)
        cat >&2 <<EOF
personal-mcp-host: FATAL: mcp-host-bash did not bind 127.0.0.1:$MCP_LOCAL_PORT
       within 15s. The process is still running but has not opened the
       listen socket. Check
       ~/.local/state/claude-container/mcp-host-bash.log for upstream
       stderr.
EOF
        cleanup_exit_code=1
        cleanup
        ;;
esac

# -----------------------------------------------------------------------------
# Start the reverse SSH tunnel
# -----------------------------------------------------------------------------

"${ssh_argv[@]}" &
ssh_pid=$!

# Steady-state: poll both children. If EITHER dies, tear the other
# down so we never leak half a bridge to launchd's KeepAlive respawn
# loop. (`wait -n` is bash 4.3+; macOS ships bash 3.2 by default. The
# poll keeps us portable.)
while kill -0 "$mcp_pid" 2>/dev/null && kill -0 "$ssh_pid" 2>/dev/null; do
    sleep 1
done

cleanup_exit_code=1
cleanup
