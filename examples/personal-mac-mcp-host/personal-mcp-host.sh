#!/usr/bin/env bash
# personal-mcp-host.sh — gate on the local MCP service, open the reverse
# SSH tunnel, and tail the live logs.
#
# What this does
#
# Brings up the operator's on-demand "remote-access" MCP server in two
# pieces:
#
#   1. mcp-host-bash-server --port $MCP_LOCAL_PORT
#      Reuses the single self-contained Rust binary from
#      crates/mcp-host-bash-server (installed to ~/bin/mcp-host-bash-server
#      via `make install-mcp-host-bash-server`). It serves the host-bash
#      MCP surface over streamable-HTTP with in-process bearer auth and
#      the cw-profile allow-list, and reads operator config from
#      ~/.config/claude-container/mcp-host-bash.env. We do not duplicate
#      that surface — operators who've already set up the compose stack
#      are already configured for it.
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
# Operating modes
#
#   Default (no flags) — STATUS-GATED tunnel + log tail. The wrapper
#   first checks whether the host MCP service (mcp-host-bash-server,
#   the thing listening on 127.0.0.1:$MCP_LOCAL_PORT) is
#   actually up by attempting a TCP connect to the port:
#
#     - RED (service NOT up): print a clear error explaining the host
#       service isn't running, print a ready-to-copy command that
#       re-runs THIS script with --enable (which brings the service up
#       for you), and exit non-zero. The wrapper does NOT start the MCP
#       server in this mode — the default path assumes you keep the
#       server always-on (e.g. the compose-stack LaunchAgent) and only
#       want the tunnel on-demand.
#
#     - GREEN (service up): open the reverse SSH tunnel and then tail
#       the live MCP host log (default
#       ~/.local/state/claude-container/mcp-host-bash.log) so the
#       operator sees JSON-RPC + run_command traffic as it happens.
#       Ctrl-C tears the tunnel down.
#
#   --enable — bring the host service up, THEN take the green path.
#   Performs the bundled-style mcp-host-bash launch + listener probe
#   (so the port is guaranteed LISTEN), then opens the tunnel and tails
#   the log. This is the "I haven't got the always-on LaunchAgent; start
#   everything from this one invocation" path. The printed RED-path
#   rerun command points here. The host service is started DETACHED in
#   its own session so it OUTLIVES this wrapper: when you Ctrl-C (or
#   launchd SIGTERMs) the wrapper, only the tunnel + log tail are torn
#   down — the MCP service keeps running. Re-running the default (GREEN)
#   path then finds it still listening and just reconnects the tunnel.
#
#   restart (/ --restart) — ONE-SHOT FULL-STACK RESTART. Not a
#   supervisor: it reaps whatever is left of BOTH pieces, brings both
#   back in dependency order, VERIFIES each one is answering again, and
#   returns. Nothing is held in the foreground and nothing is tailed.
#
#   This is the recovery verb for the "half-up corpse" case: the MCP
#   server's stdio child dies on a broken pipe while its parent keeps
#   the loopback port bound, so the port still ACCEPTS connections while
#   every request through it fails. A status probe calls that GREEN, so
#   restart never trusts the probe — it reaps unconditionally, whether
#   the pieces are up, down, or half-up.
#
#   Restart ALWAYS covers both pieces; --tunnel-only does not narrow it
#   (that flag describes what a long-running supervisor invocation
#   manages, and restart is not one). To restart only the tunnel,
#   kickstart the tunnel unit directly.
#
#   --tunnel-only (/ PERSONAL_MCP_TUNNEL_ONLY=1) — start ONLY the
#   reverse SSH tunnel, no status gate, no log tail. Assumes
#   mcp-host-bash is ALREADY listening on 127.0.0.1:$MCP_LOCAL_PORT —
#   typically because it runs always-on under the compose-stack
#   LaunchAgent (RunAtLoad=true). Holds the tunnel in the foreground;
#   when it dies, launchd's KeepAlive can respawn it. In this mode the
#   wrapper does NOT launch mcp-host-bash and does NOT run the listener
#   probe (the MCP server's lifecycle is not ours to manage). This is
#   the unattended/launchd shape.
#
# Lifecycle (default — status-gated tunnel + tail)
#
#   1. Source sibling .env file. Refuse to start if missing.
#   2. TCP-connect probe 127.0.0.1:$MCP_LOCAL_PORT.
#      - Not accepting connections → print error + the --enable rerun
#        command, exit non-zero.
#   3. Service up → start ssh -N -R ... in the background; capture pid.
#   4. Tail the live MCP host log in the foreground. SIGTERM / SIGINT
#      trap actively tears the tunnel down (kill the ssh pid, verify
#      it's gone), then exits.
#
# Lifecycle (--enable — bring up the service, then tunnel + tail)
#
#   1. Source sibling .env file. Refuse to start if missing.
#   2. Resolve mcp-host-bash binary path. Refuse if not executable.
#   3. Start mcp-host-bash --port $MCP_LOCAL_PORT in the background;
#      capture pid.
#   4. Poll-wait for 127.0.0.1:$MCP_LOCAL_PORT to enter LISTEN (same
#      probe pattern as mcp-host-bash's wait_for_listener). Fail-fast
#      if the launcher exits before binding.
#   5. Start ssh -N -R ... in the background; capture pid.
#   6. Tail the live MCP host log in the foreground. SIGTERM / SIGINT
#      trap tears down ONLY the tunnel (verify it's gone) + the log
#      tail, then exits. The detached mcp-host-bash service is left
#      RUNNING — the wrapper's exit does not stop it.
#
# Lifecycle (--tunnel-only)
#
#   1. Source sibling .env file. Refuse to start if missing.
#   2. Skip the mcp-host-bash resolve + launch + listener probe
#      entirely. No status gate, no log tail.
#   3. Start ssh -N -R ... in the background and hold it; when it dies,
#      launchd's KeepAlive can respawn the tunnel.
#   4. SIGTERM / SIGINT trap: actively tear the ssh child down (verify
#      it's gone), then exit.
#
# Lifecycle (restart — one-shot full-stack restart, then return)
#
#   1. Source sibling .env file. Refuse to start if missing.
#   2. REAP, tunnel first (it is the network-facing piece — revoke
#      remote access before churning the server):
#        a. every ssh process whose argv carries OUR reverse-forward
#           spec ($REMOTE_PORT:127.0.0.1:$MCP_LOCAL_PORT): SIGTERM,
#           then SIGKILL, then confirm gone.
#        b. whatever holds 127.0.0.1:$MCP_LOCAL_PORT (found via lsof,
#           else ss): same escalation, then a second sweep for a child
#           that inherited the listening socket from the parent we just
#           killed. Unconditional — a corpse still answers TCP.
#   3. START THE MCP HOST, preferring whoever owns its lifecycle:
#        - a bootstrapped launchd unit  -> launchctl kickstart -k
#          (kills a running instance, then starts; on a stopped unit it
#          just starts — that is what makes this idempotent across up /
#          down / half-up).
#        - nothing owns it              -> launch mcp-host-bash
#          DETACHED (own session, see start_detached) so it outlives
#          this one-shot invocation.
#      Then poll 127.0.0.1:$MCP_LOCAL_PORT for a successful TCP connect.
#      If a unit was kickstarted but the port never came up (e.g. the
#      installed unit only STATUS-GATES rather than starting the
#      server), fall back to launching it directly and poll again.
#   4. START THE TUNNEL — never before step 3 verified, so a tunnel is
#      never exposed to a server that isn't listening. Kickstart the
#      owning launchd unit, or start ssh -N -R DETACHED. Skipped if a
#      unit already reopened the tunnel in step 3.
#   5. VERIFY THE TUNNEL: wait for an ssh carrying our reverse-forward
#      spec, then re-check the SAME pid after a settle window. Because
#      the argv sets ExitOnForwardFailure=yes, an ssh that failed to
#      bind $REMOTE_PORT on the remote exits within a second or two —
#      so surviving the settle window is real evidence the remote-side
#      bind took, not merely that a restart was issued.
#   6. Exit 0 with a summary, or exit 4 naming the piece that did not
#      come back.
#
#   Note: verification is deliberately LOCAL-ONLY. The recommended
#   authorized_keys hardening (see README.md) restricts the tunnel key
#   to port-forwarding with no shell, so this script cannot run a
#   confirming `lsof` on the remote — and should not need a second,
#   less-restricted credential just to self-check.
#
# Usage
#
#   personal-mcp-host.sh                  # default: status-gate, then tunnel + tail
#   personal-mcp-host.sh restart          # reap + restart BOTH pieces, verify, exit
#   personal-mcp-host.sh --enable         # bring the MCP service up, then tunnel + tail
#   personal-mcp-host.sh --tunnel-only    # tunnel only (MCP already up locally; no gate/tail)
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
#   MCP_HOST_BASH_BIN          override path to the mcp-host-bash-server binary.
#                              Default: `mcp-host-bash-server` on PATH, else
#                              ~/bin/mcp-host-bash-server (where
#                              `make install-mcp-host-bash-server` puts it).
#   MCP_HOST_BASH_BEARER       shared-secret bearer token (recommended). Forwarded
#                              to mcp-host-bash-server, which validates it
#                              in-process. Generate once:
#                                head -c 32 /dev/urandom | base64
#   CW_PROFILE                 trust profile for mcp-host-bash-server. Default `corp-dev`
#                              (read-y floor). Set `corp-dev-trusted` to widen.
#   ALLOWED_DIR                fence run_command to this dir. Unset here =>
#                              mcp-host-bash's default "/" applies (path
#                              boundary disabled). Set to $HOME or a subdir
#                              to re-enable a boundary.
#   ALLOW_SHELL_OPERATORS      let run_command chain pipes / &&. Default false.
#   MCP_HOST_BASH_LOG          override the live log path the default / --enable
#                              modes tail after the tunnel comes up. Default:
#                              ~/.local/state/claude-container/mcp-host-bash.log
#                              (the same path mcp-host-bash writes to). Kept in
#                              sync with the launcher so `tail -F` follows the
#                              real traffic.
#   PERSONAL_MCP_TUNNEL_ONLY   set to 1 (or pass --tunnel-only) to start ONLY
#                              the reverse SSH tunnel, skipping the status gate,
#                              the mcp-host-bash launch + listener probe, and
#                              the log tail. Use when mcp-host-bash is already
#                              running locally (e.g. the always-on compose-stack
#                              LaunchAgent) and you want the unattended/launchd
#                              shape. The --tunnel-only flag and this env var
#                              are equivalent; either enables the mode.
#   PERSONAL_MCP_DISABLED      soft kill switch — script exits 0 immediately.
#                              Pair with launchd's KeepAlive to leave the unit
#                              registered without actually running mcp-host-bash
#                              and the tunnel.
#   PERSONAL_MCP_SSH_EXTRA     extra space-separated `ssh -o KEY=VALUE` opts
#                              appended to the tunnel's argv. For one-off
#                              tuning (proxy jump, lower keep-alive cadence)
#                              without editing this script.
#
# Optional env vars — `restart` only
#
#   PERSONAL_MCP_RESTART_TIMEOUT
#                              seconds to wait for EACH piece to come back
#                              before failing. Default 45 — generous on
#                              purpose: launchd's ThrottleInterval (30s in
#                              the shipped plists) can delay a respawn.
#   PERSONAL_MCP_TUNNEL_SETTLE seconds the freshly-started ssh must SURVIVE
#                              before the tunnel counts as verified.
#                              Default 3. With ExitOnForwardFailure=yes a
#                              failed remote-side bind exits ssh well inside
#                              that window.
#   PERSONAL_MCP_TUNNEL_LOG    where a DIRECTLY-started (non-launchd) tunnel
#                              appends its stderr, since restart returns
#                              instead of holding it in the foreground.
#                              Default: personal-mcp-tunnel.log next to
#                              MCP_HOST_BASH_LOG.
#   PERSONAL_MCP_HOST_LABEL    launchd label of the BUNDLED unit (server +
#                              tunnel in one). Default
#                              org.gbre.personal-mcp.host
#   PERSONAL_MCP_TUNNEL_LABEL  launchd label of the TUNNEL-ONLY unit.
#                              Default org.gbre.personal-mcp.tunnel
#   PERSONAL_MCP_SERVER_LABEL  launchd label of the always-on MCP server
#                              unit from the compose stack. Default
#                              org.gbre.claude-watch.mcp-host-bash
#                              Override any of the three if you renamed a
#                              unit; a label that is not bootstrapped is
#                              simply skipped.
#
# Exit codes
#   0   normal shutdown (or --help / --print-cmd / PERSONAL_MCP_DISABLED,
#       or a `restart` where BOTH pieces verified)
#   1   missing mcp-host-bash binary, or child died before binding, or
#       child died during steady-state and we tore the other one down.
#   2   bad flag / missing .env / missing required key in .env
#   3   default mode: host MCP service is not up (RED). The error names
#       the --enable rerun command that brings it up.
#   4   restart: the stack did not come back. The error names the piece
#       (MCP host / reverse SSH tunnel) that could not be reaped or did
#       not come back up within PERSONAL_MCP_RESTART_TIMEOUT.

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
# --enable: bring the host MCP service up before opening the tunnel.
# Without it the default mode only GATES on the service being up (RED
# path errors out if it isn't).
ENABLE=0
# restart: one-shot "reap + bring the whole stack back + verify + exit".
# Spelled as a bare verb (what an operator reaches for under pressure)
# with a --restart alias so it composes with the flag-shaped modes above.
RESTART=0
while [ "$#" -gt 0 ]; do
    case "$1" in
        --help|-h)
            usage
            exit 0
            ;;
        restart|--restart)
            # Reap BOTH pieces (however they died), bring them back in
            # dependency order — MCP host first, then the reverse SSH
            # tunnel — verify each one is answering again, and return.
            # Does not hold anything in the foreground; does not tail.
            RESTART=1
            shift
            ;;
        --tunnel-only)
            # Start ONLY the reverse SSH tunnel; skip the status gate,
            # the mcp-host-bash launch + listener probe, and the log
            # tail. For when mcp-host-bash is already running locally
            # (e.g. the always-on compose-stack LaunchAgent).
            # Equivalent to PERSONAL_MCP_TUNNEL_ONLY=1.
            TUNNEL_ONLY=1
            shift
            ;;
        --enable)
            # Bring the host MCP service up (start mcp-host-bash + wait
            # for it to bind), then continue into the green path (open
            # the tunnel + tail the log). This is the rerun command the
            # default mode's RED-path error tells the operator to run.
            ENABLE=1
            shift
            ;;
        --print-cmd)
            # Test-only: build the planned ssh argv but print it
            # (one-per-line) instead of executing. Also skips the
            # mcp-host-bash-server launch + listener probe so the test
            # runs on hosts that don't have the binary installed. In
            # tunnel-only mode the MCP_HOST_BASH_BIN:
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

