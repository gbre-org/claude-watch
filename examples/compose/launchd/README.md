# Persistent macOS auto-start for `mcp-host-bash`

The host-side `mcp-host-bash` launcher (see
[`examples/compose/bin/mcp-host-bash`](../bin/mcp-host-bash) and the
"`mcp-host-bash` — generic 'run a bash command on the host' MCP server"
section of [`examples/compose/README.md`](../README.md)) is a foreground
process. Run it by hand and it stays up until your terminal exits, you
log out, or the laptop reboots — at which point the in-container
`claude` loses its bridge into the host and any tool it depends on
(corp git, host CLIs, etc.) starts failing until you respawn the
launcher.

This directory ships a macOS LaunchAgent that registers
`mcp-host-bash` with `launchd` so it starts automatically at login,
restarts if it dies, and survives reboots without manual intervention.

LaunchAgent (NOT LaunchDaemon): the launcher runs as the operator's
user. It dials `cli-mcp-server`, which exec's commands under the
operator's `$HOME` / `$PATH` / login keychain, and it binds a loopback
port that Docker Desktop's VM reaches via `host.docker.internal`. None
of that needs root, and a LaunchDaemon would invert the trust model
(processes spawned by `mcp-host-bash` would run as root).

## 0. Prereqs

- macOS (this is a `launchd` plist; for Linux see systemd user units,
  not covered here).
- The compose stack itself works (you've gotten through
  `examples/compose/README.md` at least once).
- `mcp-proxy` and `cli-mcp-server` are statically installed via the
  bundled installer:

  ```sh
  examples/compose/bin/install-host-deps
  ```

  Both binaries land in `~/.local/bin/`. The plist's `PATH`
  environment variable below adds that to the LaunchAgent's `PATH`
  search list because `launchd` does NOT inherit your interactive
  shell's `PATH`.

- An interactive run of `mcp-host-bash` succeeded once (so you know
  the launcher itself works on this host before you wrap it in
  `launchd`):

  ```sh
  examples/compose/bin/mcp-host-bash
  # Ctrl-C after you see the "starting" banner.
  ```

## 1. Copy the plist into `~/Library/LaunchAgents/`

`launchd` only loads files directly under `~/Library/LaunchAgents/`.
It refuses to follow symlinks (and refuses files outside that tree).
So `cp`, not `ln -s`:

```sh
cp examples/compose/launchd/com.anthropic.claude-watch.mcp-host-bash.plist \
   ~/Library/LaunchAgents/
```

The filename must match the plist's `Label` key
(`com.anthropic.claude-watch.mcp-host-bash`) — `launchd` keys off the
filename for `bootstrap` / `bootout` / `print`.

## 2. Edit the absolute paths + EnvironmentVariables

`launchd` does NOT expand `~` or `${HOME}` in plist values — it uses
literal paths. Open the copy in your editor:

```sh
$EDITOR ~/Library/LaunchAgents/com.anthropic.claude-watch.mcp-host-bash.plist
```

Search/replace:

- `/PATH/TO/REPO` → absolute path to your local `claude-watch`
  checkout (e.g. `/Users/yourname/code/claude-watch`).
- `/PATH/TO/HOME` → your home directory (e.g. `/Users/yourname`).
  Run `echo $HOME` if unsure.

Then tune the `EnvironmentVariables` dict to your needs. Every key is
optional; defaults match a fresh install:

