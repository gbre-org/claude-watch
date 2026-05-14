<p align="center">
  <img src="docs/logo.png" alt="claude-watch" width="160">
</p>

# claude-watch

A Rust daemon that monitors [Claude Code](https://claude.ai/code) sessions running in tmux. Detects activity states, recovers from stalls, and manages the tmux layout.

## Quick start

Fresh-laptop path (Docker, no native install required):

```bash
git clone https://github.com/hndrewaall/claude-watch.git
cd claude-watch
make bootstrap              # checks prereqs, clones eichi sibling, seeds .env
# edit examples/compose/.env (set ANTHROPIC_API_KEY)
make compose-up             # docker compose up against examples/compose/
```

Open <http://localhost:8000/> for the queue UI and <http://localhost:8001/>
for semantic search. Full walkthrough (caveats, sibling-repo layout,
first-run indexing): [`examples/compose/README.md`](examples/compose/README.md).

Native install (build from source):

```bash
make build                  # cargo build --release
make install                # copies daemon + tools into $BIN_DIR (default ~/bin)
make install-hooks          # opt-in: warning-free build + unit-tests pre-commit gate
```

Prerequisites: `cargo` + `rustc` (1.74+), `tmux`, Python 3.11+ for the
`tools/` scripts.

Agent-onboarding files at the repo root (`CLAUDE.md`, `AGENTS.md`,
`.cursorrules`, `.github/copilot-instructions.md`) all point to a single
canonical source: [`CLAUDE.md`](CLAUDE.md). Drop a coding agent in this
repo and it will know the build + test loop without further setup.

## What it does

claude-watch captures the Claude Code tmux pane every few seconds and parses it to determine what Claude is doing:

- **Activity detection**: Thinking, Writing, ToolRunning, Idle, ForegroundBash, ShellPrompt
- **Health monitoring**: Detects zombie sessions (no heartbeat), token stalls (context exhaustion), prolonged thinking, and foreground blocks
- **Recovery actions**: Injects prompts to resume stalled sessions, triggers context clears, sends push-notification alerts (via a pluggable `pingme` shim — wire it to whatever notification service you prefer)
- **Fresh session detection**: Detects when Claude Code starts fresh (via `dashboard --recreate --fresh`) and injects a resume prompt
- **Task monitoring**: Watches Claude Code's background task output files, tracks agent lifecycle, cleans up orphaned tmux panes

## Architecture

```
claude-watch (systemd service)
    |
    +-- main loop (3s interval)
    |       Captures tmux pane -> detect_activity() -> policy decisions
    |       Tracks: tokens, bashes, dead checks, thinking duration
    |
    +-- task-watch loop (5s interval)
    |       Monitors Claude Code's task output directory via inotify
    |       Tracks task lifecycle, cleans up done tasks
    |
    +-- dashboard / dashboard-refit (shell scripts)
            Creates and manages the tmux session layout
```

### Key modules

| Module | Purpose |
|--------|---------|
| `tmux.rs` | Pane capture, `detect_activity()`, key injection |
| `policy.rs` | Decision engine: when to alert, inject, recover |
| `state.rs` | Persistent state (JSON): dead checks, inject flags, history |
| `status.rs` | Status bar parsing (tokens, bashes, compact %) |
| `task_watch.rs` | Background task and agent lifecycle monitoring |
| `alert.rs` | Push notifications (via the `pingme` shim) |
| `config.rs` | TOML configuration |

### Dashboard scripts

The `dashboard` script creates a tmux session with Claude Code and optional companion panes. Layout is configured via `~/.config/dashboard/layout.conf`:

```ini
[main]
top_right = sidebar        # fixed-width right pane
sidebar_width = 25
claude_percent = 45        # claude pane height %

[windows]
monitor = glances /// htop   # extra window, panes split by ///
logs = journalctl -f         # single-pane window
```

## Hybrid hooks + daemon fallback

claude-watch ships a **hybrid model** that pairs conversational reminders
(Claude Code hooks) with the daemon's tmux-injecting fallback:

- **Primary path — hooks.** Three Claude Code hooks call
  `claude-watch hook-fire <type>` on the relevant trigger and inject a
  reminder directly into the conversation:

  | Hook | When | Reminder |
  |---|---|---|
  | `SessionStart` (`startup\|resume`) | new Claude Code version installed | "Version X → Y available, run /restart" |
  | `Stop` | context usage > 80% | "Context at N%, consider /clear" |
  | `PreCompact` (`auto`) | auto-compaction is about to run | blocks, suggests /clear |

- **Fallback path — daemon.** For each reminder, the daemon records a
  timestamped marker in `~/.cache/claude-watch/reminders/<type>.json`.
  Before the daemon falls back to injecting `/clear` or `claude update`
  via tmux, it checks whether a matching reminder fired within the
  configured grace window (default 5 min for `/clear`, 15 min for
  `claude update`). If it did, the daemon defers; if the reminder is
  stale, the daemon proceeds with the tmux fallback and bumps the
  `fallback_*_count` metric.

### Installing the hooks

See [`skills/setup-hooks.md`](skills/setup-hooks.md). Summary:

```
/setup-claude-watch-hooks install                # global ~/.claude/settings.json
/setup-claude-watch-hooks --scope project install  # .claude/settings.json
/setup-claude-watch-hooks uninstall
```

### Tuning

```toml
# ~/.config/claude-watch/config.toml
[hybrid]
enabled = true                   # master switch (default: true)
context_fallback_secs = 300      # wait 5 min after context_high hook before /clear fallback
version_fallback_secs = 900      # wait 15 min after version_update hook before claude update fallback
```

### Observability

`claude-watch metrics` exports:

- `claude_watch_reminder_fires_total{type=...}` — how often hooks fired
  (counter, labels: `context_high`, `version_update`, `pre_compact`)
- `claude_watch_fallback_injections_total{type=...}` — how often the
  daemon fell back to tmux injection (labels: `clear`, `update`)
- `claude_watch_reminder_to_action_latency_seconds_{sum,count}{type=...}`
  — histogram-style counters for the delay between reminder and the
  self-action (context drop / version match) landing.

Ratio `fallback_injections_total / reminder_fires_total` = how often
Claude ignored the conversational hint.

## What it doesn't do

claude-watch monitors the session and recovers from failures, but it has no memory of what Claude was working on. It can detect "Claude is idle" or "Claude is stuck," but it can't tell Claude *what to resume*.

The repo also ships a set of supporting CLIs and hook scripts under
`tools/` that together with the daemon form a more complete session-
continuity layer. They live here because they're tightly coupled to the
daemon's contract (queue + obligation predicates + claude-event bus +
watcher lifecycle), and shipping them in the same public repo makes
fresh deployments self-contained.