# Resolve mcp-host-bash-server. Default: the binary on PATH, else the
# ~/bin install location `make install-mcp-host-bash-server` writes to.
MCP_HOST_BASH_BIN="${MCP_HOST_BASH_BIN:-$(command -v mcp-host-bash-server 2>/dev/null || echo "${HOME}/bin/mcp-host-bash-server")}"

# launchd labels `restart` consults, in the shapes this directory ships.
# Each is probed with `launchctl print`; whichever ones are bootstrapped
# own their piece's lifecycle and get kickstarted rather than hand-
# launched. Not bootstrapped (or no launchctl at all — Linux) => restart
# owns the launch itself. Overridable for operators who renamed a unit.
PERSONAL_MCP_HOST_LABEL="${PERSONAL_MCP_HOST_LABEL:-org.gbre.personal-mcp.host}"
PERSONAL_MCP_TUNNEL_LABEL="${PERSONAL_MCP_TUNNEL_LABEL:-org.gbre.personal-mcp.tunnel}"
PERSONAL_MCP_SERVER_LABEL="${PERSONAL_MCP_SERVER_LABEL:-org.gbre.claude-watch.mcp-host-bash}"

# The reverse-forward spec is the fingerprint `restart` uses to find OUR
# tunnel among any other ssh processes on the box: it is exactly the -R
# value in the argv built below.
tunnel_forward_spec="${REMOTE_PORT}:127.0.0.1:${MCP_LOCAL_PORT}"

