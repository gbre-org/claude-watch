# Persistent macOS auto-start for `mcp-host-bash-server`

`mcp-host-bash-server` (the single self-contained Rust binary built
from [`crates/mcp-host-bash-server`](../../../crates/mcp-host-bash-server)
— see the "`host-bash` — generic 'run a bash command on the host' MCP
server" section of [`examples/compose/README.md`](../README.md)) is a
foreground process. Run it by hand and it stays up until your terminal
exits, you log out, or the laptop reboots — at which point the
in-container `claude` loses its bridge into the host and any tool it
depends on (corp git, host CLIs, etc.) starts failing until you respawn
it.

This directory ships a macOS LaunchAgent that registers
`mcp-host-bash-server` with `launchd` so it starts automatically at
login, restarts if it dies, and survives reboots without manual
intervention.

The bearer token (`MCP_HOST_BASH_BEARER`) is configured in EITHER the
operator config file
(`~/.config/claude-container/mcp-host-bash.env`) OR the plist's
`EnvironmentVariables` block — there is no separate Keychain step. The
server validates `Authorization: Bearer <token>` in-process on every
request; leave it unset only for a loopback-only bind.

**Non-macOS operators** (Linux laptops / servers) use a systemd
`user@.service` instead of a LaunchAgent. `mcp-host-bash-server` itself
is cross-platform (it builds and runs on Linux); only this launchd
wrapper is macOS-specific. Set `MCP_HOST_BASH_BEARER` via a systemd
`Environment=` line, an `EnvironmentFile=` pointing at a 600-mode
`~/.config/claude-container/mcp-host-bash.env`, or your secrets manager.

LaunchAgent (NOT LaunchDaemon): the server runs as the operator's user.
Its `run_command` / `run_script` exec under the operator's `$HOME` /
`$PATH` / login keychain, and it binds a loopback port that Docker
Desktop's VM reaches via `host.docker.internal`. None of that needs
root, and a LaunchDaemon would invert the trust model (processes
spawned by the server would run as root).

## 0. Build + install the binary

From the repo root:

```sh
make install-mcp-host-bash-server
```

That compiles `crates/mcp-host-bash-server` and copies the release
binary to `~/bin/mcp-host-bash-server` (re-signed in place on macOS so
Gatekeeper doesn't SIGKILL it). No PyPI dependencies, no separate
installer. Re-run it any time to pick up a new build.

Verify it runs once interactively before wrapping it in `launchd`:

```sh
~/bin/mcp-host-bash-server --print-config   # prints the effective policy + bind, exits
```

### Configure the bearer (optional but recommended)

Generate a fresh secret if you don't have one:

```sh
head -c 32 /dev/urandom | base64
```

Set it in `~/.config/claude-container/mcp-host-bash.env` (the server
reads that file at startup and its values win over the process
environment):

```sh
MCP_HOST_BASH_BEARER=...your base64 secret...
```

...or as a `<string>` entry in the plist's `EnvironmentVariables` block
(step 2). Either way, the SAME secret must be set as
`CLAUDE_HOST_HOOK_BRIDGE_BEARER` in the compose `.env` so the
in-container hook bridge sends the matching header. Required for any
non-loopback bind.

## 1. Copy the plist into `~/Library/LaunchAgents/`

`launchd` only loads files directly under `~/Library/LaunchAgents/`.
It refuses to follow symlinks (and refuses files outside that tree).
So `cp`, not `ln -s`:

```sh
cp examples/compose/launchd/org.gbre.claude-watch.mcp-host-bash.plist \
   ~/Library/LaunchAgents/
```

The filename must match the plist's `Label` key
(`org.gbre.claude-watch.mcp-host-bash`) — `launchd` keys off the
filename for `bootstrap` / `bootout` / `print`.

## 2. Edit the absolute paths + EnvironmentVariables

`launchd` does NOT expand `~` or `${HOME}` in plist values — it uses
literal paths. Open the copy in your editor:

```sh
$EDITOR ~/Library/LaunchAgents/org.gbre.claude-watch.mcp-host-bash.plist
```

Search/replace:

- `/PATH/TO/HOME` → your home directory (e.g. `/Users/yourname`).
  Run `echo $HOME` if unsure. It appears in `ProgramArguments`
  (`/PATH/TO/HOME/bin/mcp-host-bash-server`), `PATH`,
  `WorkingDirectory`, and the two log paths.

Then tune the `EnvironmentVariables` dict to your needs. Every key is
optional; defaults match a fresh install:

| Key | Default in the template | When to change |
|---|---|---|
| `MCP_HOST_BASH_BIND` | `127.0.0.1` (loopback only) | `0.0.0.0` (or a specific interface IP) for Linux Docker bridge-net containers that reach the host via `host.docker.internal` — those callers can't dial host loopback. Pair with `MCP_HOST_BASH_BEARER` (below) when widening — `run_command` is a host-shell privilege escalator, anything reachable on the port can exec as the operator user. macOS Docker Desktop's `host.docker.internal` NAT routes loopback for the default setup, so the safe default works there. |
| `MCP_HOST_BASH_BEARER` | (not in template) | Shared-secret bearer token. Set it here as a `<string>` entry OR in `~/.config/claude-container/mcp-host-bash.env` (the `.env` value wins). The server validates `Authorization: Bearer <token>` in-process. Required for any non-loopback bind. Generate once with `head -c 32 /dev/urandom \| base64` and keep out of version control. Mirror the same value into `CLAUDE_HOST_HOOK_BRIDGE_BEARER` in the docker-compose `.env` file. |
| `CW_PROFILE` | `corp-dev` (read-y allow-list) | `corp-dev-trusted` to widen for host scheduling, file mutation, container management. See the server's `show_security_rules` output for the full surface. |
| `ALLOW_SHELL_OPERATORS` | `false` (block pipes / `&&` / redirects) | `true` only if a workflow specifically needs shell operators. Loosens the safety floor. |
| `SSL_CERT_FILE` | empty | Absolute path to your corporate CA bundle if `run_command` invocations of curl / git / pip have to validate a corp chain. |
| `CLAUDE_HOOK_BRIDGE_BINS` | empty | Comma-separated basenames of host hook binaries the in-container exec-hook bridge is allowed to invoke (e.g. `telemetry-hook,corp-trace-hook`). |
| `PATH` | `/PATH/TO/HOME/.local/bin:/usr/local/bin:/usr/bin:/bin` | Extend if your `run_command` workflows need binaries in `/opt/homebrew/bin`, `~/.cargo/bin`, etc. |

If you'd rather keep policy out of the plist entirely, leave the
defaults and put your full overrides in
`~/.config/claude-container/mcp-host-bash.env` instead — the server
reads that file at startup, and operator-supplied values there beat
the profile-derived defaults. The plist is the right place for things
that have to be set BEFORE the server exec's (most importantly
`PATH`); everything else can live in the operator config.

Pre-create the log directory once (launchd auto-creates the log files
but not their parent dir):

```sh
mkdir -p ~/Library/Logs
```

## 3. Bootstrap the LaunchAgent

```sh
launchctl bootstrap gui/$(id -u) \
    ~/Library/LaunchAgents/org.gbre.claude-watch.mcp-host-bash.plist
```

`gui/$(id -u)` is the per-user GUI domain — the right scope for a
LaunchAgent that needs the operator's login session (Docker Desktop,
keychain access, etc.). `bootstrap` registers the plist with launchd
AND fires it once because `RunAtLoad=true`.

