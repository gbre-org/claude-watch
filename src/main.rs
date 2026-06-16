//! claude-watch: persistent daemon that monitors Claude Code health via tmux.
//!
//! Replaces the Python cron-based predecessor with a continuously-running
//! Rust daemon. Observes tmux pane state, Claude Code status bar, heartbeat
//! files, and watcher processes to detect and recover from stuck states.
//!
//! Features:
//!   - Dead process detection (Claude Code crashed -> restart)
//!   - Fresh /clear detection (inject resume prompt)
//!   - Heartbeat stale detection (zombie -> alert + inject)
//!   - Token-stall detection (tokens unchanged + bashes declining)
//!   - Foreground blocking detection (too long in foreground ops)
//!   - Individual watcher health monitoring
//!   - Exponential backoff on alerts
//!   - State persistence across restarts
//!   - Structured JSON logging
//!
//! Usage:
//!   claude-watch          # run the daemon (default)
//!   claude-watch status   # show Claude Code status and exit

mod active_agents;
mod agent;
mod alert;
mod cadence;
mod cmd;
mod config;
mod event_bus;
mod hook_fire;
mod inject_dispatch;
mod inject_probe;
mod logging;
mod metrics;
mod policy;
mod proc_util;
mod queue_check;
mod reminders;
mod respawn;
mod session_event;
mod stale_ready;
mod state;
mod status;
mod task_filters;
mod task_watch;
mod tmux;
mod watcher;
mod workload;

use clap::{Parser, Subcommand};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::signal::unix::{signal, SignalKind};
use tokio::time::sleep;
use tracing::info;

use config::load_config;
use logging::{write_jsonl_log, write_legacy_log};
use state::{load_state, save_state};

