# watchers

Watcher scripts and the `self-clear` helper that the main loop spawns as
background tasks. These are the **canonical implementations**.

## Scripts

| Script | Type | Purpose |
|--------|------|---------|
| `claude-event-watch` | bash watcher | Block on `$CLAUDE_EVENT_QUEUE` (default `~/claude-events/`); print one-liner per pending event; append full JSON to `$CLAUDE_EVENT_LOG_DIR/consumed.jsonl`; exit. The main loop re-invokes it after each delivery. |
| `self-clear` | one-shot | Inject `/clear` + a configurable resume-prompt into the Claude Code tmux pane. Final step of a compact-prep procedure; eliminates the wait for the daemon's resume-injection path to fire on its own. |
| `self-login` | one-shot | Inject `/login`, scrape the OAuth URL back out of the pane, and take the authorization code back in. Re-authenticates a session from outside it, so nobody has to be at the terminal. The daemon drives it automatically when Claude Code warns the login is about to expire, and when a live session hits the `API Error: 401 OAuth access token has expired` banner — see `docs/watchers.md`. |

## Watcher lifecycle (cardinal rule)

> **Watchers can ONLY ever be started by Claude Code's main loop**, via the
> Bash tool's `run_in_background: true`. Never via systemd-run, never via
> nohup, never by the daemon. The daemon's only emergency action is
> tmux-injecting a `watcher-ctl run <name>` line into the main loop's pane,
> so the main loop re-spawns the watcher itself.

`watcher-ctl`, `watcher-restart`, and `watcher-status` are dispatched by
the `claude-watch` Rust binary (multicall symlinks — see
`scripts/git-hooks/pre-commit` and `src/main.rs::multicall_rewrite_args`).

## `claude-event-watch`

```
claude-event-watch [--debounce SECONDS] [--quiet SECONDS] [--min-interval SECONDS]
                   [--mode exit|monitor] [--liveness-interval SECONDS]
```

- Once at least one event is pending (whether already queued at startup or
  freshly arrived via `inotifywait -e create -e moved_to --include
  '\.json$'`), the watcher runs an **adaptive quiet-period collect loop**:
  it polls the queue every `--quiet` seconds (default 3); each time the
  pending count grows it keeps waiting, and it drains once the count holds
  steady for a full quiet interval — or the `--debounce` hard cap
  (default 30) is reached. This coalesces a staggered burst (e.g. four
  unrelated events landing within a few seconds, or a torrent-completed
  flood) into a **single** surfaced `.output`, so the main loop's
  mandatory read-act-restart cycle fires once per window instead of once
  per event. This now applies to the fast path (backlog already pending at
  startup) too — backlog is exactly the burst the main loop would
  otherwise be forced through one event at a time.
- The collect loop only ever waits and counts — it never acks, consumes,
  hides, or reorders an event. The single drain at the end covers whatever
  is on disk; an event that lands after that drain stays on disk for the
  next run, so no event is lost.
- `--debounce 0` disables batching (surface immediately — pre-debounce
  behavior).
- Output shape (exit mode): `EVENT[<source>/<tag>] <first-60-chars-of-message>… [k=v …]`
  — monitor mode prints a richer line, see *Monitor line format* below.
- Restart banner: `WATCHER EXITED. RESTART NOW: watcher-ctl run claude-event-watch`

Per-host configuration goes in the `start_cmd` field of the watcher's
`watchers.conf` entry (what `watcher-ctl run claude-event-watch` expands
to), e.g. `claude-event-watch --debounce 10 --quiet 3`, or via the env
vars below.

Environment:

- `$CLAUDE_EVENT_QUEUE` — queue dir (default `~/claude-events/`)
- `$CLAUDE_EVENT_LOG_DIR` — log dir (default `~/.config/claude-events/`)
- `$CLAUDE_EVENT_LOG_MAX_LINES` — ring-buffer rotation threshold
  (default 10000)
- `$EVENT_WATCH_DEBOUNCE_SECONDS` — equivalent of `--debounce` (hard cap)
- `$EVENT_WATCH_QUIET_SECONDS` — equivalent of `--quiet` (quiet period)

### Delivery mode (experimental, opt-in, runtime-toggleable)

