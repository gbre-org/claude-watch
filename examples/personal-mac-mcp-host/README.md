# personal-mac MCP host — reverse-tunnelled remote access

On-demand MCP host server resident on an operator's MacBook, reachable
from a *remote* Claude Code instance (running on a different machine —
homelab server, workstation, …) via a reverse SSH tunnel initiated by
the MacBook.

The MacBook is the active party — it dials out to the remote when the
operator wants to grant access, and tears the tunnel down when they're
done. No inbound TCP port on the Mac, no relay server, no NAT
punch-through.

See [`DESIGN.md`](DESIGN.md) for the architectural rationale (why
reverse-SSH and not a fixed port / a relay).

## Pieces

```
examples/personal-mac-mcp-host/
├── DESIGN.md                                 # architecture + tradeoffs
├── README.md                                 # you are here
├── .env.example                              # committed; placeholder values
├── .gitignore                                # excludes .env
├── personal-mcp-host.sh                      # wrapper: spawns mcp-host-bash + ssh tunnel
├── launchd/
│   ├── org.gbre.personal-mcp.host.plist      # LaunchAgent template (RunAtLoad=false)
│   └── README.md                             # launchctl install walkthrough
└── tests/
    ├── personal-mcp-host.test                # bash wrapper argv tests
    └── launchd-plist.test                    # plist structural tests
```

## Topology

```
   MacBook (operator's laptop)
   ────────────────────────────────────────────────────────────────
       personal-mcp-host.sh
                │
                ├──► mcp-host-bash --port $MCP_LOCAL_PORT
                │       (examples/compose/bin/mcp-host-bash:
                │        mcp-proxy + cli-mcp-server + optional
                │        bearer-auth shim, bound to 127.0.0.1)
                │
                └──► ssh -N -R $REMOTE_PORT:127.0.0.1:$MCP_LOCAL_PORT \
                         $REMOTE_USER@$REMOTE_HOST
                                │
                                ▼  (TCP over SSH, outbound from Mac)
   ════════════════════════════════════════════════════════════════
                                │
   Remote host (your homelab / workstation)
   ────────────────────────────────────────────────────────────────
                                ▼
                       127.0.0.1:$REMOTE_PORT on the remote's loopback
                                ▲
                                │
                       remote Claude Code → MCP client →
                       http://localhost:$REMOTE_PORT/mcp
```

Everything between the MacBook and the remote is SSH-encrypted. The
remote-side bound port is on `127.0.0.1` by default (`ssh -R` does not
require the remote to set `GatewayPorts`), so other hosts on the
remote's LAN can't reach it.

## Quickstart

```sh
cd examples/personal-mac-mcp-host
cp .env.example .env
$EDITOR .env
# (fill in REMOTE_HOST, REMOTE_USER, REMOTE_PORT, MCP_LOCAL_PORT,
#  SSH_KEY_PATH, MCP_HOST_BASH_BEARER)

# One-time SSH key setup (recommended: dedicated key):
ssh-keygen -t ed25519 -f ~/.ssh/id_personal_mcp_tunnel -C "personal-mcp-tunnel@$(hostname)"
ssh-copy-id -i ~/.ssh/id_personal_mcp_tunnel.pub $REMOTE_USER@$REMOTE_HOST
# Then edit ~/.ssh/authorized_keys on the remote to restrict the key —
# see "SSH key hardening" below.

# Verify the bridge interactively:
./personal-mcp-host.sh
# Ctrl-C when you're satisfied. Then run launchd setup if you want
# on-demand auto-restart (see launchd/README.md).
```

## Reuse of the compose-stack launcher

`personal-mcp-host.sh` exec's `examples/compose/bin/mcp-host-bash` for
the MCP server itself. That launcher already implements:

- `mcp-proxy` + `cli-mcp-server` bootstrapping.
- Trust profile (`CW_PROFILE=corp-dev` default; `corp-dev-trusted`
  widens for file mutation / scheduling / outbound bytes).
- `ALLOWED_DIR` fence (default `$HOME`).
- Optional bearer-auth shim (`mcp-proxy-auth-shim`) when
  `MCP_HOST_BASH_BEARER` is set.
- Soft kill switch (`MCP_HOST_BASH_DISABLED=1`).

Operators who've already set up the compose stack are already
configured for this directory — you only need to add the tunnel
config (the keys in `.env.example`) on top of what's already in
`~/.config/claude-container/mcp-host-bash.env`.

