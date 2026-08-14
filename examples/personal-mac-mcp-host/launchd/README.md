# On-demand macOS LaunchAgents for `personal-mcp-host.sh`

This directory ships **two** macOS LaunchAgent templates that wire
[`personal-mcp-host.sh`](../personal-mcp-host.sh) into `launchd` for
the **on-demand** remote-access pattern. Both are registered once but
do NOT start at login (`RunAtLoad=false`); the operator brings the
bridge up for a session with `launchctl kickstart` and tears it down
with `launchctl bootout` when they're done.

| Template | Mode | What it starts |
|---|---|---|
| `org.gbre.personal-mcp.tunnel.plist` | tunnel-only (`--tunnel-only`) | ONLY the reverse SSH tunnel. Assumes `mcp-host-bash` is already running locally. **Recommended.** |
| `org.gbre.personal-mcp.host.plist` | bundled (`--enable`) | `mcp-host-bash` AND the tunnel together. Simpler alternative. |

**Recommended split — MCP always-on locally, remote access on-demand.**
Run `mcp-host-bash` full-time via the
[compose-stack LaunchAgent](../../compose/launchd/)
(`org.gbre.claude-watch.mcp-host-bash.plist`, `RunAtLoad=true`) so the
MCP server is up at every login, then use the **tunnel-only** unit here
to grant/revoke remote access on demand without touching the server's
lifecycle. The tunnel — the only network-facing piece — stays down
until you explicitly bring it up.

The **bundled** unit (`org.gbre.personal-mcp.host.plist`) is the
simpler alternative when you'd rather have one unit own both pieces and
don't want the MCP server resident full-time.