#[derive(Parser)]
#[command(name = "claude-watch", about = "Monitor Claude Code health via tmux")]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Show Claude Code status and exit
    Status {
        /// Output as JSON
        #[arg(long)]
        json: bool,

        /// Print just the token count
        #[arg(long)]
        tokens: bool,

        /// Print just the bash/background task count
        #[arg(long)]
        bashes: bool,
    },
    /// Trigger a version update immediately (bypass schedule)
    Update {
        /// Force update even if versions match
        #[arg(long)]
        force: bool,
    },
    /// Log session lifecycle events and show statistics
    Event {
        #[command(subcommand)]
        action: EventAction,
    },
    /// List and kill Claude Code subagents
    Agent {
        #[command(subcommand)]
        action: AgentAction,
    },
    /// Enumerate live agents (subagent PIDs + running workload labels)
    ///
    /// Read-only, fact-only output for downstream tools that want to
    /// cross-reference live processes against their own task queues. The
    /// JSON shape is `{"subagents": [pid, ...], "workloads": [label, ...]}`.
    /// claude-watch does NOT consume queue.json, scope tokens, or any
    /// caller-side schema; consumers join the data on their side.
    #[command(name = "active-agents")]
    ActiveAgents {
        /// Output as JSON (default: human-readable)
        #[arg(long)]
        json: bool,
        /// Max JSONL-mtime age (seconds) for an agent to count as alive.
        /// Subagents share the parent Claude PID, so per-subagent /proc
        /// liveness is impossible — we infer "still working" from the
        /// transcript being actively appended. Default: 120s.
        #[arg(long, default_value_t = active_agents::DEFAULT_AGENT_ALIVE_MAX_AGE_SECS)]
        max_age_seconds: u64,
        /// If set, also write the JSON output atomically to this path.
        /// Intended for cron-driven publishing to a bind-mountable
        /// location (e.g. /var/lib/claude-watch/active-agents.json) so
        /// other processes can read it without shelling out.
        #[arg(long, value_name = "PATH")]
        write_state: Option<String>,
    },
    /// Manage task-watch (background task tmux panes)
    Task {
        #[command(subcommand)]
        action: TaskAction,
    },
    /// Manage watchers (supervision, enable/disable, restart)
    Watcher {
        #[command(subcommand)]
        action: WatcherAction,
    },
    /// Launch long-running workloads in the tasks tmux session
    Workload {
        #[command(subcommand)]
        action: WorkloadAction,
    },
    /// Write Prometheus textfile metrics from claude-watch state
    Metrics,
    /// Emit a `queue-stale-ready` claude-event when one or more
    /// `session-task` queue items have been ready+pending past a
    /// threshold (default 6 minutes). Single-emit per queue id; aggregates
    /// multiple qualifying items into one event per tick.
    ///
    /// Designed for cron (every 5 minutes). Reads queue state via the
    /// `session-task` CLI; persists the per-qid emit ledger at
    /// `<state-dir>/stale-ready-state.json`. Default state dir is
    /// `/var/lib/claude-watch` to align with the in-container
    /// `active-agents.json` writer; override with --state-dir or the
    /// `CLAUDE_WATCH_STATE_DIR` env var.
    #[command(name = "stale-ready-check")]
    StaleReadyCheck {
        /// Stale-ready threshold in minutes. Items must be pending +
        /// ready for at least this long to qualify.
        #[arg(long, default_value_t = stale_ready::DEFAULT_THRESHOLD_MIN)]
        threshold_min: u64,
        /// Directory holding the per-emitter state file
        /// (`stale-ready-state.json`). Falls back to
        /// `CLAUDE_WATCH_STATE_DIR` env var, then `/var/lib/claude-watch`.
        #[arg(long, value_name = "PATH")]
        state_dir: Option<String>,
        /// Print the event JSON to stdout WITHOUT emitting a file or
        /// updating the state ledger. For inspection / testing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Emit `queue-stuck` / `queue-orphaned` claude-events when one or
    /// more `session-task` queue items are STUCK (wedged, or a running
    /// item whose heartbeat is stale) or ORPHANED (a running item whose
    /// explicitly-claimed owning PID is no longer alive).
    ///
    /// This is the IN-TREE equivalent of the out-of-tree Prometheus
    /// `WorkQueueStuckSoft` / `WorkQueueOrphaned` alert rules, so stuck/
    /// orphan detection no longer depends on an external Prometheus +
    /// alertmanager. Designed for cron (every few minutes). Single-emit
    /// per (qid, condition); persists the dedup ledger at
    /// `<state-dir>/queue-check-state.json`.
    ///
    /// **Emission is gated behind `[queue_check] emit_events` in
    /// config.toml, default FALSE** — the capability ships in every build
    /// but stays silent unless explicitly enabled. `--force-emit`
    /// overrides the config for one-shot testing; `--dry-run` prints the
    /// event JSON without emitting or touching the ledger.
    #[command(name = "queue-check")]
    QueueCheck {
        /// Heartbeat-staleness threshold (minutes) for the `stuck`
        /// condition. Overrides `[queue_check] stale_heartbeat_min`.
        #[arg(long)]
        stale_heartbeat_min: Option<u64>,
        /// Directory holding the per-emitter state file
        /// (`queue-check-state.json`). Falls back to
        /// `CLAUDE_WATCH_STATE_DIR` env var, then `/var/lib/claude-watch`.
        #[arg(long, value_name = "PATH")]
        state_dir: Option<String>,
        /// Emit events regardless of the `[queue_check] emit_events`
        /// config toggle. For one-shot testing / manual runs.
        #[arg(long)]
        force_emit: bool,
        /// Print the event JSON to stdout WITHOUT emitting a file or
        /// updating the dedup ledger. For inspection / testing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Fire a hybrid-model hook reminder (invoked by Claude Code hooks).
    ///
    /// Writes a marker to `~/.cache/claude-watch/reminders/<type>.json` so
    /// the daemon knows to defer its fallback injection, and emits a hook
    /// JSON response on stdout that injects reminder text into the
    /// conversation.
    #[command(name = "hook-fire")]
    HookFire {
        /// Reminder type: context_high | version_update | pre_compact
        kind: String,
        /// Override the hookEventName echoed in the JSON response. Defaults
        /// to a sensible per-type value (Stop / SessionStart / PreCompact).
        #[arg(long)]
        hook_event: Option<String>,
    },
    /// Probe out-of-process inject into a panel-mode Claude Code agent
    /// (Linux pidfd_getfd). Confirms the inject works; does NOT replace
    /// daemon-driven interruption logic. See `docs/sse-protocol.md` for
    /// the empirical re-probe write-up.
    #[command(name = "inject-probe")]
    InjectProbe {
        /// Target agent PID (a `claude --input-format stream-json ...`
        /// process spawned by an IDE extension or SDK caller).
        #[arg(long)]
        pid: u32,
        /// Plain UTF-8 message text. Will be wrapped in a stream-json
        /// `{"type":"user","message":...}` line before injection.
        #[arg(long)]
        text: String,
        /// Emit machine-readable JSON outcome on stdout.
        #[arg(long)]
        json: bool,
    },
    /// Type text into a Claude Code (vim-mode) tmux pane and submit it,
    /// verifying the submission actually landed.
    ///
    /// This is the ONE centralized tmux send-keys / submit choreography.
    /// Shell tooling (cw-watcher-health-check, mcp-reconnect, self-clear)
    /// MUST shell out to this subcommand instead of hand-rolling
    /// `tmux send-keys` sequences — drifted copies of that logic (one of
    /// which injected alert text WITHOUT submitting it) are the bug this
    /// subcommand exists to retire. The keystroke sequence is the proven
    /// `src/tmux.rs` path: Escape→NORMAL coercion, dd line-clear, `i`
    /// INSERT verify-and-retry, literal type, then Tab→Escape→Enter to
    /// submit (or a bare Enter for `--slash-command`).
    ///
    /// Verification: a landed submit CLEARS the payload from the prompt
    /// line. If the payload is still on the input line after the verify
    /// window, the submit did NOT land and the command exits non-zero
    /// (exit 3) so callers can detect a stuck inject. `--no-submit` types
    /// without submitting and always exits 0.
    Inject {
        /// Text to type (and, unless --no-submit, submit).
        #[arg(long, value_name = "TEXT")]
        submit: String,

        /// Target tmux pane (e.g. `claude-container:0.0`). Defaults to
        /// $CW_WATCHER_HEALTH_PANE, then $CLAUDE_WATCH_PANE, then the
        /// `[tmux] dashboard_pane` config value, then auto-detection via
        /// the claude-pane scan, then `claude-container:0.0`.
        #[arg(long, value_name = "PANE")]
        pane: Option<String>,

        /// Type the text but do NOT submit it (no Tab/Escape/Enter). Always
        /// exits 0 — there is no submission to verify.
        #[arg(long)]
        no_submit: bool,

        /// Submit as a slash command: bare Enter from INSERT mode instead of
        /// the Tab→Escape→Enter regular-text sequence. Slash commands do NOT
        /// submit via Escape→NORMAL→Enter (documented self-clear `/clear` bug).
        #[arg(long)]
        slash_command: bool,

        /// Emit machine-readable JSON outcome on stdout.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum WorkloadAction {
    /// Start a workload in tmux
    Run {
        /// Short label for the workload
        label: String,
        /// Optional queue id this workload is bound to (`q-XXXX`).
        ///
        /// First-class workload model (Andrew DM 2026-05-03 05:23 ET):
        /// when set, the `workload-done` event carries the queue id
        /// AND on workload exit the queue item is transitioned to
        /// `done` (clean rc=0) or `abandoned` (non-zero rc / killed)
        /// via `session-task`. Workload completion IS queue
        /// completion — no separate respawn dance. Resolves the
        /// q-2026-05-03-1e7d orphaned-workload bug.
        ///
        /// If neither --queue-id nor --no-queue is passed, `workload
        /// run` auto-creates a queue row (scope `workload:<label>`)
        /// and binds the workload to it — workloads are first-class
        /// queue items by default (Andrew DM 2026-05-04 21:02 ET).
        #[arg(long = "queue-id", conflicts_with = "no_queue")]
        queue_id: Option<String>,
        /// Opt out of queue auto-registration. By default, every
        /// `workload run` synthesises a queue row so the workload
        /// shows up in `session-task queue list`. Use --no-queue for
        /// short throwaway workloads that shouldn't pollute queue
        /// history, or when `session-task` isn't available.
        #[arg(long = "no-queue", conflicts_with = "queue_id")]
        no_queue: bool,
        /// Command to run (after --)
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
        cmd: Vec<String>,
    },
    /// Show running workloads
    #[command(alias = "ls")]
    List,
    /// Block until workload completes, print final output.
    ///
    /// DISABLED BY DEFAULT — workloads emit a `workload-done` claude-event
    /// on exit which surfaces in the next UserPromptSubmit context, so
    /// blocking polling is redundant. Pass
    /// `--force-i-acknowledge-events-are-better` to override.
    Wait {
        /// Workload label
        label: String,
        /// Number of output lines to show (default: 20)
        #[arg(short = 'n', long, default_value_t = 20)]
        lines: usize,
        /// Acknowledge that the `workload-done` claude-event is the
        /// preferred completion signal and you specifically want to block
        /// anyway. Required — without this flag `wait` exits non-zero.
        #[arg(long = "force-i-acknowledge-events-are-better")]
        force_i_acknowledge_events_are_better: bool,
    },
    /// Block IN-PROCESS until a workload finishes, refreshing the bound
    /// queue item's heartbeat on a timer — then return.
    ///
    /// Purpose: let an agent (or the main loop) wait on a long workload
    /// WITHOUT either (a) tight-polling via separate `workload
    /// list`/`log` calls — each a fresh LLM turn that burns thousands of
    /// tokens — or (b) blocking in one long bash call that never
    /// refreshes the queue heartbeat (so WorkQueueStuckSoft /
    /// WorkQueueOrphaned eventually fire).
    ///
    /// `babysit` waits in-process (no LLM turns) and pats the queue
    /// heartbeat (`session-task queue heartbeat <qid>`) every
    /// `--heartbeat` seconds. It returns exit 0 the moment the workload
    /// reaches `done (exit N)`, or exit 75 (EX_TEMPFAIL) after
    /// `--max-block` seconds with the workload still running — the
    /// caller then re-invokes babysit to keep waiting. `--max-block`
    /// defaults to 540s, comfortably under the Bash tool's 600s hard
    /// cap, so a single babysit call never gets killed mid-wait.
    Babysit {
        /// Workload label to wait on
        label: String,
        /// Queue id (`q-XXXX`) whose heartbeat to refresh on the timer.
        #[arg(long = "qid")]
        qid: String,
        /// Seconds between `session-task queue heartbeat` refreshes
        /// (default 60). The deployed alert thresholds give this ample
        /// margin — see the babysit doc comment in `workload.rs`.
        #[arg(long = "heartbeat", default_value_t = 60)]
        heartbeat: u64,
        /// Seconds to block before returning exit 75 so the caller can
        /// re-invoke (default 540, under the 600s Bash-tool cap).
        #[arg(long = "max-block", default_value_t = 540)]
        max_block: u64,
        /// Seconds between in-process completion polls (default 15).
        #[arg(long = "poll", default_value_t = 15)]
        poll: u64,
    },
    /// Show/tail workload output
    Log {
        /// Workload label
        label: String,
        /// Follow output (tail -f)
        #[arg(short, long)]
        follow: bool,
        /// Number of lines (default: 20)
        #[arg(short = 'n', long, default_value_t = 20)]
        lines: usize,
    },
    /// Kill a running workload
    Kill {
        /// Workload label
        label: String,
    },
    /// Internal: emit a workload-done claude-event. Called by the
    /// wrapper script after the workload exits. Hidden from `--help`.
    #[command(hide = true, name = "emit-done")]
    EmitDone {
        /// Workload label
        #[arg(long)]
        label: String,
        /// Exit code from the wrapper (negative = kill marker)
        #[arg(long)]
        exit_code: i32,
        /// Path to the workload's output log
        #[arg(long)]
        log_path: String,
        /// Set if this exit was synthesised by `workload kill`
        #[arg(long)]
        killed: bool,
        /// Queue id this workload was bound to. Baked into the
        /// wrapper script by `cmd_run` when `workload run --queue-id`
        /// was passed; empty / absent for legacy / unbound runs.
        #[arg(long = "queue-id")]
        queue_id: Option<String>,
    },
}

#[derive(Subcommand)]
enum EventAction {
    /// Log a session event (boot, compaction, restart, exit, checklist, compact-prep)
    Log {
        /// Event type
        #[arg(value_parser = ["boot", "compaction", "restart", "exit", "checklist", "compact-prep"])]
        event_type: String,
        /// Optional context note
        #[arg(long, short)]
        note: Option<String>,
        /// Override token count (default: auto-detect from tmux)
        #[arg(long)]
        tokens: Option<u64>,
    },
    /// Show summary of session events
    Stats {
        /// Time window (e.g. 1h, 2d, 30m)
        #[arg(long)]
        since: Option<String>,
    },
    /// Show compaction frequency and interval analysis
    CompactionStats {
        /// Time window (e.g. 1h, 2d, 30m)
        #[arg(long)]
        since: Option<String>,
        /// Check if daily stats DM is due (exit 0 = due, exit 1 = not due)
        #[arg(long)]
        check: bool,
    },
    /// Show recent completed tasks from session-task
    History {
        /// Time window (e.g. 1h, 2d, 30m)
        #[arg(long)]
        since: Option<String>,
        /// Number of tasks to show
        #[arg(short, long, default_value = "10")]
        count: usize,
    },
}

#[derive(Subcommand)]
enum TaskAction {
    /// Create/reinit the tasks tmux session
    Init {
        /// Show all tasks including persistent watchers
        #[arg(long, short)]
        all: bool,
        /// Detach after creating (for systemd)
        #[arg(long)]
        detach: bool,
        /// Destroy and recreate session (kills workload panes)
        #[arg(long)]
        recreate: bool,
        /// Allow --recreate even with running workloads
        #[arg(long)]
        force: bool,
    },
    /// List tracked tasks
    #[command(alias = "ls")]
    List {
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Add a tail -f pane for a task
    Add {
        /// Task ID
        id: String,
        /// Display label for the pane
        #[arg(long, short)]
        label: Option<String>,
    },
    /// Kill a task pane
    Remove {
        /// Task ID
        id: String,
        /// Show what would be removed
        #[arg(long, short = 'n')]
        dry_run: bool,
    },
    /// Garbage-collect dead/orphaned panes
    Gc,
    /// Read-only attach to the tasks session (fatfinger-proof)
    Monitor {
        /// Use tmux -CC (iTerm2 control mode)
        #[arg(long)]
        cc: bool,
    },
    /// Read-write multi-client attach
    Attach {
        /// Use tmux -CC (iTerm2 control mode)
        #[arg(long)]
        cc: bool,
    },
    /// Prefix each stdin line with [HH:MM:SS]
    #[command(name = "timestamp-lines")]
    TimestampLines,
    /// Pretty-print agent JSONL from stdin
    #[command(name = "format-jsonl")]
    FormatJsonl,
    /// Compat shim: the daemon loop runs in-process in claude-watch now.
    /// Accepts and ignores --all / --poll-interval / --done-delay / --min-display.
    #[command(hide = true)]
    Daemon {
        #[arg(long, short)]
        all: bool,
        #[arg(long)]
        poll_interval: Option<u64>,
        #[arg(long)]
        done_delay: Option<u64>,
        #[arg(long)]
        min_display: Option<u64>,
    },
}

#[derive(Subcommand)]
enum AgentAction {
    /// List agents and their processes
    #[command(alias = "ls")]
    List {
        /// Also show watcher processes
        #[arg(long, short)]
        all: bool,
    },
    /// Kill a specific agent (by ID, ID prefix, or PID)
    Kill {
        /// Agent ID (or prefix) or PID
        target: String,
        /// Show what would be killed
        #[arg(short, long)]
        dry_run: bool,
    },
    /// Kill all agent processes (not watchers)
    KillAll {
        /// Show what would be killed
        #[arg(short, long)]
        dry_run: bool,
    },
}

#[derive(Subcommand)]
enum WatcherAction {
    /// Run a watcher by name (exec start_cmd, wait for exit)
    Run {
        /// Watcher name
        name: String,
    },
    /// List configured watchers
    #[command(alias = "ls")]
    List {
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Show running status of all watchers
    Status {
        /// Output as JSON
        #[arg(long)]
        json: bool,
        /// Only emit output if at least one enabled watcher is DOWN.
        /// Stays completely silent (exit 0) when everything is healthy.
        /// Designed for the PostToolUse hook that surfaces watcher health
        /// after every tool call without spamming on healthy state.
        #[arg(long)]
        unhealthy_only: bool,
    },
    /// Enable a watcher (config flip only — main loop must spawn it).
    ///
    /// Per the cardinal rule, watchers can ONLY be started by Claude Code's
    /// main loop in its own process tree. `enable` therefore does NOT spawn
    /// the start_cmd; it only flips `enabled=true` in watchers.conf.
    /// Run `watcher-ctl run <name>` (as a main-loop background task) or
    /// `watcher-restart` afterwards to actually start the watcher.
    Enable {
        /// Watcher name
        name: String,
    },
    /// Disable a watcher (toggle + kill)
    Disable {
        /// Watcher name
        name: String,
    },
    /// Kill all enabled watcher processes and clean PID files
    Restart,
}

/// Aggregated facts rendered by `claude-watch status`.
///
/// Two clearly-labeled sections:
///   * `Claude Code:` — pane + tokens + active agents/tasks/watchers/bashes
///   * `claude-watch:` — self version + daemon service state
///
/// All fields are pure data; rendering is the responsibility of
/// [`format_status_human`] / [`status_json_value`] so the I/O entry point
/// (`run_status`) stays thin.
#[derive(Debug, Default)]
struct StatusReport {
    /// tmux pane id Claude Code is running in (`""` if pane discovery failed).
    pane: String,
    /// Token usage (0 when pane discovery failed).
    tokens: u64,
    /// Token budget from claude.max_context_tokens.
    max_tokens: u64,
    /// Open-bash count from the status bar (0 when discovery failed).
    bashes: u64,
    /// Auto-compact percentage remaining, when the status bar reported it.
    compact_remaining: Option<u32>,
    /// Claude Code version actually running (from /proc/PID/exe).
    cc_version_running: Option<String>,
    /// Claude Code version installed (from ~/.local/bin/claude symlink).
    cc_version_installed: Option<String>,
    /// Live subagent PIDs (children of the Claude PID, watchers/own-cmds excluded).
    active_agents: usize,
    /// Currently-running workload labels (tmux pane alive in `tasks` session).
    running_workloads: usize,
    /// Number of enabled watchers that are healthy (`status == "ok"`).
    healthy_watchers: u32,
    /// Number of enabled watchers (`status != "off"`). Equal to total minus
    /// disabled rows.
    enabled_watchers: u32,
    /// claude-watch's own crate version (compile-time CARGO_PKG_VERSION).
    claude_watch_version: &'static str,
    /// `Some(true)` if `claude-watch.service` reports `active`, `Some(false)`
    /// for any other state, `None` if `systemctl` is not available or the
    /// query failed.
    daemon_active: Option<bool>,
}

/// Pure: render a `StatusReport` as the human-readable two-section block.
///
/// Output shape (illustrative — exact whitespace tested below):
///
/// ```text
/// Claude Code:
///   Pane:           dashboard:0.0
///   Tokens:         467,176 / 1,000,000 (47%)
///   Compact:        42% remaining
///   Active agents:  1
///   Running tasks:  0
///   Live watchers:  4/4
///   Open bashes:    2
///
/// claude-watch:
///   Version:        0.1.0
///   Service:        active
/// ```
///
/// Fields are omitted when not applicable (e.g. `Compact:` only renders when
/// the status bar surfaced a percentage; `Pane:` is dropped if pane discovery
/// failed). The two-section split makes it impossible to confuse Claude Code's
/// version with claude-watch's own — the previous output mixed both into a
/// single `Version:` line.
fn format_status_human(r: &StatusReport) -> String {
    let mut out = String::new();
    out.push_str("Claude Code:\n");
    if !r.pane.is_empty() {
        out.push_str(&format!("  Pane:           {}\n", r.pane));
    }
    if r.tokens > 0 || !r.pane.is_empty() {
        let pct = if r.max_tokens > 0 {
            r.tokens as f64 / r.max_tokens as f64 * 100.0
        } else {
            0.0
        };
        out.push_str(&format!(
            "  Tokens:         {} / {} ({:.0}%)\n",
            format_number(r.tokens),
            format_number(r.max_tokens),
            pct
        ));
    }
    if let Some(cr) = r.compact_remaining {
        out.push_str(&format!("  Compact:        {}% remaining\n", cr));
    }
    out.push_str(&format!("  Active agents:  {}\n", r.active_agents));
    out.push_str(&format!("  Running tasks:  {}\n", r.running_workloads));
    out.push_str(&format!(
        "  Live watchers:  {}/{}\n",
        r.healthy_watchers, r.enabled_watchers
    ));
    out.push_str(&format!("  Open bashes:    {}\n", r.bashes));

    out.push_str("\nclaude-watch:\n");
    let cw_ver = r.claude_watch_version;
    let cc_running = r.cc_version_running.as_deref().unwrap_or("?");
    let cc_installed = r.cc_version_installed.as_deref().unwrap_or("?");
    // claude-watch self-version line stands alone (no "up to date" check —
    // we have no remote version source for ourselves).
    out.push_str(&format!("  Version:        {}\n", cw_ver));
    if let Some(active) = r.daemon_active {
        out.push_str(&format!(
            "  Service:        {}\n",
            if active { "active" } else { "inactive" }
        ));
    }
    // Claude Code version goes in the Claude Code section as a separate line
    // — but we put it here at the bottom of the bookkeeping so the visual
    // priority (tokens / agents / watchers / bashes) stays at the top.
    out.push_str(&format!(
        "\nClaude Code version: {} (installed: {})\n",
        cc_running, cc_installed
    ));
    if cc_running != "?" && cc_installed != "?" && cc_running != cc_installed {
        out.push_str(&format!(
            "  (running != installed — restart Claude Code to pick up {})\n",
            cc_installed
        ));
    }
    out
}

/// Build the JSON object for `claude-watch status --json`.
///
/// Backward-compat: every key from the pre-refactor shape (`pane`, `tokens`,
/// `bashes`, `compact_remaining`, `version`, `latest`) is preserved with the
/// same semantics. New keys are additive:
///   * `claude_watch_version` — claude-watch's own crate version
///   * `daemon_active` — bool when known, otherwise field is omitted
///   * `active_agents` — count of live subagent PIDs
///   * `running_workloads` — count of running workload labels
///   * `live_watchers`, `enabled_watchers` — healthy / total counts
///
/// A consumer that grepped for the old keys keeps working.
fn status_json_value(r: &StatusReport) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    if !r.pane.is_empty() {
        map.insert(
            "pane".to_string(),
            serde_json::Value::String(r.pane.clone()),
        );
    }
    if r.tokens > 0 || !r.pane.is_empty() {
        map.insert(
            "tokens".to_string(),
            serde_json::Value::Number(r.tokens.into()),
        );
    }
    if r.bashes > 0 || !r.pane.is_empty() {
        map.insert(
            "bashes".to_string(),
            serde_json::Value::Number(r.bashes.into()),
        );
    }
    if let Some(cr) = r.compact_remaining {
        map.insert(
            "compact_remaining".to_string(),
            serde_json::Value::Number(cr.into()),
        );
    }
    if let Some(ref v) = r.cc_version_running {
        map.insert("version".to_string(), serde_json::Value::String(v.clone()));
    }
    if let Some(ref v) = r.cc_version_installed {
        map.insert("latest".to_string(), serde_json::Value::String(v.clone()));
    }
    map.insert(
        "claude_watch_version".to_string(),
        serde_json::Value::String(r.claude_watch_version.to_string()),
    );
    if let Some(active) = r.daemon_active {
        map.insert(
            "daemon_active".to_string(),
            serde_json::Value::Bool(active),
        );
    }
    map.insert(
        "active_agents".to_string(),
        serde_json::Value::Number(r.active_agents.into()),
    );
    map.insert(
        "running_workloads".to_string(),
        serde_json::Value::Number(r.running_workloads.into()),
    );
    map.insert(
        "live_watchers".to_string(),
        serde_json::Value::Number(r.healthy_watchers.into()),
    );
    map.insert(
        "enabled_watchers".to_string(),
        serde_json::Value::Number(r.enabled_watchers.into()),
    );
    serde_json::Value::Object(map)
}

/// Query `systemctl is-active claude-watch.service`. Returns:
///   * `Some(true)` when the service is `active`
///   * `Some(false)` when systemctl ran but reported anything else
///     (`inactive`, `failed`, `activating`, ...)
///   * `None` when systemctl is not available or the call timed out
///
/// We deliberately don't surface the granular state: the status output is
/// for human eyeballs + the JSON consumer only cares about active/not-active.
async fn check_daemon_active() -> Option<bool> {
    let (out, _) = cmd::run_cmd_any(
        &["systemctl", "is-active", "claude-watch.service"],
        5,
    )
    .await;
    let trimmed = out.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed == "active")
    }
}

async fn run_status(json: bool, tokens_only: bool, bashes_only: bool) {
    let config = load_config();
    let max_tokens = config.claude.max_context_tokens;
    let cs = status::get_claude_status().await;

    // If no pane found, try version info only
    let cs = match cs {
        Some(cs) => cs,
        None => {
            let version_info = tokio::task::spawn_blocking(status::get_version_info)
                .await
                .unwrap_or_default();

            if version_info.running.is_none() && version_info.installed.is_none() {
                eprintln!("Could not find claude tmux pane or process.");
                std::process::exit(1);
            }

            // Minimal status with version info only
            status::ClaudeStatus {
                pane: String::new(),
                tokens: 0,
                bashes: 0,
                compact_remaining: None,
                version: version_info.running,
                latest: version_info.installed,
            }
        }
    };

    // Short-mode shortcuts: skip the agent/watcher fan-out (cheap-path).
    if tokens_only && !json {
        if cs.tokens > 0 || !cs.pane.is_empty() {
            println!("{}", cs.tokens);
        } else {
            println!("?");
        }
        return;
    }

    if bashes_only && !json {
        if cs.bashes > 0 || !cs.pane.is_empty() {
            println!("{}", cs.bashes);
        } else {
            println!("?");
        }
        return;
    }

    // Fan out the three I/O calls in parallel — all are independent and
    // each is bounded by its own pgrep/tmux/systemctl call. Total wall
    // clock stays close to the slowest single call instead of summing.
    let watcher_cfg = watcher::config_path();
    let watcher_cfg_extra = watcher::config_path_extra();
    let (agents, watchers, daemon_active) = tokio::join!(
        tokio::task::spawn_blocking(active_agents::collect),
        watcher::watcher_status(&watcher_cfg, watcher_cfg_extra.as_deref()),
        check_daemon_active(),
    );
    let agents = agents.unwrap_or(active_agents::ActiveAgents {
        subagents: Vec::new(),
        workloads: Vec::new(),
        agents: Vec::new(),
    });

    let healthy_watchers = watchers.iter().filter(|w| w.status == "ok").count() as u32;
    let enabled_watchers = watchers.iter().filter(|w| w.enabled).count() as u32;

    let report = StatusReport {
        pane: cs.pane.clone(),
        tokens: cs.tokens,
        max_tokens,
        bashes: cs.bashes,
        compact_remaining: cs.compact_remaining,
        cc_version_running: cs.version.clone(),
        cc_version_installed: cs.latest.clone(),
        active_agents: agents.subagents.len(),
        running_workloads: agents.workloads.len(),
        healthy_watchers,
        enabled_watchers,
        claude_watch_version: env!("CARGO_PKG_VERSION"),
        daemon_active,
    };

    if json {
        let value = status_json_value(&report);
        println!("{}", serde_json::to_string_pretty(&value).unwrap());
        return;
    }

    print!("{}", format_status_human(&report));
}

/// Format a number with comma separators.
fn format_number(n: u64) -> String {
    let s = n.to_string();
    let mut result = String::new();
    for (i, c) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            result.push(',');
        }
        result.push(c);
    }
    result.chars().rev().collect()
}