If `bootstrap` returns nothing, it succeeded. If it errors, see
"Troubleshooting" below.

## 4. Verify it's running

```sh
launchctl print gui/$(id -u)/org.gbre.claude-watch.mcp-host-bash
```

Look for:

- `state = running` — the server is up.
- `last exit code = 0` — last clean shutdown (or never exited yet).
- `last exit reason: ...` — only present if a previous run died;
  triages crashloops.
- `program = /PATH/TO/HOME/bin/mcp-host-bash-server` — matches what
  you edited.

Then confirm the process actually owns the listen port:

```sh
lsof -nP -i :8766
```

You should see one row, `COMMAND=mcp-host-b` (the truncated binary
name), `USER=<your username>`, `NODE=TCP`, `NAME=*:8766 (LISTEN)`. If
nothing is listening, check the log files (step 6).

Inside the container, the in-container `claude` should now see
`host-bash: Connected` from `claude mcp list` (assuming
`CLAUDE_MCP_HTTP_BRIDGE` in your compose `.env` includes the
`host-bash=http://host.docker.internal:8766/mcp` entry — see the main
compose README for the wiring).

## 5. Pick up plist or env-var changes

`launchd` snapshots the plist contents at `bootstrap` time. Editing
the plist after that does NOT take effect until you re-bootstrap:

```sh
launchctl bootout gui/$(id -u)/org.gbre.claude-watch.mcp-host-bash
launchctl bootstrap gui/$(id -u) \
    ~/Library/LaunchAgents/org.gbre.claude-watch.mcp-host-bash.plist
```

Same dance for changes to `~/.config/claude-container/mcp-host-bash.env`
— the server only reads that file at process start, so a new
allow-list takes effect on the next (re)spawn. Ditto after a fresh
`make install-mcp-host-bash-server` swaps the binary — bounce the unit
so launchd exec's the new build.

If you only want to bounce the server WITHOUT touching the plist,
`launchctl kickstart -k gui/$(id -u)/org.gbre.claude-watch.mcp-host-bash`
sends SIGTERM and lets `KeepAlive` respawn it. Faster than the
bootout / bootstrap pair.

## 6. Logs

