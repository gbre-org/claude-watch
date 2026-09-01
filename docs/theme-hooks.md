# Theme hooks (`theme-hooks.d`)

`cw-theme-sync` (`container/bin/cw-theme-sync`) watches a theme-report file
and injects `/config theme=<dark|light>` into the Claude Code pane so the
TUI follows the browser's colour scheme. See
[`examples/compose/README.md`](../examples/compose/README.md) §*Dynamic
Claude Code TUI theme* for that pipeline.

The daemon has exactly **one** built-in job: that inject. Anything *else* a
deployment wants to happen when the theme changes — poke a kiosk display,
flip a host application, emit an event, notify a sidecar — goes in an
executable drop-in under `theme-hooks.d`, instead of a forked copy of the
script with the extra behaviour spliced in.

**Off by default, by absence.** With no hooks directory the daemon does not
spawn a single subprocess and does not write a single extra log line.

---

## The two events

| Event | Fires when | Latency |
|---|---|---|
| `changed` | the theme file's **validated value** flips (`dark`↔`light`), immediately, **before** the idle gate | one poll (≤ ~1.5 s) |
| `applied` | after the inject is **verified** — the settings file has been read back and confirms `theme=<x>` | seconds → **hours** |

The split is load-bearing, not a convenience. The idle gate deliberately
stays shut while the operator has a turn in flight or half-typed text on the
prompt line, and it can legitimately stay shut for *hours* — that is exactly
what `CW_THEME_BLOCKED_LOG_SECS` exists to make visible. An external side
effect must not be hostage to whether someone's prompt line happens to be
empty, so `changed` fires on the file value alone. Use `applied` only when a
hook genuinely needs the TUI to have caught up.

`applied` does **not** fire on a failed or unverified inject, on the
`GIVING UP` path, or on a `--force` that timed out waiting for idle.

`changed` does **not** fire on an unreadable file, an unrecognised value, or
a same-value rewrite. The daemon already coalesces and is idempotent — the
browser re-POSTs the current theme on every page load — and hooks inherit
that for free.

`--force` and `SIGHUP` re-assert an unchanged value, so they fire `applied`
(with `CW_THEME_REASON=force` / `sighup`) and **not** `changed`.

The daemon's first observation of the file fires `changed` with
`CW_THEME_REASON=startup` and an empty `CW_THEME_OLD`. That is deliberate: it
makes the system self-converging after a reboot or a daemon restart, and it
forces hook authors to write **idempotent** hooks — which they have to be
anyway.

---

## Invocation contract

```
exec <hook> <event> <new_theme>
```

argv stays at **two positional arguments, forever**. A positional list is a
compatibility trap the moment you want a third field, so everything else is
environment, overlaid on the daemon's own environment:

| Variable | Value |
|---|---|
| `CW_THEME_EVENT` | `changed` \| `applied` |
| `CW_THEME_NEW` | `dark` \| `light` (same as `$2`) |
| `CW_THEME_OLD` | previous value, empty on the first observation. For `changed` that is the previous **file** value; for `applied`, the previously **applied** value. |
| `CW_THEME_REASON` | `startup` \| `change` \| `force` \| `sighup` |
| `CW_THEME_SOURCE_FILE` | resolved `CW_THEME_FILE` |
| `CW_THEME_TIMESTAMP` | unix epoch (integer) |
| `CW_THEME_TIMESTAMP_ISO` | local ISO-8601, matching the log format |
| `CW_THEME_PANE` | resolved tmux pane — **`applied` only**. It is *removed* from the environment on `changed`, even when the daemon itself was configured with it, so a hook can test for its presence. |
| `CW_THEME_HOOK` | the hook's own filename |

`cwd=/`, `stdin=/dev/null`, stdout and stderr merged and logged (truncated).

### Forward-compatibility rule

> **A hook MUST `exit 0` on an event name it does not recognise.**

This is part of the contract, not a suggestion. It is what makes adding a
third event later — the obvious candidate is `failed`, on the `GIVING UP`
path, for alerting — a non-breaking change. Guard with a `case`; never
assume `$1` is one of today's two values:

```sh
#!/bin/sh
case "$1" in
  changed) : ;;          # what this hook is for
  *)       exit 0 ;;     # anything else, including future events
esac
```

---

## Discovery and execution

Discovery follows **`run-parts` conventions**, because those are the ones
every sysadmin already has reflexes for:

- direct children only — a subdirectory is skipped, not recursed;
- **executable** regular files, or symlinks to them. `chmod -x` is the
  disable switch;
