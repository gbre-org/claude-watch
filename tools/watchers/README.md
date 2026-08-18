# watchers

Watcher scripts and the `self-clear` helper that the main loop spawns as
background tasks. These are the **canonical implementations**.

## Scripts

| Script | Type | Purpose |
|--------|------|---------|
| `claude-event-watch` | bash watcher | Block on `$CLAUDE_EVENT_QUEUE` (default `~/claude-events/`); print one-liner per pending event; append full JSON to `$CLAUDE_EVENT_LOG_DIR/consumed.jsonl`; exit. The main loop re-invokes it after each delivery. |
| `self-clear` | one-shot | Inject `/clear` + a configurable resume-prompt into the Claude Code tmux pane. Final step of a compact-prep procedure; eliminates the wait for the daemon's resume-injection path to fire on its own. |
| `self-login` | one-shot | Inject `/login`, scrape the OAuth URL back out of the pane, and take the authorization code back in. Re-authenticates a session from outside it, so nobody has to be at the terminal. The daemon drives it automatically when Claude Code warns the login is about to expire — see `docs/watchers.md`. |

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
claude-event-watch [--debounce SECONDS] [--quiet SECONDS]
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
- Output shape: `EVENT[<source>/<tag>] <first-60-chars-of-message>…`
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
