# personal-mac-mcp-host — DESIGN

> **Status:** implementation landed in this PR — `personal-mcp-host.sh`,
> `.env.example`, `.gitignore`, `launchd/org.gbre.personal-mcp.host.plist`,
> `launchd/README.md`, `README.md`, and embedded test suites under `tests/`.
> See [`README.md`](README.md) for operator setup; this file is the
> architectural record (why reverse-SSH, alternatives considered).

## Goal

Stand up a **MacBook-resident MCP host server** that an operator's
*remote* Claude Code (running on a different machine — e.g. a homelab
server) can reach **on-demand**, via a reverse SSH tunnel initiated by
the MacBook. The MacBook is the active party — it dials out to the
remote when the operator wants to grant access, and tears the tunnel
down when they're done.

Concretely, the operator wants:

1. A `launchctl`-managed daemon they can `load` / `unload` from the
   MacBook to start/stop remote access.
2. The MCP server only listening on `127.0.0.1` on the MacBook — never
   directly exposed to the LAN or the internet.
3. A reverse SSH tunnel from the MacBook to the remote host that
   forwards `remote:$REMOTE_PORT → mac:127.0.0.1:$MCP_LOCAL_PORT`, so
   the remote Claude dials its own `localhost:$REMOTE_PORT` and
   reaches the MacBook's MCP server.
4. The MacBook side committable to a public repo (this directory),
   with operator-specific secrets in a `.gitignore`d `.env`.
5. Reuse of the existing `examples/compose/bin/mcp-host-bash` launcher
   (already shipped in this repo) for the MCP server itself —
   `cli-mcp-server` + `mcp-proxy` + bearer-token auth shim. No new MCP
   server implementation needed.

## Why this is *not* the workbot pattern

The existing `examples/compose/launchd/org.gbre.claude-watch.mcp-host-bash.plist`
LaunchAgent solves the *local-loopback* case: an in-container Claude
on the **same** Mac (Docker Desktop) reaches the host's MCP server
via `host.docker.internal` NAT. No cross-machine networking — the
trust boundary is the Docker VM, which is the operator's own.

The new case adds a **cross-machine** hop. Two ways to bridge that:

| Option | How | Why we picked / didn't |
|---|---|---|
| **A. Remote dials Mac directly (forward port)** | MacBook opens an inbound TCP port; remote dials it. | Requires the MacBook to be reachable from the remote — NAT punch-through, dynamic DNS, opening a router port. Awkward for "I run it from a coffee shop sometimes". |
| **B. Remote dials a relay server; Mac also connects to relay (tailscale / cloudflared / ngrok)** | Both peers connect outbound to a third party. | Adds a third-party trust boundary + a new service to operate. Probably overkill. |
| **C. Mac dials remote, opens reverse tunnel (`ssh -R`)** | MacBook initiates an outbound SSH connection to the remote; the SSH protocol forwards a remote port back to the MacBook's loopback. | Mac is the active dial-out party. Remote only needs to accept SSH (which it already does for the operator). No new daemons, no exposed inbound ports on the MacBook. **This is what we use.** |

Option C also matches the operator's "only run as needed" requirement —
the MacBook controls the tunnel lifecycle (`launchctl load` to open it,
`launchctl unload` to tear it down).

## Topology

```
   MacBook (operator's laptop, e.g. behind NAT / coffee shop wifi)
   ────────────────────────────────────────────────────────────────
       launchctl start org.gbre.personal-mcp.tunnel
                │
                ▼
       mcp-host-bash --port $MCP_LOCAL_PORT
                │
                ▼
       127.0.0.1:$MCP_LOCAL_PORT  ◄────────┐
                                            │ (mcp-proxy HTTP, local)
       ssh -N -R $REMOTE_PORT:127.0.0.1:$MCP_LOCAL_PORT \
           -o ExitOnForwardFailure=yes \
           -o ServerAliveInterval=30 \
           -o ServerAliveCountMax=3 \
           -i $SSH_KEY_PATH \
           $REMOTE_USER@$REMOTE_HOST
                │
                ▼  (TCP over SSH, outbound from Mac)
   ════════════════════════════════════════════════════════════════
                │
   Remote host (e.g. homelab server with the operator's primary Claude)
   ────────────────────────────────────────────────────────────────
       sshd accepts the reverse-forward request
                │
                ▼
       127.0.0.1:$REMOTE_PORT  (bound on the remote)
                ▲
                │
       remote Claude's MCP client dials 127.0.0.1:$REMOTE_PORT
       (configured in its own client config — generic
       "http://localhost:$REMOTE_PORT/mcp" entry)
```