```
claude-event-watch [--mode exit|monitor | --monitor] [--liveness-interval SECONDS]
claude-event-watch --print-mode        # one word, for scripts
claude-event-watch --mode-status       # configured + live instance + pending
```

`--monitor` is shorthand for `--mode monitor`. **The supported way to flip a
deployment** is the supervision layer's `watchers.conf`: set
`claude-event-watch|mode=monitor` in the user-dir override file, then run
`watcher-ctl run claude-event-watch` — which, for a monitor-mode watcher,
does not exec the one-shot but prints the exact `Monitor` command to arm
(`claude-event-watch … --mode monitor 2>&1`, `persistent: true`). Until the
Monitor is live the watcher reads `ARMING` (healthy-pending, not DOWN) in
`watcher-ctl status` for the arming grace (default 120s), and that Monitor
call is exempt from every obligations gate (`MonitorArm`) — see
`docs/watchers.md`. The mode-file toggle below still works for a
hand-launched instance, but a conf-pinned `--mode monitor` flag wins over
it.

In monitor mode the process also **merges stderr into stdout** (only stdout
is the event stream under a line-streaming launcher, so a warning must not
be invisible), prefixes its own status lines with **`[monitor-mode]`** so
they can be told from `EVENT[...]` batches, and treats **SIGTERM / SIGINT /
SIGHUP as a clean stop**: it reaps its `inotifywait` child at once
(background + `wait`, so the signal is not deferred until the inotify
window closes), prints one
`[monitor-mode] EVENT-WATCH MONITOR STOPPED signal=… pid=… uptime=… batches=… pending=…`
line, writes the clean-exit marker, and exits 0 — no `RESTART NOW` banner,
because a signal stop (`watcher-restart`, `TaskStop`) is deliberate.
`exit` mode keeps bash's default signal disposition, byte-for-byte as before.

The block-print-exit shape above is `--mode exit` and remains **the
default** — landing this changes nothing until someone flips the switch.

| | `exit` (default) | `monitor` |
|---|---|---|
| Launcher | background Bash task (`run_in_background`) | a **line-streaming** launcher that turns each stdout line into a notification |
| Lifetime | exits after every batch | stays alive across batches |
| Cost per batch | notification **+ a restart call** | one notification |
| Restart banner | printed | not printed (nothing exited) |

**Why the exit exists, and what it actually buys.** Under a background-task
launcher, captured stdout is handed back to the session only when the process
*terminates*. So in `exit` mode the exit is not a nudge to handle the batch —
it *is* the delivery mechanism. That is why `monitor` mode is only valid under
a launcher that streams lines as they are written, and why the script refuses
to enter it when it can tell it was started by the block-print-exit supervisor
(see the guard below).

**The mode is resolved from** (first hit wins) the `--mode` flag, then
`$CLAUDE_EVENT_WATCH_MODE`, then a one-word **mode file** (default
`$CLAUDE_EVENT_LOG_DIR/mode`, override `$CLAUDE_EVENT_WATCH_MODE_FILE`), then
`exit`. Flag and env **pin** the mode for that process; only the file is a
runtime toggle, and it is re-read on **every loop iteration**:

```bash
echo monitor > ~/.config/claude-events/mode    # on
echo exit    > ~/.config/claude-events/mode    # off (default)
```

Neither direction needs a rebuild, a code revert, or a session restart, and
neither needs anything killed:

- **monitor → exit**: the live monitor notices within one loop iteration (at
  most `$EVENT_WATCH_INOTIFY_TIMEOUT`, default 30s), writes its clean-exit
  marker, prints the ordinary `WATCHER EXITED …` banner and exits — handing
  itself back to the block-print-exit cycle the surrounding loop already drives.
- **exit → monitor**: `exit` mode terminates after every batch anyway, so the
  next ordinary restart comes up in monitor mode.

An unreadable or unrecognised mode-file value **fails safe to `exit`** with a
warning, so a typo cannot silently blackhole event delivery. An explicit bad
`--mode` flag, being a direct instruction rather than ambient config, is a hard
error instead.

**Nothing about draining changes between modes.** Both call the same
`print_pending`, so the no-consume contract is identical: events are surfaced,
logged to `consumed.jsonl` and routed through `event-ack ingest`, and are never
acked on the loop's behalf. Batching, the `--min-interval` throttle and the
flock singleton are likewise untouched.