| Subsystem | Path | Purpose |
|-----------|------|---------|
| `session-task` | [`tools/session-task/`](tools/session-task/) | Cross-session work-queue + resume-action CLI. See [`docs/queue.md`](docs/queue.md). |
| `obligations` | [`tools/obligations/`](tools/obligations/) | Generic "must do X before Y" gate; bounded predicate vocabulary. |
| Hook scripts | [`tools/hooks/`](tools/hooks/) | PreToolUse / PostToolUse hooks that wire the queue + obligations gate into Claude Code's hook contract. See [`docs/hooks.md`](docs/hooks.md). |
| `agent-msg` | [`tools/agent-msg/`](tools/agent-msg/) | Async-messaging CLI for delivering inbox messages to running subagents via the obligations gate. See [`docs/agent-msg.md`](docs/agent-msg.md). |
| `claude-event` + `claude-event-tail` | [`tools/claude-event/`](tools/claude-event/) | Source-agnostic JSON event bus (emitter + ring-buffer reader). See [`docs/events.md`](docs/events.md). |
| `claude-event-watch` + `self-clear` | [`tools/watchers/`](tools/watchers/) | Watcher script (inotify-blocking event surfacer) and the `/clear` + resume-prompt injector. See [`docs/watchers.md`](docs/watchers.md) for operator hygiene and [`docs/adding-watchers.md`](docs/adding-watchers.md) for authoring a custom watcher. |
| `queue-minisite` | [`queue-minisite/`](queue-minisite/) | Mobile-friendly Flask UI for the `session-task` work queue. Renders running/pending/blocked items with Stop / Abandon / Force-start buttons. Designed to sit behind an upstream auth proxy. See [`queue-minisite/README.md`](queue-minisite/README.md). |
| `container` | [`container/`](container/) | Containerized deployment of Claude Code + the `claude-watch` daemon + tmux as a single Docker image, plus a host-side `claude-tmux` wrapper with bind mounts, env passthrough, POSIX signal handling, and TTY. Lets the same Claude Code environment run identically on Linux servers and macOS work laptops. See [`container/README.md`](container/README.md). |