Everything between the MacBook and the remote is encrypted by SSH. The
remote-side bound port is **on the remote's loopback only** (not its
LAN), because the `ssh -R` default is to bind to `127.0.0.1` on the
remote — the operator does NOT need to set `GatewayPorts` on the
remote's sshd.

## Repo layout (proposed — implementation PR)

```
examples/personal-mac-mcp-host/
├── DESIGN.md                                 # this file
├── README.md                                 # operator install walkthrough
├── .env.example                              # committed; placeholder values
├── .gitignore                                # excludes .env
├── personal-mcp-host.sh                      # wrapper: starts mcp-host-bash + ssh tunnel
├── tunnel.sh                                 # standalone ssh tunnel wrapper (called by personal-mcp-host.sh)
└── launchd/
    ├── org.gbre.personal-mcp.host.plist      # runs personal-mcp-host.sh
    └── README.md                             # launchctl install walkthrough
```

**Naming:** `org.gbre.personal-mcp.*` per the
[claude-watch CLAUDE.md naming convention](../../CLAUDE.md) for
operator-owned launchd Labels and plist filenames.

**Reuse:** the wrapper *exec's* the existing
`examples/compose/bin/mcp-host-bash` launcher — we don't duplicate the
MCP-server bootstrapping. Operators who've already run
`examples/compose/bin/install-host-deps` (puts `mcp-proxy` and
`cli-mcp-server` into `~/.local/bin/`) are already set up for the
server side; this directory only adds the tunnel.

## `.env.example` (sketch)

```sh
# personal-mac-mcp-host/.env.example
#
# Copy to .env (gitignored), fill in your own values, then load the
# LaunchAgent. See README.md for the full walkthrough.
#
# Required:

# Remote host the MacBook dials out to (the host that runs your
# *other* Claude). DNS name or IP. Must accept SSH from this Mac.
REMOTE_HOST="your-server.example.com"

# Remote SSH user. Must have an entry in ~/.ssh/authorized_keys on
# REMOTE_HOST for SSH_KEY_PATH below.
REMOTE_USER="yourname"

# Remote port the tunnel binds on REMOTE_HOST. Your remote-side Claude
# reaches the MacBook's MCP server by dialing localhost:$REMOTE_PORT on
# REMOTE_HOST. Pick something unused (default below is "high, unlikely
# to collide with anything").
REMOTE_PORT="18766"

# Local port the MCP server binds on the MacBook. The reverse tunnel
# forwards REMOTE_PORT → this. Default 8766 matches mcp-host-bash's
# default; change if 8766 is already taken on the Mac (e.g. because
# the compose-stack launcher is also running).
MCP_LOCAL_PORT="8766"

# SSH key the tunnel uses. Recommend a DEDICATED key (not your daily
# id_ed25519) so the remote can restrict it to port-forward-only.
# See README.md "SSH key hardening" for the authorized_keys options
# entry.
SSH_KEY_PATH="$HOME/.ssh/id_personal_mcp_tunnel"

# Optional:

# Bearer token for the MCP server's auth shim. Same value must be
# configured on the remote side's MCP client. Generate with:
#   head -c 32 /dev/urandom | base64
# Strongly recommended even though the tunnel is SSH-encrypted — a
# defense-in-depth layer in case the remote's loopback is reachable
# by other local processes on the remote.
MCP_HOST_BASH_BEARER=""

# Trust profile for mcp-host-bash. Default "corp-dev" (read-y floor).
# Use "corp-dev-trusted" to allow file mutation / scheduling / curl /
# etc. See examples/compose/bin/mcp-host-bash header for the surface.
CW_PROFILE="corp-dev"
```

## Wrapper script `personal-mcp-host.sh` (sketch)

