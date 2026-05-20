# Example: user-level scheduled tasks (no root, no system crontab)

End-to-end example for wiring **recurring user-level jobs** that run alongside
a Claude Code workspace — index refreshes, repo-snapshot updates, periodic
re-scans, anything you'd otherwise drop into `~/.config/cron.d/` if user-owned
drop-ins were a thing (they aren't).

This example is **deliberately separate** from `examples/cron/`, which targets
host-wide system cron (`/etc/cron.d/`) and requires root. Most users running
claude-watch on a personal box don't have (or want) root access for one-off
recurring tasks, so this directory ships the rootless alternatives instead.

The worked example here re-indexes a per-user notes directory every six hours.
Swap the command for any recurring task you want to run under your own user
account.

## What do you have when you don't have crontab?

Modern operating systems each ship a per-user scheduler that does not require
root, integrates with the system logger, and survives reboots. Pick the one
that matches your OS:

| Mechanism             | Where it runs           | Root needed?  | Notes                                                                  |
| --------------------- | ----------------------- | ------------- | ---------------------------------------------------------------------- |
| **systemd user timers** | Linux (systemd hosts)   | No            | `~/.config/systemd/user/*.timer` + `*.service`. Journal-integrated. Recommended for Linux. |
| **launchd LaunchAgents** | macOS                 | No            | `~/Library/LaunchAgents/*.plist`. Reverse-DNS Label per agent. Recommended for macOS. |
| `crontab -e` (user)   | POSIX (Linux/macOS/BSD) | No            | Single per-user file, awkward to install/uninstall programmatically.   |
| `/etc/cron.d/` drops  | Linux                   | **Yes**       | What `examples/cron/` uses. Host-wide; root-owned regular files only.  |
| `systemd-run --user --on-calendar` | Linux (systemd)  | No            | One-shot transient; doesn't survive logout/reboot without a service.   |
| `anacron`             | Linux (laptops/desktops) | Depends      | For hosts that aren't always on. Most claude-watch users are always-on. |

**Recommendation**: systemd user timers on Linux, launchd LaunchAgents on
macOS. Both are no-root, journal-integrated, and survive reboot.

## Files in this directory

```
examples/scheduled-tasks/
  README.md                                            this file
  systemd/
    claude-watch-index-refresh.service                 oneshot unit (linux)
    claude-watch-index-refresh.timer                   6h cadence
  launchd/
    org.gbre.claude-watch.index-refresh.plist          macos parallel
  install.sh                                           idempotent installer
  uninstall.sh                                         idempotent remover
```

## The worked example

A trivial recurring task that re-indexes a directory of markdown notes every
six hours. The command itself is just `echo` by default so the example is safe
to run unmodified — replace `INDEX_CMD` with your own indexer (`vsearch index
<path>`, `notmuch new`, `recoll xapian`, custom script, whatever) before you
care about real output.

The unit reads two environment variables that the install script will set up
from your shell, or you can edit them in the unit files directly:

- `WHATEVER_REPO_DIR` — directory the unit should `cd` into before running.
  Defaults to `$HOME`.
- `INDEX_CMD` — the actual command to run. Defaults to a harmless echo so a
  fresh checkout doesn't break on you.

Both default to safe placeholders — the install script will not auto-replace
them with anything machine-specific.

## Install

The simplest path is `./install.sh`, which detects your OS, copies the
relevant unit files into the right per-user location, and verifies one run.

```sh
cd examples/scheduled-tasks
./install.sh
```

What it does, in order:

1. Detect Linux (systemd) vs macOS (launchd). Abort on unknown OS.
2. **Linux**: copy `systemd/*.service` + `systemd/*.timer` into
   `~/.config/systemd/user/`, run `systemctl --user daemon-reload`, then
   `systemctl --user enable --now claude-watch-index-refresh.timer`. Trigger
   one immediate run via `systemctl --user start
   claude-watch-index-refresh.service` and confirm completion via
   `journalctl --user -u claude-watch-index-refresh --since '5 minutes ago'`.
3. **macOS**: copy `launchd/org.gbre.claude-watch.index-refresh.plist` into
   `~/Library/LaunchAgents/`, load it with `launchctl bootstrap gui/$UID
   <plist>`, kick one run with `launchctl kickstart -k
   gui/$UID/org.gbre.claude-watch.index-refresh`, and tail the stdout log at
   `/tmp/org.gbre.claude-watch.index-refresh.out`.

The installer is idempotent: running it twice is safe and will just re-enable
the timer / agent without duplicating units.

### Manual install (Linux)

If you'd rather do it by hand:

```sh
mkdir -p ~/.config/systemd/user
cp systemd/claude-watch-index-refresh.service ~/.config/systemd/user/
cp systemd/claude-watch-index-refresh.timer   ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now claude-watch-index-refresh.timer
```

### Manual install (macOS)

```sh
mkdir -p ~/Library/LaunchAgents
cp launchd/org.gbre.claude-watch.index-refresh.plist ~/Library/LaunchAgents/
launchctl bootstrap gui/$UID ~/Library/LaunchAgents/org.gbre.claude-watch.index-refresh.plist
launchctl enable gui/$UID/org.gbre.claude-watch.index-refresh
```

## Check it's running

### Linux

```sh
# Timer state + next firing
systemctl --user list-timers claude-watch-index-refresh.timer

# Last service run + exit status
systemctl --user status claude-watch-index-refresh.service

# Full log (last 15 minutes)
journalctl --user -u claude-watch-index-refresh --since '15 minutes ago'
```

### macOS

```sh
# Agent loaded?
launchctl print gui/$UID/org.gbre.claude-watch.index-refresh

# Stdout / stderr (paths from the plist)
tail -n 50 /tmp/org.gbre.claude-watch.index-refresh.out
tail -n 50 /tmp/org.gbre.claude-watch.index-refresh.err
```

## Uninstall

```sh
./uninstall.sh
```

What it does:

- **Linux**: `systemctl --user disable --now
  claude-watch-index-refresh.timer`, remove both unit files from
  `~/.config/systemd/user/`, `daemon-reload`.
- **macOS**: `launchctl bootout gui/$UID
  ~/Library/LaunchAgents/org.gbre.claude-watch.index-refresh.plist`, remove
  the plist.

`uninstall.sh` is idempotent — re-running on a clean system is a no-op.

## Customise

To repurpose the example for your own recurring task:

1. Edit `WHATEVER_REPO_DIR` and `INDEX_CMD` in `systemd/*.service` (Linux) or
   the `EnvironmentVariables` block in `launchd/*.plist` (macOS).
2. Change the cadence:
   - Linux: edit `OnCalendar=` in the `.timer` file. `systemd.time(7)` covers
     the calendar-event syntax.
   - macOS: edit `StartInterval` (seconds) or replace with `StartCalendarInterval`
     in the plist.
3. Optionally rename the unit. The systemd `.service` and `.timer` must share
   a base name; the launchd plist's `Label` must match its filename minus
   `.plist`.
4. Re-run `./install.sh` to apply.

## Why not just use `crontab -e`?

User crontabs work, but they have two ergonomic problems for an example
that ships in a repo:

1. **Single-file namespace** — one crontab per user, no drop-in directory.
   Programmatic install/uninstall has to parse and rewrite the file.
2. **No native logging** — cron emails stdout/stderr (and most desktop hosts
   don't have a working mailer). systemd-user has the journal, launchd has
   `StandardOutPath`/`StandardErrorPath`. Both are easier to debug.

If you're on a host without systemd and without launchd, falling back to
`crontab -e` is fine — just paste the equivalent line manually:

```cron
0 */6 * * * cd "$HOME" && echo "[claude-watch-index-refresh] tick" >> /tmp/claude-watch-index-refresh.log
```

## See also

- `examples/cron/` — host-wide system cron (`/etc/cron.d/`) examples for
  claude-event emission. Requires root.
- `docs/watcher-vs-cron.md` — when to use a watcher vs a cron-equivalent.
- `systemd.timer(5)`, `systemd.service(5)`, `launchd.plist(5)` man pages.