/// Trigger file path for signaling the daemon to run an update.
const UPDATE_TRIGGER_FILE: &str = "/tmp/claude-watch-update-trigger";

async fn run_update(force: bool) {
    // Write a trigger file for the daemon to pick up.
    // The daemon runs the actual update sequence (it survives Claude Code's exit).
    let content = if force { "force" } else { "" };
    match std::fs::write(UPDATE_TRIGGER_FILE, content) {
        Ok(_) => {
            println!(
                "Update trigger written. The daemon will pick it up within ~10s and perform the restart.{}",
                if force { " (forced)" } else { "" }
            );
        }
        Err(e) => {
            eprintln!("Failed to write trigger file: {}", e);
            std::process::exit(1);
        }
    }
}

async fn run_daemon() {
    // Set up tracing (stderr for human-readable, jsonl file handled separately)
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_target(false)
        .init();

    info!("claude-watch starting");

    let config = load_config();
    let mut state = load_state(&config.general.state_file);

    // Wire the post-escape settle delay into the tmux module's process-global
    // atomic. Default is 0 (no extra wait — fast path); see TmuxConfig for
    // when to tune it up.
    tmux::set_post_escape_settle_ms(config.tmux.post_escape_settle_ms);

    // Ensure log directory exists
    for path in [&config.general.log_file, &config.general.legacy_log_file] {
        if let Some(parent) = Path::new(path).parent() {
            let _ = std::fs::create_dir_all(parent);
        }
    }

    write_jsonl_log(
        &config.general.log_file,
        "daemon_start",
        serde_json::json!({
            "version": env!("CARGO_PKG_VERSION"),
            "check_interval": config.general.check_interval,
        }),
    );
    write_legacy_log(
        &config.general.legacy_log_file,
        &format!(
            "daemon started (v{}, interval={}s)",
            env!("CARGO_PKG_VERSION"),
            config.general.check_interval
        ),
    );

    // Signal handling
    let shutdown = Arc::new(AtomicBool::new(false));
    let reload = Arc::new(AtomicBool::new(false));

    // SIGTERM / SIGINT -> graceful shutdown
    let shutdown_flag = shutdown.clone();
    tokio::spawn(async move {
        let mut sigterm = signal(SignalKind::terminate()).expect("failed to register SIGTERM");
        let mut sigint = signal(SignalKind::interrupt()).expect("failed to register SIGINT");
        tokio::select! {
            _ = sigterm.recv() => {
                info!("received SIGTERM, shutting down");
            }
            _ = sigint.recv() => {
                info!("received SIGINT, shutting down");
            }
        }
        shutdown_flag.store(true, Ordering::Relaxed);
    });

    // SIGHUP -> reload config
    let reload_flag = reload.clone();
    tokio::spawn(async move {
        let mut sighup = signal(SignalKind::hangup()).expect("failed to register SIGHUP");
        loop {
            sighup.recv().await;
            info!("received SIGHUP, will reload config on next cycle");
            reload_flag.store(true, Ordering::Relaxed);
        }
    });

    // Spawn task-watch loop if enabled
    if config.task_watch.enabled {
        let tw_config = config.task_watch.clone();
        let tw_shutdown = shutdown.clone();
        tokio::spawn(async move {
            task_watch::run_task_watch_loop(tw_config, tw_shutdown).await;
        });
        info!("task-watch loop spawned");
    }

    // Main loop
    //
    // The loop sleeps for the foreground_monitor.check_interval (or general.check_interval
    // if foreground monitoring is disabled). Full check cycles run at general.check_interval,
    // while lightweight foreground checks run on every iteration.
    let mut current_config = config;
    let general_interval = Duration::from_secs(current_config.general.check_interval);
    let fg_interval = if current_config.foreground_monitor.enabled {
        Duration::from_secs(current_config.foreground_monitor.check_interval)
    } else {
        general_interval
    };
    let loop_interval = fg_interval.min(general_interval);
    info!(
        loop_interval_ms = loop_interval.as_millis() as u64,
        general_interval_ms = general_interval.as_millis() as u64,
        fg_interval_ms = fg_interval.as_millis() as u64,
        "main loop intervals"
    );
    let mut last_full_check = std::time::Instant::now() - general_interval; // run immediately

    // Cadence emitter: the daemon sources the `heartbeat-tick` (5min) and
    // `memory-reminder` (15min) cadence signals, replacing the out-of-tree
    // self-rescheduling reminder background task. heartbeat-tick is delivered
    // via the event queue (reminds the main loop to touch the heartbeat file);
    // memory-reminder is tmux-injected. NOTE: the daemon deliberately does NOT
    // touch the host heartbeat file itself — that remains the main loop's job
    // so a wedged loop still goes stale and trips wedge detection. See
    // `crate::cadence`.
    let mut cadence_tracker = cadence::CadenceTracker::with_intervals(
        Duration::from_secs(current_config.cadence.heartbeat_tick_interval_secs),
        Duration::from_secs(current_config.cadence.memory_reminder_interval_secs),
    );

    loop {
        if shutdown.load(Ordering::Relaxed) {
            break;
        }

        // Reload config if SIGHUP received
        if reload.load(Ordering::Relaxed) {
            reload.store(false, Ordering::Relaxed);
            info!("reloading config");
            current_config = load_config();
            // Refresh the post-escape settle delay in case the operator
            // tuned it via config.toml + SIGHUP.
            tmux::set_post_escape_settle_ms(current_config.tmux.post_escape_settle_ms);
            // Re-arm the cadence tracker in case the operator tuned the
            // intervals via config.toml + SIGHUP.
            cadence_tracker = cadence::CadenceTracker::with_intervals(
                Duration::from_secs(current_config.cadence.heartbeat_tick_interval_secs),
                Duration::from_secs(current_config.cadence.memory_reminder_interval_secs),
            );
            write_jsonl_log(
                &current_config.general.log_file,
                "config_reload",
                serde_json::json!({}),
            );
        }

        let now = std::time::Instant::now();

        // Cadence signals (heartbeat-tick / memory-reminder).
        //
        // heartbeat-tick: writes a single low-priority claude-event into the
        // event queue every 5 min. This is the reminder that prompts the main
        // loop to touch the host heartbeat file (its wedge-detector). Without a
        // delivered event the main loop has nothing to react to while idle, the
        // heartbeat goes stale at the 10-min threshold, and the daemon fires a
        // spurious "heartbeat stale" alert. A lone 5-min single-event cadence is
        // an acceptable cost: the watcher-restart treadmill is driven by event
        // *bursts* during active threads, not by one steady periodic event.
        // The daemon still does NOT touch the host heartbeat file itself — that
        // remains the main loop's job so a wedged loop is detectable.
        //
        // memory-reminder: tmux-inject the checklist directly into the pane so
        // the main loop sees it as a user-typed prompt. This is the same delivery
        // mechanism used by other daemon interventions (nudge, resume, etc.) and
        // intentionally bypasses the event queue.
        if current_config.cadence.enabled {
            let due = cadence_tracker.due(now);
            if !due.is_empty() {
                tracing::debug!(
                    heartbeat_tick = due.heartbeat_tick,
                    memory_reminder = due.memory_reminder,
                    "cadence signal(s) due"
                );
            }
            if due.heartbeat_tick {
                // Body carries the configured host heartbeat-file path so the
                // main loop knows WHICH file to touch. It is the canonical
                // `[claude].heartbeat_file` path — the same one the daemon
                // monitors for staleness — so the reminder and the detector
                // stay pinned to one user-configurable path.
                event_bus::emit_cadence(&event_bus::CadenceEvent {
                    tag: cadence::HEARTBEAT_TICK_TAG,
                    source: cadence::CADENCE_SOURCE,
                    message: "heartbeat tick",
                    priority: "low",
                    data: event_bus::heartbeat_tick_data(
                        &current_config.claude.heartbeat_file,
                        current_config.cadence.heartbeat_tick_interval_secs,
                    ),
                });
            }
            if due.memory_reminder {
                // Memory-reminder is non-urgent context hygiene, so it is
                // delivered as an AMBIENT claude-event on the queue (surfaced
                // on the next UserPromptSubmit) rather than as a
                // mid-generation tmux-inject interruption. Mirrors the
                // heartbeat-tick delivery path above; the checklist body
                // rides in the event `message`, and event-classify routes
                // claude-watch/memory-reminder to the ambient tier.
                event_bus::emit_cadence(&event_bus::CadenceEvent {
                    tag: cadence::MEMORY_REMINDER_TAG,
                    source: cadence::CADENCE_SOURCE,
                    message: cadence::MEMORY_REMINDER_CHECKLIST,
                    priority: "low",
                    data: serde_json::json!({
                        "interval_secs":
                            current_config.cadence.memory_reminder_interval_secs,
                    }),
                });
            }
        }

        // Full check cycle at general.check_interval
        if now.duration_since(last_full_check) >= general_interval {
            policy::check_cycle(&current_config, &mut state).await;
            last_full_check = now;
        } else if current_config.foreground_monitor.enabled {
            // Lightweight foreground-only check between full cycles
            let pane = state.last_known_pane.clone();
            let tokens = state.last_known_tokens;
            let bashes = state.last_known_bashes;
            tracing::debug!("foreground-only check (between full cycles)");
            policy::check_foreground(&current_config, &mut state, &pane, tokens, bashes).await;
        }

        sleep(loop_interval).await;
    }

    // Graceful shutdown
    info!("claude-watch shutting down");
    write_jsonl_log(
        &current_config.general.log_file,
        "daemon_stop",
        serde_json::json!({}),
    );
    write_legacy_log(&current_config.general.legacy_log_file, "daemon stopped");
    save_state(&current_config.general.state_file, &state);
}

