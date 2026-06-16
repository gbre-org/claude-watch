# Toggling baked cron entries (no sudo)

## The incident (2026-06-16)

The baked cron entries in `/etc/cron.d/cw-default` are root-owned (`0644`)
and cannot be edited or disabled from inside the container without an
interactive `sudo`. On a macOS host, every `sudo` triggers a Touch ID /
fingerprint prompt — prohibitive for an automated loop, and painful even
for an operator who just wants to silence one misbehaving job (the trigger
was the `cw-watcher-health-check` entry injecting a `[CLAUDE-WATCH]` alert
into the tmux pane every minute). There was no lever to disable a single
baked entry short of an interactive root shell.

## The approach: flag-file guard (zero new sudo)

Each `cw-default` cron command is prefixed with the `cw-cron-run` wrapper
plus a stable job-name. On every tick the wrapper checks for a flag file
named after the job under a disable dir. If the flag exists, the wrapper
exits `0` **without running** the real command (silent skip — `exit 0` so
cron logs no failure). Otherwise it `exec`s the real command, adding no
extra process and preserving the command's exit code and signals.

The flag dir lives at `/var/lib/claude-watch/cron-disabled`, under the
already-`uid-1000`-owned `/var/lib/claude-watch` (backed by the
`claude-watch-state` named volume). So:

- **No sudo** — toggling is a `uid-1000` file write.
- **Persistent** — disabled state survives container redeploys (named volume).
- **General** — works for ANY `cw-default` entry, not just the incident job.

This was chosen over the alternative (approach B: a new sudoers carve-out
letting `uid-1000` `tee`/`rm` a generated drop-in cron file) because the
flag-file guard needs **no sudoers change** — a strictly smaller blast
radius — and reuses an existing writable, persistent state dir.

## Operator commands

```sh
cw-cron-toggle list                 # show each job + enabled/disabled state
cw-cron-toggle status               # alias of list
cw-cron-toggle disable <job-name>   # stop a job (creates flag file)
cw-cron-toggle enable  <job-name>   # resume a job (removes flag file)
cw-cron-toggle --help
```

Known job-names (one per `cw-default` entry):

| job-name | what it runs |
| --- | --- |
| `active-agents` | `claude-watch active-agents` (writes active-agents.json) |
| `metrics` | `claude-watch metrics` (Prometheus textfile emit) |
| `stale-ready-check` | `claude-watch stale-ready-check` (stale-ready watchdog) |
| `queue-check` | `claude-watch queue-check` (orphaned/stuck watchdog) |
| `watcher-health-check` | `cw-watcher-health-check` (dead-event-watcher [CLAUDE-WATCH] injector) |

### Silence the incident job right now

```sh
cw-cron-toggle disable watcher-health-check
```

## Flag file location + timing

- Flag files: `/var/lib/claude-watch/cron-disabled/<job-name>`
  (override the dir with `CW_CRON_DISABLED_DIR`, mainly for tests).
- Disabling/enabling an **already-deployed wrapped entry** takes effect on
  the **next cron tick (~1 min)** — no redeploy needed.
- The disabled state **persists across container redeploys** via the
  `claude-watch-state` named volume.

## What needs a rebuild + redeploy

The `cw-cron-run` wrapper and any **new** cron entries (or new job-names)
only take effect after `make container-build` + redeploy, because the
cron.d file and the wrapper scripts are baked into the image. Until the
image carrying the wrapper is deployed, the unwrapped baked entries still
fire and the toggle has nothing to gate. Once the wrapper-carrying image is
live, toggling is instant (next tick) and needs no further redeploy.
