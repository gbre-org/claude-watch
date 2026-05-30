# watchers

Watcher scripts and the `self-clear` helper that the main loop spawns as
background tasks. These are the **canonical implementations**.

## Scripts

| Script | Type | Purpose |
|--------|------|---------|
| `claude-event-watch` | bash watcher | Block on `$CLAUDE_EVENT_QUEUE` (default `~/claude-events/`); print one-liner per pending event; append full JSON to `$CLAUDE_EVENT_LOG_DIR/consumed.jsonl`; exit. The main loop re-invokes it after each delivery. |
| `self-clear` | one-shot | Inject `/clear` + a configurable resume-prompt into the Claude Code tmux pane. Final step of a compact-prep procedure; eliminates the wait for the daemon's resume-injection path to fire on its own. |

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
  `~/.local/state/claude-watch/self-clear.log`)
- `$CLAUDE_SELF_CLEAR_LOCK` — lock path (default
  `$XDG_RUNTIME_DIR/claude-self-clear.lock`, falling back to
  `/tmp/claude-self-clear.lock`)
- `$CLAUDE_SELF_CLEAR_RESUME_PROMPT` — resume-prompt text (default is a
  generic placeholder; override to point at a host-specific
  resume-checklist).

## What's NOT here

`session-resume` is intentionally NOT migrated — it's a host-specific
resume-checklist driver that calls site-local CLIs (request tracker,
system health-check, messaging-history, etc.). The portable equivalent is the
`claude-watch hook-fire` system + the resume-prompt that `self-clear`
injects, plus whatever per-host resume-checklist the operator writes.

## Tests

```
make test-watchers      # runs both:
python3 tools/watchers/tests/test_self_clear_config.py
tools/watchers/tests/test_claude_event_watch.sh
```

`self-clear`'s end-to-end inject flow needs a live Claude Code tmux pane,
so the unit tests cover only the portable config-resolution path. The
event-watch test covers the fast-path drain + log append + malformed-event
handling.