async fn run_event(action: EventAction) {
    match action {
        EventAction::Log {
            event_type,
            note,
            tokens,
        } => {
            let events_file = session_event::events_file_path();
            match session_event::log_event(&event_type, note.as_deref(), tokens, &events_file).await
            {
                Ok(entry) => {
                    let token_str = entry
                        .tokens
                        .map(|t| format!("  [{} tokens]", session_event::format_number(t)))
                        .unwrap_or_default();
                    let note_str = entry
                        .note
                        .as_ref()
                        .map(|n| format!(" ({})", n))
                        .unwrap_or_default();
                    println!(
                        "Logged: {} at {}{}{}",
                        entry.event, entry.timestamp, note_str, token_str
                    );
                }
                Err(e) => {
                    eprintln!("Error: {}", e);
                    std::process::exit(1);
                }
            }
        }
        EventAction::Stats { since } => {
            let since_secs = since
                .as_deref()
                .map(session_event::parse_duration_secs)
                .transpose()
                .unwrap_or_else(|e| {
                    eprintln!("Error: {}", e);
                    std::process::exit(1);
                });
            let events_file = session_event::events_file_path();
            let events = session_event::read_events(&events_file, since_secs);
            print!("{}", session_event::format_stats(&events));
        }
        EventAction::CompactionStats { since, check } => {
            if check {
                let due = session_event::check_compaction_stats_due();
                match due {
                    session_event::CompactionStatsDue::Due(hours) => {
                        println!("DUE ({}h since last post)", hours);
                        std::process::exit(0);
                    }
                    session_event::CompactionStatsDue::NotDue(hours) => {
                        println!(
                            "Not due ({}h since last post, next in ~{}h)",
                            hours,
                            24 - hours
                        );
                        std::process::exit(1);
                    }
                    session_event::CompactionStatsDue::NeverPosted => {
                        println!("DUE (never posted)");
                        std::process::exit(0);
                    }
                    session_event::CompactionStatsDue::Error(e) => {
                        eprintln!("Error: {}", e);
                        std::process::exit(2);
                    }
                }
            }
            let since_secs = since
                .as_deref()
                .map(session_event::parse_duration_secs)
                .transpose()
                .unwrap_or_else(|e| {
                    eprintln!("Error: {}", e);
                    std::process::exit(1);
                });
            let events_file = session_event::events_file_path();
            let events = session_event::read_events(&events_file, since_secs);
            print!("{}", session_event::format_compaction_stats(&events));
        }
        EventAction::History { since: _, count } => {
            let tasks_file = session_event::completed_tasks_path();
            let tasks = session_event::read_completed_tasks(&tasks_file, count);
            print!("{}", session_event::format_history(&tasks));
        }
    }
}