launchd captures the server's `stdout` / `stderr`:

- `~/Library/Logs/mcp-host-bash.out.log` (mostly empty — the server
  logs to stderr)
- `~/Library/Logs/mcp-host-bash.err.log` (the startup banner + every
  tracing line, including `run_command` / `run_script` invocations)

Tail either live with `tail -F <path>`.

## 7. Disable temporarily

```sh
launchctl bootout gui/$(id -u)/org.gbre.claude-watch.mcp-host-bash
```

`bootout` unregisters the LaunchAgent. The plist file under
`~/Library/LaunchAgents/` stays put, so a future `bootstrap` brings
it back without re-editing.

## 8. Permanently uninstall

```sh
launchctl bootout gui/$(id -u)/org.gbre.claude-watch.mcp-host-bash
rm ~/Library/LaunchAgents/org.gbre.claude-watch.mcp-host-bash.plist
```

Optionally remove the log files, operator config, and the binary:

```sh
rm -f ~/Library/Logs/mcp-host-bash.out.log
rm -f ~/Library/Logs/mcp-host-bash.err.log
rm -f ~/.config/claude-container/mcp-host-bash.env
rm -f ~/bin/mcp-host-bash-server
```

## Troubleshooting

### `launchctl bootstrap` exit codes

- **5** (`Input/output error`): the plist is malformed XML or
  references an invalid key. Validate with `plutil -lint <path>` —
  it points at the offending line.
- **22** (`Invalid argument`): something inside the plist is the
  wrong type (e.g. a string where launchd expects a boolean).
  `plutil -lint` again, plus check the template's type annotations
  (`<true/>`, `<integer>`, `<string>`).
- **37** (`Operation already in progress`): the LaunchAgent is
  already bootstrapped. Run `bootout` first, then `bootstrap`.
- **78** (`Function not implemented`): the domain target is wrong.
  `gui/$(id -u)` is the right one for a LaunchAgent on a logged-in
  user. `system/` would only work for a LaunchDaemon under
  `/Library/LaunchDaemons/`.
- **125** (`Domain does not support specified action`): usually
  means you tried `bootstrap gui/$(id -u)` from a non-GUI session
  (SSH without a graphical login). `ssh -Y` won't fix it; you need
  a real Console session, OR switch to `bootstrap user/$(id -u)`
  for a user-domain (no-GUI) LaunchAgent. The trade-off: the
  user-domain agent runs even when no one is logged in graphically,
  but doesn't get GUI access (Docker Desktop's daemon launches at
  GUI login on most setups, so the bridge can't reach a non-running
  Docker engine — usually moot).

### File permissions

`launchd` enforces:

- The plist file must be owned by the operator (`stat -f '%Su' <path>`).
- Mode `0644` or stricter (no world-writable). The default `cp`
  preserves your umask; `chmod 0644 ~/Library/LaunchAgents/<file>`
  if `bootstrap` complains.

### Env-var inheritance differs from your interactive shell

`launchd` starts each LaunchAgent with a near-empty environment. The
common surprises:

- **`PATH`** is `/usr/bin:/bin:/usr/sbin:/sbin` — no Homebrew, no
  `~/.local/bin`, no `~/.cargo/bin`. The plist template adds
  `${HOME}/.local/bin`; extend the list if your `run_command`
  workflows need others.
- **`HOME`** IS set (to `/Users/<you>`).
- **Keychain access** works in the GUI domain (`gui/$(id -u)`) but
  NOT in the user domain (`user/$(id -u)`). If a `run_command`
  invocation reads the login keychain (codesign, some corp CLIs),
  use the GUI domain.
- **No `~/.zshrc` / `~/.bash_profile` sourcing.** Anything those
  files set has to be declared in `EnvironmentVariables` or in
  `~/.config/claude-container/mcp-host-bash.env`.

When a `run_command` invocation works in your interactive shell but
fails under launchd, the diff is almost always one of these.

### "Couldn't load: ... Operation not permitted"

macOS's app-management protections (System Settings → Privacy &
Security → App Management / Full Disk Access) sometimes block
LaunchAgents that exec from a path outside your home directory. The
binary lives under `${HOME}/bin` (the template default), which sits
inside your home dir, so this is usually moot; if the error persists,
grant Terminal / your editor "Full Disk Access" so it can write the
LaunchAgent in the first place, or run
`log show --predicate 'subsystem == "com.apple.xpc.launchd"' --last 5m`
to get launchd's actual rejection reason.

### The server exits immediately with a bind error

`~/Library/Logs/mcp-host-bash.err.log` shows `cannot bind
127.0.0.1:8766`. A stale prior instance still owns the port:

```sh
lsof -nP -iTCP:8766 -sTCP:LISTEN
```

Kill the surviving PID (or pick a different port via
`MCP_HOST_BASH_BIND` / the `--port` arg) and re-bootstrap.