# Resolve the live log path tailed by the default / --enable green
# paths. Keep this in lockstep with mcp-host-bash's own default so the
# tail follows the real JSON-RPC + run_command traffic.
MCP_HOST_BASH_LOG="${MCP_HOST_BASH_LOG:-${HOME}/.local/state/claude-container/mcp-host-bash.log}"

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
    -R "$tunnel_forward_spec"
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
    #
    # restart: a RESTART: block first — the ordered plan (what gets
    # reaped, what gets restarted, what gets verified) plus the launchd
    # labels consulted — then BOTH of the blocks below, because restart
    # may end up launching either piece itself.
    if [ "$RESTART" = "1" ]; then
        echo "RESTART:"
        echo "reap-tunnel"
        echo "$tunnel_forward_spec"
        echo "reap-mcp-port"
        echo "127.0.0.1:${MCP_LOCAL_PORT}"
        echo "restart-mcp-host"
        echo "restart-tunnel"
        echo "verify-mcp-host"
        echo "127.0.0.1:${MCP_LOCAL_PORT}"
        echo "verify-tunnel"
        echo "$tunnel_forward_spec"
        echo "launchd-label-bundled"
        echo "$PERSONAL_MCP_HOST_LABEL"
        echo "launchd-label-tunnel"
        echo "$PERSONAL_MCP_TUNNEL_LABEL"
        echo "launchd-label-server"
        echo "$PERSONAL_MCP_SERVER_LABEL"
        echo
    fi
    if [ "$TUNNEL_ONLY" = "0" ] || [ "$RESTART" = "1" ]; then
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
# Only required for --enable, the one mode that launches the MCP server.
# The default (status-gate) mode and --tunnel-only assume the server is
# already up locally, so the launcher binary need not even be present.
# -----------------------------------------------------------------------------

if [ "$ENABLE" = "1" ] && [ ! -x "$MCP_HOST_BASH_BIN" ]; then
    cat >&2 <<EOF
personal-mcp-host: mcp-host-bash-server not found / not executable: $MCP_HOST_BASH_BIN

Build + install it once from the repo root:

    make install-mcp-host-bash-server

That drops the binary at ~/bin/mcp-host-bash-server. If your
mcp-host-bash-server binary lives elsewhere, set MCP_HOST_BASH_BIN in
$env_file to its absolute path.
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
tail_pid=""
cleanup_exit_code=0
shutting_down=0
cleanup_ran=0

# Set immediately BEFORE the matching `cmd &`, so the teardown can tell
# "this child was never started" from "this child is running but its pid
# never made it into the variable" — see adopt_unrecorded_child.
ssh_started=0
tail_started=0

# Has this pid stopped doing anything?
#
# `kill -0` alone is the wrong question: a process that has exited but
# whose parent has not yet wait()ed for it is a ZOMBIE, and `kill -0`
# still succeeds on one. A zombie has already released every fd it held
# — it cannot own a listening socket or a tunnel — so for teardown
# purposes it is gone, and reporting it as a survivor would send the
# operator chasing a corpse.
pid_is_gone() {
    local pid=$1 state
    kill -0 "$pid" 2>/dev/null || return 0
    command -v ps >/dev/null 2>&1 || return 1
    state=$(ps -o state= -p "$pid" 2>/dev/null | tr -d '[:space:]' || true)
    case "$state" in
        Z*) return 0 ;;   # zombie: exited, awaiting reap
        "") return 0 ;;   # vanished between the two probes
        *)  return 1 ;;
    esac
}