async fn run_task(action: TaskAction) -> i32 {
    let config = load_config();
    let session = &config.task_watch.session;
    match action {
        TaskAction::Init {
            all,
            detach,
            recreate,
            force,
        } => {
            task_watch::cmd_task_init(session, all, detach, recreate, force).await;
            0
        }
        TaskAction::List { json } => {
            task_watch::cmd_task_list(session, json).await;
            0
        }
        TaskAction::Add { id, label } => {
            task_watch::cmd_task_add(session, &id, label.as_deref()).await
        }
        TaskAction::Remove { id, dry_run } => {
            task_watch::cmd_task_remove(session, &id, dry_run).await
        }
        TaskAction::Gc => task_watch::cmd_task_gc(session).await,
        TaskAction::Monitor { cc } => task_watch::cmd_task_monitor(session, true, cc).await,
        TaskAction::Attach { cc } => task_watch::cmd_task_monitor(session, false, cc).await,
        TaskAction::TimestampLines => task_filters::cmd_timestamp_lines(),
        TaskAction::FormatJsonl => task_filters::cmd_format_jsonl(),
        TaskAction::Daemon { .. } => {
            // Compat shim for old callers (e.g. the init pane 0 command).
            // The real daemon loop runs inside claude-watch as a tokio task
            // when `[task_watch] enabled = true` in the config.
            println!("task-watch daemon handled by claude-watch");
            // Sleep indefinitely so this behaves like the Python daemon
            // would from a process-lifetime perspective.
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
            }
        }
    }
}