`make install` builds the daemon and copies all of the above into
`$BIN_DIR` (default `~/bin/`). Each subsystem has its own README, tests
under `tests/`, and a public-facing reference doc in `docs/`.

What the daemon plus tools still don't cover by design: a host-specific
resume checklist, a request tracker, external messaging integrations,
and any other site-specific surface. Those belong in your own dotfiles
or ops repo and call into these tools as primitives.

## Build & run

```bash
make test                # all Rust tests in parallel
make test-session-task   # session-task pytest suite
make test-hooks          # obligations + queue PreToolUse hook tests
make test-agent-msg      # agent-msg embedded --test (38 cases)
make test-claude-event   # claude-event + claude-event-tail unit tests
make test-watchers       # claude-event-watch fast-path + self-clear config

make build               # release build
make install             # build + copy daemon + tools into $BIN_DIR (default ~/bin/)
make deploy              # build + systemctl restart
make install-hooks       # install the git pre-commit hook (warnings + tests)
```

### Pre-commit hook

`make install-hooks` symlinks
[`scripts/git-hooks/pre-commit`](scripts/git-hooks/pre-commit) into
`.git/hooks/pre-commit`. The hook runs two gates before each commit:

1. **Warning-free release build** — `RUSTFLAGS="-D warnings" cargo build --release --tests`. Any rustc warning (dead code, unused imports, etc.) blocks the commit.
2. **Unit + fixture tests** via `cargo nextest run -E 'not binary(~e2e_)'` (~0.5s in parallel).

Bypass with `git commit --no-verify` for RED-phase TDD commits. CI runs the same warning-free build gate (`Warning-Free Build` job in [`.github/workflows/ci.yml`](.github/workflows/ci.yml)) on every PR + push to main.

## Configuration

`~/.config/claude-watch/config.toml`:

```toml
[tmux]
dashboard_session = "dashboard"
dashboard_pane = ""   # auto-detected from /var/run/claude/pane-id

[tasks]
# Claude Code writes background task output here. claude-watch auto-discovers
# the path by scanning /proc for the Claude Code process, but you can override:
# tasks_dir = "/run/user/1000/claude/tasks"

[thresholds]
dead_process_checks = 5        # consecutive dead checks before action
thinking_interrupt_secs = 180  # prolonged thinking threshold
fg_block_secs = 15             # foreground bash block threshold

[alerts]
# Push notifications are delegated to an external `pingme` shim on
# PATH. Configure your notification service of choice (Pushover,
# ntfy, Apprise, a homebrew script, etc.) by providing a `pingme`
# executable that accepts `pingme [-p PRIORITY] <message> [title]`.
# Cap the number of pings per stuck-state to avoid notification storms:
max_pingme_alerts = 6
```

Claude Code stores task output in `/tmp/claude-<UID>/<HOME>/UUID/tasks/`. claude-watch auto-discovers this path via `/proc/<PID>/fd` scanning. The path changes on every Claude Code restart (new UUID), so auto-discovery is the default. A manual override (`tasks_dir`) is useful for testing or non-standard setups.

## License

MIT