# Actively tear down a single child: SIGTERM, give it a moment, then
# SIGKILL if it's still alive, and confirm it's actually gone. Echoes a
# warning (does not abort cleanup) if the pid survives a SIGKILL — that
# only happens for unkillable states (stuck in an uninterruptible
# syscall) the operator must chase manually. Returns 0 if the pid is
# gone afterward, 1 otherwise.
teardown_pid() {
    local label=$1 pid=$2
    [ -n "$pid" ] || return 0
    if pid_is_gone "$pid"; then
        return 0
    fi
    kill -TERM "$pid" 2>/dev/null || true
    # Poll for graceful exit before escalating to SIGKILL.
    local i
    for i in 1 2 3 4 5; do
        pid_is_gone "$pid" && break
        sleep 0.1
    done
    if ! pid_is_gone "$pid"; then
        kill -KILL "$pid" 2>/dev/null || true
        sleep 0.2
    fi
    if ! pid_is_gone "$pid"; then
        echo "personal-mcp-host: WARNING: $label (pid $pid) survived teardown" >&2
        return 1
    fi
    return 0
}

# Recover a child that was started but whose pid never got recorded.
#
# `cmd &` and the `pid=$!` that records it are two statements, and bash
# runs a pending signal handler at the statement boundary between them.
# A SIGTERM that arrives while the `&` is still forking therefore reaches
# the teardown with the pid variable EMPTY even though the child is
# running — and a teardown that trusted that variable would walk away
# from a live reverse SSH tunnel, or leave the log follower writing to
# the operator's terminal after the wrapper exited.
#
# `$!` is set by the `&` itself, not by the assignment, so it still names
# that child. The `*_started` markers say which child it is, so an
# unrelated background job (notably the DETACHED --enable MCP service,
# which we deliberately leave running) is never adopted by mistake.
adopt_unrecorded_child() {
    local last_bg=${!:-}
    [ -n "$last_bg" ] || return 0
    if [ "$tail_started" = "1" ] && [ -z "$tail_pid" ]; then
        tail_pid=$last_bg
    elif [ "$ssh_started" = "1" ] && [ -z "$ssh_pid" ]; then
        ssh_pid=$last_bg
    fi
    return 0
}

cleanup() {
    # Re-entrancy guard: a SIGTERM during the SIGINT-triggered teardown
    # must not restart the sequence.
    if [ "$cleanup_ran" = "1" ]; then
        return
    fi
    cleanup_ran=1

    adopt_unrecorded_child

    # Stop the log tail first so its output doesn't race the teardown
    # banner. It's a local follower, not part of the bridge.
    if [ -n "$tail_pid" ] && kill -0 "$tail_pid" 2>/dev/null; then
        kill -TERM "$tail_pid" 2>/dev/null || true
    fi

    # Actively tear down the reverse tunnel — that's the network-facing
    # piece. Do NOT merely exit and leave a half-open forward dangling;
    # kill the ssh process and verify it's gone.
    if [ -n "$ssh_pid" ]; then
        echo "personal-mcp-host: tearing down reverse SSH tunnel (pid $ssh_pid)" >&2
        if teardown_pid "ssh tunnel" "$ssh_pid"; then
            echo "personal-mcp-host: reverse SSH tunnel torn down" >&2
        else
            # Couldn't confirm the tunnel died — surface a non-zero exit
            # so launchd / the operator notices.
            [ "$cleanup_exit_code" = "0" ] && cleanup_exit_code=1
        fi
    fi

    # We deliberately DO NOT tear down the host MCP service here, even
    # when --enable started it. The tunnel is the ephemeral "grant remote
    # access for now" piece; the MCP service is persistent. Ctrl-C / a
    # launchd SIGTERM is the operator saying "close the tunnel", NOT
    # "shut the server down". The --enable path starts mcp-host-bash
    # DETACHED in its own session (see start_detached) precisely so it
    # survives this wrapper's exit — re-running the default (GREEN) path
    # then detects the still-listening service and just reconnects the
    # tunnel. To stop the service itself, the operator stops it directly
    # (Ctrl-C its own foreground, or MCP_HOST_BASH_DISABLED / launchd).
    if [ -n "$mcp_pid" ]; then
        echo "personal-mcp-host: leaving host MCP service running (pid $mcp_pid) — only the tunnel was torn down" >&2
    fi

    exit "$cleanup_exit_code"
}

# The SIGTERM / SIGINT handler.
#
# Deliberately a plain function call, and the handler string registered
# below is deliberately nothing but this function's name. bash re-parses
# a handler string at signal-delivery time, so everything in it is
# parser work done at the least predictable moment in the script's life.
# Keeping it to a single word means there is no state baked into the
# string (the flag is set here, from a variable, where the rest of the
# teardown can see it) and the least possible parsing between the signal
# and the teardown actually running.
on_terminate() {
    shutting_down=1
    cleanup
}
trap 'on_terminate' TERM INT