If you haven't set up the compose stack: install the host-side
binaries once via:

```sh
examples/compose/bin/install-host-deps
```

That drops `mcp-proxy` and `cli-mcp-server` into `~/.local/bin/` (one
static install — subsequent launches are offline).

## Configuration

All operator-specific config lives in the sibling `.env` (gitignored).
`.env.example` is the committed template; copy it, fill in the
required keys, leave the optional ones alone unless you have a
reason. See the comments in `.env.example` for the full surface.

### Required keys

| Key | Description |
|---|---|
| `REMOTE_HOST` | Remote host the MacBook dials out to (DNS name or IP). |
| `REMOTE_USER` | Remote SSH user. |
| `REMOTE_PORT` | Port the reverse tunnel binds on the remote's loopback. |
| `MCP_LOCAL_PORT` | Port `mcp-host-bash` binds on the MacBook's loopback. |
| `SSH_KEY_PATH` | Private SSH key the tunnel uses. |

### Recommended optional keys

| Key | Default | Why set it |
|---|---|---|
| `MCP_HOST_BASH_BEARER` | empty | Defense-in-depth. The SSH tunnel encrypts the wire, but anyone else on the remote's loopback can also dial `localhost:$REMOTE_PORT` — the bearer is the layer that bounds access to callers with the secret. Generate with `head -c 32 /dev/urandom \| base64`. |
| `CW_PROFILE` | `corp-dev` | Conservative read-y floor. Widen to `corp-dev-trusted` only if your remote-side Claude needs to mutate files / fire webhooks / manage containers on the Mac. |
| `ALLOWED_DIR` | `$HOME` (in `mcp-host-bash`) | Narrow to e.g. `$HOME/personal-mcp-scratch` if you want to limit the blast radius. |

### Remote-side MCP client config (operator's private notes)

On the remote (the host running the remote Claude), add an entry to
the operator's MCP client config pointing at the local end of the
reverse tunnel:

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

(Substitute the actual `$REMOTE_PORT` and `$MCP_HOST_BASH_BEARER`
values from your `.env`.)

The `Authorization` header is required only when
`MCP_HOST_BASH_BEARER` is set. When unset, the auth shim is disabled
and the MCP server is reached straight through the tunnel — SSH
encryption is the only protection. **Setting the bearer is strongly
recommended** for the defense-in-depth reason above.

This client-config entry is operator-specific and lives in the
operator's private config — NOT in this public repo.

## SSH key hardening (on the remote)

The dedicated SSH key the tunnel uses should be restricted to
port-forward-only on the remote's `~/.ssh/authorized_keys`. After
running `ssh-copy-id`, edit the remote's `authorized_keys` and prepend
options to the line:

```
command="echo no-shell; sleep infinity",no-pty,no-agent-forwarding,no-X11-forwarding,no-user-rc,permitopen="127.0.0.1:18766" ssh-ed25519 AAAA... operator@macbook
```

(Replace `18766` with your actual `REMOTE_PORT`.)

What each option does:

- `command="echo no-shell; sleep infinity"` — replace the default
  login shell with a no-op. The `sleep infinity` keeps the SSH
  connection open so the port-forward stays alive; a stolen key
  cannot drop into a login shell or run arbitrary commands.
- `no-pty,no-agent-forwarding,no-X11-forwarding,no-user-rc` — strip
  side-channels.
- `permitopen="127.0.0.1:$REMOTE_PORT"` — restrict the
  reverse-forward target. The key cannot open forwards to any other
  host or port.

The remote's `sshd_config` defaults are fine — no `GatewayPorts yes`,
`AllowTcpForwarding` already on. No sshd-level change needed.

### Pre-populating `known_hosts` (defeats first-connect MITM)

`personal-mcp-host.sh` uses `StrictHostKeyChecking=accept-new`, which
pins the remote's host key the first time it connects. To defeat a
MITM at first-connect too, pre-populate `known_hosts` on the Mac
before bootstrapping the LaunchAgent — ideally fetched from a known-
good network:

```sh
ssh-keyscan -H $REMOTE_HOST >> ~/.ssh/known_hosts
```

If `$REMOTE_HOST` runs sshd on a non-default port, use
`ssh-keyscan -p $PORT -H $REMOTE_HOST`.

## Lifecycle

### Interactive (foreground)

```sh
cd examples/personal-mac-mcp-host
./personal-mcp-host.sh
# Bridge stays up until Ctrl-C, the tunnel breaks, or you reboot.
```

