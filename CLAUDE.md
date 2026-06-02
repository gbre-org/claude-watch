# claude-watch

Rust daemon that monitors Claude Code health via tmux pane capture. Detects activity states (Thinking, ToolRunning, Writing, Idle), heartbeat stalls, token stalls, zombie sessions, and foreground blocks. Runs as a systemd service (`claude-watch.service`).

## Alerting hierarchy

claude-watch and its sibling tools form a **three-tier alerting hierarchy**.
Each tier escalates the intervention level over the one below it. The README
has a visual diagram and tier table — see
[`README.md` § Alerting hierarchy](README.md#alerting-hierarchy). For the
conceptual distinction between the three (and the crucial "a harness-injected
tool rejection is NOT an interruption" point), see
[`docs/concepts/event-hierarchy.md`](docs/concepts/event-hierarchy.md). The
short form:

```
events  <  obligations  <  interruptions
(mild)     (blocking)        (forced)
```

### events — informational, non-blocking

- **Mechanism**: watchers + `claude-event` CLI.
- **Path**: producer drops JSON into `~/claude-events/`; `claude-event-watch`
  debounces and surfaces an `EVENT[source/tag]` one-liner in the next
  `UserPromptSubmit` context.
- **Use when**: the right action is just "next loop pass, check this." Cron
  ticks, queue state changes, completed-torrent notifications, scheduled
  reminders, alerts that don't need to block work.
- **Do NOT use when**: ignoring the signal would let the agent proceed with
  an invariant violation. Events can be ignored — there's no enforcement
  beyond the line in context. If you need "the agent MUST handle this before
  the next destructive tool call," reach for an **obligation** instead.

### obligations — blocking guardrails

- **Mechanism**: `PreToolUse` / `PostToolUse` hooks invoking the
  `obligations` CLI.
- **Path**: a hook in `settings.json` fires on every (or matched) tool call;
  the `obligations` CLI evaluates registered predicates; a failing predicate
  returns a DENY decision and the tool call never executes. The agent must
  `obligations satisfy <id>` (after fixing the underlying state) or
  `obligations override "<reason>" --duration <ttl>` (audited, time-boxed)
  before the tool call goes through.
- **Use when**: an invariant must hold before a class of tool calls runs.
  Must-ack inbox before `signal-send`, must-read captured watcher output
  before restarting watchers, must-include queue id in `Agent` prompt,
  no-private-leakage gates on public-repo work, ack-gate enforcement.
- **Do NOT use when**: the signal is purely advisory (use an **event**), or
  when the situation is so urgent that waiting for the next tool call is too
  late (use an **interruption**). Also don't use an obligation as a soft
  reminder — predicates that DENY frequently and get bypass-overridden lose
  their audit value.

### interruptions — forced mid-generation intervention

- **Mechanism**: the `claude-watch` Rust daemon directly injects keystrokes
  into the main-loop tmux pane via `tmux send-keys`.
- **Path**: claude-watch's monitor loop detects an urgent condition (context
  usage approaching limit, dead watchers, prolonged thinking >300s, zombie
  session, token stall) and sends a prompt fragment that cancels current
  generation and forces the loop to handle the issue.
- **Use when**: letting the in-flight generation finish would make recovery
  harder or impossible. Context-window exhaustion is the canonical case —
  hitting compaction with uncommitted state is worse than canceling a
  message mid-generation.
- **Do NOT use when**: a natural turn boundary will arrive within a few
  seconds. Interruptions are disruptive — they cancel partial work. Reserve
  them for situations where the cost of NOT interrupting is higher than the
  cost of a dropped message. For routine signaling, an **event** is correct.

### Escalation

Each layer escalates the one below: events surface state, obligations enforce
invariants on state, interruptions force the loop to act on state. A correctly
designed signal lives at the lowest tier that works. Promote a signal when
the lower tier demonstrably fails (e.g. an event-only reminder that the agent
routinely ignores becomes an obligation; an obligation that consistently
fires too late becomes an interruption).

### External alerting (Prometheus / Alertmanager / etc) — out of scope

External alerting systems are **not** a fourth tier and are out of scope for
claude-watch itself. They route INTO one of the three native tiers per use
case:

- **into events** (most common): webhook handler emits a `claude-event` so
  the alert surfaces in the next `UserPromptSubmit` context.
- **into obligations**: alert state drives a predicate that blocks certain
  tool calls while firing.
- **into interruptions**: a sufficiently urgent alert triggers a
  claude-watch-driven tmux injection.

claude-watch provides the surfaces; external alerting wires INTO them.

## Build & Test

```bash
make test              # all tests in parallel (nextest if available, else cargo test)
make test-unit         # unit + fixture tests only (~0.1s)
make test-e2e          # e2e tmux tests only (~10s)
make test-live         # live e2e tests (spawn real Claude Code, ~1-2 min each)
make test-verbose      # all tests with stdout/stderr visible
make build             # release build
make deploy            # release build + systemctl restart
make install-hooks     # install git pre-commit hook
```

Or directly:
```bash
cargo nextest run                    # preferred — parallel test runner
cargo test                           # also parallel (via .cargo/config.toml RUST_TEST_THREADS)
cargo test -- --ignored              # live e2e tests only
```

## Test Parallelism

Tests run fully parallel by default. Configuration:

- **cargo-nextest** (preferred): `.config/nextest.toml` sets `test-threads = "num-cpus"` with 60s slow-timeout for e2e tests.
- **cargo test** (fallback): defaults to num_cpus threads. Override with `RUST_TEST_THREADS=1` for serial execution if debugging.
- **No ordering dependencies**: e2e tests use unique tmux session names (PID + atomic counter), so parallel execution is safe.

## Pre-commit Hook

Run `make install-hooks` to install the pre-commit hook. It sets `core.hooksPath` to the tracked `scripts/git-hooks/` dir (local to this repo, relative path — so it applies to every worktree, including fresh `git worktree add` checkouts, not just the main checkout). The hook runs two gates per commit:

1. **Warning-free release build** — `RUSTFLAGS="-D warnings" cargo build --release --tests`. Any rustc warning (dead code, unused imports, etc.) blocks the commit. Mirrors the CI `Warning-Free Build` job.
2. **Unit + fixture tests** via `cargo nextest run -E 'not binary(~e2e_)'` (~0.5s in parallel, skips e2e tests that do real sleeps).

For RED-phase TDD commits (tests that intentionally fail) or known-warning WIP, use `git commit --no-verify` to skip the hook.

Full test suite (including e2e): `cargo nextest run` (~49s, 292 tests in parallel).

## Test Categories

- **Unit tests** (`src/` inline `#[cfg(test)]`): Fast, pure logic tests. ~0s.
- **Integration tests** (`tests/e2e_*.rs`): Spawn tmux sessions with mock processes. ~7-10s each.
- **Live e2e tests** (`tests/e2e_live_detection.rs`): Spawn real Claude Code instances. `#[ignore]` by default. ~1-2 min each. Run with `--ignored`.
- **Fixture tests** (`tests/unit_activity_detection.rs`): Test `detect_activity()` against saved tmux captures. Fast, ~0s.

## Key Files

- `src/tmux.rs` — tmux pane capture, `detect_activity()`, activity state detection
- `src/daemon.rs` — main monitoring loop, heartbeat/token tracking
- `src/config.rs` — configuration loading
- `src/actions.rs` — recovery actions (inject resume, etc.)
- `tests/fixtures/` — saved tmux captures for fixture tests

## Dashboard Scripts

claude-watch monitors Claude Code by capturing its tmux pane. The `dashboard` and `dashboard-refit` scripts manage that tmux session, creating a consistent layout that claude-watch knows how to find and observe.

### `dashboard`

Creates a tmux session called `dashboard` with Claude Code in window 0 ("main").

**Default layout (no config file): one window, one full-screen pane running
Claude Code.** Nothing else. Quiet by design — claude-watch only needs the
pane to exist; everything else is opt-in.

To add companion panes / windows, drop a config file at
`~/.config/dashboard/layout.conf` (INI). Full schema in
[docs/dashboard-layout.md](docs/dashboard-layout.md). Quick example:

```ini
[main]
top_right = sidebar          # add a fixed-width pane to the right of claude
sidebar_width = 25
claude_percent = 45          # only used when top_left is also set

[windows]
monitor = glances /// sudo htop   # extra window, two panes (split by ///)
logs = journalctl -f              # extra window, single pane
```

**Layout modes** (determined by which `[main]` keys are present):
- *Neither* `top_left` *nor* `top_right`: Claude Code only (single full-screen pane). **This is the default.**
- *Only* `top_right`: side-by-side — Claude Code left, sidebar right (full height).
- *Both* `top_left` and `top_right`: three panes — two on top, Claude Code below.

**Usage:**
```bash
dashboard                # create layout or refit+attach if it exists
dashboard --recreate     # kill and rebuild the session (restarts Claude Code)
dashboard --no-attach    # build but don't attach (headless / test invocations)
dashboard --attach       # read-write attach (SSH / phone)
dashboard --attach --cc  # read-write attach (iTerm2 -CC mode)
dashboard --read-only    # safe monitoring attach
dashboard --detach       # headless start for systemd (no layout, just Claude Code)
```

**Env overrides** (mostly for tests):
- `DASHBOARD_SESSION` — tmux session name (default `dashboard`).
- `DASHBOARD_CONF` — layout config path.

### `dashboard-lib.sh`

Pure-parsing INI helpers (`conf_get`, `conf_windows`, `has_split`,
`expected_panes`) sourced by `dashboard`. No side effects. Sourced
directly by `tools/dashboard/tests/dashboard-parser.test` (33 cases).

### `dashboard-refit`

Resizes the sidebar pane to its configured width. Intended to be called by tmux hooks (`client-resized`, `client-attached`) so the sidebar stays fixed when the terminal is resized. No-op if there's no sidebar pane.

### Why these live in claude-watch

The dashboard layout determines which tmux pane claude-watch monitors. claude-watch's `[tmux]` config (`dashboard_pane`, `dashboard_session`) must match the layout these scripts create. Keeping them together prevents the layout and the monitor from drifting out of sync.

## Service Management

```bash
sudo systemctl restart claude-watch   # restart with new binary
journalctl -u claude-watch -f         # follow logs
```

Binary path: `target/release/claude-watch` (the systemd unit points here directly).