# `mkdir -p "$(dirname "$path")"` without the command substitution.
#
# This is NOT stylistic. A command substitution makes the shell re-enter
# its own parser mid-expansion, and a SIGTERM that lands inside that
# re-entry can get its handler string parsed while the substitution's
# still-open `(` is on the parser's delimiter stack. bash then kills the
# handler with
#
#   personal-mcp-host.sh: trap: line 2: unexpected EOF while looking for matching `)'
#
# and — this is the damaging part — carries on running. The signal is
# swallowed: no teardown, no exit, a reverse tunnel left wide open by a
# SIGTERM that looked like it was delivered. Parameter expansion needs no
# parser re-entry, so it cannot lose a signal that way. Anything on the
# path between "tunnel is up" and the steady-state wait must stay free of
# command substitutions for the same reason.
ensure_parent_dir() {
    local path=$1 dir=${1%/*}
    if [ "$dir" = "$path" ]; then
        dir=.
    elif [ -z "$dir" ]; then
        dir=/
    fi
    mkdir -p "$dir" 2>/dev/null || true
}

# -----------------------------------------------------------------------------
# Listener probe — poll until the MCP server's loopback port enters
# LISTEN (or the child dies / shutdown fires).
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
# Service status probe — a single TCP connect to the MCP port, no child
# pid involved. Used by the default mode's status gate to decide RED vs
# GREEN. Returns 0 if something is accepting connections on
# host:port, 1 otherwise.
# -----------------------------------------------------------------------------

service_is_up() {
    local host=$1 port=$2
    python3 -c "
import socket, sys
s = socket.socket()
s.settimeout(0.5)
try:
    s.connect(('$host', $port))
    s.close()
except OSError:
    sys.exit(1)
" 2>/dev/null
}

# -----------------------------------------------------------------------------
# Open the reverse SSH tunnel in the background, then follow the live
# MCP host log in the foreground. The tail is what keeps us in the
# foreground; the SIGINT/SIGTERM trap tears the tunnel (and any
# --enable mcp-host-bash child) down. If the tunnel dies on its own,
# stop tailing and exit non-zero so launchd's KeepAlive respawns the
# whole unit.
# -----------------------------------------------------------------------------

run_tunnel_and_tail() {
    # Make sure there's a file to follow even on first run — tail -F
    # tolerates a missing file but emits a noisy warning; pre-create the
    # directory + file so the follow is clean from the start.
    #
    # This happens BEFORE the tunnel is spawned, on purpose. Once ssh is
    # running, a SIGTERM has real work to do (kill the tunnel, verify it
    # is gone), so the window between the spawn and the steady-state wait
    # has to be as short as possible and free of command substitutions —
    # see ensure_parent_dir for what a substitution does to a signal that
    # arrives inside it.
    ensure_parent_dir "$MCP_HOST_BASH_LOG"
    [ -f "$MCP_HOST_BASH_LOG" ] || : >"$MCP_HOST_BASH_LOG" 2>/dev/null || true

    ssh_started=1
    "${ssh_argv[@]}" &
    ssh_pid=$!
    echo "personal-mcp-host: reverse SSH tunnel started (pid $ssh_pid)" >&2

    echo "personal-mcp-host: following $MCP_HOST_BASH_LOG (Ctrl-C to stop)" >&2
    # -F (follow + retry on rotate/recreate) so log rotation doesn't
    # silently end the follow.
    tail_started=1
    tail -n 50 -F "$MCP_HOST_BASH_LOG" &
    tail_pid=$!

    # Steady-state: hold while the tunnel lives. If the tunnel dies,
    # stop tailing and tear down (cleanup verifies the ssh pid is gone).
    while kill -0 "$ssh_pid" 2>/dev/null; do
        sleep 1
    done

    echo "personal-mcp-host: reverse SSH tunnel exited; shutting down" >&2
    cleanup_exit_code=1
    cleanup
}

# -----------------------------------------------------------------------------
# Banner
# -----------------------------------------------------------------------------

{
    if [ "$RESTART" = "1" ]; then
        echo "personal-mcp-host: restarting the full stack (MCP host + reverse SSH tunnel)"
        if [ "$TUNNEL_ONLY" = "1" ]; then
            echo "  NOTE:                  tunnel-only is IGNORED by restart — it always"
            echo "                         restarts BOTH pieces. Kickstart the tunnel unit"
            echo "                         directly if you want only the tunnel."
        fi
    elif [ "$TUNNEL_ONLY" = "1" ]; then
        echo "personal-mcp-host: starting (tunnel-only)"
    elif [ "$ENABLE" = "1" ]; then
        echo "personal-mcp-host: starting (--enable: bring service up, then tunnel + tail)"
    else
        echo "personal-mcp-host: starting (default: status-gate, then tunnel + tail)"
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
    if [ "$ENABLE" = "1" ] || [ "$RESTART" = "1" ]; then
        echo "  launcher:              $MCP_HOST_BASH_BIN"
    else
        echo "  launcher:              <not managed here; mcp-host-bash assumed already running>"
    fi
    if [ "$TUNNEL_ONLY" = "0" ] || [ "$RESTART" = "1" ]; then
        echo "  live log:              $MCP_HOST_BASH_LOG"
    fi
    if [ -n "${PERSONAL_MCP_SSH_EXTRA:-}" ]; then
        echo "  SSH extras:            $PERSONAL_MCP_SSH_EXTRA"
    fi
    echo
    if [ "$RESTART" = "0" ]; then
        echo "Ctrl-C to stop."
        echo
    fi
} >&2

# -----------------------------------------------------------------------------
# Tunnel-only: skip the status gate, the mcp-host-bash launch + listener
# probe, and the log tail entirely. Just open the reverse SSH tunnel and
# hold it. mcp-host-bash is assumed already listening on
# 127.0.0.1:$MCP_LOCAL_PORT (e.g. the always-on compose-stack
# LaunchAgent). If the tunnel dies, exit non-zero so launchd's KeepAlive
# respawns it.
#
# `restart` skips this block deliberately: an operator whose .env carries
# PERSONAL_MCP_TUNNEL_ONLY=1 (the recommended split) must still get a
# FULL-stack restart out of the restart verb, not a tunnel-only one.
# -----------------------------------------------------------------------------

if [ "$TUNNEL_ONLY" = "1" ] && [ "$RESTART" = "0" ]; then
    "${ssh_argv[@]}" &
    ssh_pid=$!
    while kill -0 "$ssh_pid" 2>/dev/null; do
        sleep 1
    done
    cleanup_exit_code=1
    cleanup
fi

# -----------------------------------------------------------------------------
# start_detached — launch a command in its OWN session (new session ⇒ new
# process group), so it is NOT in this wrapper's process group and is
# therefore NOT hit by a terminal-delivered Ctrl-C (SIGINT goes to the
# foreground process group) nor by this wrapper's own exit. The detached
# child must outlive us: that's the whole point of --enable starting a
# persistent service while the wrapper only manages the ephemeral tunnel.
#
# Portability: `setsid` is the clean primitive but is NOT present in the
# macOS base system. Perl ships on macOS (and Linux) and exposes
# POSIX::setsid(), so we fall back to a tiny perl wrapper. Either form
# backgrounded with `&` yields a capturable `$!` whose PID lives in a
# fresh session — verified the liveness probe (`kill -0 $mcp_pid`) still
# works because the PID we capture IS the exec'd service, not an
# intermediate that exits.
#
# Echoes nothing; the caller captures the pid via `$!` right after.
# -----------------------------------------------------------------------------

if command -v setsid >/dev/null 2>&1; then
    start_detached() { setsid "$@"; }
elif command -v perl >/dev/null 2>&1; then
    start_detached() { perl -e 'use POSIX qw(setsid); setsid() or die "setsid: $!"; exec @ARGV or die "exec: $!";' -- "$@"; }
else
    # Last-resort fallback: no setsid, no perl. `nohup` detaches from the
    # controlling terminal's SIGHUP but does NOT move the child into a new
    # process group, so a terminal Ctrl-C (SIGINT to the foreground pgrp)
    # could still reach it. We accept that on the rare host with neither
    # setsid nor perl; cleanup() still never signals mcp_pid itself.
    start_detached() { nohup "$@"; }
fi

# -----------------------------------------------------------------------------
# restart — one-shot full-stack restart.
#
# Everything below runs ONLY for the restart verb. The helpers are kept
# here (rather than up with the probes) because nothing else uses them
# and they need start_detached, defined immediately above.
# -----------------------------------------------------------------------------

# Poll until host:port accepts a TCP connect. This is the "it is
# answering again" gate — not "we issued a restart".
wait_for_port() {
    local host=$1 port=$2 timeout=$3 deadline
    deadline=$(( $(date +%s) + timeout ))
    while [ "$(date +%s)" -lt "$deadline" ]; do
        if service_is_up "$host" "$port"; then
            return 0
        fi
        sleep 0.3
    done
    return 1
}

# Inverse: poll until nothing accepts on host:port. Used to CONFIRM a
# reap actually released the port instead of assuming the kill worked.
wait_for_port_free() {
    local host=$1 port=$2 timeout=$3 deadline
    deadline=$(( $(date +%s) + timeout ))
    while [ "$(date +%s)" -lt "$deadline" ]; do
        if ! service_is_up "$host" "$port"; then
            return 0
        fi
        sleep 0.2
    done
    return 1
}

# pids of whatever is LISTENing on a loopback port. lsof is the macOS
# answer (and usually present on Linux); ss covers Linux hosts without
# it. Neither available => empty, and the caller says so out loud rather
# than pretending the port was clean.
port_listener_pids() {
    local port=$1
    if command -v lsof >/dev/null 2>&1; then
        lsof -nP -iTCP:"$port" -sTCP:LISTEN -t 2>/dev/null || true
    elif command -v ss >/dev/null 2>&1; then
        ss -H -ltnp "sport = :$port" 2>/dev/null \
            | grep -o 'pid=[0-9]*' | cut -d= -f2 | sort -u || true
    fi
}

# pids of ssh processes carrying OUR reverse-forward spec. The spec is
# specific enough (remote port + loopback + local port) that it will not
# collide with an unrelated ssh, and matching on argv finds the tunnel
# whoever started it — this wrapper, a launchd unit, or a hand-run ssh.
tunnel_pids() {
    if command -v pgrep >/dev/null 2>&1; then
        pgrep -f -- "${REMOTE_PORT}:127\.0\.0\.1:${MCP_LOCAL_PORT}" 2>/dev/null || true
    fi
}

# Reap the reverse SSH tunnel, whatever state it is in. Returns 1 only
# if a matching ssh survived a SIGKILL.
reap_tunnel() {
    local pids pid
    pids=$(tunnel_pids)
    if [ -z "$pids" ]; then
        if ! command -v pgrep >/dev/null 2>&1; then
            echo "personal-mcp-host: WARNING: pgrep unavailable — cannot find a stale reverse SSH tunnel to reap; relying on the restart below" >&2
        else
            echo "personal-mcp-host: no reverse SSH tunnel running for $tunnel_forward_spec — nothing to reap" >&2
        fi
        return 0
    fi
    for pid in $pids; do
        [ "$pid" = "$$" ] && continue
        echo "personal-mcp-host: reaping reverse SSH tunnel (pid $pid)" >&2
        teardown_pid "ssh tunnel" "$pid" || true
    done
    # Confirm, don't assume.
    local i
    for i in 1 2 3 4 5 6 7 8 9 10; do
        pids=$(tunnel_pids)
        [ -z "$pids" ] && return 0
        sleep 0.3
    done
    echo "personal-mcp-host: WARNING: ssh still matching $tunnel_forward_spec after teardown: $(echo "$pids" | tr '\n' ' ')" >&2
    return 1
}

# Reap whatever holds the MCP loopback port. UNCONDITIONAL: the failure
# this verb exists for is a server whose stdio child died on a broken
# pipe while the parent kept the port bound, so "the port answers" is
# NOT evidence of health and must never short-circuit the reap.
reap_mcp_port() {
    local port=$1 pids pid
    pids=$(port_listener_pids "$port")
    if [ -z "$pids" ]; then
        if ! command -v lsof >/dev/null 2>&1 && ! command -v ss >/dev/null 2>&1; then
            echo "personal-mcp-host: WARNING: neither lsof nor ss available — cannot identify what holds 127.0.0.1:$port; skipping the reap. If a stale server owns it, the restart below will fail to bind and say so." >&2
            return 0
        fi
        echo "personal-mcp-host: nothing listening on 127.0.0.1:$port — nothing to reap" >&2
        return 0
    fi
    for pid in $pids; do
        [ "$pid" = "$$" ] && continue
        echo "personal-mcp-host: reaping MCP host holding 127.0.0.1:$port (pid $pid)" >&2
        teardown_pid "MCP host" "$pid" || true
    done
    if wait_for_port_free 127.0.0.1 "$port" 5; then
        return 0
    fi
    # Second sweep: a child that inherited the listening socket can keep
    # the port bound after its parent is gone. Re-query (the holder is a
    # different pid now) and SIGKILL outright.
    pids=$(port_listener_pids "$port")
    for pid in $pids; do
        [ "$pid" = "$$" ] && continue
        echo "personal-mcp-host: port still held after teardown; SIGKILL inherited holder (pid $pid)" >&2
        kill -KILL "$pid" 2>/dev/null || true
    done
    wait_for_port_free 127.0.0.1 "$port" 5
}

# Is a launchd unit bootstrapped in this user's GUI domain? False on any
# non-macOS host (no launchctl) — restart then owns the launches itself.
unit_registered() {
    local label=$1
    command -v launchctl >/dev/null 2>&1 || return 1
    launchctl print "gui/$(id -u)/${label}" >/dev/null 2>&1
}

# kickstart -k: SIGKILL a running instance, then start it. On a unit that
# is NOT running it just starts it — which is exactly what makes restart
# idempotent whether the piece was up, down, or wedged.
kickstart_unit() {
    local label=$1
    if launchctl kickstart -k "gui/$(id -u)/${label}" >/dev/null 2>&1; then
        echo "personal-mcp-host: kickstarted launchd unit ${label}" >&2
        return 0
    fi
    echo "personal-mcp-host: WARNING: launchctl kickstart -k ${label} failed; verification below decides the outcome" >&2
    return 1
}

# Track whether we (rather than launchd) launched the server, so the
# fallback path below is never attempted twice.
RESTART_HOST_STARTED_DIRECTLY=0

# Launch mcp-host-bash ourselves, DETACHED — restart returns instead of
# supervising, so anything it starts has to outlive it.
start_host_directly() {
    if [ ! -x "$MCP_HOST_BASH_BIN" ]; then
        cat >&2 <<EOF
personal-mcp-host: FATAL: cannot restart the MCP host — no launchd unit
       owns it and the launcher is not executable:
         $MCP_HOST_BASH_BIN
       Set MCP_HOST_BASH_BIN in $env_file, or bootstrap one of the
       LaunchAgent units so launchd owns the server's lifecycle.
EOF
        return 1
    fi
    if [ -z "${MCP_HOST_BASH_BEARER:-}" ]; then
        echo "personal-mcp-host: NOTE: launching the MCP host directly with no MCP_HOST_BASH_BEARER in scope. If your bearer normally comes from the login Keychain via a LaunchAgent wrapper, this instance will NOT have it and remote clients sending Authorization headers may not match." >&2
    fi
    ensure_parent_dir "$MCP_HOST_BASH_LOG"
    start_detached "$MCP_HOST_BASH_BIN" --port "$MCP_LOCAL_PORT" \
        </dev/null >>"$MCP_HOST_BASH_LOG" 2>&1 &
    RESTART_HOST_STARTED_DIRECTLY=1
    echo "personal-mcp-host: launched MCP host directly (pid $!, detached); stderr -> $MCP_HOST_BASH_LOG" >&2
    return 0
}

# Same for the tunnel: detached, with its own log, because restart does
# not stay around to hold it in the foreground.
start_tunnel_directly() {
    local log
    log="${PERSONAL_MCP_TUNNEL_LOG:-$(dirname "$MCP_HOST_BASH_LOG")/personal-mcp-tunnel.log}"
    ensure_parent_dir "$log"
    start_detached "${ssh_argv[@]}" </dev/null >>"$log" 2>&1 &
    echo "personal-mcp-host: launched reverse SSH tunnel directly (pid $!, detached); stderr -> $log" >&2
    return 0
}

# Wait for a tunnel and prove it STAYED up. ExitOnForwardFailure=yes
# means an ssh that could not bind $REMOTE_PORT on the remote exits
# within a second or two, so the same pid surviving the settle window is
# real evidence the remote-side bind took.
verify_tunnel() {
    local timeout=$1 settle="${PERSONAL_MCP_TUNNEL_SETTLE:-3}"
    local deadline pid=""
    deadline=$(( $(date +%s) + timeout ))
    while [ "$(date +%s)" -lt "$deadline" ]; do
        # First pid only. Trimmed with parameter expansion rather than
        # `| head -1`: under `set -o pipefail` a SIGPIPE'd pgrep would
        # fail the whole command substitution.
        pid=$(tunnel_pids)
        pid=${pid%%$'\n'*}
        [ -n "$pid" ] && break
        sleep 0.5
    done
    if [ -z "$pid" ]; then
        return 1
    fi
    sleep "$settle"
    if pid_is_gone "$pid"; then
        # It came up and died inside the settle window — the classic
        # "remote port forwarding failed" shape.
        return 1
    fi
    echo "personal-mcp-host: reverse SSH tunnel is up (pid $pid, $tunnel_forward_spec)" >&2
    return 0
}

restart_stack() {
    local timeout="${PERSONAL_MCP_RESTART_TIMEOUT:-45}"
    local bundled=0 server_unit="" tunnel_unit=""

    if unit_registered "$PERSONAL_MCP_HOST_LABEL"; then
        bundled=1
        echo "personal-mcp-host: bundled launchd unit $PERSONAL_MCP_HOST_LABEL is bootstrapped" >&2
    fi
    if unit_registered "$PERSONAL_MCP_SERVER_LABEL"; then
        server_unit="$PERSONAL_MCP_SERVER_LABEL"
        echo "personal-mcp-host: MCP server launchd unit $server_unit is bootstrapped" >&2
    fi
    if unit_registered "$PERSONAL_MCP_TUNNEL_LABEL"; then
        tunnel_unit="$PERSONAL_MCP_TUNNEL_LABEL"
        echo "personal-mcp-host: tunnel launchd unit $tunnel_unit is bootstrapped" >&2
    fi

    # --- 1. Reap. Tunnel first: it is the network-facing piece, so
    #        remote access goes away before we churn the server. ---
    if ! reap_tunnel; then
        cat >&2 <<EOF
personal-mcp-host: FATAL: reverse SSH tunnel — could not reap the stale
       tunnel. An ssh carrying $tunnel_forward_spec survived SIGKILL, so
       a fresh tunnel would collide with it. Chase the pid above by hand
       (it is likely unkillable / stuck in a syscall) and re-run.
EOF
        return 4
    fi
    if ! reap_mcp_port "$MCP_LOCAL_PORT"; then
        cat >&2 <<EOF
personal-mcp-host: FATAL: MCP host — 127.0.0.1:$MCP_LOCAL_PORT is STILL
       held after teardown, so a restarted server cannot bind it:
         lsof -nP -iTCP:$MCP_LOCAL_PORT -sTCP:LISTEN
       Clear the surviving process by hand and re-run.
EOF
        return 4
    fi

    # --- 2. Bring the MCP host back. Prefer whoever owns its lifecycle:
    #        a bootstrapped unit relaunches it with the environment
    #        launchd was configured with (e.g. a Keychain-sourced
    #        bearer), which a hand launch here cannot reproduce. ---
    if [ -n "$server_unit" ]; then
        kickstart_unit "$server_unit" || true
    elif [ "$bundled" = "1" ]; then
        kickstart_unit "$PERSONAL_MCP_HOST_LABEL" || true
    else
        start_host_directly || return 4
    fi

    if ! wait_for_port 127.0.0.1 "$MCP_LOCAL_PORT" "$timeout"; then
        if [ "$RESTART_HOST_STARTED_DIRECTLY" = "0" ]; then
            # The unit was kickstarted but nothing bound the port. The
            # usual cause: the installed unit's argv only STATUS-GATES
            # (the default mode) instead of starting the server. Own the
            # launch ourselves rather than reporting a failure we can fix.
            echo "personal-mcp-host: launchd unit did not bring the MCP host up within ${timeout}s — launching it directly instead" >&2
            start_host_directly || return 4
            if ! wait_for_port 127.0.0.1 "$MCP_LOCAL_PORT" "$timeout"; then
                cat >&2 <<EOF
personal-mcp-host: FATAL: MCP host did NOT come back — nothing is
       accepting connections on 127.0.0.1:$MCP_LOCAL_PORT after
       ${timeout}s. Check $MCP_HOST_BASH_LOG for the launcher's stderr.
       The reverse SSH tunnel was NOT started: a tunnel to a dead server
       would look healthy from the remote side while failing every call.
EOF
                return 4
            fi
        else
            cat >&2 <<EOF
personal-mcp-host: FATAL: MCP host did NOT come back — nothing is
       accepting connections on 127.0.0.1:$MCP_LOCAL_PORT after
       ${timeout}s. Check $MCP_HOST_BASH_LOG for the launcher's stderr.
       The reverse SSH tunnel was NOT started: a tunnel to a dead server
       would look healthy from the remote side while failing every call.
EOF
            return 4
        fi
    fi
    echo "personal-mcp-host: MCP host is answering on 127.0.0.1:$MCP_LOCAL_PORT" >&2

    # --- 3. Bring the tunnel back, now that the server is verified. ---
    if [ -n "$(tunnel_pids)" ]; then
        # A bundled unit that owns both pieces may already have reopened
        # it during step 2. Don't start a second one; just verify.
        echo "personal-mcp-host: reverse SSH tunnel already reopened by launchd" >&2
    elif [ -n "$tunnel_unit" ]; then
        kickstart_unit "$tunnel_unit" || true
    elif [ "$bundled" = "1" ]; then
        # The bundled unit gates on the server being up; it is now up, so
        # this kickstart takes the GREEN path and opens the tunnel.
        kickstart_unit "$PERSONAL_MCP_HOST_LABEL" || true
    else
        start_tunnel_directly || return 4
    fi

    # --- 4. Verify the tunnel actually held. ---
    if ! verify_tunnel "$timeout"; then
        cat >&2 <<EOF
personal-mcp-host: FATAL: reverse SSH tunnel did NOT come back — no ssh
       carrying $tunnel_forward_spec stayed up within ${timeout}s.
       The MCP host IS running (127.0.0.1:$MCP_LOCAL_PORT answers); only
       the tunnel is missing, so the remote cannot reach it. Usual
       causes, in order:
         - $REMOTE_PORT still bound on the remote by a stale forward
           (ExitOnForwardFailure=yes makes ssh exit rather than sit there
           silently forwarding nothing).
         - key in SSH_KEY_PATH no longer accepted by the remote.
         - the remote is unreachable from this network.
       Check the tunnel log named above (or, under launchd,
       ~/Library/Logs/personal-mcp-tunnel.err.log) for ssh's own stderr.
EOF
        return 4
    fi

    return 0
}

if [ "$RESTART" = "1" ]; then
    restart_rc=0
    restart_stack || restart_rc=$?
    if [ "$restart_rc" = "0" ]; then
        {
            echo
            echo "personal-mcp-host: full stack restarted and VERIFIED."
            echo "  MCP host:              127.0.0.1:${MCP_LOCAL_PORT} (accepting connections)"
            echo "  reverse SSH tunnel:    ${tunnel_forward_spec} -> ${REMOTE_USER}@${REMOTE_HOST} (up)"
            echo
            echo "Follow live traffic with:  tail -F $MCP_HOST_BASH_LOG"
        } >&2
    fi
    exit "$restart_rc"
fi

# -----------------------------------------------------------------------------
# --enable: bring the host MCP service up, then fall through to the
# green path (open the tunnel + tail the log).
#
# Launch mcp-host-bash DETACHED (its own session/process-group, see
# start_detached) and wait for it to bind the loopback port before
# opening the tunnel, so we never expose a tunnel to a server that isn't
# listening yet. Detaching it means a Ctrl-C / SIGTERM to this wrapper
# tears down ONLY the tunnel + tail — the service keeps running.
# -----------------------------------------------------------------------------

if [ "$ENABLE" = "1" ]; then
    start_detached "$MCP_HOST_BASH_BIN" --port "$MCP_LOCAL_PORT" </dev/null &
    mcp_pid=$!

    probe_rc=0
    wait_for_listener 127.0.0.1 "$MCP_LOCAL_PORT" 15 || probe_rc=$?
    case "$probe_rc" in
        0)
            echo "personal-mcp-host: mcp-host-bash-server listening on 127.0.0.1:$MCP_LOCAL_PORT" >&2
            ;;
        2)
            cat >&2 <<EOF
personal-mcp-host: FATAL: mcp-host-bash-server exited before binding 127.0.0.1:$MCP_LOCAL_PORT.
       Common causes:
         - $MCP_LOCAL_PORT already owned by a stale prior instance —
           lsof -nP -iTCP:$MCP_LOCAL_PORT -sTCP:LISTEN
         - bad operator config under
           ~/.config/claude-container/mcp-host-bash.env
       Check the server's stderr above for the underlying error.
EOF
            cleanup_exit_code=1
            cleanup
            ;;
        3)
            exit 1
            ;;
        *)
            cat >&2 <<EOF
personal-mcp-host: FATAL: mcp-host-bash-server did not bind 127.0.0.1:$MCP_LOCAL_PORT
       within 15s. The process is still running but has not opened the
       listen socket. Check
       $MCP_HOST_BASH_LOG for upstream stderr.
EOF
            cleanup_exit_code=1
            cleanup
            ;;
    esac

    # Service is up (we just brought it up). Open the tunnel + tail.
    run_tunnel_and_tail
fi

# -----------------------------------------------------------------------------
# Default mode: STATUS GATE.
#
# Probe the host MCP service (the thing listening on
# 127.0.0.1:$MCP_LOCAL_PORT). We do NOT launch it here — the default
# path assumes the operator keeps the server always-on and only wants
# the tunnel on-demand.
#
#   RED  (not accepting connections): print a clear error, print the
#        ready-to-copy --enable rerun command that brings the service
#        up, and exit non-zero (3).
#   GREEN (up): open the tunnel + tail the log.
# -----------------------------------------------------------------------------

if service_is_up 127.0.0.1 "$MCP_LOCAL_PORT"; then
    echo "personal-mcp-host: host MCP service is UP on 127.0.0.1:$MCP_LOCAL_PORT" >&2
    run_tunnel_and_tail
fi

# RED path. Build the rerun command, quoting the script path so a path
# with spaces still copy-pastes cleanly.
rerun_cmd=$(printf '%q --enable' "$0")
cat >&2 <<EOF
personal-mcp-host: host MCP service is NOT running.

       Nothing is accepting connections on 127.0.0.1:$MCP_LOCAL_PORT, so
       there is no MCP server for the reverse tunnel to forward to. The
       default mode does NOT start the server — it assumes you keep it
       always-on (e.g. the compose-stack LaunchAgent) and only opens the
       tunnel on-demand.

       To bring the host MCP service up AND open the tunnel in one shot,
       re-run this script with --enable:

           $rerun_cmd

       (Or start mcp-host-bash-server some other way, then re-run with no flags.)
EOF
exit 3
