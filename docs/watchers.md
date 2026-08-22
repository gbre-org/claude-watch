# Background tasks + watcher hygiene

Watchers are the canonical way to surface external state changes to a Claude
Code main loop. A **watcher** here is the precise thing: a *one-shot tool the
main loop invokes* (`watcher-ctl run <name>`) that blocks until its event
fires, prints it to stdout, and **exits** — the supervisor respawns a fresh
instance for the next burst (see [`adding-watchers.md`](adding-watchers.md) for
the lifecycle contract). It is *not* a long-lived poll loop, and it is distinct
from an **event producer** (a cron job, alertmanager, the queue) that merely
*emits* a claude-event onto the bus for the `claude-event-watch` watcher to
surface — producers are not watchers. Watchers MUST be spawned, supervised, and
restarted following the rules below — drift silently turns them into orphans
that fire into the void.

> **Writing a new watcher?** See [`adding-watchers.md`](adding-watchers.md)
> for the authoring walkthrough — the on-disk file layout, the
> fire-and-exit lifecycle contract, the `watchers.conf` schema (host)
> and `<name>.toml` schema (container), and a fully worked Jenkins-
> build-failure example for either surface. This file covers the
> operator-side hygiene rules; the authoring doc covers how to write
> the watcher itself.

## Cardinal rule: watchers belong to the main loop

> **Watchers can ONLY ever be started by Claude Code's main loop**, via the
> Bash tool's `run_in_background: true` — or, for a watcher whose
> `watchers.conf` entry says `mode=monitor`, via the main loop's `Monitor`
> tool (`persistent: true`), which `watcher-ctl run <name>` prints the exact
> command for instead of exec'ing. Never via systemd-run, never via nohup,
> never by the `claude-watch` daemon, never by a subagent.