- names must match `^[A-Za-z0-9_-]+$`. **Anything containing a dot or a
  `~` is skipped**, which is what stops `10-kiosk.sh.bak`,
  `20-notify.disabled` and editor backups from silently firing alongside the
  file they were copied from;
- sorted in **C-locale byte order**, with `NN-name` numeric prefixes by
  convention. Byte order, not numeric: `10-a` sorts *before* `2-c`.

Execution:

- **Sequential.** Theme changes are rare, ordering is occasionally
  meaningful, and a thread pool buys nothing.
- **One subprocess per hook, per event**, in its own process group.
- **Re-discovered on every fire**, so installing a hook needs no restart.
- **Per-hook timeout**, default 10 s (`CW_THEME_HOOK_TIMEOUT_SECS`). On
  timeout the whole process group is `SIGKILL`ed, the timeout is logged, and
  the next hook runs.
- **Failure isolation is absolute.** Non-zero exit, timeout, exec failure,
  decode error — all logged and swallowed. A hook can never fail the inject
  path, and the runner never raises.

The worst case is `N_hooks × timeout` of daemon stall. A hook with long work
to do should **self-background** — and must redirect the background child's
output (`cmd >/dev/null 2>&1 &`), or the inherited stdout pipe keeps the hook
"running" until the timeout kills it. Theme changes during a stall are still
coalesced (the daemon always reads the *current* file value), so correctness
holds regardless.

**Anti-pattern:** a hook must never write `CW_THEME_FILE`. Coalescing makes
such a loop self-limiting, but don't.

---

## Configuration

| Variable | Default | Meaning |
|---|---|---|
| `CW_THEME_HOOKS_DIR` | `/etc/claude-watch/theme-hooks.d` | drop-in directory |
| `CW_THEME_HOOK_TIMEOUT_SECS` | `10.0` | per-hook wall-clock timeout |

A container shape points `CW_THEME_HOOKS_DIR` at a mounted path (e.g.
`/host-clipboard/theme-hooks.d`) so hooks can be maintained on the host.

`cw-theme-sync --status` prints `hooks_dir` and the hooks it discovered
there, so a typo'd path or a hook that lost its `+x` is visible without
reading the log.

### Security

Hooks execute with the daemon's identity, inside the blast radius of the
Claude Code session. Install the directory root-owned `0755`, with files
`0755 root:root` — **a user-writable hooks directory is a privilege
escalation path**. A hook that needs root should get a dedicated
`NOPASSWD` sudoers line for one specific command, never a blanket one.

---

## Logging

One line per hook, in the existing theme-sync log:

```
firing 3 changed hook(s) from /etc/claude-watch/theme-hooks.d
hook 10-kiosk-push (changed dark): ok in 0.31s
hook 20-notify (changed dark): FAILED rc=1 in 0.05s -- curl: (7) connection refused
hook 30-slow (applied light): TIMEOUT after 10.0s; killed
```

The summary line only appears when hooks actually exist, so an absent or
empty directory stays completely silent.

When you install your first hook, **prove it with a no-op probe before you
trust it**: a `10-probe` that appends `$CW_THEME_EVENT $CW_THEME_NEW` to a
file, then toggle the theme and check *both* the probe's file and the
theme-sync log. Silence is not success — the log line is the proof.

---

## Example

```sh
#!/bin/sh
# /etc/claude-watch/theme-hooks.d/10-kiosk-push
# Push the new theme to a kiosk display the moment it changes, instead of
# waiting for the kiosk's next poll.
set -eu
case "$1" in
  changed) ;;
  *) exit 0 ;;   # includes any event added after this hook was written
esac

curl -fsS --max-time 5 -X POST "http://kiosk.local:9113/theme" \
     --data-urlencode "theme=$2"
```

A hook is also the recommended way to get an **LLM-mediated** reaction to a
theme change, by composing with the event bus rather than replacing it:

```sh
#!/bin/sh
# /etc/claude-watch/theme-hooks.d/50-claude-event
case "$1" in changed) ;; *) exit 0 ;; esac
claude-event "theme changed to $2" --tag theme-change --source cw-theme-sync
```

That gives you the immediate mechanical path *and* the event, without making
the mechanical action depend on the main loop choosing to act on it.

---

## Why the inject is not itself a hook

It is inseparable from the idle gate, the self-clear flock, the settings-file
verification, the retry/backoff ladder and the `GIVING UP` cap — expressing
it as a hook would mean exporting all of that as a public contract. Hook
failure policy is "log and continue", which is exactly the *wrong* policy for
the inject, which must back off and then fail loudly. And `applied` is
*defined* as "after a verified inject", so the inject has to be privileged
relative to the hooks anyway.

Hooks are for extras. The daemon keeps exactly one built-in job.