| Key | Default in the template | When to change |
|---|---|---|
| `MCP_HOST_BASH_BIND` | `127.0.0.1` (loopback only) | `0.0.0.0` (or a specific interface IP) for Linux Docker bridge-net containers that reach the host via `host.docker.internal` — those callers can't dial host loopback. Pair with bearer-token auth (see [`examples/compose/.env.example`](../.env.example) host-bash block) when widening — `run_command` is a host-shell privilege escalator, anything reachable on the port can exec as the operator user. macOS Docker Desktop's `host.docker.internal` NAT routes loopback for the default setup, so the safe default works there. |
| `CW_PROFILE` | `corp-dev` (read-y allow-list) | `corp-dev-trusted` to widen for host scheduling, file mutation, container management. See the launcher's script header for the full surface. |
| `ALLOW_SHELL_OPERATORS` | `false` (block pipes / `&&` / redirects) | `true` only if a workflow specifically needs shell operators. Loosens the safety floor. |
| `SSL_CERT_FILE` | empty | Absolute path to your corporate CA bundle if `run_command` invocations of curl / git / pip have to validate a corp chain. |
| `CLAUDE_HOOK_BRIDGE_BINS` | empty | Comma-separated basenames of host hook binaries the in-container exec-hook bridge is allowed to invoke (e.g. `telemetry-hook,corp-trace-hook`). |
| `PATH` | `/PATH/TO/HOME/.local/bin:/usr/local/bin:/usr/bin:/bin` | Extend if `mcp-proxy` / `cli-mcp-server` live elsewhere, or if your `run_command` workflows need binaries in `/opt/homebrew/bin`, `~/.cargo/bin`, etc. |

If you'd rather keep policy out of the plist entirely, leave the
defaults and put your full overrides in
`~/.config/claude-container/mcp-host-bash.env` instead — the launcher
sources that file at startup, and operator-supplied values there beat
the profile-derived defaults. The plist is the right place for things
that have to be set BEFORE the launcher exec's (most importantly
`PATH`); everything else can live in the operator config.

Pre-create the log directory once (launchd auto-creates the log files
but not their parent dir):

```sh
mkdir -p ~/Library/Logs
```

## 3. Bootstrap the LaunchAgent

```sh
launchctl bootstrap gui/$(id -u) \
    ~/Library/LaunchAgents/com.anthropic.claude-watch.mcp-host-bash.plist
```

`gui/$(id -u)` is the per-user GUI domain — the right scope for a
LaunchAgent that needs the operator's login session (Docker Desktop,
keychain access, etc.). `bootstrap` registers the plist with launchd
AND fires it once because `RunAtLoad=true`.

If `bootstrap` returns nothing, it succeeded. If it errors, see
"Troubleshooting" below.

## 4. Verify it's running

```sh
launchctl print gui/$(id -u)/com.anthropic.claude-watch.mcp-host-bash
```

Look for:

- `state = running` — the launcher is up.
- `last exit code = 0` — last clean shutdown (or never exited yet).
- `last exit reason: ...` — only present if a previous run died;
  triages crashloops.
- `program = /PATH/TO/REPO/examples/compose/bin/mcp-host-bash` —
  matches what you edited.

Then confirm the process actually owns the listen port:

```sh
lsof -nP -i :8766
```