### On-demand (LaunchAgent, RunAtLoad=false)

Walks through copying the plist, replacing `/PATH/TO/REPO` /
`/PATH/TO/HOME`, bootstrapping the unit (registers but does not fire),
and the per-session `kickstart` / `bootout` cycle. See
[`launchd/README.md`](launchd/README.md).

In short:

```sh
# One-time install:
cp launchd/org.gbre.personal-mcp.host.plist ~/Library/LaunchAgents/
$EDITOR ~/Library/LaunchAgents/org.gbre.personal-mcp.host.plist
# (replace /PATH/TO/REPO and /PATH/TO/HOME)

launchctl bootstrap gui/$(id -u) \
    ~/Library/LaunchAgents/org.gbre.personal-mcp.host.plist
# Registers the unit. Doesn't fire it (RunAtLoad=false).

# Per-session: start
launchctl kickstart gui/$(id -u)/org.gbre.personal-mcp.host

# Per-session: stop
launchctl bootout gui/$(id -u)/org.gbre.personal-mcp.host

# OR: leave registered + soft-disable
echo 'PERSONAL_MCP_DISABLED="1"' >> .env
launchctl kickstart gui/$(id -u)/org.gbre.personal-mcp.host
# (wrapper exits 0 immediately; KeepAlive idles)
```

## Failure modes

| Failure | What you see | Diagnosis |
|---|---|---|
| `.env` missing | "missing .env at …" + exit 2 | `cp .env.example .env`, fill in. |
| Required `.env` key missing | shell error like `REMOTE_HOST not set in …` + exit | Edit `.env`, set the missing key. |
| SSH key wrong path | "SSH key not readable: …" + exit 1 | Verify `SSH_KEY_PATH` in `.env` points at an existing private key file. |
| SSH key revoked / not in `authorized_keys` | `Permission denied (publickey)` in stderr; respawn loop under launchd | Re-run `ssh-copy-id`; check the `command="…",permitopen="…" KEY` line on the remote. |
| `REMOTE_PORT` already bound on the remote | `remote port forwarding failed` in stderr | `ssh $REMOTE_USER@$REMOTE_HOST 'lsof -i :$REMOTE_PORT'`; clear the stale PID or pick a new port. |
| Remote unreachable | `Connection timed out` / `Could not resolve hostname` | Verify network + DNS. Try plain `ssh $REMOTE_USER@$REMOTE_HOST` from the Mac. |
| `MCP_LOCAL_PORT` already bound on the Mac | "mcp-host-bash exited before binding" | `lsof -nP -iTCP:$MCP_LOCAL_PORT -sTCP:LISTEN`; clear the stale PID, or pick a new port. |
| Mac sleeps / wifi drops | `ServerAliveInterval` fires; SSH exits within ~90s | Under launchd, `KeepAlive` respawns the wrapper when the network is back. Interactively, the wrapper itself exits and you re-run it. |
| `mcp-host-bash` deps missing | "cannot find required binaries on PATH" from `mcp-host-bash` | Run `examples/compose/bin/install-host-deps` once. |

For LaunchAgent-specific failures, see
[`launchd/README.md`](launchd/README.md) "Troubleshooting".

## Tests

Two embedded test scripts run on Linux CI:

```sh
make test-personal-mcp-host          # bash wrapper argv tests
make test-personal-mcp-host-plist    # plist structural tests
```

The first uses `personal-mcp-host.sh --print-cmd`, which builds the
planned `ssh` argv but prints it one-per-line instead of executing —
no `mcp-host-bash` / `ssh` invocation needed. The second parses the
plist via Python's stdlib `plistlib` and verifies the labels / paths
/ KeepAlive / RunAtLoad shapes.

Neither requires macOS; both run unchanged in GitHub Actions Linux
runners.

Real macOS verification (LaunchAgent bootstrap + actual SSH tunnel)
is the maintainer's local check before merging.

## See also

- [`DESIGN.md`](DESIGN.md) — architecture, alternatives considered.
- [`launchd/README.md`](launchd/README.md) — LaunchAgent install
  walkthrough.
- [`../compose/bin/mcp-host-bash`](../compose/bin/mcp-host-bash) —
  the MCP server launcher this wrapper exec's.
- [`../compose/launchd/README.md`](../compose/launchd/README.md) —
  persistent (always-on) LaunchAgent for the compose-stack shape.