async fn run_watcher(action: WatcherAction) {
    let cfg = watcher::config_path();
    let extra = watcher::config_path_extra();
    let extra_ref = extra.as_deref();
    let exit_code = match action {
        WatcherAction::Run { name } => watcher::cmd_run(&cfg, extra_ref, &name).await,
        WatcherAction::List { json } => {
            watcher::cmd_list(&cfg, extra_ref, json);
            0
        }
        WatcherAction::Status {
            json,
            unhealthy_only,
        } => {
            watcher::cmd_status(&cfg, extra_ref, json, unhealthy_only).await;
            0
        }
        WatcherAction::Enable { name } => watcher::cmd_toggle(&cfg, &name, true).await,
        WatcherAction::Disable { name } => watcher::cmd_toggle(&cfg, &name, false).await,
        WatcherAction::Restart => {
            watcher::cmd_restart(&cfg, extra_ref).await;
            0
        }
    };
    if exit_code != 0 {
        std::process::exit(exit_code);
    }
}

fn run_workload(action: WorkloadAction) -> i32 {
    match action {
        WorkloadAction::Run {
            label,
            queue_id,
            no_queue,
            mut cmd,
        } => {
            // Strip leading '--' from command remainder (shell passes it through)
            if cmd.first().map(|s| s.as_str()) == Some("--") {
                cmd.remove(0);
            }
            workload::cmd_run(&label, &cmd, queue_id.as_deref(), no_queue)
        }
        WorkloadAction::List => workload::cmd_list(),
        WorkloadAction::Wait {
            label,
            lines,
            force_i_acknowledge_events_are_better,
        } => workload::cmd_wait(&label, lines, force_i_acknowledge_events_are_better),
        WorkloadAction::Babysit {
            label,
            qid,
            heartbeat,
            max_block,
            poll,
        } => workload::cmd_babysit(&label, &qid, heartbeat, max_block, poll),
        WorkloadAction::Log {
            label,
            follow,
            lines,
        } => workload::cmd_log(&label, lines, follow),
        WorkloadAction::Kill { label } => workload::cmd_kill(&label),
        WorkloadAction::EmitDone {
            label,
            exit_code,
            log_path,
            killed,
            queue_id,
        } => workload::cmd_emit_done(
            &label,
            exit_code,
            &log_path,
            killed,
            queue_id.as_deref(),
        ),
    }
}