You should see one row, `COMMAND=mcp-proxy` (the static binary the
launcher exec's), `USER=<your username>`, `NODE=TCP`,
`NAME=*:8766 (LISTEN)`. If nothing is listening, check the launcher's
log files (step 6).

Inside the container, the in-container `claude` should now see
`host-bash: Connected` from `claude mcp list` (assuming
`CLAUDE_MCP_HTTP_BRIDGE` in your compose `.env` includes the
`host-bash=http://host.docker.internal:8766/mcp` entry — see the main
compose README for the wiring).

## 5. Pick up plist or env-var changes

`launchd` snapshots the plist contents at `bootstrap` time. Editing
the plist after that does NOT take effect until you re-bootstrap:

```sh
launchctl bootout gui/$(id -u)/com.anthropic.claude-watch.mcp-host-bash
launchctl bootstrap gui/$(id -u) \
    ~/Library/LaunchAgents/com.anthropic.claude-watch.mcp-host-bash.plist
```

Same dance for changes to `~/.config/claude-container/mcp-host-bash.env`
— the launcher only sources that file at process start, so a new
allow-list takes effect on the next launcher (re)spawn.

If you only want to bounce the launcher WITHOUT touching the plist,
`launchctl kickstart -k gui/$(id -u)/com.anthropic.claude-watch.mcp-host-bash`
sends SIGTERM and lets `KeepAlive` respawn it. Faster than the
bootout / bootstrap pair.

## 6. Logs

The launcher writes to two places by default:

- launchd-captured `stdout` / `stderr`:
  - `~/Library/Logs/mcp-host-bash.out.log` (mostly empty — the
    launcher logs to stderr)
  - `~/Library/Logs/mcp-host-bash.err.log` (the startup banner +
    every JSON-RPC line from `mcp-proxy` + every `run_command`
    invocation from `cli-mcp-server`)
- The launcher's own audit log:
  - `~/.local/state/claude-container/mcp-host-bash.log` — same
    stderr stream, tee'd by the launcher itself. Useful when you
    want a per-launch chronological view independent of launchd's
    rotation.

Tail any of them live with `tail -F <path>`.

## 7. Disable temporarily

```sh
launchctl bootout gui/$(id -u)/com.anthropic.claude-watch.mcp-host-bash
```

`bootout` unregisters the LaunchAgent. The plist file under
`~/Library/LaunchAgents/` stays put, so a future `bootstrap` brings
it back without re-editing.

For a soft kill switch that survives reboots WITHOUT touching launchd,
set `MCP_HOST_BASH_DISABLED=1` in the plist's `EnvironmentVariables`
(or in `~/.config/claude-container/mcp-host-bash.env`) and
re-bootstrap. The launcher then exits 0 immediately on every
(re)spawn, and `KeepAlive` settles into the `ThrottleInterval` cadence
without doing real work.

## 8. Permanently uninstall

```sh
launchctl bootout gui/$(id -u)/com.anthropic.claude-watch.mcp-host-bash
rm ~/Library/LaunchAgents/com.anthropic.claude-watch.mcp-host-bash.plist
```

Optionally remove the log files and operator config:

```sh
rm -f ~/Library/Logs/mcp-host-bash.out.log
rm -f ~/Library/Logs/mcp-host-bash.err.log
rm -f ~/.local/state/claude-container/mcp-host-bash.log
rm -f ~/.config/claude-container/mcp-host-bash.env
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
  `${HOME}/.local/bin` because that's where `install-host-deps`
  drops the static binaries; extend the list if your `run_command`
  workflows need others.
- **`HOME`** IS set (to `/Users/<you>`).
- **Keychain access** works in the GUI domain (`gui/$(id -u)`) but
  NOT in the user domain (`user/$(id -u)`). If `mcp-host-bash`
  passes through to a tool that reads the login keychain (codesign,
  some corp CLIs), use the GUI domain.
- **No `~/.zshrc` / `~/.bash_profile` sourcing.** Anything those
  files set has to be declared in `EnvironmentVariables` or in
  `~/.config/claude-container/mcp-host-bash.env`.

When a `run_command` invocation works in your interactive shell but
fails under launchd, the diff is almost always one of these.

### "Couldn't load: ... Operation not permitted"

macOS's app-management protections (System Settings → Privacy &
Security → App Management / Full Disk Access) sometimes block
LaunchAgents that exec from a path outside your home directory. The
fix is either to keep the launcher under `${HOME}` (the template
default — `examples/compose/bin/mcp-host-bash` lives wherever you
cloned the repo) or to grant Terminal / your editor "Full Disk
Access" so it can write the LaunchAgent in the first place. If the
error persists, run `log show --predicate 'subsystem == "com.apple.xpc.launchd"' --last 5m`
to get launchd's actual rejection reason.

### The launcher exits with `cannot find required binaries on PATH`

Two common causes:

1. `install-host-deps` was never run (or ran in a shell with a
   different `PATH`, so the shims went somewhere else). Re-run
   `examples/compose/bin/install-host-deps` and confirm
   `~/.local/bin/mcp-proxy` exists.
2. The plist's `EnvironmentVariables` `PATH` entry doesn't include
   `${HOME}/.local/bin` (or wherever the shims actually live).
   `which mcp-proxy` from your interactive shell tells you the
   real path; mirror it in the plist.