```sh
#!/usr/bin/env bash
# personal-mcp-host.sh — boot the MCP server AND the reverse tunnel.
#
# Sourced .env vars: REMOTE_HOST, REMOTE_USER, REMOTE_PORT,
# MCP_LOCAL_PORT, SSH_KEY_PATH, MCP_HOST_BASH_BEARER, CW_PROFILE.
#
# Lifecycle:
#   1. Source .env (sibling file). Refuse if missing.
#   2. Start mcp-host-bash --port $MCP_LOCAL_PORT in the background;
#      capture pid.
#   3. Wait for 127.0.0.1:$MCP_LOCAL_PORT to be in LISTEN (reuse the
#      same probe pattern as examples/compose/bin/mcp-host-bash's
#      wait_for_listener).
#   4. Start ssh -N -R ... in the background; capture pid.
#   5. Trap SIGTERM / SIGINT: kill both pids, exit.
#   6. Poll both pids; if either dies, kill the other and exit non-zero
#      so launchd's KeepAlive can respawn.

set -euo pipefail

script_dir="$(cd "$(dirname "$0")" && pwd)"
env_file="${script_dir}/.env"

if [ ! -f "$env_file" ]; then
    echo "personal-mcp-host: missing .env at $env_file" >&2
    echo "personal-mcp-host: copy .env.example to .env and fill in values" >&2
    exit 2
fi

# shellcheck disable=SC1090
. "$env_file"

# Validate required keys.
: "${REMOTE_HOST:?REMOTE_HOST not set in .env}"
: "${REMOTE_USER:?REMOTE_USER not set in .env}"
: "${REMOTE_PORT:?REMOTE_PORT not set in .env}"
: "${MCP_LOCAL_PORT:?MCP_LOCAL_PORT not set in .env}"
: "${SSH_KEY_PATH:?SSH_KEY_PATH not set in .env}"

# Find mcp-host-bash. Default: sibling repo path.
MCP_HOST_BASH_BIN="${MCP_HOST_BASH_BIN:-${script_dir}/../compose/bin/mcp-host-bash}"
if [ ! -x "$MCP_HOST_BASH_BIN" ]; then
    echo "personal-mcp-host: mcp-host-bash not found / not executable: $MCP_HOST_BASH_BIN" >&2
    echo "personal-mcp-host: set MCP_HOST_BASH_BIN in .env to override" >&2
    exit 1
fi

# Export to the mcp-host-bash child.
export MCP_HOST_BASH_BIND="127.0.0.1"
export MCP_HOST_BASH_BEARER="${MCP_HOST_BASH_BEARER:-}"
export CW_PROFILE="${CW_PROFILE:-corp-dev}"

mcp_pid=""
ssh_pid=""

cleanup() {
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
    exit "${cleanup_exit_code:-0}"
}
trap cleanup TERM INT

# Start MCP server.
"$MCP_HOST_BASH_BIN" --port "$MCP_LOCAL_PORT" &
mcp_pid=$!

# Wait for the MCP server's loopback port to bind before opening the
# tunnel — same probe pattern as examples/compose/bin/mcp-host-bash
# wait_for_listener (3-state Python TCP connect).
deadline=$(( $(date +%s) + 10 ))
while [ "$(date +%s)" -lt "$deadline" ]; do
    if ! kill -0 "$mcp_pid" 2>/dev/null; then
        echo "personal-mcp-host: mcp-host-bash exited before binding" >&2
        cleanup_exit_code=1
        cleanup
    fi
    if python3 -c "
import socket, sys
s = socket.socket()
s.settimeout(0.3)
try:
    s.connect(('127.0.0.1', $MCP_LOCAL_PORT))
    s.close()
except OSError:
    sys.exit(1)
" 2>/dev/null; then
        break
    fi
    sleep 0.2
done

# Start reverse SSH tunnel.
ssh -N \
    -R "$REMOTE_PORT:127.0.0.1:$MCP_LOCAL_PORT" \
    -o ExitOnForwardFailure=yes \
    -o ServerAliveInterval=30 \
    -o ServerAliveCountMax=3 \
    -o BatchMode=yes \
    -o StrictHostKeyChecking=accept-new \
    -i "$SSH_KEY_PATH" \
    "$REMOTE_USER@$REMOTE_HOST" &
ssh_pid=$!

# Poll-wait on BOTH children. If either dies, take the other down
# and exit non-zero so launchd's KeepAlive respawns the whole thing.
while kill -0 "$mcp_pid" 2>/dev/null && kill -0 "$ssh_pid" 2>/dev/null; do
    sleep 1
done

cleanup_exit_code=1
cleanup
```

Key behaviors:

- `BatchMode=yes` — fail rather than prompt for password (the dedicated
  key MUST work non-interactively).