fn run_agent(action: AgentAction) {
    let exit_code = match action {
        AgentAction::List { all } => agent::cmd_list(all),
        AgentAction::Kill { target, dry_run } => agent::cmd_kill(&target, dry_run),
        AgentAction::KillAll { dry_run } => agent::cmd_kill_all(dry_run),
    };
    if exit_code != 0 {
        std::process::exit(exit_code);
    }
}

/// Multicall dispatch: detect argv[0] symlink name and rewrite args to the
/// appropriate subcommand. This is like busybox — `agent-ctl list` becomes
/// `claude-watch agent list`, `session-event boot` becomes
/// `claude-watch event log boot`, etc.
fn multicall_rewrite_args() -> Vec<String> {
    let args: Vec<String> = std::env::args().collect();
    let binary_name = args
        .first()
        .and_then(|a| Path::new(a).file_name())
        .and_then(|n| n.to_str())
        .unwrap_or("claude-watch");

    match binary_name {
        "agent-ctl" => {
            // agent-ctl <args...> → claude-watch agent <args...>
            let mut new_args = vec!["claude-watch".to_string(), "agent".to_string()];
            new_args.extend_from_slice(&args[1..]);
            new_args
        }
        "task-watch-ctl" | "task-watch" => {
            // task-watch <args...> → claude-watch task <args...>
            let mut new_args = vec!["claude-watch".to_string(), "task".to_string()];
            new_args.extend_from_slice(&args[1..]);
            new_args
        }
        "watcher-ctl" => {
            // watcher-ctl <args...> → claude-watch watcher <args...>
            let mut new_args = vec!["claude-watch".to_string(), "watcher".to_string()];
            new_args.extend_from_slice(&args[1..]);
            new_args
        }
        "watcher-status" => {
            // watcher-status → claude-watch watcher status
            let mut new_args = vec![
                "claude-watch".to_string(),
                "watcher".to_string(),
                "status".to_string(),
            ];
            new_args.extend_from_slice(&args[1..]);
            new_args
        }
        "workload" => {
            // workload <args...> → claude-watch workload <args...>
            let mut new_args = vec!["claude-watch".to_string(), "workload".to_string()];
            new_args.extend_from_slice(&args[1..]);
            new_args
        }
        "claude-watch-metrics" => {
            // claude-watch-metrics → claude-watch metrics
            let mut new_args = vec!["claude-watch".to_string(), "metrics".to_string()];
            new_args.extend_from_slice(&args[1..]);
            new_args
        }
        "watcher-restart" => {
            // watcher-restart → claude-watch watcher restart
            let mut new_args = vec![
                "claude-watch".to_string(),
                "watcher".to_string(),
                "restart".to_string(),
            ];
            new_args.extend_from_slice(&args[1..]);
            new_args
        }
        "session-event" => {
            // Backward compat: if first arg is a known event type, insert "log"
            // session-event boot --note X → claude-watch event log boot --note X
            // session-event stats → claude-watch event stats
            let mut new_args = vec!["claude-watch".to_string(), "event".to_string()];
            let first_arg = args.get(1).map(|s| s.as_str());
            match first_arg {
                Some("boot" | "compaction" | "restart" | "exit" | "checklist" | "compact-prep") => {
                    new_args.push("log".to_string());
                    new_args.extend_from_slice(&args[1..]);
                }
                _ => {
                    new_args.extend_from_slice(&args[1..]);
                }
            }
            new_args
        }
        _ => args,
    }
}

#[tokio::main]
/// Resolve the target pane for `claude-watch inject`, in precedence order:
/// explicit `--pane` flag > $CW_WATCHER_HEALTH_PANE > $CLAUDE_WATCH_PANE >
/// `[tmux] dashboard_pane` config (when non-empty) > auto-detection via the
/// claude-pane scan > the `claude-container:0.0` fallback the shell callers
/// historically defaulted to.
async fn resolve_inject_pane(flag: Option<&str>) -> String {
    if let Some(p) = flag {
        if !p.is_empty() {
            return p.to_string();
        }
    }
    for var in ["CW_WATCHER_HEALTH_PANE", "CLAUDE_WATCH_PANE"] {
        if let Ok(v) = std::env::var(var) {
            if !v.is_empty() {
                return v;
            }
        }
    }
    if let Ok(cfg) = config::try_load_config() {
        if !cfg.tmux.dashboard_pane.is_empty() {
            return cfg.tmux.dashboard_pane.clone();
        }
    }
    if let Some(p) = status::find_claude_pane().await {
        return p;
    }
    "claude-container:0.0".to_string()
}

/// Handler for `claude-watch inject`. Returns a process exit code:
///   0 = typed (no-submit) OR submission verified
///   3 = submit keystrokes sent but the payload was still on the prompt line
///       after the verify window (submission likely did NOT land)
async fn run_inject(
    text: &str,
    pane_flag: Option<&str>,
    no_submit: bool,
    slash_command: bool,
    json: bool,
) -> i32 {
    let pane = resolve_inject_pane(pane_flag).await;
    let submit = !no_submit;
    let outcome = tmux::inject_and_verify(&pane, text, submit, slash_command).await;

    let (code, status) = match outcome {
        tmux::InjectOutcome::Typed => (0, "typed"),
        tmux::InjectOutcome::Submitted => (0, "submitted"),
        tmux::InjectOutcome::SubmitUnverified => (3, "submit_unverified"),
    };

    if json {
        println!(
            "{{\"pane\":{},\"status\":\"{}\",\"submitted\":{},\"slash_command\":{}}}",
            serde_json::to_string(&pane).unwrap_or_else(|_| "\"\"".to_string()),
            status,
            submit,
            slash_command
        );
    } else if code == 0 {
        eprintln!("[claude-watch inject] {} on pane {}", status, pane);
    } else {
        eprintln!(
            "[claude-watch inject] WARNING: submit may not have landed (payload still on prompt line) on pane {}",
            pane
        );
    }

    code
}