**Monitor line format.** The one thing that *does* change per mode is the
shape of each `EVENT[...]` line. In `exit` mode the main loop reads a captured
`.output` file and can open the event JSON / `consumed.jsonl` for detail, so
the one-shot line stays terse and byte-identical to before:
`EVENT[<source>/<tag>] <message, whitespace-collapsed, cut at 60 chars>… [k=v …]`.
In `monitor` mode the stdout line **is** the notification — the `Monitor`
tool's own description is static, set at arm time — so the line has to carry
the useful part itself:

```
EVENT[<source>/<tag>] <lead> — <full message> [k=v …]
```

- `EVENT[<source>/<tag>]` stays first and identical in both modes; routing
  keys on it.
- `<lead>` is a terse human headline derived from `data` where the producer's
  shape is stable — `PR #652 CI failure` / `PR #651 merged`
  (`pr-status-change`: `pr_id` + `field` + `new_value`), `torrent done: <name>`
  (`torrent-completed`), `queue done q-…: <summary>` (`queue-*`, with a
  populated reason field lifted in as ` — reason: …`), `alert FIRING
  HDDTempWarn: <summary>` / `alert RESOLVED …` (`alert-firing`/`-resolved`),
  `workload done <label> rc=0` / `hostjob done …`, `heartbeat tick`, `memory
  reminder`, `claude-watch alert <type> (<severity>)`, `watcher DOWN <name>`,
  `cron FAILED <job> rc=N`, `request fulfilled #N: <title> for <handle>`,
  `promote candidates: N ready, …`, `obligations override <id>`. A tag with no
  pattern (or one whose data lacks the fields the pattern needs) falls back to
  the full message as the lead.
- `<full message>` is the whole message, never the 60-char cut, with newline
  runs flattened to ` ⏎ `. It is omitted when the lead already covers it
  (the template-message tags above, or a message contained in the lead).
- `[k=v …]` are the same scalar data tags as the one-shot line.
- Hard cap ~400 characters per line: the data tags are truncated first, then
  the message; the prefix and a derived lead are never cut.

Before / after for the same event:

```
EVENT[cron/pr-status-change] PR gbre-org/claude-watch#652: CI failure (was: pending) [field=ci_status new_value=failure …]
EVENT[cron/pr-status-change] PR #652 CI failure — PR gbre-org/claude-watch#652: CI failure (was: pending) [field=ci_status new_value=failure old_value=pending pr_id=github:gbre-org/claude-watch#652 pr_url=…]

EVENT[claude-watch/claude-watch-alert] [CLAUDE-WATCH] Context at 90% — auto-clear pending. Commit/p… [alert_type=context-low severity=high …]
EVENT[claude-watch/claude-watch-alert] claude-watch alert context-low (high) — [CLAUDE-WATCH] Context at 90% — auto-clear pending. Commit/push in-flight work and save state NOW before compaction. [alert_type=context-low severity=high …]
```

**Making a monitor's death visible.** A monitor never exits, so silence is
ambiguous between *idle*, *wedged* and *dead* — three states that look
identical from outside. Two mechanisms disambiguate them, at different layers:

- **Dead** is caught from **outside**, by the existing supervision layer and
  with no new machinery. The flock guard writes this process's PID into
  `<runtime-dir>/claude-event-watch.lock`, and watcher liveness is resolved by
  checking that a recorded PID is a genuinely-live, cmdline-matching process —
  a check that inspects a PID, not an exit, and so is mode-independent. A
  killed monitor leaves a dead PID, no clean-exit marker, and (because the
  lockfile is written once at startup rather than per batch) no fresh
  runtime-file mtime to hold it inside the restart grace window: it reads as a
  genuine outage, which it is. The separate spool-staleness health check —
  which fires when `.json` events sit unconsumed past its window — is likewise
  mode-independent.
- **Wedged** (process alive, loop stuck) is invisible to a PID check, so it is
  caught from **inside**: after `--liveness-interval` seconds with *no stdout*
  (default 900, `0` disables) the monitor emits one
  `EVENT-WATCH ALIVE mode=monitor pid=… uptime=… batches=… since_delivery=… pending=…`
  line. The timer is reset by real deliveries, so a busy watcher never pays for
  it and the line only appears during a lull. Reporting `pending` makes an
  alive-but-not-draining loop visible rather than merely inferable.