- `StrictHostKeyChecking=accept-new` — pin the remote's host key on
  first connect; refuse if the key changes.
- `ExitOnForwardFailure=yes` — if the remote port can't be bound
  (already in use, sshd config rejects it, key revoked), `ssh -N`
  exits immediately rather than sitting in a useless connected state.
  Combined with the launchd `KeepAlive` policy, the operator sees a
  restart loop in `~/Library/Logs/personal-mcp-host.err.log` instead
  of silent "looks fine but isn't working".
- `ServerAliveInterval=30 ServerAliveCountMax=3` — detect a dead
  remote within ~90s and exit so launchd respawns.

## launchd plist `org.gbre.personal-mcp.host.plist` (sketch)

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
                       "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>org.gbre.personal-mcp.host</string>

    <!-- ProgramArguments: absolute path to personal-mcp-host.sh.
         Replace /PATH/TO/REPO. launchd does NOT expand `~` or
         `${HOME}` in plist values. -->
    <key>ProgramArguments</key>
    <array>
        <string>/PATH/TO/REPO/examples/personal-mac-mcp-host/personal-mcp-host.sh</string>
    </array>

    <!-- EnvironmentVariables: minimal. PATH is the only thing the
         wrapper itself needs; everything else comes from the sibling
         .env file. Replace /PATH/TO/HOME. -->
    <key>EnvironmentVariables</key>
    <dict>
        <key>PATH</key>
        <string>/PATH/TO/HOME/.local/bin:/usr/local/bin:/usr/bin:/bin</string>
    </dict>

    <!-- RunAtLoad=false: operator runs this "only as needed" — the
         plist registers the unit but does NOT fire it at login.
         Operator triggers manually via:
             launchctl kickstart gui/$(id -u)/org.gbre.personal-mcp.host
         or
             launchctl start org.gbre.personal-mcp.host
         (after `launchctl bootstrap` once). -->
    <key>RunAtLoad</key>
    <false/>

    <!-- KeepAlive=true: while the unit IS running, relaunch if it
         crashes / loses the tunnel. Combined with RunAtLoad=false,
         the operator's `launchctl start` brings it up, and it stays
         up until they `launchctl stop` / `launchctl bootout`. -->
    <key>KeepAlive</key>
    <true/>

    <key>ThrottleInterval</key>
    <integer>30</integer>

    <key>WorkingDirectory</key>
    <string>/PATH/TO/REPO/examples/personal-mac-mcp-host</string>

    <key>StandardOutPath</key>
    <string>/PATH/TO/HOME/Library/Logs/personal-mcp-host.out.log</string>

    <key>StandardErrorPath</key>
    <string>/PATH/TO/HOME/Library/Logs/personal-mcp-host.err.log</string>

    <key>ProcessType</key>
    <string>Interactive</string>