Two delivery modes coexist, chosen per watcher in the layered
`watchers.conf` (base file + user-dir override, see
[`adding-watchers.md`](adding-watchers.md#registering-with-watchersconf)):

| `mode=` | launched as | per batch | stopped by |
|---|---|---|---|
| `oneshot` (default) | `watcher-ctl run <name>` as a background Bash task | one notification **+ one restart call** | its own exit / `watcher-restart` |
| `monitor` | the Monitor tool armed ONCE with the command `watcher-ctl run <name>` prints | one notification, no restart | `TaskStop` / `watcher-restart` |

Flipping is one line in the override file (`<name>|mode=monitor`) plus a
re-arm; nothing is rebuilt, reverted or restarted. Health is judged the same
way in both modes (pidfile liveness), so a live monitor is `ok (1/1)`. The
one monitor-only state is **`ARMING`**: `watcher-ctl run <name>` records
`<pid_dir>/<name>.monitor-intent` when it prints the Monitor command, and
for `[watcher_monitor].monitor_arming_grace_secs` (default 120s) after that
— until the Monitor is live, unless a runtime file is written in between —
the watcher is healthy-pending, not DOWN: neither the obligations
`watchers_healthy` gate nor the daemon's WATCHER(S) DOWN path fires in the
gap between "printed the command" and "armed". Past the window with no
live pid it is DOWN again (re-ARM footer); `watcher-restart` clears the
intent. The Monitor call itself (exactly the printed command) passes every
obligations gate via the `MonitorArm` exempt — see
[`tools/obligations/README.md`](../tools/obligations/README.md).

**Monitor-mode replies must surface the event.** Under the Monitor tool the
terminal collapses every delivery to the Monitor's static description, so
the main loop's own one-line reply is the only human-visible trace of what
was read. When acting on an item line (`EVENT[...]` / `BOTCHAT[...]` /
`SIGNAL[...]`), quote its lead (the text after the prefix, ~80 chars) plus
what was done — never a bare "Acknowledged" / "Idle". Bad: `Liveness ping —
status only. Idle.` Good: `EVENT[claude-watch/heartbeat-tick] heartbeat tick
— touched the heartbeat file; nothing pending.` The arm text and
`claude-event-watch`'s `[monitor-mode] REPLY RULE:` banner line both restate
this.

The daemon's only emergency action is **tmux-injecting** a
`watcher-ctl run <name>` line into the main loop's pane, so the main
loop re-spawns the watcher itself. If anything else spawns the watcher,
the main loop has no handle on it and the watcher's stdout disappears.

### The one narrow, opt-in exception: last-resort event-consumer self-heal

The tmux-inject above has a fragility: it only recovers a dead watcher if a
**responsive main loop** receives and acts on the inject. When the loop is
wedged, compacting, or simply not turning, the inject lands on deaf ears — and
if the dead watcher is the **event consumer** (`claude-event-watch`), the
*entire event bus* goes blind and events pile up unconsumed indefinitely
(observed in production: >1.5h of stuck events + relentless `WATCHER(S) DOWN`
churn, because nothing actually restarted the consumer).

For that ONE watcher only, the in-container `cw-watcher-health-check` cron
(`tools/event-must-act/cw-watcher-health-check`, baked + scheduled via
`/etc/cron.d/cw-default`) ships an **opt-in, bounded last-resort auto-restart**
(`CW_WATCHER_HEALTH_AUTORESTART=1`, enabled in the container cron):

- The inject (main-loop) path still fires **first** and remains primary.
- Only after the inject has fired `CW_WATCHER_HEALTH_AUTORESTART_AFTER` times
  (default 3, each gated by the script's cooldown) **without the queue
  draining** — i.e. the main-loop path has *demonstrably failed* — does the
  cron relaunch `claude-event-watch` itself, detached.
- The watcher's own fd-based **flock singleton guard** makes that relaunch a
  guaranteed **no-op whenever a real watcher is alive** (it exits 3 on a held
  lock), so the cron can only ever win when the bus is genuinely unconsumed.
  That guarantee depends on both launchers resolving the **same lockfile**,
  which is why the lock path is a fixed location and is never derived from the
  caller's environment: cron runs without the per-user runtime directory that
  an interactive/main-loop launch has, so an environment-derived path would put
  the two launchers on two different locks and let both watchers run at once —
  exactly the outcome the guard exists to prevent.
- It **self-corrects**: once the bus drains, the next main-loop restart cycle
  re-parents a fresh in-tree watcher; a cron-parented-but-alive consumer for a
  few minutes is strictly better than a blind bus.

This respects the cardinal rule's *intent* (don't silently orphan watchers in
a detached slice) while accepting a narrow, explicit, opt-in exception for the
single failure mode that the inject-only design could not recover from. It is
**off by default** so the exception is always an operator opt-in, never silent.
(The historical daemon-side `auto_restart_watcher` was removed precisely
because it spawned EVERY watcher detached, unconditionally, in a systemd user
slice — see `src/watcher.rs`. This fallback differs on all three axes: one
watcher, only after the main-loop path fails, gated by the singleton lock.)

## 30-second rule (variable-latency ops)

Any Bash command that **might** take >30s MUST use `run_in_background:
true`. No exceptions — SSH, `gsutil` / `aws s3` uploads, long ffmpeg,
big rsyncs, etc. Blocking the foreground prevents message processing.

(Per-deployment policy may layer a stricter foreground ceiling on top —
e.g. a 15-second cap. The 30-second rule is the floor.)

This is a **main-loop** rule. A *subagent* must NOT arm a background
task and return — it is not woken when the task completes, and the work
orphans. Subagents block in the foreground with one long Bash call
(`timeout` up to 600000 ms) polling an artifact, or for CI hand off via
`pr-watch add <PR-URL>` and exit — see the `Monitor` gate in
[hooks.md](hooks.md#monitor-tool-in-subagent-context).

## Never use `&` in background commands

`run_in_background: true` already handles backgrounding. Adding `&`
double-forks: the shell exits → Claude Code loses the task handle →
`watcher-status` sees the process but Claude Code thinks the task
completed. The watcher runs as an orphan that can never deliver results.

After starting watchers, verify with a non-blocking `TaskOutput` peek
that tasks show `status: running`. If they show `completed`, the handle
was lost — kill and restart without `&`.

## Minimize background tasks — chain instead

Chain related work with `&&` rather than spawning separate background
tasks. Each background task costs a handle slot, and three sequential
tasks running on the same data can usually be expressed as one chain.

Within a single watcher domain (e.g. event surfacing) keep ONE watcher
running, not multiple. Duplicates race for inotify events and silently
drop deliveries.

## Watcher vs. producer (cron) — pick the right tool

This is a choice between the two roles in the terminology above: a **watcher**
(a supervised, main-loop-owned tool that blocks-prints-exits and gets respawned
each burst) versus an **event producer** (most often a cron job — a single-shot
script that emits a claude-event and exits, surfaced by the *one*
`claude-event-watch` watcher).

**Default to a cron producer.** Each watcher you stand up — even though every
single invocation is short-lived — costs a *supervised slot*: supervisor
overhead, restart cycles on every resume / `/clear` / compaction, DOWN-state
alerts, and mental load to track. A cron producer has none of that persistent
footprint; it just emits onto the bus that the existing watcher already
surfaces. Prefer the producer unless the criteria below genuinely require a
dedicated watcher.

### When cron is the right choice

- **Reactivity requirement is loose.** Cron's minimum resolution is one
  minute. For most health checks, promotion scans, index ticks, and
  periodic event emitters, one-minute granularity is more than sufficient.
- **Script is stateless or diffs against a tiny state file.** A script that
  runs, compares current state to a saved cursor, emits events for any delta,
  and exits cleanly is easy to reason about and safe to restart at any time.
- **Failure alerting is built in.** Wrap cron jobs with `event-cron-wrapper`
  to automatically emit a `cron-failure` claude-event on non-zero exit. No
  extra supervision logic needed.
- **Representative examples**: `cron-promote-candidates`, `tv-check`,
  `index-tick`, `cron-security-check-daily`, `cron-queue-check` — all
  periodic, stateless, fine at one-minute or coarser resolution.

### When you need a watcher instead

A dedicated watcher is justified when BOTH of these are true:

1. **Sub-minute reactivity is required** — you need to react within seconds
   of an external state change, AND
2. **No kernel event mechanism fits** — inotify, systemd path units, and
   similar facilities are not applicable for the event source.

If the event source exposes a kernel mechanism (filesystem changes, socket
events, etc.) prefer that over polling at any granularity.

### Alternatives to a new watcher process

Even when sub-minute reactivity is genuinely needed, reach for these before
spawning a new supervised watcher:

- **Kernel event facilities** (`inotifywait`, `fswatch`, systemd path units,
  eBPF) — react to filesystem or socket events with zero polling. The
  canonical `claude-event-watch` watcher is built on `inotifywait` for
  exactly this reason.
- **Extend an existing daemon** — `claude-watch` itself emits claude-events
  for queue state changes, watcher-down alerts, stale-ready detections, and
  more. If the event you need fits inside claude-watch's monitor loop,
  extend it rather than spawning a peer process.
- **Cron + internal poll loop** — a cron job that runs at the top of every
  minute and internally sleeps-and-polls for up to 59 seconds achieves
  sub-minute resolution without a new supervised process. Appropriate for
  cases where a few extra seconds of latency are tolerable and the event
  source has no kernel mechanism.

### Watchers are a tax, not a feature

Each watcher you add:

- Consumes a Claude Code background-task handle slot.
- Generates restart noise on every resume, `/clear`, and compaction.
- Triggers DOWN-state alerts when it crashes unexpectedly.
- Requires mental load to track across sessions.

Start with cron + state-diff. Convert to a watcher only when you have
empirical evidence that cron's one-minute resolution is insufficient and
none of the above alternatives apply.

**Concrete example:** `subtorrent-watch` was originally a long-running
watcher polling Transmission RPC every few seconds. It was replaced with a
`*/5` cron job: same event coverage, zero supervisor overhead, no DOWN-state
alerts, restarts trivially on resume.

## Watcher restart on resume

- **On every resume** — boot, `/clear`, restart, compaction — **kill and
  restart ALL watchers**. Background tasks survive the resume, but the
  main loop loses its handles, so the watchers become orphans that
  cannot deliver results to this session. A `mode=monitor` watcher is
  re-ARMED (Monitor tool, command from `watcher-ctl run <name>`) rather
  than re-run; it is killed by `watcher-restart` like any other.
- **Cleanup**: first `TaskStop` every known task id, THEN run
  `watcher-restart` to kill any remaining orphaned processes (reads from
  config, handles all watchers in one shot). Never use bare `pgrep -f` /
  `kill` for watcher cleanup — it misses the right children and clobbers
  the wrong ones.

## Restart watchers BEFORE acting on results

When a watcher returns results, restart it **immediately** as the first
action — before replying, processing, or doing anything else. Otherwise
the watcher is dead during the time it takes you to act on the previous
fire, and any new event in that window is lost.

For event-surfacing watchers (e.g. `claude-event-watch`), the canonical
shape is: receive the watcher's output, fire `watcher-ctl run <name>`
in parallel with the action that consumes the output, and only after
the watcher is back up should you decide what to do with the events.

## Foreground-blocking forbidden

- NEVER use blocking waits in the foreground — no `sleep 60 && ...`, no
  `TaskOutput block:true` with timeouts greater than the per-deployment
  ceiling. These freeze the CLI.
- Let background completion notifications arrive naturally (Claude Code
  auto-notifies on task end).
- Only use `TaskOutput block:true` for tasks you KNOW completed quickly.
  Otherwise `block:false` to peek, or wait for the auto-notify.
- If you need to poll something, do it in a background task — never in
  the foreground main loop.

## Self-clear and resume-prompt injection

`tools/watchers/self-clear` is the canonical helper for "inject `/clear`
plus a resume-prompt into the Claude Code tmux pane". Called as the
final step of a compact-prep procedure; eliminates the wait for the
daemon's resume-injection path to fire on its own. See
[`watchers.md`](../tools/watchers/README.md) for config.

## Self-login: re-authenticating from outside the session

`tools/watchers/self-login` is the same idea applied to `/login`. When Claude
Code's credentials lapse, the session cannot fix itself: the login screen is a
modal that swallows the loop's own keystrokes, and normally somebody has to be
sitting at the terminal. `self-login` drives the whole flow from outside, so it
works from a phone.

```
self-login start                 # inject /login, scrape the OAuth URL, publish it
self-login code <CODE>           # type the authorization code into the dialog
self-login cancel                # escape out of a dialog nobody will finish
self-login url                   # print the URL on the pane (no injection)
self-login status                # print the state file
```

`start` forks and returns immediately, exactly as `self-clear` does — when the
caller is the session itself, the TUI is busy running that very command, and
the turn has to end before the pane reaches an idle prompt. The result lands in
the state file. Pass `--foreground --json` to block and get a single JSON
object on stdout instead; that is the entry point for driving the flow
programmatically (a supervisor that notices credentials are about to lapse, for
instance) rather than by hand.

The URL is published to three independent sinks: the state file (always), a
`claude-event` when that binary is on PATH, and `$CLAUDE_SELF_LOGIN_NOTIFY_CMD`
when set (invoked as `<cmd...> <url>`). Both optional sinks may be absent
without affecting the state file.

**Failures are loud by design.** No URL can mean the session is already
authenticated, the dialog never rendered, or the pane is wedged — all things an
operator has to see. A missing URL is a non-zero exit plus a high-priority
event, never a quiet success.

Two details worth knowing if you touch this code:

- The OAuth URL is parsed by `claude-watch login-url`, which wraps
  `tmux::extract_login_url` — the same reassembler the daemon's reactive reauth
  path uses. Do not add a second copy.
- The `/login` submission DOES go through `claude-watch inject` — explicitly
  without `--escape`, so a login dialog that raced in between the pre-flight
  check and the inject is not cancelled.
- The authorization code is still typed with raw `tmux send-keys`, **not**
  `claude-watch inject`. The original reason (inject always opened with an
  Escape blast, and Escape cancels the login modal) expired when the flag
  became opt-in on 2026-08-18; others did not. Inject enters INSERT by probing
  with a literal `i` it can only un-type by seeing it on a prompt line, and a
  modal has none — so the code arrives as `i<code>`. Any configured FleetView
  focus-to-main keys are sent first and land in the modal's text field as raw
  escape sequences.

  Since 2026-08-19 inject additionally refuses to press Enter unless the prompt
  line holds its payload and nothing else. A modal has no prompt line, so that
  gate can never be satisfied there: inject backspaces its payload out and
  reports `prompt_dirty` / exit 4. That replaced the older and more dangerous
  behaviour, where the success check ("the payload cleared from the prompt
  line") was vacuous in a modal and inject reported `submitted` over the
  corrupted code. The typing defects above are unchanged, so the raw path is
  still required — but a mistake here now fails loudly instead of
  authenticating with a corrupted code.

  All of this is reproduced against a real tmux pane in
  `tools/watchers/tests/test_self_login_tmux.sh` — the typing defects via a
  `--no-submit` probe committed with a raw Enter, the refusal via a real
  `--submit`. Read those checks before trying to delete the raw path.

`cancel` exists because `start` leaves a **modal** on the pane. Until somebody
pastes the code, the dialog swallows the session's keystrokes and the loop
stops working. `cancel` presses Escape only while a dialog is actually up,
which makes it a no-op in the normal case — the property that lets the daemon
fire it on a timer without first proving anything about the pane.

Config: `$CLAUDE_SELF_LOGIN_LOG`, `$CLAUDE_SELF_LOGIN_STATE`,
`$CLAUDE_SELF_LOGIN_LOCK`, `$CLAUDE_SELF_LOGIN_NOTIFY_CMD` (each with a
matching CLI flag).

## Automatic re-login before the credentials lapse

The daemon has always had a *reactive* reauth path: notice the login screen,
inject `/login`, alert with the URL. Its problem is structural — it only ever
runs on a session that is already dead, so the recovery always lands at the
worst possible moment, and everything queued behind it has already stopped.

Claude Code warns first. Inside a three-day window it renders

```
Your login expires in 2 days · run /login to renew
```

and the daemon now acts on that (`[reauth]` in `config.toml`, second half).

**Two signals, because neither is sufficient alone.**

The pane text is the signal, but it is also just a sentence: a session reading
this document, or the tests for the detector, or the diff that added them, has
that sentence on screen while perfectly well authenticated. Auto-firing
`/login` at it would park a healthy loop in a modal. So a pane sighting is
corroborated against Claude Code's OAuth credential store, which is ground
truth and cannot be spoofed by anything on screen. If the store disagrees, the
sighting is conversation text and is dropped.

An unreadable credential store is UNKNOWN, never a negative: the pane signal
still acts, and the alert says out loud that it stands alone.

**Why the store only vetoes, and does not trigger.** The obvious symmetry —
let the credential expiry fire the path by itself, closing the gap left by the
transient notice, which is on screen for about fifteen seconds at a time — is
`expiry_from_credentials`, and it is **off by default** for a measured reason.
A refresh token can be short-lived and rolling. On one live host its entire
lifetime was under five hours, renewed silently long before it lapsed. Against
a three-day window that credential classifies as "expires in 1 day" for every
second of its healthy life, so a store-driven trigger would fire forever on a
session in no trouble at all. Check yours with `claude-watch login-expiry`
before turning it on.

That same rolling credential is why "resolved" is defined as the expiry
**moving forward**, not as it leaving the window — a token that never leaves
the window would otherwise never resolve, and the attempt budget would never
reset.

### Inspecting it

```
claude-watch login-expiry [--pane PANE] [--credentials-file PATH] [--json]
```

Read only — it never injects, types, or opens a dialog. It prints both halves
of the signal and exits 0 (nothing expiring), 3 (inside the window) or 4
(already expired, which is the reactive path's territory).

**What stops it re-firing.** The warning persists for days, so a naive detector
re-fires every poll — 8,600 times a day at a ten-second cadence. Four separate
brakes:

| Brake | Default | What it stops |
| --- | --- | --- |
| `expiry_auto_days` | 1 | Firing three days out. Claude Code shows the warning at three days but only nags inside one; one day is the right side of that line to interrupt a working session on. |
| `self_login_retry_seconds` | 3600 | Re-firing on the next poll. |
| `self_login_max_attempts` | 3 | Re-firing forever. The budget resets only when the credentials are actually renewed — never on a timer. |
| pending-dialog check | — | Firing a second `/login` into the first one's text field. |

Alerts are separately rate-limited by `alert_interval_seconds`, the same
cooldown the reactive path uses.

**And if nobody answers it.** Auto-fire at 3am publishes a URL to a sleeping
operator, and the modal it opened would hold the session until morning. The
OAuth link has a short life of its own, so waiting it out buys nothing and
costs everything: after `self_login_abandon_seconds` (default 30 minutes) the
daemon runs `self-login cancel`, the session gets its pane back, and the next
attempt produces a fresh link. Set it to 0 to leave the dialog standing.

Set `expiry_auto_self_login = false` to keep the detection and the alerting
but handle the login by hand. `expiry_watch_enabled = false` turns the whole
proactive half off; the reactive path is untouched either way.

**Known limit, stated plainly:** the on-screen signatures this path keys on
were read out of a shipped Claude Code binary, not observed during a real
expiry. The wording is verbatim and the parsing is covered by tests against
wrapped, truncated and near-miss panes, but nothing here has yet met a live
credential lapse in production.

## Automatic re-login when the access token has already lapsed

The proactive path above watches the *refresh* token, which is what Claude
Code's "login expires in N days" warning is about. There is a second, more
common way to lose the session, and it looks nothing like either of the
screens the daemon used to react to. When the short-lived OAuth *access* token
lapses and Claude Code's silent refresh does not happen, the TUI stays fully
intact — tokens footer, permission-mode banner, `❯` prompt — and one inline
line appears:

```
● Please run /login · API Error: 401 OAuth access token has expired. Re-authenticate to continue.
```

Nothing is "dead" about that screen to a detector that looks for the TUI to be
gone, and the refresh token can be weeks from lapsing, so the proactive path
has nothing to corroborate either. The session just sits there, unable to make
an API call, until a human types `/login`.

The daemon now treats that banner as phase 1 of the reactive path
(`auth_error_auto_self_login`, default on). **It is still only text**, and text
on a live pane is conversation until something off-screen says otherwise — the
same false positive the TUI guard exists for, since a session reading this
paragraph has the banner on its pane while perfectly well authenticated. So the
sighting is corroborated against the credential store's *access* token:

| On the pane | `expiresAt` on disk | Result |
| --- | --- | --- |
| banner | in the past, or no `accessToken` | **fire `self-login`** — the incident shape |
| banner | in the future | ignore outright; it is conversation text |
| banner | store unreadable | alert only, qualified as uncorroborated; never fire on UNKNOWN |

A corroborated banner goes through the **same** `fire_self_login` as the
proactive path — the same `self-login start --foreground --json`, the same
one-dialog-at-a-time latch, the same `self_login_retry_seconds` /
`self_login_max_attempts` spacing and budget, the same
`self_login_abandon_seconds` watchdog — so the two can never open a dialog on
top of each other or cancel each other's. A banner the daemon may not or can
not act on (auto off, budget spent, dialog pending, store unreadable) raises
the high-priority `reauth-needed` alert on the usual `alert_interval_seconds`
cooldown. It is never silent.

Diagnosing it from the log: the first sighting of a banner writes one
`reauth_401_banner` JSONL event carrying what the store said (`access_token`:
`expired` / `missing` / `valid` / `unknown`) and what was decided (`action`,
`reason`); a fire writes `self_login_autofire` with `trigger: "401_banner"`;
the banner leaving the pane writes `reauth_401_banner_resolved`. A banner that
resolves into a valid access token is a login that went through, and resets
the attempt budget for the next window. `claude-watch login-expiry` now prints
the access-token state next to the refresh-token one.

**Known limit:** the pane capture is the visible screen, so a banner still on
screen from an *earlier* 401 on an otherwise idle session, combined with an
access token that has since passed its `expiresAt` without Claude Code yet
being asked to refresh it, reads the same as a live failure and would fire.
The attempt budget and the abandon watchdog bound the cost; a session that is
doing anything at all scrolls the old banner off long before that.

## Tests

```
make test-watchers         # claude-event-watch fast-path + self-clear/self-login
make test-self-login-tmux  # self-login end-to-end against a throwaway tmux pane
```