A monitor also prints one `EVENT-WATCH MONITOR MODE ACTIVE …` line at startup,
so a launch that fails immediately is distinguishable from a quiet queue,
immediately followed by one `[monitor-mode] REPLY RULE: …` line addressed to
the operator session: because the terminal collapses each Monitor delivery to
the tool's static description, the operator's visible reply is the only
human-readable trace of what was read, so it must quote the event's lead (the
text after `EVENT[...]`, ~80 chars) plus what was done — never a bare
"Acknowledged" / "Idle". Both lines are `[monitor-mode]`-tagged status, not
events, and neither appears in `exit` mode.

**Supervised-monitor guard.** If the watcher can see that an ancestor is a
`watcher-ctl run claude-event-watch` supervisor (identified by *both* a
supervisor `/proc` comm *and* the `run <watcher>` argv, so a shell whose
command line merely contains the phrase is not a false positive), it declines
monitor mode and stays in `exit` mode with a warning — a monitor there would
drain events to a stdout nobody reads. Override with
`$CLAUDE_EVENT_WATCH_ALLOW_SUPERVISED_MONITOR=1`.

**Interaction with the unread-watcher-output gate.** The obligation that blocks
tool calls until the captured watcher output has been read is armed by a
PostToolUse sidecar written when a *background Bash task* runs `watcher-ctl run
<name>`. Monitor mode is not launched that way and produces no captured
`.output` file, so no sidecar is registered and that predicate is vacuously
satisfied rather than broken — the batch arrives inline in the conversation
instead of in a file that has to be read. In `exit` mode the gate is completely
unaffected. Note that this is a real trade: in monitor mode the *forcing* of a
read comes from the notification itself, not from a gate, so a deployment that
wants hard enforcement should seed the `event_must_act` obligation (which both
modes arm identically, via the unchanged `event-ack ingest` call).

Extra environment:

- `$CLAUDE_EVENT_WATCH_MODE` — pins the mode for one process
- `$CLAUDE_EVENT_WATCH_MODE_FILE` — mode-file path (default
  `$CLAUDE_EVENT_LOG_DIR/mode`)
- `$EVENT_WATCH_LIVENESS_INTERVAL` — equivalent of `--liveness-interval`
- `$CLAUDE_EVENT_WATCH_ALLOW_SUPERVISED_MONITOR` — bypass the supervisor guard

## `self-clear`

```
self-clear [--delay SECONDS] [--no-resume] [--timeout SECONDS]
           [--log-file PATH] [--lock-file PATH] [--resume-prompt TEXT]
```

Forks immediately so the calling tool call can complete; the child polls
the tmux pane via `claude-watch status --json`, injects `/clear` (vim-mode
sequence: `Escape, dd, i, /clear, Enter`), waits for tokens to drop below
`FRESH_SESSION_MAX_TOKENS` (30000), dismisses the post-/clear feedback
prompt, then injects the resume prompt.

Environment defaults (all overridable via flag):

- `$CLAUDE_SELF_CLEAR_LOG` — log path (default
  `$XDG_STATE_HOME/claude-watch/self-clear.log`, falling back to
  `/var/log/claude-watch/self-clear.log`)
- `$CLAUDE_SELF_CLEAR_LOCK` — lock path (default
  `$XDG_RUNTIME_DIR/claude-self-clear.lock`, falling back to
  `/var/run/claude/claude-self-clear.lock`)
- `$CLAUDE_SELF_CLEAR_RESUME_PROMPT` — resume-prompt text (default is a
  generic placeholder; override to point at a host-specific
  resume-checklist).

## `self-login`

```
self-login [--pane PANE] [--log-file PATH] [--state-file PATH]
           [--lock-file PATH] [--json]
           start [--foreground] [--url-timeout SECS] [--timeout SECS]
                 [--login-method claudeai|console|platform]
                 [--menu-attempts N] [--force]
         | code CODE [--verify-timeout SECS] [--force]
         | cancel
         | url
         | status
```

The `/login` counterpart to `self-clear`. When Claude Code's credentials lapse
the session cannot recover on its own — the login screen is a modal that
swallows the loop's own keystrokes — so this drives the flow from outside:
inject `/login`, answer the "Select login method" picker, scrape the OAuth
authorize URL off the pane, publish it, and later type the operator's
authorization code into the dialog.