</dict>
</plist>
```

## Remote-side MCP client config (operator's private notes)

The remote-side Claude needs an entry in its MCP client config pointing
at the local end of the reverse tunnel. The exact location depends on
the operator's setup (claude-watch container, Claude Code on the
remote shell, etc.). Generic shape:

```json
{
  "mcpServers": {
    "personal-mac": {
      "type": "http",
      "url": "http://localhost:$REMOTE_PORT/mcp",
      "headers": {
        "Authorization": "Bearer $MCP_HOST_BASH_BEARER"
      }
    }
  }
}
```

The bearer header is only needed when `MCP_HOST_BASH_BEARER` is set in
the MacBook's `.env`. When unset, the auth shim is disabled — the
MCP server is reached straight through the tunnel and the SSH
encryption is the only protection. **Setting the bearer is strongly
recommended** even for a tunneled deployment, because it gates access
in case anything else on the remote's loopback (other users, other
processes) can reach `localhost:$REMOTE_PORT`.

This entry is **operator-specific** and lives in the operator's
private config — NOT in this public repo.

## SSH key hardening (operator's remote-side configuration)

The dedicated SSH key the tunnel uses should be restricted to
port-forward-only on the remote's `~/.ssh/authorized_keys`. Append the
public half with these options:

```
command="echo no-shell; sleep infinity",no-pty,no-agent-forwarding,no-X11-forwarding,no-user-rc,permitopen="127.0.0.1:$REMOTE_PORT" ssh-ed25519 AAAA... operator@macbook
```

- `command="echo no-shell; sleep infinity"` — replace the user's
  default shell with a no-op so a stolen key can't drop into a
  login shell. The `sleep infinity` keeps the SSH connection open so
  the port-forward stays alive.
- `no-pty,no-agent-forwarding,no-X11-forwarding,no-user-rc` — strip
  side-channels.
- `permitopen="127.0.0.1:$REMOTE_PORT"` — restrict the reverse-forward
  target. The key cannot open forwards to anything else.

Belt-and-suspenders: the remote's `sshd_config` already enforces
`AllowTcpForwarding yes` (the default) and `GatewayPorts no` (the
default — binds reverse-forwards to the remote's loopback only). No
sshd-level change needed if the operator's existing config is stock.

## Failure modes

| Failure | What the operator sees | Diagnosis |
|---|---|---|
| `.env` missing | launchd respawn loop; `personal-mcp-host: missing .env` in stderr log | `cp .env.example .env`, fill in. |
| SSH key wrong / revoked | Respawn loop; ssh `Permission denied (publickey)` in stderr log | Check `authorized_keys` on remote; verify key path in `.env`. |
| Remote port already bound | Respawn loop; ssh `remote port forwarding failed` in stderr log | `ssh remote 'lsof -i :$REMOTE_PORT'`; pick a different `REMOTE_PORT` in `.env`. |
| Remote DNS / network down | Respawn loop; ssh connection-timeout in stderr log | Verify `REMOTE_HOST` reachability with a plain `ssh` test. |
| Local `mcp-host-bash` deps missing | Respawn loop; "cannot find required binaries on PATH" in stderr log | Run `../compose/bin/install-host-deps` once. |
| MacBook sleeps / wifi drops | `ServerAliveInterval` triggers SSH exit within ~90s; launchd respawns | Tunnel reconnects when network is back. No operator action needed. |

`KeepAlive=true` + `ThrottleInterval=30` means any failure surfaces as
a respawn-every-30-seconds loop, with stderr captured to
`~/Library/Logs/personal-mcp-host.err.log`. The operator's diagnostic
loop is always "tail the err log, look at the most recent stderr".

## Lifecycle (operator)

```sh
# One-time install:
cp examples/personal-mac-mcp-host/launchd/org.gbre.personal-mcp.host.plist \
   ~/Library/LaunchAgents/
$EDITOR ~/Library/LaunchAgents/org.gbre.personal-mcp.host.plist
# (replace /PATH/TO/REPO + /PATH/TO/HOME with absolute paths)

cp examples/personal-mac-mcp-host/.env.example \
   examples/personal-mac-mcp-host/.env
$EDITOR examples/personal-mac-mcp-host/.env
# (fill in REMOTE_HOST, REMOTE_USER, etc.)

ssh-keygen -t ed25519 -f ~/.ssh/id_personal_mcp_tunnel -C "personal-mcp-tunnel@$(hostname)"
ssh-copy-id -i ~/.ssh/id_personal_mcp_tunnel.pub $REMOTE_USER@$REMOTE_HOST
# Then edit ~/.ssh/authorized_keys on the remote to add the restrictions
# (see "SSH key hardening" above).

launchctl bootstrap gui/$(id -u) \
    ~/Library/LaunchAgents/org.gbre.personal-mcp.host.plist
# Registers the unit. Doesn't fire it (RunAtLoad=false).

# Per-session start:
launchctl kickstart gui/$(id -u)/org.gbre.personal-mcp.host

# Verify:
launchctl print gui/$(id -u)/org.gbre.personal-mcp.host
ssh $REMOTE_USER@$REMOTE_HOST "lsof -i :$REMOTE_PORT"   # should show ssh listening
ssh $REMOTE_USER@$REMOTE_HOST "curl -s http://localhost:$REMOTE_PORT/mcp" \
    # should NOT 404; an MCP server response means the tunnel works

# Per-session stop:
launchctl kill TERM gui/$(id -u)/org.gbre.personal-mcp.host
# launchd's KeepAlive will *not* respawn after a kill within the
# ThrottleInterval... actually yes it will. To fully stop:
launchctl bootout gui/$(id -u)/org.gbre.personal-mcp.host

# To re-enable later:
launchctl bootstrap gui/$(id -u) \
    ~/Library/LaunchAgents/org.gbre.personal-mcp.host.plist