Steps 1–8 below walk the **bundled** unit end to end; the
[tunnel-only unit](#tunnel-only-unit) section calls out the (small)
differences. Everything else — bootstrap / kickstart / bootout /
logs / troubleshooting — is identical between the two; substitute the
Label `org.gbre.personal-mcp.tunnel` for `org.gbre.personal-mcp.host`.

## LaunchAgent vs LaunchDaemon

LaunchAgent (under `~/Library/LaunchAgents/`, scope `gui/$(id -u)`).
The wrapper exec's `ssh` with the operator's private key, exec's
`mcp-host-bash-server` whose run_command / run_script run under the
operator's `$HOME` / `$PATH` / login keychain, and opens an outbound
TCP connection. None of that needs root, and a LaunchDaemon would
invert the trust model.

## 0. Prereqs

- macOS (this is a `launchd` plist; Linux operators use `systemd`
  user units — not covered here).
- A working `mcp-host-bash-server` binary. Either:
  1. You built + installed it (`make install-mcp-host-bash-server`
     from the repo root drops it at `~/bin/mcp-host-bash-server`), OR
  2. Your custom `mcp-host-bash-server` binary lives somewhere else
     and you point `MCP_HOST_BASH_BIN` at it in the sibling `.env`.
- An interactive run of `personal-mcp-host.sh` succeeded once. That
  proves your `.env`, your SSH key, and the remote-side reverse-port
  bind all work BEFORE you wrap any of it in `launchd`:

  ```sh
  cd examples/personal-mac-mcp-host
  cp .env.example .env
  $EDITOR .env
  ./personal-mcp-host.sh
  # Ctrl-C after you see the banner + "mcp-host-bash listening on
  # 127.0.0.1:$MCP_LOCAL_PORT". Then verify the remote sees the
  # forward:
  #   ssh $REMOTE_USER@$REMOTE_HOST "lsof -i :$REMOTE_PORT"
  ```

- The SSH key declared in `.env`'s `SSH_KEY_PATH` is recognised by
  the remote (`ssh-copy-id` or manual append to
  `~/.ssh/authorized_keys`). See the main
  [`README.md`](../README.md) "SSH key hardening" section for the
  recommended `authorized_keys` line restricting the key to
  port-forward-only.

## 1. Install the plist (one command)

Use the bundled installer — it auto-resolves your checkout path and home
directory, substitutes the `/PATH/TO/REPO` / `/PATH/TO/HOME`
placeholders, copies the plist into `~/Library/LaunchAgents/`, and
pre-creates `~/Library/Logs/`. No manual `cp` + editor dance:

```sh
cd examples/personal-mac-mcp-host
./install.sh --bundled
```

That prints the resolved paths and the `launchctl bootstrap` command to
run next (step 2). Pass `--bootstrap` to also bootstrap immediately:

```sh
./install.sh --bundled --bootstrap
```

Other flags:

- `--tunnel-only` — install `org.gbre.personal-mcp.tunnel.plist` instead
  of the bundled wrapper unit (only if that template exists in this
  checkout).
- `--print-cmd` — dry run: print the resolved paths + the rendered plist
  + the bootstrap command, write nothing.
- `--help` — full flag reference.

The installer is idempotent — re-run it any time (e.g. after moving your
checkout) and it overwrites the installed plist with freshly-resolved
paths.

Two things to know when re-running over an existing install:

- **Re-running only rewrites the file.** `launchd` is still running the
  copy it snapshotted at `bootstrap` time, so a re-install changes
  nothing until you `bootout` + `bootstrap` again — see
  [step 5](#5-pick-up-plist-changes). This bites specifically if you
  installed the bundled unit before it gained its `--enable` argument:
  the registered unit keeps the old bare argv, and its
  status-gate exit 3 keeps looping under `KeepAlive`, until you
  re-bootstrap.
- **It overwrites the installed plist wholesale.** Any hand edits you
  made to the copy under `~/Library/LaunchAgents/` are lost. Keep
  operator config in the sibling `.env` (the wrapper re-reads it on
  every start) rather than in the installed plist, or re-apply the edits
  after each re-install.

### What it does under the hood (manual fallback)

If you'd rather do it by hand, the installer is equivalent to:

```sh
# 1. Copy. launchd only loads files directly under
#    ~/Library/LaunchAgents/ — no symlinks. The filename must match the
#    plist's Label (org.gbre.personal-mcp.host); launchd keys bootstrap /
#    bootout / print / kickstart off it.
cp launchd/org.gbre.personal-mcp.host.plist ~/Library/LaunchAgents/

# 2. Edit absolute paths. launchd does NOT expand ~ or ${HOME} in plist
#    values — literal paths only.
$EDITOR ~/Library/LaunchAgents/org.gbre.personal-mcp.host.plist
#    Search/replace:
#      /PATH/TO/REPO → absolute path to your claude-watch checkout
#                      (e.g. /Users/yourname/code/claude-watch)
#      /PATH/TO/HOME → your home directory (e.g. /Users/yourname;
#                      `echo $HOME` if unsure)

# 3. Pre-create the log directory once (launchd makes the log files but
#    not the parent).
mkdir -p ~/Library/Logs
```

You generally do NOT need to add anything to `EnvironmentVariables`
beyond `PATH`. All operator-specific config (`REMOTE_HOST`,
`REMOTE_PORT`, `SSH_KEY_PATH`, `MCP_HOST_BASH_BEARER`, `CW_PROFILE`,
…) lives in the sibling `.env` file that `personal-mcp-host.sh`
sources on every start. To change one, edit
`examples/personal-mac-mcp-host/.env` and `kickstart` the unit again
— no re-bootstrap needed.

## 2. Bootstrap the LaunchAgent (one-time, registers but does NOT start)

```sh
launchctl bootstrap gui/$(id -u) \
    ~/Library/LaunchAgents/org.gbre.personal-mcp.host.plist
```

`gui/$(id -u)` is the per-user GUI domain — the right scope for a
LaunchAgent that needs the operator's login session (Docker Desktop /
Keychain access / etc.).

Because `RunAtLoad=false`, this **registers** the unit without firing
it. Verify:

```sh
launchctl print gui/$(id -u)/org.gbre.personal-mcp.host
```

Look for `state = not running` and `last exit code = (never exited)`.

## 3. Per-session: bring up the bridge

When you want to grant your remote Claude access to this Mac:

```sh
launchctl kickstart gui/$(id -u)/org.gbre.personal-mcp.host
```

That fires `personal-mcp-host.sh --enable` (the flag is baked into the
unit's `ProgramArguments`), which:

1. Starts `mcp-host-bash --port $MCP_LOCAL_PORT` **detached** — in its
   own session, so it outlives the wrapper.
2. Waits for `127.0.0.1:$MCP_LOCAL_PORT` to enter `LISTEN`. If it never
   binds, the wrapper exits non-zero rather than exposing a tunnel to a
   server that isn't there.
3. Opens the reverse SSH tunnel
   (`ssh -N -R $REMOTE_PORT:127.0.0.1:$MCP_LOCAL_PORT`).
4. Tails the live MCP host log in the foreground. That tail is what
   holds the unit in the `running` state; the tunnel is the piece being
   watched. If the tunnel dies, the wrapper tears down and exits
   non-zero, and `KeepAlive` respawns the unit.

Two consequences of the detached start worth knowing before you read
the logs:

- **Stopping the unit does not stop the MCP server.** A `bootout` /
  `kill TERM` tears down the tunnel and the tail only. The server keeps
  listening on `127.0.0.1:$MCP_LOCAL_PORT` until you stop it yourself.
- **A `KeepAlive` respawn logs a harmless bind failure.** The respawned
  wrapper tries to start `mcp-host-bash` again, the original is still
  holding the port, so the duplicate exits with an "address already in
  use" `FATAL` in `~/Library/Logs/personal-mcp-host.err.log`. The
  reopened tunnel connects to the original server, so the bridge works;
  the line is noise, not a failure to chase.

Why `--enable` and not a bare argv: without the flag the wrapper runs
its default **status-gate** mode, which does not start anything — it
probes the port and, finding nothing, prints the "re-run with
`--enable`" error and exits 3. Under `KeepAlive=true` that exit is just
another respawn, so a bare-argv bundled unit becomes a
`ThrottleInterval`-paced restart loop that never brings the bridge up.
The status-gated shape belongs to the
[tunnel-only unit](#tunnel-only-unit) plus an always-on MCP server.

Confirm the tunnel is up from the remote side:

```sh
ssh $REMOTE_USER@$REMOTE_HOST "lsof -i :$REMOTE_PORT -sTCP:LISTEN"
# Should show one row, COMMAND=sshd, NAME=127.0.0.1:$REMOTE_PORT.
```

Confirm the in-process side from the Mac:

```sh
launchctl print gui/$(id -u)/org.gbre.personal-mcp.host
# state = running; last exit code = (never exited)
```

Tail the wrapper's logs live:

```sh
tail -F ~/Library/Logs/personal-mcp-host.err.log
```

## 4. Per-session: tear the bridge down

Two options.

**A. Soft stop (preferred for "I'll start it again later"):**

```sh
launchctl bootout gui/$(id -u)/org.gbre.personal-mcp.host
```

`bootout` unregisters the unit. The plist file stays in
`~/Library/LaunchAgents/`, so a future `bootstrap` brings it back
without any re-edit.

**B. Soft kill switch (leaves the unit registered):**

Set `PERSONAL_MCP_DISABLED=1` in
`examples/personal-mac-mcp-host/.env` (or as a `<string>` entry in
the plist's `EnvironmentVariables`) and `kickstart` again. The
wrapper exits 0 immediately on every (re)spawn, and `KeepAlive`
settles into the `ThrottleInterval` cadence without doing real work.
Unset / `0` to re-enable. This is the "leave launchd registered, but
don't actually bring up the bridge" mode.

A plain `launchctl kill TERM` does **not** stop the unit — `KeepAlive`
respawns it within `ThrottleInterval`. Use `bootout` or the soft kill
switch.

Either way, for the **bundled** unit "tear the bridge down" means the
tunnel, not the server: `--enable` starts `mcp-host-bash` detached, so
it survives the wrapper's exit and keeps listening on
`127.0.0.1:$MCP_LOCAL_PORT`. That is the intended split — stopping the
unit revokes remote access without cold-starting the server next time.
To stop the server too, stop it directly (`MCP_HOST_BASH_DISABLED`, its
own LaunchAgent, or kill whatever `lsof -nP -iTCP:$MCP_LOCAL_PORT
-sTCP:LISTEN` reports).

## 4b. Recover a wedged bridge (`restart`)

When the bridge is broken and you'd rather not work out *which* half
died — or which of `bootout` / `kickstart` / a manual `kill` applies to
whichever unit you installed:

```sh
cd examples/personal-mac-mcp-host
./personal-mcp-host.sh restart
```

It reaps whatever is left of the MCP host **and** the reverse SSH
tunnel, brings both back, verifies each one is answering again, and
exits 0 — or exits 4 with a message naming the piece that didn't come
back. It is idempotent: run it whether the stack is up, down, or
half-up.

It works *with* launchd rather than around it. Any of the three units
below that is bootstrapped gets `launchctl kickstart -k` (SIGKILL the
running instance, then start), so the relaunch happens with launchd's
own environment — including a Keychain-sourced `MCP_HOST_BASH_BEARER`,
which a hand-launch could not reproduce:

| Label | Piece |
|---|---|
| `org.gbre.claude-watch.mcp-host-bash` | always-on MCP server (compose stack) |
| `org.gbre.personal-mcp.tunnel` | tunnel-only unit |
| `org.gbre.personal-mcp.host` | bundled unit (both pieces) |

Labels that aren't bootstrapped are skipped, and anything no unit owns
is started directly, detached. Override
`PERSONAL_MCP_SERVER_LABEL` / `PERSONAL_MCP_TUNNEL_LABEL` /
`PERSONAL_MCP_HOST_LABEL` if you renamed a unit.

Two things it deliberately does NOT do:

- **Trust the port probe.** The failure it exists for is an MCP server
  whose stdio child died on a broken pipe while the parent kept
  `$MCP_LOCAL_PORT` bound — the port answers, so a status check calls it
  healthy. `restart` reaps unconditionally, including a child that
  inherited the listening socket from a parent that already died.
- **Treat "kickstarted" as "working".** The MCP host must accept a real
  TCP connect, and the tunnel's `ssh` must still be alive after a settle
  window — with `ExitOnForwardFailure=yes`, an ssh that couldn't bind
  `$REMOTE_PORT` on the remote exits within a second or two, so
  surviving the settle is what proves the remote-side bind took.

If the tunnel is the piece that fails, `$REMOTE_PORT` still bound on the
remote by a stale forward is the usual cause — see "Wrapper exits with
'ssh exit code N'" in Troubleshooting below.

## 5. Pick up plist changes

`launchd` snapshots the plist contents at `bootstrap` time. Editing
the plist — or re-running `install.sh` over it — does NOT take effect
until you re-bootstrap:

```sh
launchctl bootout gui/$(id -u)/org.gbre.personal-mcp.host
launchctl bootstrap gui/$(id -u) \
    ~/Library/LaunchAgents/org.gbre.personal-mcp.host.plist
```

Run exactly this after updating a checkout in which the bundled unit
gained its `--enable` argument (re-run `./install.sh --bundled` first so
the file on disk is current). Confirm the registered unit picked it up:

```sh
launchctl print gui/$(id -u)/org.gbre.personal-mcp.host | grep -A3 arguments
# should list personal-mcp-host.sh followed by --enable
```

`.env` changes are different — the wrapper sources the file on every
(re)start, so a fresh `launchctl kickstart` (or letting `KeepAlive`
respawn after a `kill TERM`) is enough.

## 6. Logs

The wrapper writes to two places by default:

- launchd-captured `stdout` / `stderr`:
  - `~/Library/Logs/personal-mcp-host.out.log` (mostly empty)
  - `~/Library/Logs/personal-mcp-host.err.log` (banner + child
    stderr)
- `mcp-host-bash`'s own audit log (independent of this wrapper):
  - `~/.local/state/claude-container/mcp-host-bash.log`

`tail -F <path>` either to follow live.

## 7. Permanently uninstall

```sh
launchctl bootout gui/$(id -u)/org.gbre.personal-mcp.host
rm ~/Library/LaunchAgents/org.gbre.personal-mcp.host.plist
```

Optionally remove the logs:

```sh
rm -f ~/Library/Logs/personal-mcp-host.out.log
rm -f ~/Library/Logs/personal-mcp-host.err.log
```

And, if you set up a dedicated SSH key per the main README's
hardening section, you may want to remove its
`~/.ssh/authorized_keys` entry on the remote and the local key files:

```sh
rm -f ~/.ssh/id_personal_mcp_tunnel ~/.ssh/id_personal_mcp_tunnel.pub
ssh $REMOTE_USER@$REMOTE_HOST   # then edit ~/.ssh/authorized_keys
```

## Tunnel-only unit

The `org.gbre.personal-mcp.tunnel.plist` template is the recommended
shape: the MCP server (`mcp-host-bash`) runs always-on locally under
the [compose-stack LaunchAgent](../../compose/launchd/)
(`org.gbre.claude-watch.mcp-host-bash.plist`, `RunAtLoad=true`), and
this unit opens ONLY the reverse SSH tunnel on demand. It invokes the
wrapper with `--tunnel-only`, so it does NOT launch `mcp-host-bash` and
does NOT run the listener probe.

The install / lifecycle is identical to the bundled unit (steps 1–8),
with these substitutions:

- **Prereq:** `mcp-host-bash` must already be listening on
  `127.0.0.1:$MCP_LOCAL_PORT` — bring up the compose-stack LaunchAgent
  first (see [`../../compose/launchd/README.md`](../../compose/launchd/README.md)).
  Confirm with `lsof -nP -iTCP:$MCP_LOCAL_PORT -sTCP:LISTEN`.
- **Plist + Label:** copy `org.gbre.personal-mcp.tunnel.plist`; its
  Label is `org.gbre.personal-mcp.tunnel`. Use that Label in every
  `launchctl` command below.
- **Logs:** distinct paths so the two units can run side by side —
  `~/Library/Logs/personal-mcp-tunnel.out.log` and
  `~/Library/Logs/personal-mcp-tunnel.err.log`.

```sh
# One-time install — install.sh auto-substitutes /PATH/TO/REPO and
# /PATH/TO/HOME and creates ~/Library/Logs (--bootstrap to also bootstrap
# now; --print-cmd for a dry run):
cd examples/personal-mac-mcp-host
./install.sh --tunnel-only

launchctl bootstrap gui/$(id -u) \
    ~/Library/LaunchAgents/org.gbre.personal-mcp.tunnel.plist
# Registers the unit. Doesn't fire it (RunAtLoad=false).

# Grant remote access: start the tunnel (MCP server already up).
launchctl kickstart gui/$(id -u)/org.gbre.personal-mcp.tunnel

# Revoke remote access: stop the tunnel. MCP server keeps running.
launchctl bootout gui/$(id -u)/org.gbre.personal-mcp.tunnel
```

(Prefer to do it by hand? Same manual `cp` + editor + `mkdir -p
~/Library/Logs` steps as the bundled unit's
[under-the-hood fallback](#what-it-does-under-the-hood-manual-fallback),
substituting `org.gbre.personal-mcp.tunnel.plist`.)

Because the wrapper skips the `mcp-host-bash` launch + listener probe
in this mode, the only failure surface is the SSH tunnel itself —
"Wrapper exits with 'mcp-host-bash …'" troubleshooting below does not
apply to the tunnel-only unit. If the tunnel can't dial through to a
working MCP server, double-check the compose-stack LaunchAgent is
actually listening on `$MCP_LOCAL_PORT`.

## Troubleshooting

### `launchctl bootstrap` exit codes

- **5** (`Input/output error`): malformed plist XML or invalid key.
  `plutil -lint <path>` points at the offending line.
- **22** (`Invalid argument`): wrong type inside the plist (a string
  where launchd expects a bool, e.g.). `plutil -lint` again.
- **37** (`Operation already in progress`): unit is already
  bootstrapped. Run `bootout` first, then `bootstrap`.
- **78** (`Function not implemented`): domain target wrong.
  `gui/$(id -u)` is correct for a LaunchAgent on a logged-in user.
- **125** (`Domain does not support specified action`): usually
  trying `bootstrap gui/$(id -u)` from a non-GUI session (SSH without
  a graphical login). Use a real Console session, or switch to
  `user/$(id -u)` (loses Keychain / GUI access).

### File permissions

`launchd` enforces:

- Plist file owned by the operator (`stat -f '%Su' <path>`).
- Mode `0644` or stricter. `chmod 0644 ~/Library/LaunchAgents/<file>`
  if `bootstrap` complains.

### Bundled unit restart-loops with "host MCP service is NOT running"

`~/Library/Logs/personal-mcp-host.err.log` fills with that message
every `ThrottleInterval`, and `launchctl print` shows `last exit code
= 3`. The registered unit is invoking the wrapper with a bare argv, so
it runs the default status-gate mode: it never starts `mcp-host-bash`,
finds nothing on the port, and exits 3 — which `KeepAlive` treats as
just another respawn.

The bundled template passes `--enable` for exactly this reason. If your
installed copy predates that, refresh it and re-bootstrap:

```sh
cd examples/personal-mac-mcp-host
./install.sh --bundled
launchctl bootout gui/$(id -u)/org.gbre.personal-mcp.host
launchctl bootstrap gui/$(id -u) \
    ~/Library/LaunchAgents/org.gbre.personal-mcp.host.plist
```

(Re-installing alone is not enough — `launchd` runs the snapshot it
took at `bootstrap` time. See [step 5](#5-pick-up-plist-changes).)

### Wrapper exits with "missing .env"

The wrapper looks for `.env` next to the script by default. Either
copy the template (`cp .env.example .env`) and fill it in, or set
`PERSONAL_MCP_ENV_FILE=/absolute/path/to/.env` in the plist's
`EnvironmentVariables` dict.

### Wrapper exits with "mcp-host-bash-server not found"

Two common causes:

1. The binary was never built. Run `make install-mcp-host-bash-server`
   from the repo root once — it drops it at `~/bin/mcp-host-bash-server`.
2. Your binary lives somewhere `~/bin` isn't. Set `MCP_HOST_BASH_BIN`
   in `.env` to its absolute path.

### Wrapper exits with "ssh exit code N"

Tail `~/Library/Logs/personal-mcp-host.err.log` — `ssh`'s stderr is
captured there. Common shapes:

- `Permission denied (publickey)`: key in `.env`'s `SSH_KEY_PATH`
  isn't recognised by the remote. `ssh-copy-id` it, or edit
  `~/.ssh/authorized_keys` on the remote.
- `remote port forwarding failed`: `$REMOTE_PORT` already bound on
  the remote (probably a stale prior tunnel). `ssh
  $REMOTE_USER@$REMOTE_HOST 'lsof -i :$REMOTE_PORT'` to find the
  prior PID and clear it, or pick a different `REMOTE_PORT`.
- `Could not resolve hostname`: `REMOTE_HOST` in `.env` is wrong
  / your network can't reach the DNS.
- `Connection timed out`: remote unreachable from this network
  (corp wifi blocking outbound 22, e.g.). `PERSONAL_MCP_SSH_EXTRA="-o
  Port=443 -o ProxyJump=..."` is the usual workaround if you have
  another path to the remote.

### Wrapper exits with "mcp-host-bash exited before binding"

The MCP server side died before opening its loopback port. Check the
underlying launcher's stderr in
`~/.local/state/claude-container/mcp-host-bash.log`. Common causes:

- `MCP_LOCAL_PORT` already owned by a stale prior instance. `lsof
  -nP -iTCP:$MCP_LOCAL_PORT -sTCP:LISTEN` to find the prior PID.
- A bad operator config under
  `~/.config/claude-container/mcp-host-bash.env` (the server reads it
  at startup) — check the server's stderr in the log named above.

### Env-var inheritance differs from your interactive shell

`launchd` starts each LaunchAgent with a near-empty environment. The
common surprises:

- **`PATH`** is `/usr/bin:/bin:/usr/sbin:/sbin` — no Homebrew, no
  `~/.local/bin`, no `~/.cargo/bin`. The plist template adds
  `${HOME}/.local/bin`; extend if needed.
- **`HOME`** IS set.
- **No `~/.zshrc` / `~/.bash_profile` sourcing.** Anything those
  files set has to be declared in `EnvironmentVariables` or in the
  sibling `.env`.
- **SSH agent** is NOT forwarded. The wrapper uses the dedicated
  key file directly (`-i $SSH_KEY_PATH`), so this is moot for the
  tunnel — but it's why interactive `ssh-add`'d keys won't be
  picked up.