`start` forks like `self-clear` and for the same reason: when the session
itself is the caller, the TUI is busy running that very command, and the turn
must end before the pane reaches an idle prompt. Results land in the state
file. `--foreground --json` blocks instead and emits one JSON object on stdout
— the entry point for driving the flow programmatically rather than by hand.

Three publication sinks, all independent:

1. the state file (always written),
2. a `claude-event` when that binary is on PATH,
3. `$CLAUDE_SELF_LOGIN_NOTIFY_CMD` when set, invoked as `<cmd...> <url>`.

**A missing URL is always an error** (exit 4 + a high-priority event), never a
quiet success: it can mean the session is already authenticated, the dialog
never rendered, or the pane is wedged, and the operator has to see all three.

`cancel` escapes out of a login dialog nobody is going to finish. It exists
because `start` leaves a **modal** on the pane: until the code arrives the
dialog swallows the session's keystrokes and the loop stops working, so a login
that goes unanswered overnight is an outage. It presses Escape only while a
dialog is actually up, so calling it on a healthy session does nothing — which
is what lets the daemon fire it on a timer without first proving the dialog is
still there.

Exit codes: `0` success (for `cancel`: the pane is not in a login dialog,
whether or not this call is what got it out of one), `1` usage / no pane /
internal error, `4` no URL, login did not complete, or the dialog would not
close, `5` a code was submitted with no login dialog on screen.

Two implementation constraints, both easy to get wrong:

- The URL is parsed by `claude-watch login-url`, which wraps
  `tmux::extract_login_url` — the same tmux-wrap reassembler the daemon's
  reactive reauth path uses. There must not be a second copy.
- The `/login` submission goes through `claude-watch inject`, explicitly
  without `--escape` so a dialog that raced in is not cancelled.
- The authorization code is typed with raw `tmux send-keys`, **not**
  `claude-watch inject`. Inject stopped leading with an Escape by default on
  2026-08-18, but three problems remain in a modal: its INSERT-mode probe
  leaves a literal `i` glued to the payload (a modal has no prompt line to
  detect it on), any configured FleetView focus-to-main keys land in the text
  field as raw escape sequences, and its "payload cleared from the prompt line"
  success check is vacuous there — so it reports success over a corrupted code.
  `tools/watchers/tests/test_self_login_tmux.sh` reproduces all three.

Environment defaults (all overridable via flag):

- `$CLAUDE_SELF_LOGIN_LOG` — log path (default
  `$XDG_STATE_HOME/claude-watch/self-login.log`)
- `$CLAUDE_SELF_LOGIN_STATE` — state path (default
  `$XDG_STATE_HOME/claude-watch/self-login.json`)
- `$CLAUDE_SELF_LOGIN_LOCK` — lock path (default
  `$XDG_RUNTIME_DIR/claude-self-login.lock`)
- `$CLAUDE_SELF_LOGIN_NOTIFY_CMD` — optional push command, invoked as
  `<cmd...> <url>`

## What's NOT here

`session-resume` is intentionally NOT migrated — it's a host-specific
resume-checklist driver that calls site-local CLIs (request tracker,
system health-check, messaging-history, etc.). The portable equivalent is the
`claude-watch hook-fire` system + the resume-prompt that `self-clear`
injects, plus whatever per-host resume-checklist the operator writes.

## Tests

```
make test-watchers          # runs all three:
python3 tools/watchers/tests/test_self_clear_config.py
python3 tools/watchers/tests/test_self_login.py
tools/watchers/tests/test_claude_event_watch.sh

make test-self-login-tmux   # end-to-end, needs tmux + a built claude-watch
tools/watchers/tests/test_self_login_tmux.sh
```

`test_self_login_tmux.sh` spins up its OWN throwaway tmux session running a
fake login screen and points `self-login --pane` at it, so the whole
inject-scrape-type path is exercised for real without going anywhere near a
live Claude Code session. It self-skips when tmux or a built binary is
missing, which is why it runs in the e2e CI job (which has both) rather than
alongside the shell tests.

`self-clear`'s end-to-end inject flow needs a live Claude Code tmux pane,
so the unit tests cover only the portable config-resolution path. The
event-watch test covers the fast-path drain + log append + malformed-event
handling.