launchctl kickstart gui/$(id -u)/org.gbre.personal-mcp.host
```

## Design decisions resolved in this PR

The questions surfaced in the original design doc are answered below,
with pointers to where the choice landed in the shipped code.

1. **Reuse vs. duplicate `mcp-host-bash`** — **Reuse**.
   `personal-mcp-host.sh` exec's `../compose/bin/mcp-host-bash`
   (configurable via `MCP_HOST_BASH_BIN` for operators whose launcher
   lives elsewhere). Avoids duplicating the cw-profile / allow-list /
   bearer-shim surface; operators who've already set up the compose
   stack are pre-configured for this directory.

2. **`launchctl bootout` vs. soft kill switch** — **Both supported**.
   `RunAtLoad=false` in the plist means the unit registers without
   firing; operators run `launchctl kickstart` to start a session and
   `launchctl bootout` to stop. For "leave the unit registered but
   don't actually run the bridge", `PERSONAL_MCP_DISABLED=1` in
   `.env` makes the wrapper exit 0 immediately on every spawn.
   Documented side-by-side in `launchd/README.md` §5.

3. **Bearer token storage** — **`.env` plaintext (phase 1)**. The
   shipped code reads `MCP_HOST_BASH_BEARER` from the sibling `.env`
   and exports it for `mcp-host-bash` (which already implements the
   shim). The Keychain hop modelled on `load-bearer-from-keychain` is
   deliberately out of scope for this PR — left as future work; a
   later PR can drop in a Keychain wrapper without changing the
   wrapper's contract (it reads the env var regardless of how it got
   there).

4. **`tunnel.sh` separate from `personal-mcp-host.sh`** — **No**.
   Single combined wrapper (YAGNI; the design doc anticipated this).
   Splitting buys nothing for v1; future-work bullet below.

5. **Host key pinning** — **`StrictHostKeyChecking=accept-new` plus
   documented `ssh-keyscan` recipe**. The wrapper ships
   `accept-new` as the floor (pin on first connect). The main README
   "Pre-populating `known_hosts`" section walks operators through the
   `ssh-keyscan -H $REMOTE_HOST` pre-population path for those who
   want to defeat first-connect MITM too.

6. **`CW_PROFILE` default** — **`corp-dev` (conservative)**.
   `.env.example` ships `CW_PROFILE="corp-dev"` with an inline note
   that operators with broader needs (file mutation, scheduling,
   outbound bytes) can flip to `corp-dev-trusted`. Conservative floor,
   easy to widen.

7. **`ALLOWED_DIR` default** — **`$HOME` (inherits from
   `mcp-host-bash`)**. `.env.example` includes a commented-out
   `ALLOWED_DIR` line with the narrow-blast-radius
   (`$HOME/personal-mcp-scratch`) suggestion for operators who want
   it; the default leaves `mcp-host-bash`'s `$HOME` floor in place.

8. **Implementation PR sequence** — **collapsed into this PR**.
   Wrapper + `.env.example` + `.gitignore` + plist + READMEs +
   embedded test suites all land together. Tests run on Linux CI
   without needing macOS (`--print-cmd` debug hook for the wrapper,
   `plistlib` for the plist); real macOS verification is the
   maintainer's local check before merging.

## Future work

- **macOS Keychain bearer hop.** A `load-bearer-from-keychain`-style
  wrapper for `personal-mcp-host.sh` would let operators keep the
  bearer out of `.env` entirely. Shape: a small `bin/` script that
  fetches the bearer via `security find-generic-password -w`,
  exports it, then exec's `personal-mcp-host.sh`. The compose-stack
  Keychain wrapper at `../compose/bin/load-bearer-from-keychain` is
  the model. Out of scope for this PR.

- **Split `tunnel.sh` from `personal-mcp-host.sh`.** Would let
  advanced operators run only the tunnel (pointing at a different
  host-side MCP server) or only the MCP server (without remote
  access). Deferred — single use case for v1.

- **Per-session ephemeral REMOTE_PORT.** Today the operator picks a
  fixed `REMOTE_PORT` in `.env`. An ephemeral-port mode would have
  `personal-mcp-host.sh` ask sshd for `0` (let sshd pick), then emit
  the resolved port (e.g. via `claude-event`) for the remote-side
  Claude to pick up. Lower collision risk on shared remotes; more
  client-config plumbing. Deferred.