async fn main() {
    // Restore default SIGPIPE handling so piping to `head` etc. exits
    // cleanly instead of panicking on println! with a broken pipe.
    // Safe: we only call this once at startup before any async work.
    // See https://github.com/rust-lang/rust/issues/46016
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }

    let args = multicall_rewrite_args();
    let cli = Cli::parse_from(args);

    match cli.command {
        Some(Commands::Status {
            json,
            tokens,
            bashes,
        }) => {
            run_status(json, tokens, bashes).await;
        }
        Some(Commands::Update { force }) => {
            run_update(force).await;
        }
        Some(Commands::Event { action }) => {
            run_event(action).await;
        }
        Some(Commands::Agent { action }) => {
            run_agent(action);
        }
        Some(Commands::ActiveAgents {
            json,
            max_age_seconds,
            write_state,
        }) => {
            let code = active_agents::cmd_active_agents(
                json,
                max_age_seconds,
                write_state.as_deref(),
            );
            if code != 0 {
                std::process::exit(code);
            }
        }
        Some(Commands::Task { action }) => {
            let code = run_task(action).await;
            if code != 0 {
                std::process::exit(code);
            }
        }
        Some(Commands::Watcher { action }) => {
            run_watcher(action).await;
        }
        Some(Commands::Workload { action }) => {
            let code = run_workload(action);
            if code != 0 {
                std::process::exit(code);
            }
        }
        Some(Commands::Metrics) => {
            let code = metrics::cmd_metrics().await;
            if code != 0 {
                std::process::exit(code);
            }
        }
        Some(Commands::StaleReadyCheck {
            threshold_min,
            state_dir,
            dry_run,
        }) => {
            let code = stale_ready::cmd_stale_ready_check(
                threshold_min,
                state_dir.as_deref(),
                dry_run,
            );
            if code != 0 {
                std::process::exit(code);
            }
        }
        Some(Commands::QueueCheck {
            stale_heartbeat_min,
            state_dir,
            force_emit,
            dry_run,
        }) => {
            // Resolve the stale-heartbeat threshold: explicit CLI flag wins,
            // else the `[queue_check] stale_heartbeat_min` config value,
            // else the built-in default. config load is best-effort (the
            // subcommand must still run on a host without a config file).
            let stale_min = stale_heartbeat_min.unwrap_or_else(|| {
                config::try_load_config()
                    .map(|c| c.queue_check.stale_heartbeat_min)
                    .unwrap_or(queue_check::DEFAULT_STALE_HEARTBEAT_MIN)
            });
            let code = queue_check::cmd_queue_check(
                stale_min,
                state_dir.as_deref(),
                force_emit,
                dry_run,
            );
            if code != 0 {
                std::process::exit(code);
            }
        }
        Some(Commands::HookFire { kind, hook_event }) => {
            let code = hook_fire::cmd_hook_fire(&kind, hook_event.as_deref()).await;
            if code != 0 {
                std::process::exit(code);
            }
        }
        Some(Commands::InjectProbe { pid, text, json }) => {
            let code = inject_probe::cmd_inject_probe(pid, &text, json);
            if code != 0 {
                std::process::exit(code);
            }
        }
        Some(Commands::Inject {
            submit,
            pane,
            no_submit,
            slash_command,
            json,
        }) => {
            let code = run_inject(&submit, pane.as_deref(), no_submit, slash_command, json).await;
            if code != 0 {
                std::process::exit(code);
            }
        }
        None => {
            run_daemon().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_report() -> StatusReport {
        StatusReport {
            pane: "dashboard:0.0".to_string(),
            tokens: 467_176,
            max_tokens: 1_000_000,
            bashes: 2,
            compact_remaining: None,
            cc_version_running: Some("2.1.126".to_string()),
            cc_version_installed: Some("2.1.126".to_string()),
            active_agents: 1,
            running_workloads: 0,
            healthy_watchers: 4,
            enabled_watchers: 4,
            claude_watch_version: "0.1.0",
            daemon_active: Some(true),
        }
    }

    #[test]
    fn format_status_human_two_section_headers() {
        let out = format_status_human(&base_report());
        // The whole point of the refactor: two clearly-labeled sections.
        // If either header disappears, the output collapses back into the
        // ambiguous pre-refactor form.
        assert!(out.starts_with("Claude Code:\n"), "out:\n{}", out);
        assert!(out.contains("\nclaude-watch:\n"), "out:\n{}", out);
    }

    #[test]
    fn format_status_human_includes_all_runtime_counts() {
        let out = format_status_human(&base_report());
        // Each labeled count must be present and attributed to Claude Code.
        // These four lines are the new active-agents-derived data the
        // pre-refactor output completely lacked.
        assert!(out.contains("Active agents:  1"), "out:\n{}", out);
        assert!(out.contains("Running tasks:  0"), "out:\n{}", out);
        assert!(out.contains("Live watchers:  4/4"), "out:\n{}", out);
        assert!(out.contains("Open bashes:    2"), "out:\n{}", out);
    }

    #[test]
    fn format_status_human_separates_versions() {
        // The bug we're fixing: pre-refactor output had a single `Version:`
        // line that mixed Claude Code's version with no marker for the
        // claude-watch version. Now the claude-watch section gets its own
        // `Version:` and Claude Code gets a dedicated line elsewhere.
        let out = format_status_human(&base_report());
        // claude-watch version under its section header.
        let cw_idx = out.find("claude-watch:\n").expect("claude-watch section");
        let after_cw = &out[cw_idx..];
        assert!(
            after_cw.contains("Version:        0.1.0"),
            "claude-watch section must show 0.1.0; out:\n{}",
            out
        );
        // Claude Code version is on its own line, clearly attributed.
        assert!(
            out.contains("Claude Code version: 2.1.126"),
            "Claude Code version must be attributed; out:\n{}",
            out
        );
    }

    #[test]
    fn format_status_human_service_active_inactive() {
        let mut r = base_report();
        r.daemon_active = Some(true);
        assert!(format_status_human(&r).contains("Service:        active"));
        r.daemon_active = Some(false);
        assert!(format_status_human(&r).contains("Service:        inactive"));
        // None means systemctl wasn't queryable — drop the line entirely
        // rather than print a misleading "unknown".
        r.daemon_active = None;
        assert!(!format_status_human(&r).contains("Service:"));
    }

    #[test]
    fn format_status_human_omits_pane_when_empty() {
        let mut r = base_report();
        r.pane = String::new();
        let out = format_status_human(&r);
        assert!(!out.contains("Pane:"), "pane line should be omitted; out:\n{}", out);
    }

    #[test]
    fn format_status_human_compact_only_when_present() {
        let mut r = base_report();
        assert!(!format_status_human(&r).contains("Compact:"));
        r.compact_remaining = Some(42);
        assert!(format_status_human(&r).contains("Compact:        42% remaining"));
    }

    #[test]
    fn format_status_human_tokens_pct_calculation() {
        let r = base_report();
        // 467176 / 1000000 = 46.7176%, rounds to 47%.
        let out = format_status_human(&r);
        assert!(out.contains("Tokens:         467,176 / 1,000,000 (47%)"), "out:\n{}", out);
    }

    #[test]
    fn format_status_human_running_vs_installed_mismatch() {
        let mut r = base_report();
        r.cc_version_running = Some("2.1.125".to_string());
        r.cc_version_installed = Some("2.1.126".to_string());
        let out = format_status_human(&r);
        assert!(out.contains("Claude Code version: 2.1.125 (installed: 2.1.126)"));
        // The mismatch hint nudges the user to restart Claude Code.
        assert!(out.contains("running != installed"), "out:\n{}", out);
    }

    #[test]
    fn format_status_human_versions_match_no_warning() {
        let r = base_report();
        let out = format_status_human(&r);
        assert!(!out.contains("running != installed"));
    }

    #[test]
    fn status_json_value_preserves_pre_refactor_keys() {
        // Backward compat: every key the old shape produced must still be
        // present with the same name. Adding new keys is fine; renaming or
        // dropping any of these breaks downstream cron shims that grep for
        // specific fields.
        let r = base_report();
        let v = status_json_value(&r);
        assert_eq!(v["pane"], serde_json::json!("dashboard:0.0"));
        assert_eq!(v["tokens"], serde_json::json!(467_176));
        assert_eq!(v["bashes"], serde_json::json!(2));
        assert_eq!(v["version"], serde_json::json!("2.1.126"));
        assert_eq!(v["latest"], serde_json::json!("2.1.126"));
    }

    #[test]
    fn status_json_value_adds_new_count_keys() {
        let r = base_report();
        let v = status_json_value(&r);
        assert_eq!(v["claude_watch_version"], serde_json::json!("0.1.0"));
        assert_eq!(v["daemon_active"], serde_json::json!(true));
        assert_eq!(v["active_agents"], serde_json::json!(1));
        assert_eq!(v["running_workloads"], serde_json::json!(0));
        assert_eq!(v["live_watchers"], serde_json::json!(4));
        assert_eq!(v["enabled_watchers"], serde_json::json!(4));
    }

    #[test]
    fn status_json_value_omits_daemon_active_when_unknown() {
        let mut r = base_report();
        r.daemon_active = None;
        let v = status_json_value(&r);
        assert!(
            !v.as_object().unwrap().contains_key("daemon_active"),
            "daemon_active must be omitted (not null) when systemctl is unavailable"
        );
    }

    #[test]
    fn status_json_value_omits_pane_when_empty() {
        let mut r = base_report();
        r.pane = String::new();
        r.tokens = 0;
        r.bashes = 0;
        let v = status_json_value(&r);
        let obj = v.as_object().unwrap();
        // pane / tokens / bashes all skipped when there's no Claude pane
        // and no positive count — matches the pre-refactor behavior.
        assert!(!obj.contains_key("pane"));
        assert!(!obj.contains_key("tokens"));
        assert!(!obj.contains_key("bashes"));
    }

    #[test]
    fn format_status_human_unhealthy_watchers_visible_in_count() {
        let mut r = base_report();
        r.healthy_watchers = 3;
        r.enabled_watchers = 4;
        let out = format_status_human(&r);
        assert!(out.contains("Live watchers:  3/4"), "out:\n{}", out);
    }
}
