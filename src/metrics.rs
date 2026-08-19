//! metrics — write Prometheus textfile metrics for node-exporter.
//!
//! Rust port of `claude-watch-metrics` (Python). Reads
//! `~/.config/claude-watch/state.json` and writes
//! `/var/lib/node-exporter/textfile/claude_watch.prom` atomically.
//!
//! Run from cron every minute:
//!     * * * * * /path/to/claude-watch metrics

use crate::reminders::all_fire_counts;
use crate::status::get_version_info;
use chrono::{DateTime, Local, NaiveDate};
use serde_json::Value;
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

const PROM_FILE: &str = "/var/lib/node-exporter/textfile/claude_watch.prom";

/// Resolve the node-exporter textfile output path. Honors the
/// `CLAUDE_WATCH_PROM_FILE` env override so containerized / macOS deployments
/// — where the hardcoded `/var/lib/node-exporter/textfile` dir isn't writable
/// (needs root; often doesn't exist) and node-exporter mounts a DIFFERENT dir
/// — can point the emitter straight at the dir node-exporter actually scrapes.
/// Without this, such deployments must run a separate out-of-tree reimplementation
/// of `build_metrics` to bridge the textfile into the scraped dir; that bridge
/// then silently drifts from this source (e.g. it lacked the
/// `claude_operator_desk_streak_seconds` gauge). Falls back to `PROM_FILE`.
fn prom_file_path() -> PathBuf {
    match std::env::var("CLAUDE_WATCH_PROM_FILE") {
        Ok(s) if !s.trim().is_empty() => PathBuf::from(s),
        _ => PathBuf::from(PROM_FILE),
    }
}

/// Live-process snapshot collected at metrics-emission time.
///
/// Mirrors the four counts in `claude-watch status`'s "Claude Code" section:
/// active agents, running tasks (workloads), live + enabled watcher counts,
/// and open bashes. Singletons — there's only one Claude Code on this host.
/// Kept as a plain struct so `build_metrics` stays a pure function (no I/O).
#[derive(Debug, Default, Clone, Copy)]
pub struct LiveCounts {
    /// Live subagent PIDs (children of the Claude PID, watchers/own-cmds excluded).
    pub active_agents: u32,
    /// Currently-running workload labels (tmux pane alive in `tasks` session).
    pub running_tasks: u32,
    /// Number of enabled watchers that are healthy (`status == "ok"`).
    pub live_watchers: u32,
    /// Number of enabled watchers (config rows with `enabled=true`).
    pub enabled_watchers: u32,
    /// Open-bash count parsed from Claude Code's status bar.
    pub open_bashes: u32,
}

fn default_state_file() -> PathBuf {
    if let Ok(s) = std::env::var("CLAUDE_WATCH_STATE") {
        if !s.trim().is_empty() {
            return PathBuf::from(s);
        }
    }
    // Fall back to the daemon-configured `general.state_file` so the metrics
    // reader points at the SAME file the daemon writes. The historical
    // hardcoded `~/.config/claude-watch/state.json` default silently diverged
    // from container deployments, where the baked config.toml sets
    // `~/.cache/claude-watch/state.json`; the mismatch made the emitter read a
    // nonexistent file and bail to `down_metrics` (claude_watch_up 0). Reading
    // the config here is the same pattern `cmd_metrics` already uses for
    // `claude.heartbeat_file`. Falls through to the legacy default only when no
    // config is loadable (fresh/bootstrap host).
    if let Ok(cfg) = crate::config::try_load_config() {
        let sf = cfg.general.state_file.trim().to_string();
        if !sf.is_empty() {
            return PathBuf::from(sf);
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/root".to_string());
    PathBuf::from(home).join(".config/claude-watch/state.json")
}

/// Parse an ISO 8601 timestamp into epoch seconds (float).
/// Returns 0.0 on failure — matches Python behavior.
fn parse_iso_timestamp(ts: &str) -> f64 {
    let ts = ts.trim();
    if ts.is_empty() {
        return 0.0;
    }
    // Try RFC3339 first (covers +HH:MM offsets that Rust's chrono handles)
    if let Ok(dt) = DateTime::parse_from_rfc3339(ts) {
        return dt.timestamp() as f64 + (dt.timestamp_subsec_nanos() as f64 / 1e9);
    }
    // Fallback: naive / other format
    if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(ts, "%Y-%m-%dT%H:%M:%S%.f") {
        return dt.and_utc().timestamp() as f64;
    }
    0.0
}

fn num(v: &Value, key: &str) -> u64 {
    v.get(key)
        .and_then(|x| x.as_u64().or_else(|| x.as_i64().map(|n| n.max(0) as u64)))
        .unwrap_or(0)
}

/// Epoch seconds (float, with sub-second precision) of the mtime of the host
/// main-loop heartbeat file. Returns `None` if the file is missing or its
/// mtime can't be read — the caller then omits the gauge entirely (matching
/// how an absent optional series is handled: no stale value is exported).
///
/// This is the file the main loop `touch`es on each `heartbeat-tick`. It is
/// the canonical liveness signal: if the main loop wedges and stops touching
/// the file, its mtime freezes and the gauge's age climbs without bound —
/// unlike `claude_heartbeat_timestamp_seconds`, which tracks the *daemon's*
/// own ~60s check cycle (`state.last_check`) and stays fresh even when the
/// main loop is dead.
fn heartbeat_file_mtime_secs(path: &Path) -> Option<f64> {
    let meta = fs::metadata(path).ok()?;
    let modified = meta.modified().ok()?;
    let dur = modified.duration_since(std::time::UNIX_EPOCH).ok()?;
    Some(dur.as_secs_f64())
}

/// True when claude-watch is running inside a container. Container age is only
/// meaningful there (PID 1 == the container's init, so its start time == the
/// container's start time); on a bare host PID 1 is the host init, so we omit
/// the gauge rather than mislabel host uptime as container age.
fn running_in_container() -> bool {
    Path::new("/.dockerenv").exists() || Path::new("/run/.containerenv").exists()
}

/// Parse the process start time (field 22 of `/proc/<pid>/stat`, in clock ticks
/// since system boot). The `comm` field (2nd) can itself contain spaces and
/// parentheses, so we split *after the last ')'* -- the canonical robust parse
/// -- then index the whitespace-separated tail (starttime is field 22, i.e. the
/// 0-indexed 19th token after `comm`).
fn parse_pid_starttime_ticks(stat: &str) -> Option<u64> {
    let tail = stat.rsplit_once(')')?.1;
    tail.split_whitespace().nth(19)?.parse::<u64>().ok()
}

/// Parse the `btime` line (system boot epoch, secs) from `/proc/stat`.
fn parse_btime(proc_stat: &str) -> Option<u64> {
    proc_stat
        .lines()
        .find_map(|l| l.strip_prefix("btime "))
        .and_then(|v| v.trim().parse::<u64>().ok())
}

/// Epoch (float secs) at which PID 1 started: `btime + starttime / clk_tck`.
/// Pure combiner so the arithmetic is unit-testable without `/proc`.
fn container_start_epoch(pid1_stat: &str, proc_stat: &str, clk_tck: u64) -> Option<f64> {
    if clk_tck == 0 {
        return None;
    }
    let starttime = parse_pid_starttime_ticks(pid1_stat)?;
    let btime = parse_btime(proc_stat)?;
    Some(btime as f64 + starttime as f64 / clk_tck as f64)
}

/// Epoch seconds of the container's start, derived from PID 1's start time via
/// `/proc`. `None` (gauge omitted, matching the heartbeat pattern) when not in
/// a container, when `/proc` is unreadable, or on any non-Linux host.
fn container_start_time_secs() -> Option<f64> {
    if !running_in_container() {
        return None;
    }
    let pid1_stat = fs::read_to_string("/proc/1/stat").ok()?;
    let proc_stat = fs::read_to_string("/proc/stat").ok()?;
    // SAFETY: sysconf(_SC_CLK_TCK) is a pure libc query with no preconditions.
    let clk_tck = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    let clk_tck = if clk_tck > 0 { clk_tck as u64 } else { 100 };
    container_start_epoch(&pid1_stat, &proc_stat, clk_tck)
}

/// Prometheus block for the container start-time gauge. Empty when
/// `container_start_time_secs` yields `None`, so nothing stale is exported.
fn container_start_block() -> Vec<String> {
    match container_start_time_secs() {
        Some(ts) => vec![
            "# HELP claude_container_start_timestamp_seconds Epoch at which the container (PID 1 / init) started; container age = time() - this. Omitted when not running in a container.".to_string(),
            "# TYPE claude_container_start_timestamp_seconds gauge".to_string(),
            format!("claude_container_start_timestamp_seconds {:.3}", ts),
        ],
        None => Vec::new(),
    }
}

fn build_metrics(
    state: &Value,
    current_version: &str,
    latest_version: &str,
    live: &LiveCounts,
    mainloop_heartbeat_mtime: Option<f64>,
    session_start_fallback: Option<f64>,
) -> Vec<String> {
    let last_check = state
        .get("last_check")
        .and_then(|v| v.as_str())
        .map(parse_iso_timestamp)
        .unwrap_or(0.0);
    // Epoch (float secs) anchoring the "time since last context clear" panel.
    // Priority: (1) an explicitly OBSERVED clear (`last_context_clear`); else
    // (2) a session-start fallback (the container start epoch, passed in by the
    // caller); else (3) the daemon's own start epoch persisted in state. We
    // fall back rather than omit so the panel ALWAYS renders a real elapsed
    // duration: after a deploy/recreate the observed-clear state is wiped, but
    // "time since the session became fresh" (== container / daemon start) is
    // still a truthful duration. We DO NOT default a missing/unparseable value
    // to `0.0`: a zero epoch makes a "now - last_clear" panel render ~56.5
    // years (2026 - 1970), the classic epoch-zero bug -- so every candidate is
    // filtered to `t > 0.0`, and the gauge is omitted only when NONE of the
    // three anchors is available.
    let last_context_clear = state
        .get("last_context_clear")
        .and_then(|v| v.as_str())
        .map(parse_iso_timestamp)
        .filter(|&t| t > 0.0)
        .or_else(|| session_start_fallback.filter(|&t| t > 0.0))
        .or_else(|| {
            state
                .get("daemon_start_epoch")
                .and_then(|v| v.as_f64())
                .filter(|&t| t > 0.0)
        });

    let last_known_tokens = num(state, "last_known_tokens");
    let last_known_bashes = num(state, "last_known_bashes");
    let consecutive_failures = num(state, "consecutive_failures");
    let consecutive_dead = num(state, "consecutive_dead_checks");
    let alert_count = num(state, "alert_count");
    let restart_count = num(state, "restart_count");

    // Watcher health
    let (watchers_missing, watchers_total) = match state.get("watcher_health") {
        Some(Value::Object(map)) => {
            let mut missing = 0u64;
            let mut total = 0u64;
            for (_k, w) in map {
                let enabled = w.get("enabled").and_then(|x| x.as_bool()).unwrap_or(false);
                if enabled {
                    total += 1;
                    let cm = w
                        .get("consecutive_missing")
                        .and_then(|x| x.as_u64())
                        .unwrap_or(0);
                    if cm > 3 {
                        missing += 1;
                    }
                }
            }
            (missing, total)
        }
        _ => (0, 0),
    };

    let watcher_inject = num(state, "watcher_inject_count");
    let thinking_interrupt = num(state, "thinking_interrupt_count");
    let auto_update = num(state, "auto_update_count");
    let heartbeat_stale = num(state, "heartbeat_stale_count");
    let fallback_clear = num(state, "fallback_clear_count");
    let fallback_update = num(state, "fallback_update_count");

    // Per-interrupt-type counters (cumulative — persisted across daemon restarts
    // through the state file). Each one increments exactly once per fire at the
    // corresponding site in src/policy.rs. Rendered as a single labeled
    // counter so Grafana can aggregate or break down by kind.
    let prolonged_thinking_interrupts = num(state, "prolonged_thinking_interrupts_total");
    let foreground_blocking_interrupts = num(state, "foreground_blocking_interrupts_total");
    let context_warning_interrupts = num(state, "context_warning_interrupts_total");
    let watcher_down_interrupts = num(state, "watcher_down_interrupts_total");
    let wedged_clear_interrupts = num(state, "wedged_clear_interrupts_total");
    let malformed_tool_call_nudges = num(state, "malformed_tool_call_nudge_count");
    let malformed_tool_call_hard_blocks = num(state, "malformed_tool_call_hard_block_count");
    let auto_update_interrupts = num(state, "auto_update_interrupts_total");
    let reauth_inject_interrupts = num(state, "reauth_inject_interrupts_total");
    let self_login_autofire = num(state, "self_login_autofire_total");
    let post_restart_resume_inject_interrupts =
        num(state, "post_restart_resume_inject_interrupts_total");
    let fresh_session_inject_interrupts = num(state, "fresh_session_inject_interrupts_total");
    let fresh_clear_resume_inject_interrupts =
        num(state, "fresh_clear_resume_inject_interrupts_total");
    let restart_claude_interrupts = num(state, "restart_claude_interrupts_total");
    let api_retry_suppressions = num(state, "api_retry_suppressions_total");
    let reminder_to_clear_count = num(state, "reminder_to_clear_latency_count");
    let reminder_to_update_count = num(state, "reminder_to_update_latency_count");
    let reminder_to_clear_sum = state
        .get("reminder_to_clear_latency_secs_sum")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);
    let reminder_to_update_sum = state
        .get("reminder_to_update_latency_secs_sum")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);

    let mut lines = vec![
        "# HELP claude_watch_up Whether claude-watch state file is readable".to_string(),
        "# TYPE claude_watch_up gauge".to_string(),
        "claude_watch_up 1".to_string(),
        "".to_string(),
        "# HELP claude_heartbeat_timestamp_seconds Epoch of last successful claude-watch check"
            .to_string(),
        "# TYPE claude_heartbeat_timestamp_seconds gauge".to_string(),
        format!("claude_heartbeat_timestamp_seconds {:.3}", last_check),
        "".to_string(),
        "# HELP claude_context_tokens Current context token count".to_string(),
        "# TYPE claude_context_tokens gauge".to_string(),
        format!("claude_context_tokens {}", last_known_tokens),
        "".to_string(),
        "# HELP claude_bash_count Number of bash calls in current context".to_string(),
        "# TYPE claude_bash_count gauge".to_string(),
        format!("claude_bash_count {}", last_known_bashes),
        "".to_string(),
        "# HELP claude_consecutive_failures Number of consecutive check failures".to_string(),
        "# TYPE claude_consecutive_failures gauge".to_string(),
        format!("claude_consecutive_failures {}", consecutive_failures),
        "".to_string(),
        "# HELP claude_consecutive_dead_checks Number of consecutive dead-process checks"
            .to_string(),
        "# TYPE claude_consecutive_dead_checks gauge".to_string(),
        format!("claude_consecutive_dead_checks {}", consecutive_dead),
        "".to_string(),
        "# HELP claude_alert_count Total alerts fired this session".to_string(),
        "# TYPE claude_alert_count gauge".to_string(),
        format!("claude_alert_count {}", alert_count),
        "".to_string(),
        "# HELP claude_restart_count Total restarts performed".to_string(),
        "# TYPE claude_restart_count gauge".to_string(),
        format!("claude_restart_count {}", restart_count),
        "".to_string(),
        "# HELP claude_watchers_missing Number of enabled watchers currently missing".to_string(),
        "# TYPE claude_watchers_missing gauge".to_string(),
        format!("claude_watchers_missing {}", watchers_missing),
        "".to_string(),
        "# HELP claude_watchers_total Total number of enabled watchers".to_string(),
        "# TYPE claude_watchers_total gauge".to_string(),
        format!("claude_watchers_total {}", watchers_total),
        "".to_string(),
        "# HELP claude_version_info Claude Code version info".to_string(),
        "# TYPE claude_version_info gauge".to_string(),
        format!(
            "claude_version_info{{current=\"{}\",latest=\"{}\"}} 1",
            current_version, latest_version
        ),
        "".to_string(),
        "# HELP claude_watch_build_info Build identity of the running claude-watch binary"
            .to_string(),
        "# TYPE claude_watch_build_info gauge".to_string(),
        format!(
            "claude_watch_build_info{{version=\"{}\",commit=\"{}\",pr=\"{}\"}} 1",
            env!("CARGO_PKG_VERSION"),
            env!("CW_GIT_COMMIT"),
            env!("CW_GIT_PR")
        ),
        "".to_string(),
        "# HELP claude_watcher_inject_total Total watcher inject events".to_string(),
        "# TYPE claude_watcher_inject_total counter".to_string(),
        format!("claude_watcher_inject_total {}", watcher_inject),
        "".to_string(),
        "# HELP claude_thinking_interrupt_total Total thinking interrupt events".to_string(),
        "# TYPE claude_thinking_interrupt_total counter".to_string(),
        format!("claude_thinking_interrupt_total {}", thinking_interrupt),
        "".to_string(),
        "# HELP claude_auto_update_total Total auto-update events".to_string(),
        "# TYPE claude_auto_update_total counter".to_string(),
        format!("claude_auto_update_total {}", auto_update),
        "".to_string(),
        "# HELP claude_heartbeat_stale_total Total heartbeat stale events".to_string(),
        "# TYPE claude_heartbeat_stale_total counter".to_string(),
        format!("claude_heartbeat_stale_total {}", heartbeat_stale),
        "".to_string(),
        "# HELP claude_watch_reminder_fires_total Total hybrid-hook reminder fires by type"
            .to_string(),
        "# TYPE claude_watch_reminder_fires_total counter".to_string(),
        reminder_fire_lines(),
        "".to_string(),
        "# HELP claude_watch_fallback_injections_total Total daemon fallback injections when hook reminder went unheeded".to_string(),
        "# TYPE claude_watch_fallback_injections_total counter".to_string(),
        format!(
            "claude_watch_fallback_injections_total{{type=\"clear\"}} {}",
            fallback_clear
        ),
        format!(
            "claude_watch_fallback_injections_total{{type=\"update\"}} {}",
            fallback_update
        ),
        "".to_string(),
        "# HELP claude_interrupts_total Total interrupt events by kind (claude-watch interrupting the managed Claude Code session)".to_string(),
        "# TYPE claude_interrupts_total counter".to_string(),
        format!(
            "claude_interrupts_total{{kind=\"prolonged_thinking\"}} {}",
            prolonged_thinking_interrupts
        ),
        format!(
            "claude_interrupts_total{{kind=\"foreground_blocking\"}} {}",
            foreground_blocking_interrupts
        ),
        format!(
            "claude_interrupts_total{{kind=\"context_warning\"}} {}",
            context_warning_interrupts
        ),
        format!(
            "claude_interrupts_total{{kind=\"watcher_down\"}} {}",
            watcher_down_interrupts
        ),
        format!(
            "claude_interrupts_total{{kind=\"wedged_clear\"}} {}",
            wedged_clear_interrupts
        ),
        format!(
            "claude_interrupts_total{{kind=\"malformed_tool_call_nudge\"}} {}",
            malformed_tool_call_nudges
        ),
        format!(
            "claude_interrupts_total{{kind=\"malformed_tool_call_hard_block\"}} {}",
            malformed_tool_call_hard_blocks
        ),
        format!(
            "claude_interrupts_total{{kind=\"auto_update\"}} {}",
            auto_update_interrupts
        ),
        format!(
            "claude_interrupts_total{{kind=\"reauth_inject\"}} {}",
            reauth_inject_interrupts
        ),
        // Proactive re-login. Separate kind from `reauth_inject` on purpose:
        // that one only fires on a session that is ALREADY dead, so if the two
        // were pooled there would be no way to tell "we caught it in time"
        // from "we did not", which is the only question this counter answers.
        format!(
            "claude_interrupts_total{{kind=\"self_login_autofire\"}} {}",
            self_login_autofire
        ),
        format!(
            "claude_interrupts_total{{kind=\"post_restart_resume_inject\"}} {}",
            post_restart_resume_inject_interrupts
        ),
        format!(
            "claude_interrupts_total{{kind=\"fresh_session_inject\"}} {}",
            fresh_session_inject_interrupts
        ),
        format!(
            "claude_interrupts_total{{kind=\"fresh_clear_resume_inject\"}} {}",
            fresh_clear_resume_inject_interrupts
        ),
        format!(
            "claude_interrupts_total{{kind=\"restart_claude\"}} {}",
            restart_claude_interrupts
        ),
        "".to_string(),
        "# HELP claude_watch_api_retry_suppressions_total Cycles where claude-watch suppressed an interrupt because Claude Code was in upstream-API retry backoff".to_string(),
        "# TYPE claude_watch_api_retry_suppressions_total counter".to_string(),
        format!(
            "claude_watch_api_retry_suppressions_total {}",
            api_retry_suppressions
        ),
        "".to_string(),
        "# HELP claude_watch_reminder_to_action_latency_seconds_sum Sum of seconds between hook reminder and Claude self-action".to_string(),
        "# TYPE claude_watch_reminder_to_action_latency_seconds_sum counter".to_string(),
        format!(
            "claude_watch_reminder_to_action_latency_seconds_sum{{type=\"clear\"}} {:.3}",
            reminder_to_clear_sum
        ),
        format!(
            "claude_watch_reminder_to_action_latency_seconds_sum{{type=\"update\"}} {:.3}",
            reminder_to_update_sum
        ),
        "# HELP claude_watch_reminder_to_action_latency_seconds_count Number of reminder-to-action latency samples".to_string(),
        "# TYPE claude_watch_reminder_to_action_latency_seconds_count counter".to_string(),
        format!(
            "claude_watch_reminder_to_action_latency_seconds_count{{type=\"clear\"}} {}",
            reminder_to_clear_count
        ),
        format!(
            "claude_watch_reminder_to_action_latency_seconds_count{{type=\"update\"}} {}",
            reminder_to_update_count
        ),
        "".to_string(),
        // Claude Code live-process counts — the four numbers exposed by
        // `claude-watch status`'s top section. Singleton gauges (no
        // session_id label) because there's only one Claude Code on this
        // host. Names use the `claude_code_*` prefix to make ownership
        // unambiguous (Claude Code itself, not claude-watch).
        "# HELP claude_code_active_agents Number of live Claude Code subagent processes".to_string(),
        "# TYPE claude_code_active_agents gauge".to_string(),
        format!("claude_code_active_agents {}", live.active_agents),
        "".to_string(),
        "# HELP claude_code_running_tasks Number of currently-running workloads (tmux tasks session)".to_string(),
        "# TYPE claude_code_running_tasks gauge".to_string(),
        format!("claude_code_running_tasks {}", live.running_tasks),
        "".to_string(),
        "# HELP claude_code_live_watchers Number of enabled watchers currently healthy".to_string(),
        "# TYPE claude_code_live_watchers gauge".to_string(),
        format!("claude_code_live_watchers {}", live.live_watchers),
        "".to_string(),
        "# HELP claude_code_enabled_watchers Number of watchers enabled in watchers.conf".to_string(),
        "# TYPE claude_code_enabled_watchers gauge".to_string(),
        format!("claude_code_enabled_watchers {}", live.enabled_watchers),
        "".to_string(),
        "# HELP claude_code_open_bashes Number of open background-bash slots in Claude Code".to_string(),
        "# TYPE claude_code_open_bashes gauge".to_string(),
        format!("claude_code_open_bashes {}", live.open_bashes),
    ];

    // Main-loop heartbeat FILE mtime — the true liveness signal. The main
    // loop touches the host heartbeat file on each `heartbeat-tick`; this
    // gauge exports that file's mtime so a wedged main loop (which stops
    // touching the file) shows a climbing age. Distinct from
    // `claude_heartbeat_timestamp_seconds`, which tracks the daemon's own
    // check cycle and stays fresh regardless of main-loop liveness. Omitted
    // entirely when the file is absent so no stale value is exported.
    if let Some(mtime) = mainloop_heartbeat_mtime {
        lines.push("".to_string());
        lines.push(
            "# HELP claude_mainloop_heartbeat_timestamp_seconds Epoch (mtime) of the host main-loop heartbeat file, touched by the main loop on each heartbeat-tick"
                .to_string(),
        );
        lines.push("# TYPE claude_mainloop_heartbeat_timestamp_seconds gauge".to_string());
        lines.push(format!(
            "claude_mainloop_heartbeat_timestamp_seconds {:.3}",
            mtime
        ));
    }

    // Last context-clear timestamp (with session-start fallback -- see the
    // `last_context_clear` derivation above). Emitted whenever ANY anchor is
    // available (observed clear, container start, or daemon start) so the
    // downstream "time since last clear" panel ALWAYS renders a real elapsed
    // duration. Omitted only when none of the three exists -- and never with a
    // bogus ~56-year duration computed from epoch zero.
    if let Some(ts) = last_context_clear {
        lines.push("".to_string());
        lines.push(
            "# HELP claude_last_context_clear_timestamp_seconds Epoch of last observed context clear, or the session/daemon start epoch as a fallback when none observed"
                .to_string(),
        );
        lines.push("# TYPE claude_last_context_clear_timestamp_seconds gauge".to_string());
        lines.push(format!(
            "claude_last_context_clear_timestamp_seconds {:.3}",
            ts
        ));
    }

    lines
}

/// Build the multi-line `claude_watch_reminder_fires_total{type=...}`
/// block. Reads fire counts from the reminder marker files.
fn reminder_fire_lines() -> String {
    let counts = all_fire_counts();
    counts
        .iter()
        .map(|(label, count)| {
            format!(
                "claude_watch_reminder_fires_total{{type=\"{}\"}} {}",
                label, count
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn down_metrics() -> Vec<String> {
    vec![
        "# HELP claude_watch_up Whether claude-watch state file is readable".to_string(),
        "# TYPE claude_watch_up gauge".to_string(),
        "claude_watch_up 0".to_string(),
    ]
}

/// Atomic write: temp file in same dir + rename.
fn write_prom(lines: &[String], path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let content = format!("{}\n", lines.join("\n"));
    let tmp_path = path.with_extension("prom.tmp");
    {
        let mut f = fs::File::create(&tmp_path)?;
        f.write_all(content.as_bytes())?;
        f.sync_all()?;
        fs::set_permissions(&tmp_path, fs::Permissions::from_mode(0o644))?;
    }
    fs::rename(&tmp_path, path)?;
    Ok(())
}

/// Get version info directly from /proc and symlinks (no subprocess).
///
/// Previous implementation shelled out to `claude-watch status --json`, which
/// broke when PATH resolved to a stale binary at /usr/local/bin/claude-watch
/// that couldn't parse the current config. Calling get_version_info() directly
/// avoids the recursive subprocess and config dependency entirely.
fn fetch_version_info() -> (String, String) {
    let info = get_version_info();
    // "latest" is the on-disk installed version (the versions-dir active
    // symlink); it resolves independently of any running process.
    let installed = info.installed;
    // "current" is the running version. If it can't be resolved (transient
    // startup race before the claude PID is up, or an unexpected comm/exe
    // layout), fall back to the installed version — the versions-dir active
    // symlink is what a freshly-(re)spawned native-install claude runs — rather
    // than emitting the useless `unknown`. NOTE: this fallback lives HERE, at
    // the metric layer, on purpose: `get_version_info().running` MUST stay a
    // truthful Option for `hook_fire::handle_version_update`, whose
    // running != installed check drives the restart nudge. Collapsing them in
    // get_version_info would silence that nudge.
    let current = info
        .running
        .or_else(|| installed.clone())
        .unwrap_or_else(|| "unknown".to_string());
    let latest = installed.unwrap_or_else(|| "unknown".to_string());
    (current, latest)
}

/// Collect the live-process counts that mirror `claude-watch status`'s
/// "Claude Code" section. Best-effort: any sub-collection failure degrades
/// to zero rather than failing the whole metrics emission. The textfile
/// collector cron job runs every minute; one transiently-broken count
/// shouldn't take down the whole exporter.
async fn collect_live_counts() -> LiveCounts {
    use crate::active_agents;
    use crate::status::get_claude_status;
    use crate::watcher;

    // Fan out the three independent collections in parallel — same pattern
    // as `run_status` in main.rs. Total wall-clock stays near the slowest
    // single call (typically watcher_status's pgrep round-trips).
    let watcher_cfg = watcher::config_path();
    let watcher_cfg_extra = watcher::config_path_extra();
    let (agents, watchers, claude_status) = tokio::join!(
        tokio::task::spawn_blocking(active_agents::collect),
        watcher::watcher_status(&watcher_cfg, watcher_cfg_extra.as_deref()),
        get_claude_status(),
    );

    let agents = agents.unwrap_or(active_agents::ActiveAgents {
        subagents: Vec::new(),
        workloads: Vec::new(),
        agents: Vec::new(),
    });

    let live_watchers = watchers.iter().filter(|w| w.status == "ok").count() as u32;
    let enabled_watchers = watchers.iter().filter(|w| w.enabled).count() as u32;

    // open_bashes: prefer a fresh status-bar parse. If that fails (no pane
    // visible, parser miss, etc.), fall back to 0 — the existing
    // `claude_bash_count` gauge already surfaces last_known_bashes from
    // state.json for trend continuity.
    let open_bashes = claude_status.map(|cs| cs.bashes as u32).unwrap_or(0);

    LiveCounts {
        // Count live agents from the JSONL-transcript alive flag, NOT from
        // `subagents` (child PIDs). Subagents share the parent Claude Code
        // PID — they're in-process event loops, not child processes — so the
        // child-PID set actually enumerates non-watcher child PROCESSES (MCP
        // servers like the chrome-devtools stdio server, transient bash), not
        // agents, inflating the gauge by a near-constant offset. Transcript
        // mtime (within the 120s max-age window) is the canonical agent
        // liveness signal.
        active_agents: agents.agents.iter().filter(|a| a.alive).count() as u32,
        running_tasks: agents.workloads.len() as u32,
        live_watchers,
        enabled_watchers,
        open_bashes,
    }
}

// ---------------------------------------------------------------------------
// Operator desk-streak gauge
//
// `claude_operator_desk_streak_seconds{kind="current"|"max"}` -- length of the
// operator's CONTINUOUS at-desk presence run. `current` is the trailing run
// ending "now" (resets to 0 the moment presence drops to away); `max` is the
// longest continuous run observed TODAY (see below). Because `claude-watch
// metrics` is a one-shot cron invocation (no long-lived process), the streak
// must be RECONSTRUCTED on every emit rather than accumulated in RAM.
//
// PRIMARY SOURCE OF TRUTH: Prometheus. Each emit queries the scraped
// `claude_operator_present` series (query_range over a trailing window) and
// REPLAYS it through the same `advance_streak` fold the sidecar path uses, so
// BOTH `current` AND `max` are derived purely from the dataset -- they survive
// a container/cw restart with no local state. This is what fixes the "streak +
// daily-max reset to zero on every container restart" bug: the sidecar did not
// survive a container recreate, and `current` was never persisted at all.
//
// FALLBACK: a small sidecar JSON file next to the daemon state, used only when
// Prometheus is unreachable/disabled at emit time (restart-lossy, but better
// than blanking the gauge). It is kept warm on the Prometheus path too, so a
// later Prometheus outage degrades gracefully. A DEDICATED file (not
// state.json) keeps this cron writer from racing the daemon's own writes.
//
// `max` is scoped to a SINGLE LOCAL CALENDAR DAY: the sidecar stamps the
// max with the local date (`max_date`, "%Y-%m-%d" in the host's local
// timezone -- the same `now()`/local-wall-clock convention the rest of
// presence uses). On each emit, if the stored `max_date` is a PRIOR day the
// day has rolled over -> the day's max restarts from the current run
// (0 when away); a same-day sample ratchets `max = max(stored, current)`.
// Net effect: `kind="max"` is "the longest continuous at-desk streak TODAY",
// resetting at LOCAL MIDNIGHT and surviving restarts within the day.
//
// Presence is derived from the same operator-presence CARRIER file the
// presence-gate uses: present == the carrier's mtime is fresh within
// CW_PRESENCE_MAX_AGE. Reading the carrier directly (rather than the sibling
// `claude_operator_present` gauge) keeps this block self-contained.
// ---------------------------------------------------------------------------

/// Current wall-clock epoch seconds (float).
fn now_epoch() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Candidate paths for the presence-gate obligation manifest, in resolution
/// order: `$CW_PRESENCE_GATE_MANIFEST`, the in-container RO obligations mount,
/// then the bind-mounted repo path (which resolves both on the host and inside
/// the container). Mirrors the Python textfile bridge's `read_gate_max_age`.
fn gate_manifest_candidates() -> Vec<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(p) = std::env::var("CW_PRESENCE_GATE_MANIFEST") {
        if !p.trim().is_empty() {
            candidates.push(PathBuf::from(p));
        }
    }
    candidates.push(PathBuf::from(
        "/mnt/host-obligations-config/presence-gate.json",
    ));
    let home = std::env::var("HOME").unwrap_or_else(|_| "/root".to_string());
    candidates.push(PathBuf::from(home).join("repos/claude-config/obligations/presence-gate.json"));
    candidates
}

/// Pure: return the first readable manifest's `params.max_age_secs` (must be
/// > 0), else `default`. Split out from `read_gate_max_age` so it is unit-
/// testable without touching process env / real filesystem paths.
fn read_gate_max_age_from(candidates: &[PathBuf], default: f64) -> f64 {
    for path in candidates {
        let Ok(s) = fs::read_to_string(path) else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<Value>(&s) else {
            continue;
        };
        if let Some(secs) = v
            .get("params")
            .and_then(|p| p.get("max_age_secs"))
            .and_then(|x| x.as_f64())
        {
            if secs > 0.0 {
                return secs;
            }
        }
    }
    default
}

/// Operator-presence gate freshness window (seconds), read from the
/// presence-gate obligation manifest -- the SINGLE SOURCE OF TRUTH
/// (`claude-config/obligations/presence-gate.json` -> `params.max_age_secs`,
/// currently 420s). This is the SAME window the `claude_operator_present`
/// gauge (Python textfile bridge) and the botchat header dot consume, so the
/// desk-streak's presence view can never drift from the presence gauge.
/// Falls back to `default` when no manifest is readable.
fn read_gate_max_age(default: f64) -> f64 {
    read_gate_max_age_from(&gate_manifest_candidates(), default)
}

/// Presence freshness window in seconds. `CW_PRESENCE_MAX_AGE` overrides;
/// otherwise the window is single-sourced from the presence-gate manifest
/// (`presence-gate.json` params.max_age_secs -- currently 420s) so it matches
/// the `claude_operator_present` gauge + botchat. Falls back to 420s only if
/// the manifest is unreadable (never the old hardcoded 90s, which silently
/// disagreed with the 420s gauge/gate window).
fn presence_max_age() -> f64 {
    std::env::var("CW_PRESENCE_MAX_AGE")
        .ok()
        .and_then(|s| s.trim().parse::<f64>().ok())
        .filter(|v| *v > 0.0)
        .unwrap_or_else(|| read_gate_max_age(420.0))
}

/// Resolve the operator-presence carrier file. `CW_PRESENCE_FILE` overrides;
/// otherwise the first existing of the tmpfs-farm path then the `~/.claude`
/// fallback (mirrors the ambient-inject hook's candidate order).
fn presence_carrier_path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("CW_PRESENCE_FILE") {
        if !p.trim().is_empty() {
            return Some(PathBuf::from(p));
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/root".to_string());
    let candidates = [
        PathBuf::from("/run/claude-presence/operator-present"),
        PathBuf::from(home).join(".claude/operator-present"),
    ];
    candidates.into_iter().find(|p| p.exists())
}

/// mtime (epoch secs) of the presence carrier, or `None` if absent/unreadable.
fn presence_carrier_mtime() -> Option<f64> {
    let path = presence_carrier_path()?;
    let meta = fs::metadata(&path).ok()?;
    let modified = meta.modified().ok()?;
    modified
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs_f64())
}

/// Pure presence decision: present iff the carrier mtime exists and is within
/// `max_age` seconds of `now`. A stale mtime (laptop asleep, presence dropped)
/// or an absent carrier reads as away.
fn presence_is_fresh(mtime: Option<f64>, now: f64, max_age: f64) -> bool {
    match mtime {
        Some(m) => now - m <= max_age,
        None => false,
    }
}

/// Render the operator-presence gauges: `claude_operator_present` (1=present,
/// 0=absent), `claude_operator_present_timestamp_seconds` (the carrier file
/// mtime, epoch secs), and `claude_presence_gate_max_age_secs` (the resolved
/// gate freshness window `max_age` itself -- the SINGLE SOURCE OF TRUTH read
/// from presence-gate.json, so consumers read the window instead of hardcoding
/// it). Present iff the carrier mtime is fresh within `max_age`
/// -- the SAME decision (`presence_is_fresh`), carrier (`presence_carrier_mtime`),
/// and window (`presence_max_age`) the desk-streak block uses, so the present
/// flag can never disagree with the streak's presence view. The timestamp gauge
/// exports the raw carrier mtime, defaulting to 0.0 when the carrier is
/// absent/unreadable (matching the Python textfile bridge's
/// `os.path.getmtime`-failure fallback). Gauge names, HELP, and TYPE lines are
/// byte-for-byte identical to that bridge so retiring it (once
/// `CLAUDE_WATCH_PROM_FILE` points this emitter at the scraped dir) drops
/// nothing.
fn operator_present_lines(mtime: Option<f64>, now: f64, max_age: f64) -> Vec<String> {
    let present = if presence_is_fresh(mtime, now, max_age) {
        1
    } else {
        0
    };
    let mtime_secs = mtime.unwrap_or(0.0);
    vec![
        "# HELP claude_operator_present Whether the operator is present (carrier mtime fresh within CW_PRESENCE_MAX_AGE secs); 1=present 0=absent".to_string(),
        "# TYPE claude_operator_present gauge".to_string(),
        format!("claude_operator_present {}", present),
        "".to_string(),
        "# HELP claude_operator_present_timestamp_seconds Epoch (mtime) of the operator-present carrier file touched by the host presence-detector while the operator is present".to_string(),
        "# TYPE claude_operator_present_timestamp_seconds gauge".to_string(),
        format!(
            "claude_operator_present_timestamp_seconds {:.3}",
            mtime_secs
        ),
        "".to_string(),
        "# HELP claude_presence_gate_max_age_secs Operator-presence gate freshness window in seconds -- SINGLE SOURCE OF TRUTH (presence-gate.json params.max_age_secs). Consumers should read this gauge instead of hardcoding a window.".to_string(),
        "# TYPE claude_presence_gate_max_age_secs gauge".to_string(),
        format!("claude_presence_gate_max_age_secs {:.0}", max_age),
    ]
}

/// Collect the operator-presence gauges, reading the live carrier mtime + the
/// gate-derived freshness window (the SAME inputs as `desk_streak_block`).
fn operator_present_block() -> Vec<String> {
    let now = now_epoch();
    operator_present_lines(presence_carrier_mtime(), now, presence_max_age())
}

/// Persisted streak state (sidecar JSON). Load is tolerant of missing fields.
#[derive(Debug, Clone, Default)]
struct StreakState {
    /// Epoch when the current continuous-present run began; `None` when away.
    run_start: Option<f64>,
    /// Longest continuous-present run observed THIS WEEK (seconds). Persisted,
    /// so it survives restarts; scoped to the operator's local calendar week
    /// (Sunday-start) via `week_start`.
    weekly_max_secs: f64,
    /// Local date ("%Y-%m-%d") of the SUNDAY that starts the week
    /// `weekly_max_secs` belongs to. When a sample's week-start differs, the
    /// week has rolled over (Sunday 00:00 local) and the weekly max restarts.
    /// `None` on a fresh/legacy sidecar (treated as "no week yet" -> the first
    /// sample seeds this week's max).
    week_start: Option<String>,
    /// Longest continuous-present run observed EVER (seconds). Persisted and
    /// never reset -- an all-time high-water mark that only ratchets up.
    alltime_max_secs: f64,
    /// Epoch of the previous sample (for gap detection); `None` on first ever.
    last_sample: Option<f64>,
    /// Whether the operator was present at the previous sample.
    last_present: bool,
}

fn default_streak_state_file() -> PathBuf {
    if let Ok(s) = std::env::var("CW_DESK_STREAK_STATE") {
        return PathBuf::from(s);
    }
    // Sibling of the daemon state file, in the same config dir.
    default_state_file().with_file_name("desk_streak.json")
}

fn load_streak_state(path: &Path) -> StreakState {
    let s = match fs::read_to_string(path) {
        Ok(s) => s,
        Err(_) => return StreakState::default(),
    };
    let v: Value = match serde_json::from_str(&s) {
        Ok(v) => v,
        Err(_) => return StreakState::default(),
    };
    // Legacy sidecars stored a single daily `max_streak_secs`/`max_date`. Use
    // the legacy daily max as an all-time floor so an in-place upgrade doesn't
    // blank the all-time high-water mark; the weekly max simply restarts.
    let legacy_max = v
        .get("max_streak_secs")
        .and_then(|x| x.as_f64())
        .unwrap_or(0.0)
        .max(0.0);
    StreakState {
        run_start: v.get("run_start").and_then(|x| x.as_f64()),
        weekly_max_secs: v
            .get("weekly_max_secs")
            .and_then(|x| x.as_f64())
            .unwrap_or(0.0)
            .max(0.0),
        week_start: v
            .get("week_start")
            .and_then(|x| x.as_str())
            .map(|s| s.to_string()),
        alltime_max_secs: v
            .get("alltime_max_secs")
            .and_then(|x| x.as_f64())
            .unwrap_or(legacy_max)
            .max(0.0),
        last_sample: v.get("last_sample").and_then(|x| x.as_f64()),
        last_present: v
            .get("last_present")
            .and_then(|x| x.as_bool())
            .unwrap_or(false),
    }
}

fn save_streak_state(path: &Path, state: &StreakState) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let v = serde_json::json!({
        "run_start": state.run_start,
        "weekly_max_secs": state.weekly_max_secs,
        "week_start": state.week_start,
        "alltime_max_secs": state.alltime_max_secs,
        "last_sample": state.last_sample,
        "last_present": state.last_present,
    });
    let content = serde_json::to_string(&v).unwrap_or_else(|_| "{}".to_string());
    let tmp_path = path.with_extension("json.tmp");
    {
        let mut f = fs::File::create(&tmp_path)?;
        f.write_all(content.as_bytes())?;
        f.sync_all()?;
    }
    fs::rename(&tmp_path, path)?;
    Ok(())
}

/// Advance the streak state by one sample. Pure -- no I/O, fully unit-tested.
///
/// - While present, the current run accumulates as `now - run_start`; the
///   weekly and all-time maxes ratchet up to the longest run seen in their
///   respective windows.
/// - On present->away the current run resets to 0 (run_start cleared); the
///   maxes are untouched (already captured while present).
/// - Gap handling: if the operator was present at the last sample AND the
///   elapsed time since that sample exceeds `max_gap_secs`, continuity across
///   the unobserved gap can't be asserted (cron/daemon was down, or the laptop
///   slept and the carrier re-freshened between samples) -> the run restarts
///   at `now` rather than over-counting the gap.
/// - Weekly scoping: `weekly_max_secs` is scoped to the local calendar week
///   identified by `week_key` (the "%Y-%m-%d" date of that week's Sunday). If
///   the stored `week_start` differs from `week_key` the week has rolled over,
///   so the week's max restarts from the current run (0 when away) -- i.e. it
///   resets at the operator's local Sunday 00:00. A same-week sample carries
///   the stored weekly max forward and ratchets it. The returned state's
///   `week_start` is always stamped to `week_key`.
/// - All-time scoping: `alltime_max_secs` is never reset -- it carries the
///   prior value forward and ratchets up to the longest run ever observed.
///
/// Returns the new state and the current-run length in seconds.
fn advance_streak(
    prev: &StreakState,
    present: bool,
    now: f64,
    max_gap_secs: f64,
    week_key: &str,
) -> (StreakState, f64) {
    // Weekly-scoped baseline: carry the stored weekly max forward only when it
    // belongs to `week_key`; otherwise the week rolled over (Sunday 00:00
    // local) and this week's max starts at 0. The all-time baseline always
    // carries forward -- it never resets.
    let prev_weekly = match &prev.week_start {
        Some(w) if w == week_key => prev.weekly_max_secs,
        _ => 0.0,
    };
    let prev_alltime = prev.alltime_max_secs;
    if !present {
        return (
            StreakState {
                run_start: None,
                weekly_max_secs: prev_weekly,
                week_start: Some(week_key.to_string()),
                alltime_max_secs: prev_alltime,
                last_sample: Some(now),
                last_present: false,
            },
            0.0,
        );
    }
    let gap_too_long = matches!(
        (prev.last_present, prev.last_sample),
        (true, Some(ls)) if now - ls > max_gap_secs
    );
    let run_start = match prev.run_start {
        Some(rs) if prev.last_present && !gap_too_long => rs,
        _ => now,
    };
    let current = (now - run_start).max(0.0);
    let weekly_max = prev_weekly.max(current);
    let alltime_max = prev_alltime.max(current);
    (
        StreakState {
            run_start: Some(run_start),
            weekly_max_secs: weekly_max,
            week_start: Some(week_key.to_string()),
            alltime_max_secs: alltime_max,
            last_sample: Some(now),
            last_present: true,
        },
        current,
    )
}

/// Render the `claude_operator_desk_streak_seconds` gauge block.
fn desk_streak_lines(current: f64, weekly_max: f64, alltime_max: f64) -> Vec<String> {
    vec![
        "# HELP claude_operator_desk_streak_seconds Continuous operator at-desk presence streak in seconds (kind=current: trailing run ending now, resets to 0 on away; kind=weekly_max: longest continuous run THIS WEEK, resets at the operator's local Sunday 00:00; kind=max: longest continuous run EVER, never resets). ALL rehydrated from the Prometheus claude_operator_present series each emit (weekly_max/max also merged with the persisted sidecar high-water marks), so they survive container/cw restarts.".to_string(),
        "# TYPE claude_operator_desk_streak_seconds gauge".to_string(),
        format!(
            "claude_operator_desk_streak_seconds{{kind=\"current\"}} {:.3}",
            current
        ),
        format!(
            "claude_operator_desk_streak_seconds{{kind=\"weekly_max\"}} {:.3}",
            weekly_max
        ),
        format!(
            "claude_operator_desk_streak_seconds{{kind=\"max\"}} {:.3}",
            alltime_max
        ),
    ]
}

/// Pure: the "%Y-%m-%d" date of the SUNDAY that starts the local calendar week
/// containing `date`. `Weekday::num_days_from_sunday()` is 0 for Sunday..6 for
/// Saturday, so subtracting it lands on that week's Sunday. Split out so the
/// Sunday-week boundary is unit-testable without touching the system clock/tz.
fn sunday_week_key(date: NaiveDate) -> String {
    use chrono::Datelike;
    let back = date.weekday().num_days_from_sunday() as i64;
    let sunday = date - chrono::Duration::days(back);
    sunday.format("%Y-%m-%d").to_string()
}

/// Local calendar-week key (the current week's Sunday date, "%Y-%m-%d") used to
/// scope the weekly max. Uses the host's LOCAL timezone -- the same local-wall-
/// clock convention the rest of the presence pipeline uses -- so the week
/// boundary is the operator's local Sunday 00:00, matching the Grafana panel's
/// local boundaries.
fn local_week_string() -> String {
    sunday_week_key(Local::now().date_naive())
}

/// Prometheus HTTP API base URL for rehydrating the desk-streak from the
/// scraped `claude_operator_present` series. `CW_PROMETHEUS_URL` overrides;
/// when UNSET it defaults to `http://localhost:9090` (host-native deploys where
/// cw and Prometheus share localhost). Set it EMPTY to DISABLE the Prometheus
/// path entirely (force the sidecar fallback) -- e.g. a deployment with no
/// reachable Prometheus. A trailing slash is trimmed.
fn prometheus_base_url() -> Option<String> {
    match std::env::var("CW_PROMETHEUS_URL") {
        Ok(s) if !s.trim().is_empty() => Some(s.trim().trim_end_matches('/').to_string()),
        Ok(_) => None, // explicitly empty => disabled
        Err(_) => Some("http://localhost:9090".to_string()),
    }
}

/// Trailing window (seconds) of `claude_operator_present` history to pull when
/// rehydrating. Must comfortably cover the current local week (for the weekly
/// max) plus a run that began before the week boundary, so a cold start (no sidecar) can
/// still reconstruct this week's max. `CW_DESK_STREAK_LOOKBACK_SECS` overrides;
/// defaults to 8 days (7-day week + a day of slack). All-time beyond this window
/// relies on the persisted sidecar floor.
fn desk_streak_lookback_secs() -> f64 {
    std::env::var("CW_DESK_STREAK_LOOKBACK_SECS")
        .ok()
        .and_then(|s| s.trim().parse::<f64>().ok())
        .filter(|v| *v > 0.0)
        .unwrap_or(691_200.0)
}

/// query_range step (seconds). Should match the metrics emit cadence (cron
/// every minute). `CW_DESK_STREAK_STEP_SECS` overrides; defaults to 60s.
fn desk_streak_step_secs() -> f64 {
    std::env::var("CW_DESK_STREAK_STEP_SECS")
        .ok()
        .and_then(|s| s.trim().parse::<f64>().ok())
        .filter(|v| *v > 0.0)
        .unwrap_or(60.0)
}

/// Prometheus' `query_range` rejects any request whose point count
/// (`(end-start)/step + 1`) exceeds its resolution cap -- 11 000 points per
/// timeseries by default. The configured step (`desk_streak_step_secs`, 60s)
/// over the default 8-day lookback is 691200/60 = 11 520 points, so Prometheus
/// returns `status:"error"`, `parse_prom_presence` bails, and the desk-streak
/// SILENTLY falls back to the sidecar on EVERY emit -- disabling the Prometheus
/// rehydration the `kind=max` gauge relies on to survive a container/cw restart
/// (the "desk-streak max resets on restart" regression). We clamp the step UP
/// so the request stays comfortably under this cap.
const PROM_MAX_RANGE_POINTS: f64 = 10_000.0;

/// Pure: the effective `query_range` step (seconds) that keeps `lookback/step`
/// under the Prometheus point cap (`PROM_MAX_RANGE_POINTS`). Never returns below
/// the configured `step` (resolution is only ever COARSENED, never finer than
/// asked) nor below 1s. Coarsening the step merely widens sample spacing --
/// still far finer than `max_gap` (180s default) -- so it never introduces
/// spurious run breaks, and widening the lookback later can only raise the step,
/// never re-break the query.
fn effective_step_secs(lookback: f64, step: f64) -> f64 {
    let min_step = (lookback / PROM_MAX_RANGE_POINTS).ceil();
    step.max(min_step).max(1.0)
}

/// Maximum unobserved data gap (seconds) between two consecutive presence
/// samples that is still bridged as ONE continuous run. A gap LARGER than this
/// means the emitter simply was not running across it (host metrics cron paused
/// -- laptop closed/asleep -- or a Prometheus scrape outage), so there is NO
/// evidence the operator stayed at their desk: the current run restarts at the
/// first post-gap sample rather than counting the unobserved gap as desk time.
///
/// `CW_DESK_STREAK_MAX_GAP_SECS` overrides; otherwise defaults to 3x the emit
/// cadence (`desk_streak_step_secs`, 60s default => 180s). That tolerates a
/// couple of missed cron/scrape cycles without over-eagerly resetting a real
/// streak, while a genuine absence (laptop closed for minutes) resets it.
///
/// IMPORTANT: this is a DIFFERENT quantity from `presence_max_age` (the
/// carrier-mtime freshness window, 420s). presence_max_age governs how stale a
/// SINGLE carrier mtime may be and still read "present"; max_gap governs how big
/// a hole in the SAMPLE STREAM we bridge as continuous. The two were previously
/// conflated (`max_gap = presence_max_age * 2 = 840s / 14min`), which silently
/// counted 7-14min laptop-close gaps as continuous at-desk time. Note that
/// Prometheus' query_range lookback-delta (default 5m) carries the last present
/// sample forward across the front of a gap, so on the Prometheus path the
/// effective break point is roughly this threshold plus that lookback window;
/// tune this env down if a tighter bound is needed.
fn desk_streak_max_gap_secs() -> f64 {
    std::env::var("CW_DESK_STREAK_MAX_GAP_SECS")
        .ok()
        .and_then(|s| s.trim().parse::<f64>().ok())
        .filter(|v| *v > 0.0)
        .unwrap_or_else(|| desk_streak_step_secs() * 3.0)
}

/// LOCAL calendar-week key of an epoch second (the Sunday date of that second's
/// local week), in the host's local timezone (same convention as
/// `local_week_string`). Scopes each rehydrated Prometheus sample to its week
/// for the weekly-max Sunday reset.
fn local_week_of(epoch: f64) -> String {
    use chrono::TimeZone;
    match Local.timestamp_opt(epoch as i64, 0).single() {
        Some(dt) => sunday_week_key(dt.date_naive()),
        None => local_week_string(),
    }
}

/// One presence sample rehydrated from Prometheus: epoch second, present flag,
/// and the LOCAL calendar-week key (that second's week's Sunday date) it falls
/// in (for weekly-max scoping).
#[derive(Debug, Clone, PartialEq)]
struct PresenceSample {
    ts: f64,
    present: bool,
    week: String,
}

/// Parse a Prometheus `query_range` matrix response into ordered presence
/// samples. Pure + testable: `week_fn` maps an epoch second to a LOCAL calendar-
/// week key so tests stay timezone-independent. Returns:
///   - `None` when the body is not a `status:"success"` matrix (malformed /
///     error response) -> the caller falls back to the sidecar.
///   - `Some([])` when the query succeeded but the series has no points (fresh
///     Prometheus / metric absent in the window) -> a legitimate empty history
///     (streak 0), NOT a fallback.
/// node-exporter's textfile collector yields a single series, so only the first
/// series' `values` are read. Point values use Prometheus' string encoding
/// ("1"/"0"); `>= 0.5` counts as present. Samples are returned sorted by ts.
fn parse_prom_presence(
    json: &str,
    week_fn: &dyn Fn(f64) -> String,
) -> Option<Vec<PresenceSample>> {
    let v: Value = serde_json::from_str(json).ok()?;
    if v.get("status").and_then(|s| s.as_str()) != Some("success") {
        return None;
    }
    let result = v.get("data")?.get("result")?.as_array()?;
    if result.is_empty() {
        return Some(Vec::new());
    }
    let values = result[0].get("values")?.as_array()?;
    let mut samples: Vec<PresenceSample> = Vec::with_capacity(values.len());
    for pair in values {
        let arr = pair.as_array()?;
        let ts = arr.first()?.as_f64()?;
        let raw = arr.get(1)?.as_str()?;
        let numv: f64 = raw.trim().parse().unwrap_or(0.0);
        samples.push(PresenceSample {
            ts,
            present: numv >= 0.5,
            week: week_fn(ts),
        });
    }
    samples.sort_by(|a, b| a.ts.partial_cmp(&b.ts).unwrap_or(std::cmp::Ordering::Equal));
    Some(samples)
}

/// Reconstruct the streak by folding the tested `advance_streak` over the
/// rehydrated Prometheus samples (each carrying its own local-week key, so
/// weekly-max Sunday resets AND scrape-gap breaks reuse the exact logic the
/// sidecar path uses), then over the LIVE "now" sample last so the emitted
/// `current` reflects this instant. Historical samples at or after `now` are
/// ignored. Returns the reconstructed state and the current-run length (secs).
fn compute_streak_from_samples(
    samples: &[PresenceSample],
    now: f64,
    present_now: bool,
    now_week: &str,
    max_gap_secs: f64,
) -> (StreakState, f64) {
    let mut state = StreakState::default();
    for s in samples {
        if s.ts >= now {
            continue;
        }
        let (next, _) = advance_streak(&state, s.present, s.ts, max_gap_secs, &s.week);
        state = next;
    }
    advance_streak(&state, present_now, now, max_gap_secs, now_week)
}

/// Merge the Prometheus-recomputed maxes with the persisted sidecar high-water
/// marks. The Prometheus lookback window can be shorter than a week (and is
/// always shorter than all-time), so a pure recompute from the window would
/// under-report both maxes. The persisted sidecar is therefore a FLOOR: the
/// all-time max always ratchets against it, and the weekly max ratchets against
/// it ONLY when the persisted value belongs to the same (current) week --
/// otherwise the week has rolled over and the recomputed (reset) value stands.
/// Pure + testable.
fn merge_persisted_maxes(mut computed: StreakState, persisted: &StreakState) -> StreakState {
    computed.alltime_max_secs = computed.alltime_max_secs.max(persisted.alltime_max_secs);
    if computed.week_start.is_some() && computed.week_start == persisted.week_start {
        computed.weekly_max_secs = computed.weekly_max_secs.max(persisted.weekly_max_secs);
    }
    computed
}

/// Query Prometheus `query_range` for the `claude_operator_present` series over
/// `[now - lookback, now]`. Shells out to `curl` -- matching the daemon's
/// established "shell out to an external tool" convention (ps/tmux/session-task)
/// and keeping an HTTP client out of the dependency tree, appropriate for this
/// one-shot cron command. Returns the raw response body, or `None` on ANY
/// failure (curl missing, non-zero exit, empty body) so the caller falls back
/// to the sidecar. A short `--max-time` keeps a wedged Prometheus from stalling
/// the emit.
fn fetch_prom_presence_range(base: &str, now: f64, lookback: f64, step: f64) -> Option<String> {
    let start = (now - lookback).max(0.0);
    // The query is a bare metric name (no special chars) -> no URL-encoding.
    let url = format!(
        "{base}/api/v1/query_range?query=claude_operator_present&start={start:.3}&end={now:.3}&step={step:.0}s"
    );
    let out = std::process::Command::new("curl")
        .arg("-sS")
        .arg("--max-time")
        .arg("5")
        .arg(&url)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let body = String::from_utf8_lossy(&out.stdout).to_string();
    if body.trim().is_empty() {
        return None;
    }
    Some(body)
}

/// Collect the desk-streak gauge lines. Rehydrates BOTH `current` and `max`
/// from the Prometheus `claude_operator_present` series so they survive a
/// container/cw restart; falls back to the on-disk sidecar when Prometheus is
/// unreachable/disabled. Fail-open: a persistence error still emits the sample.
fn desk_streak_block() -> Vec<String> {
    let now = now_epoch();
    let present = presence_is_fresh(presence_carrier_mtime(), now, presence_max_age());
    let max_gap = desk_streak_max_gap_secs();
    let week = local_week_string();
    let path = default_streak_state_file();

    // Preferred: rehydrate from Prometheus (survives restarts, no local state).
    if let Some(base) = prometheus_base_url() {
        let lookback = desk_streak_lookback_secs();
        // Clamp the step UP so `lookback/step` stays under Prometheus'
        // query_range point cap; otherwise the request errors and the whole
        // rehydration silently falls back to the sidecar (see
        // `effective_step_secs`).
        let step = effective_step_secs(lookback, desk_streak_step_secs());
        if let Some(body) = fetch_prom_presence_range(&base, now, lookback, step) {
            if let Some(samples) = parse_prom_presence(&body, &|ts| local_week_of(ts)) {
                let (computed, current) =
                    compute_streak_from_samples(&samples, now, present, &week, max_gap);
                // The Prometheus window can be shorter than a week (and is
                // always shorter than all-time), so fold the persisted sidecar
                // high-water marks in as a floor before emitting/saving.
                let persisted = load_streak_state(&path);
                let next = merge_persisted_maxes(computed, &persisted);
                // Keep the sidecar warm so a later Prometheus outage degrades
                // gracefully rather than restarting from zero.
                let _ = save_streak_state(&path, &next);
                return desk_streak_lines(current, next.weekly_max_secs, next.alltime_max_secs);
            } else {
                // A non-success / malformed body (e.g. the point-cap "bad_data"
                // error) means rehydration is broken, NOT that the operator was
                // absent. Warn LOUD so this can't silently regress again, then
                // fall through to the sidecar rather than blanking the max.
                eprintln!(
                    "claude-watch metrics: desk-streak Prometheus rehydration failed \
                     (non-success/malformed query_range response from {base}); \
                     falling back to sidecar. Check CW_PROMETHEUS_URL and the \
                     query_range point cap (lookback={lookback:.0}s step={step:.0}s)."
                );
            }
        }
    }

    // Fallback: single-sample accumulation persisted to the sidecar (the weekly
    // + all-time maxes carry forward through `advance_streak`).
    let prev = load_streak_state(&path);
    let (next, current) = advance_streak(&prev, present, now, max_gap, &week);
    let _ = save_streak_state(&path, &next);
    desk_streak_lines(current, next.weekly_max_secs, next.alltime_max_secs)
}

/// CLI entry point: `claude-watch metrics`.
pub async fn cmd_metrics() -> i32 {
    let state_path = default_state_file();
    let prom_path = prom_file_path();

    let state_str = match fs::read_to_string(&state_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Error reading state file: {e}");
            let _ = write_prom(&down_metrics(), &prom_path);
            return 1;
        }
    };
    let state: Value = match serde_json::from_str(&state_str) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("Error parsing state file: {e}");
            let _ = write_prom(&down_metrics(), &prom_path);
            return 1;
        }
    };

    let (cur, latest) = fetch_version_info();
    let live = collect_live_counts().await;

    // Resolve the host main-loop heartbeat file path from config (the
    // canonical `[claude].heartbeat_file` field). If config can't be loaded,
    // fall back to the documented default path so the gauge still works on a
    // normally-provisioned host. The gauge is omitted if the file is absent.
    let heartbeat_path = crate::config::try_load_config()
        .map(|c| c.claude.heartbeat_file)
        .unwrap_or_else(|_| "/run/claude/heartbeat".to_string());
    let mainloop_heartbeat_mtime = heartbeat_file_mtime_secs(Path::new(&heartbeat_path));

    // Session-start fallback for the "since last clear" gauge: the container's
    // start epoch (Some only in-container). build_metrics falls back further to
    // the persisted daemon_start_epoch when this is None (bare-host case), so
    // the panel always renders a duration even before any /clear is observed.
    let session_start_fallback = container_start_time_secs();
    let mut lines = build_metrics(
        &state,
        &cur,
        &latest,
        &live,
        mainloop_heartbeat_mtime,
        session_start_fallback,
    );

    // Token usage — aggregated from the Claude Code JSONL transcripts (same
    // observation surface as active_agents) and appended to the existing
    // textfile output. Kept as a separate pure block so `build_metrics`'s
    // signature + tests stay untouched. Fail-open: a missing projects dir
    // yields zeros rather than blanking the rest of the emission.
    let token_usage = tokio::task::spawn_blocking(crate::token_usage::collect_token_usage)
        .await
        .unwrap_or_default();
    lines.push(String::new());
    lines.extend(crate::token_usage::token_metric_lines(&token_usage));

    // Operator-presence gauges (present flag + carrier mtime). Reads the SAME
    // carrier mtime + freshness window as the desk-streak block below, so the
    // present flag never disagrees with the streak's presence view. A drop-in
    // for the out-of-tree Python textfile bridge's identical gauges so that
    // retiring the bridge (via CLAUDE_WATCH_PROM_FILE) drops nothing.
    lines.push(String::new());
    lines.extend(operator_present_block());

    // Operator desk-streak gauge (self-contained: reads the presence
    // carrier + a sidecar state file; see the block above cmd_metrics).
    lines.push(String::new());
    lines.extend(desk_streak_block());

    // Container start-time gauge -- container age = time() minus this. Empty
    // (gauge omitted) when not in a container / `/proc` unreadable, mirroring
    // the main-loop-heartbeat pattern. Kept as an appended block so
    // `build_metrics`'s signature + tests stay untouched.
    lines.push(String::new());
    lines.extend(container_start_block());

    if let Err(e) = write_prom(&lines, &prom_path) {
        eprintln!("Error writing prom file: {e}");
        return 1;
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // --- Operator desk-streak -------------------------------------------

    // A fixed local-week key used across the same-week streak tests. The value
    // is opaque to `advance_streak` (it only compares week keys for equality);
    // 2026-01-04 is a Sunday, so it reads as a real week-start.
    const D0: &str = "2026-01-04";

    #[test]
    fn streak_accumulates_while_present() {
        let s0 = StreakState::default();
        // First present sample starts a run at now; current is 0.
        let (s1, c1) = advance_streak(&s0, true, 1000.0, 200.0, D0);
        assert_eq!(c1, 0.0);
        assert_eq!(s1.run_start, Some(1000.0));
        assert_eq!(s1.week_start.as_deref(), Some(D0));
        // Still present 30s later: current accumulates, run_start unchanged.
        let (s2, c2) = advance_streak(&s1, true, 1030.0, 200.0, D0);
        assert_eq!(c2, 30.0);
        assert_eq!(s2.run_start, Some(1000.0));
        assert_eq!(s2.weekly_max_secs, 30.0);
        assert_eq!(s2.alltime_max_secs, 30.0);
    }

    #[test]
    fn streak_resets_on_away() {
        let s0 = StreakState::default();
        let (s1, _) = advance_streak(&s0, true, 1000.0, 200.0, D0);
        let (s2, c2) = advance_streak(&s1, true, 1050.0, 200.0, D0);
        assert_eq!(c2, 50.0);
        // Presence drops: current resets to 0, run cleared, max preserved.
        let (s3, c3) = advance_streak(&s2, false, 1060.0, 200.0, D0);
        assert_eq!(c3, 0.0);
        assert_eq!(s3.run_start, None);
        assert_eq!(s3.weekly_max_secs, 50.0);
        assert_eq!(s3.alltime_max_secs, 50.0);
        // Present again: brand-new run from now, current 0.
        let (s4, c4) = advance_streak(&s3, true, 1100.0, 200.0, D0);
        assert_eq!(c4, 0.0);
        assert_eq!(s4.run_start, Some(1100.0));
        assert_eq!(s4.weekly_max_secs, 50.0);
        assert_eq!(s4.alltime_max_secs, 50.0);
    }

    #[test]
    fn streak_max_tracks_longest_run() {
        // A 100s run.
        let (s, _) = advance_streak(&StreakState::default(), true, 0.0, 200.0, D0);
        let (s, c) = advance_streak(&s, true, 100.0, 200.0, D0);
        assert_eq!(c, 100.0);
        assert_eq!(s.weekly_max_secs, 100.0);
        // Away, then a shorter 20s run -- max must retain the longer 100s.
        let (s, _) = advance_streak(&s, false, 110.0, 200.0, D0);
        let (s, _) = advance_streak(&s, true, 120.0, 200.0, D0);
        let (s, c) = advance_streak(&s, true, 140.0, 200.0, D0);
        assert_eq!(c, 20.0);
        assert_eq!(s.weekly_max_secs, 100.0);
        assert_eq!(s.alltime_max_secs, 100.0);
    }

    #[test]
    fn streak_restarts_after_long_sample_gap() {
        let (s, _) = advance_streak(&StreakState::default(), true, 0.0, 100.0, D0);
        let (s, c) = advance_streak(&s, true, 50.0, 100.0, D0);
        assert_eq!(c, 50.0);
        // Gap of 300s > max_gap 100s while present: continuity broken, restart.
        let (s, c) = advance_streak(&s, true, 350.0, 100.0, D0);
        assert_eq!(c, 0.0);
        assert_eq!(s.run_start, Some(350.0));
        // Longest observed run is still the pre-gap 50s.
        assert_eq!(s.weekly_max_secs, 50.0);
        assert_eq!(s.alltime_max_secs, 50.0);
    }

    #[test]
    fn desk_streak_max_gap_default_and_override() {
        // Default is 3x the emit cadence (60s step => 180s) -- MUCH smaller than
        // the old presence_max_age()*2 (840s / 14min) that bridged laptop-close
        // gaps as continuous desk time.
        std::env::remove_var("CW_DESK_STREAK_MAX_GAP_SECS");
        std::env::remove_var("CW_DESK_STREAK_STEP_SECS");
        assert_eq!(desk_streak_max_gap_secs(), 180.0);
        // An explicit override wins.
        std::env::set_var("CW_DESK_STREAK_MAX_GAP_SECS", "90");
        assert_eq!(desk_streak_max_gap_secs(), 90.0);
        std::env::remove_var("CW_DESK_STREAK_MAX_GAP_SECS");
    }

    #[test]
    fn effective_step_stays_under_prom_point_cap() {
        // The 8-day default lookback at the 60s configured step is 11 520
        // points -- OVER Prometheus' 11 000-point query_range cap, which made
        // the rehydration query error and silently fall back to the sidecar on
        // every emit. The effective step must be clamped up so the point count
        // stays at/under PROM_MAX_RANGE_POINTS.
        let lookback = 691_200.0; // 8 days, the default
        let eff = effective_step_secs(lookback, 60.0);
        assert!(eff >= 60.0, "never finer than the configured step");
        assert!(
            lookback / eff <= PROM_MAX_RANGE_POINTS,
            "points {} must be <= cap {}",
            lookback / eff,
            PROM_MAX_RANGE_POINTS
        );
        // A short lookback leaves the configured step untouched.
        assert_eq!(effective_step_secs(60_000.0, 60.0), 60.0);
        // Never below 1s, even for a pathological tiny lookback / zero step.
        assert!(effective_step_secs(10.0, 0.0) >= 1.0);
    }

    #[test]
    fn laptop_close_gap_resets_current_streak() {
        // Build a continuous at-desk run from samples spaced UNDER max_gap, then
        // inject a laptop-close data gap (no samples emitted while the host cron
        // is paused) that EXCEEDS max_gap. The current run MUST reset -- the gap
        // is a genuine absence, not continuous desk time -- and the weekly/
        // all-time maxes must NOT be inflated by the unobserved gap.
        let max_gap = 180.0;
        // Continuous run: samples 150s apart (each gap <= max_gap) keep run_start.
        let (s, _) = advance_streak(&StreakState::default(), true, 0.0, max_gap, D0);
        let (s, c) = advance_streak(&s, true, 150.0, max_gap, D0);
        assert_eq!(c, 150.0);
        let (s, c) = advance_streak(&s, true, 300.0, max_gap, D0);
        assert_eq!(c, 300.0, "continuous run keeps growing across sub-threshold gaps");
        assert_eq!(s.run_start, Some(0.0));
        assert_eq!(s.weekly_max_secs, 300.0);
        assert_eq!(s.alltime_max_secs, 300.0);
        // Laptop closed ~10 min: NO samples recorded. The next present sample
        // arrives 600s later -- 600 > max_gap 180 => continuity broken.
        let (s, c) = advance_streak(&s, true, 900.0, max_gap, D0);
        assert_eq!(c, 0.0, "genuine >max_gap absence resets the current run");
        assert_eq!(s.run_start, Some(900.0));
        // The maxes captured the pre-gap 300s run and did NOT absorb the 600s gap.
        assert_eq!(
            s.weekly_max_secs, 300.0,
            "weekly max not extended across the unobserved gap"
        );
        assert_eq!(
            s.alltime_max_secs, 300.0,
            "all-time max not extended across the unobserved gap"
        );
    }

    #[test]
    fn short_gap_within_threshold_stays_continuous() {
        // A brief scrape hiccup (a single missed 60s cycle) is UNDER max_gap and
        // must NOT reset the run -- continuity survives transient blips.
        let max_gap = 180.0;
        let (s, _) = advance_streak(&StreakState::default(), true, 0.0, max_gap, D0);
        let (s, c) = advance_streak(&s, true, 60.0, max_gap, D0);
        assert_eq!(c, 60.0);
        // 120s gap (<= 180) while present: the run continues, current keeps growing.
        let (s, c) = advance_streak(&s, true, 180.0, max_gap, D0);
        assert_eq!(c, 180.0, "sub-threshold gap keeps the run continuous");
        assert_eq!(s.run_start, Some(0.0));
    }

    #[test]
    fn streak_weekly_max_resets_at_week_boundary_alltime_does_not() {
        // Build up a 100s run in week 0.
        let (s, _) = advance_streak(&StreakState::default(), true, 0.0, 200.0, D0);
        let (s, c) = advance_streak(&s, true, 100.0, 200.0, D0);
        assert_eq!(c, 100.0);
        assert_eq!(s.weekly_max_secs, 100.0);
        assert_eq!(s.alltime_max_secs, 100.0);
        assert_eq!(s.week_start.as_deref(), Some(D0));

        // Week rolls over (Sunday 00:00). A shorter run in the new week must NOT
        // inherit last week's 100s WEEKLY max -- but the ALL-TIME max must.
        const D1: &str = "2026-01-11"; // the following Sunday
        let (s, c) = advance_streak(&s, true, 1000.0, 200.0, D1);
        assert_eq!(c, 0.0, "new run starts at 0 across the week boundary");
        assert_eq!(
            s.weekly_max_secs, 0.0,
            "last week's 100s weekly max must not carry into the new week"
        );
        assert_eq!(
            s.alltime_max_secs, 100.0,
            "all-time max must survive the week boundary"
        );
        assert_eq!(s.week_start.as_deref(), Some(D1));
        let (s, c) = advance_streak(&s, true, 1030.0, 200.0, D1);
        assert_eq!(c, 30.0);
        assert_eq!(s.weekly_max_secs, 30.0, "this week's max is this week's longest run");
        assert_eq!(s.alltime_max_secs, 100.0, "all-time still holds the 100s peak");
        assert_eq!(s.week_start.as_deref(), Some(D1));
    }

    #[test]
    fn streak_max_persists_across_restart_same_week() {
        // Persistence regression: a sidecar carrying a same-week weekly max + an
        // all-time max must be preserved even though the current run restarts
        // (restart clears run_start/last_present but the sidecar survives).
        let prev = StreakState {
            run_start: None,
            weekly_max_secs: 600.0,
            week_start: Some(D0.to_string()),
            alltime_max_secs: 9000.0,
            last_sample: Some(500.0),
            last_present: false,
        };
        // Fresh present sample same week: current is 0 (new run) but the week's
        // max and the all-time max are retained from the sidecar.
        let (s, c) = advance_streak(&prev, true, 1000.0, 200.0, D0);
        assert_eq!(c, 0.0);
        assert_eq!(s.weekly_max_secs, 600.0);
        assert_eq!(s.alltime_max_secs, 9000.0);
    }

    // --- Prometheus rehydration (restart-survives) ----------------------

    #[test]
    fn parse_prom_presence_valid_matrix() {
        let json = r#"{"status":"success","data":{"resultType":"matrix","result":[{"metric":{"__name__":"claude_operator_present"},"values":[[1000,"1"],[1060,"1"],[1120,"0"]]}]}}"#;
        let s = parse_prom_presence(json, &|_| "2026-01-01".to_string()).unwrap();
        assert_eq!(s.len(), 3);
        assert_eq!(
            s[0],
            PresenceSample {
                ts: 1000.0,
                present: true,
                week: "2026-01-01".to_string()
            }
        );
        assert!(s[1].present);
        assert!(!s[2].present);
    }

    #[test]
    fn parse_prom_presence_empty_result_is_empty_history() {
        // Query succeeded but the metric had no points in the window: a valid
        // empty history (streak 0), NOT a fallback signal.
        let json = r#"{"status":"success","data":{"resultType":"matrix","result":[]}}"#;
        let s = parse_prom_presence(json, &|_| "d".to_string()).unwrap();
        assert!(s.is_empty());
    }

    #[test]
    fn parse_prom_presence_malformed_is_none() {
        // Non-success / unparseable => None => caller falls back to the sidecar.
        assert!(parse_prom_presence(r#"{"status":"error"}"#, &|_| "d".to_string()).is_none());
        assert!(parse_prom_presence("not json at all", &|_| "d".to_string()).is_none());
    }

    #[test]
    fn parse_prom_presence_sorts_by_timestamp() {
        let json =
            r#"{"status":"success","data":{"result":[{"values":[[200,"1"],[100,"0"],[300,"1"]]}]}}"#;
        let s = parse_prom_presence(json, &|_| "d".to_string()).unwrap();
        assert_eq!(
            s.iter().map(|x| x.ts).collect::<Vec<_>>(),
            vec![100.0, 200.0, 300.0]
        );
    }

    #[test]
    fn rehydrate_continuous_run_survives_restart() {
        // No sidecar involved: the streak is reconstructed PURELY from the
        // Prometheus samples -- exactly the post-restart path.
        let samples: Vec<PresenceSample> = (0..=5)
            .map(|i| PresenceSample {
                ts: 1000.0 + i as f64 * 60.0,
                present: true,
                week: D0.to_string(),
            })
            .collect();
        // now = 60s after the last sample, still present.
        let (state, current) = compute_streak_from_samples(&samples, 1360.0, true, D0, 200.0);
        assert_eq!(
            current, 360.0,
            "current run reconstructed from the first present sample's ts"
        );
        assert_eq!(state.weekly_max_secs, 360.0);
        assert_eq!(state.alltime_max_secs, 360.0);
    }

    #[test]
    fn rehydrate_breaks_continuity_on_scrape_gap() {
        let samples = vec![
            PresenceSample { ts: 0.0, present: true, week: D0.to_string() },
            PresenceSample { ts: 60.0, present: true, week: D0.to_string() },
            // 300s gap > max_gap 100 (cron/scrape outage): continuity broken.
            PresenceSample { ts: 360.0, present: true, week: D0.to_string() },
            PresenceSample { ts: 420.0, present: true, week: D0.to_string() },
        ];
        let (state, current) = compute_streak_from_samples(&samples, 480.0, true, D0, 100.0);
        assert_eq!(current, 120.0, "trailing run restarts after the gap (360..480)");
        assert_eq!(state.weekly_max_secs, 120.0);
    }

    #[test]
    fn rehydrate_current_zero_when_away_now() {
        let samples = vec![
            PresenceSample { ts: 0.0, present: true, week: D0.to_string() },
            PresenceSample { ts: 60.0, present: true, week: D0.to_string() },
        ];
        // Live sample = away: current resets, this week's max is preserved.
        let (state, current) = compute_streak_from_samples(&samples, 120.0, false, D0, 200.0);
        assert_eq!(current, 0.0);
        assert_eq!(state.weekly_max_secs, 60.0);
        assert_eq!(state.alltime_max_secs, 60.0);
    }

    #[test]
    fn rehydrate_weekly_max_resets_across_week_boundary() {
        let samples = vec![
            PresenceSample { ts: 0.0, present: true, week: "2026-01-04".to_string() },
            PresenceSample { ts: 60.0, present: true, week: "2026-01-04".to_string() },
            PresenceSample { ts: 120.0, present: true, week: "2026-01-04".to_string() },
            // Next local week, after a gap.
            PresenceSample { ts: 1000.0, present: true, week: "2026-01-11".to_string() },
        ];
        let (state, current) =
            compute_streak_from_samples(&samples, 1060.0, true, "2026-01-11", 200.0);
        assert_eq!(current, 60.0);
        assert_eq!(
            state.weekly_max_secs, 60.0,
            "last week's 120s weekly max must not carry into the new week"
        );
        // All-time is not week-scoped: the fold keeps the 120s peak.
        assert_eq!(state.alltime_max_secs, 120.0);
        assert_eq!(state.week_start.as_deref(), Some("2026-01-11"));
    }

    #[test]
    fn rehydrate_ignores_samples_at_or_after_now() {
        // A stray sample >= now (clock skew) must not corrupt the fold.
        let samples = vec![
            PresenceSample { ts: 1000.0, present: true, week: D0.to_string() },
            PresenceSample { ts: 2000.0, present: true, week: D0.to_string() }, // == now, skipped
        ];
        let (_, current) = compute_streak_from_samples(&samples, 2000.0, true, D0, 5000.0);
        assert_eq!(current, 1000.0);
    }

    #[test]
    fn prometheus_base_url_env_semantics() {
        std::env::set_var("CW_PROMETHEUS_URL", "http://prom:9090/");
        assert_eq!(prometheus_base_url().as_deref(), Some("http://prom:9090"));
        std::env::set_var("CW_PROMETHEUS_URL", "");
        assert_eq!(prometheus_base_url(), None, "empty => disabled");
        std::env::remove_var("CW_PROMETHEUS_URL");
        assert_eq!(
            prometheus_base_url().as_deref(),
            Some("http://localhost:9090")
        );
    }

    #[test]
    fn presence_freshness_window() {
        assert!(presence_is_fresh(Some(1000.0), 1050.0, 90.0));
        assert!(presence_is_fresh(Some(1000.0), 1090.0, 90.0));
        assert!(!presence_is_fresh(Some(1000.0), 1200.0, 90.0));
        assert!(!presence_is_fresh(None, 1000.0, 90.0));
    }

    #[test]
    fn read_gate_max_age_reads_manifest_else_default() {
        let dir = std::env::temp_dir().join(format!("cw_gate_test_{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        // A valid manifest yields params.max_age_secs (the real deploy value 420).
        let ok = dir.join("presence-gate.json");
        fs::write(&ok, r#"{"params":{"max_age_secs":420}}"#).unwrap();
        assert_eq!(read_gate_max_age_from(&[ok.clone()], 999.0), 420.0);
        // Missing file falls back to the default (never the old hardcoded 90).
        let missing = dir.join("does-not-exist.json");
        assert_eq!(read_gate_max_age_from(&[missing.clone()], 420.0), 420.0);
        // Non-positive / malformed values are skipped; first VALID candidate wins.
        let bad = dir.join("bad.json");
        fs::write(&bad, r#"{"params":{"max_age_secs":0}}"#).unwrap();
        assert_eq!(read_gate_max_age_from(&[missing, bad, ok], 999.0), 420.0);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn desk_streak_lines_exact_name_and_labels() {
        let lines = desk_streak_lines(42.0, 100.0, 250.0);
        let joined = lines.join("\n");
        assert!(joined.contains("# TYPE claude_operator_desk_streak_seconds gauge"));
        assert!(joined.contains("claude_operator_desk_streak_seconds{kind=\"current\"} 42.000"));
        assert!(joined.contains("claude_operator_desk_streak_seconds{kind=\"weekly_max\"} 100.000"));
        assert!(joined.contains("claude_operator_desk_streak_seconds{kind=\"max\"} 250.000"));
    }

    #[test]
    fn operator_present_lines_present_when_fresh() {
        // Carrier mtime within the window -> present=1; timestamp echoes mtime.
        let lines = operator_present_lines(Some(1000.0), 1050.0, 420.0);
        let joined = lines.join("\n");
        assert!(joined.contains("# TYPE claude_operator_present gauge"));
        assert!(joined.contains("claude_operator_present 1"));
        assert!(joined.contains("# TYPE claude_operator_present_timestamp_seconds gauge"));
        assert!(joined.contains("claude_operator_present_timestamp_seconds 1000.000"));
    }

    #[test]
    fn operator_present_lines_absent_when_stale() {
        // Carrier mtime older than the window -> present=0 (timestamp still echoed).
        let lines = operator_present_lines(Some(1000.0), 2000.0, 420.0);
        let joined = lines.join("\n");
        assert!(joined.contains("claude_operator_present 0"));
        assert!(joined.contains("claude_operator_present_timestamp_seconds 1000.000"));
    }

    #[test]
    fn operator_present_lines_absent_when_no_carrier() {
        // No carrier -> present=0 and timestamp defaults to 0.000 (matches the
        // Python bridge's os.path.getmtime-failure fallback).
        let lines = operator_present_lines(None, 1000.0, 420.0);
        let joined = lines.join("\n");
        assert!(joined.contains("claude_operator_present 0"));
        assert!(joined.contains("claude_operator_present_timestamp_seconds 0.000"));
    }

    #[test]
    fn operator_present_lines_byte_compatible_with_python_bridge() {
        // HELP/TYPE/value text must match the Python textfile bridge verbatim so
        // retiring the bridge (via CLAUDE_WATCH_PROM_FILE) is a drop-in.
        let lines = operator_present_lines(Some(1_767_225_600.0), 1_767_225_601.0, 420.0);
        assert_eq!(
            lines[0],
            "# HELP claude_operator_present Whether the operator is present (carrier mtime fresh within CW_PRESENCE_MAX_AGE secs); 1=present 0=absent"
        );
        assert_eq!(lines[1], "# TYPE claude_operator_present gauge");
        assert_eq!(lines[2], "claude_operator_present 1");
        assert_eq!(lines[3], "");
        assert_eq!(
            lines[4],
            "# HELP claude_operator_present_timestamp_seconds Epoch (mtime) of the operator-present carrier file touched by the host presence-detector while the operator is present"
        );
        assert_eq!(
            lines[5],
            "# TYPE claude_operator_present_timestamp_seconds gauge"
        );
        assert_eq!(
            lines[6],
            "claude_operator_present_timestamp_seconds 1767225600.000"
        );
        assert_eq!(lines[7], "");
        assert_eq!(
            lines[8],
            "# HELP claude_presence_gate_max_age_secs Operator-presence gate freshness window in seconds -- SINGLE SOURCE OF TRUTH (presence-gate.json params.max_age_secs). Consumers should read this gauge instead of hardcoding a window."
        );
        assert_eq!(lines[9], "# TYPE claude_presence_gate_max_age_secs gauge");
        // Integer-formatted window value (Python `:.0f`); 420.0 -> "420".
        assert_eq!(lines[10], "claude_presence_gate_max_age_secs 420");
    }

    #[test]
    fn streak_state_round_trips_and_defaults() {
        let dir = std::env::temp_dir().join(format!("cw_streak_test_{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let p = dir.join("desk_streak.json");
        let st = StreakState {
            run_start: Some(123.0),
            weekly_max_secs: 456.0,
            week_start: Some("2026-01-04".to_string()),
            alltime_max_secs: 7890.0,
            last_sample: Some(789.0),
            last_present: true,
        };
        save_streak_state(&p, &st).unwrap();
        let loaded = load_streak_state(&p);
        assert_eq!(loaded.run_start, Some(123.0));
        assert_eq!(loaded.weekly_max_secs, 456.0);
        assert_eq!(loaded.week_start.as_deref(), Some("2026-01-04"));
        assert_eq!(loaded.alltime_max_secs, 7890.0);
        assert_eq!(loaded.last_sample, Some(789.0));
        assert!(loaded.last_present);
        // Missing file -> default (away, zero maxes, no week stamp).
        let d = load_streak_state(&dir.join("does-not-exist.json"));
        assert_eq!(d.run_start, None);
        assert_eq!(d.weekly_max_secs, 0.0);
        assert_eq!(d.alltime_max_secs, 0.0);
        assert_eq!(d.week_start, None);
        assert!(!d.last_present);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn sunday_week_key_groups_sunday_through_saturday() {
        use chrono::{Datelike, NaiveDate, Weekday};
        // 2026-01-04 is a Sunday; that week runs Sun 01-04 .. Sat 01-10.
        let sunday = NaiveDate::from_ymd_opt(2026, 1, 4).unwrap();
        assert_eq!(sunday.weekday(), Weekday::Sun, "fixture sanity");
        assert_eq!(sunday_week_key(sunday), "2026-01-04");
        // Every day Sun..Sat maps to that same Sunday key.
        for d in 4..=10 {
            let day = NaiveDate::from_ymd_opt(2026, 1, d).unwrap();
            assert_eq!(sunday_week_key(day), "2026-01-04", "same week");
        }
        // The next Sunday starts a new week.
        let next_sun = NaiveDate::from_ymd_opt(2026, 1, 11).unwrap();
        assert_eq!(next_sun.weekday(), Weekday::Sun);
        assert_eq!(sunday_week_key(next_sun), "2026-01-11");
        // The returned key is itself always a Sunday (Tue 2026-08-18 -> Sun 08-16).
        let key = sunday_week_key(NaiveDate::from_ymd_opt(2026, 8, 18).unwrap());
        assert_eq!(key, "2026-08-16");
        assert_eq!(
            NaiveDate::parse_from_str(&key, "%Y-%m-%d").unwrap().weekday(),
            Weekday::Sun
        );
    }

    #[test]
    fn merge_persisted_maxes_floors_alltime_and_same_week_weekly() {
        // A short-window recompute under-reports; the persisted sidecar floors.
        let computed = StreakState {
            run_start: Some(0.0),
            weekly_max_secs: 50.0,
            week_start: Some("2026-01-04".to_string()),
            alltime_max_secs: 50.0,
            last_sample: Some(0.0),
            last_present: true,
        };
        let persisted = StreakState {
            run_start: None,
            weekly_max_secs: 300.0, // earlier this week, outside the window
            week_start: Some("2026-01-04".to_string()),
            alltime_max_secs: 9000.0, // months ago
            last_sample: None,
            last_present: false,
        };
        let m = merge_persisted_maxes(computed, &persisted);
        assert_eq!(m.weekly_max_secs, 300.0, "same-week persisted weekly floors");
        assert_eq!(m.alltime_max_secs, 9000.0, "all-time persisted floors");
    }

    #[test]
    fn merge_persisted_maxes_drops_stale_week_weekly_but_keeps_alltime() {
        let computed = StreakState {
            run_start: Some(0.0),
            weekly_max_secs: 40.0,
            week_start: Some("2026-01-11".to_string()), // NEW week
            alltime_max_secs: 40.0,
            last_sample: Some(0.0),
            last_present: true,
        };
        let persisted = StreakState {
            run_start: None,
            weekly_max_secs: 300.0,
            week_start: Some("2026-01-04".to_string()), // last week
            alltime_max_secs: 9000.0,
            last_sample: None,
            last_present: false,
        };
        let m = merge_persisted_maxes(computed, &persisted);
        assert_eq!(m.weekly_max_secs, 40.0, "stale-week persisted weekly is NOT merged");
        assert_eq!(m.alltime_max_secs, 9000.0, "all-time floors regardless of week");
    }

    #[test]
    fn load_streak_state_migrates_legacy_daily_max_to_alltime_floor() {
        let dir = std::env::temp_dir().join(format!("cw_streak_migrate_{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let p = dir.join("desk_streak.json");
        // Legacy sidecar shape: only run_start + daily max_streak_secs/max_date.
        fs::write(
            &p,
            r#"{"run_start":null,"max_streak_secs":1234.0,"max_date":"2026-01-01","last_sample":500.0,"last_present":false}"#,
        )
        .unwrap();
        let st = load_streak_state(&p);
        assert_eq!(st.alltime_max_secs, 1234.0, "legacy daily max seeds all-time floor");
        assert_eq!(st.weekly_max_secs, 0.0, "no legacy weekly field -> starts fresh");
        assert_eq!(st.week_start, None);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_iso_rfc3339() {
        let v = parse_iso_timestamp("2026-01-01T00:00:00-05:00");
        assert!(v > 0.0);
    }

    #[test]
    fn parse_iso_empty_is_zero() {
        assert_eq!(parse_iso_timestamp(""), 0.0);
    }

    #[test]
    fn parse_iso_garbage_is_zero() {
        assert_eq!(parse_iso_timestamp("not a date"), 0.0);
    }

    #[test]
    fn build_metrics_minimal() {
        let state = json!({});
        let lines = build_metrics(&state, "1.2.3", "1.2.4", &LiveCounts::default(), None, None);
        // Key lines present
        assert!(lines.iter().any(|l| l == "claude_watch_up 1"));
        assert!(lines.iter().any(|l| l == "claude_context_tokens 0"));
        assert!(lines
            .iter()
            .any(|l| l.contains("claude_version_info{current=\"1.2.3\",latest=\"1.2.4\"} 1")));
        // Build-info gauge is emitted with version/commit/pr labels and value 1.
        // commit/pr come from build.rs env stamping (fall back to "unknown"/"").
        assert!(lines.iter().any(|l| l == "# TYPE claude_watch_build_info gauge"));
        assert!(lines.iter().any(|l| {
            l.starts_with("claude_watch_build_info{version=\"")
                && l.contains(",commit=\"")
                && l.contains(",pr=\"")
                && l.ends_with("} 1")
        }));
    }

    #[test]
    fn last_context_clear_omitted_when_no_anchor() {
        // Regression: with NO observed clear, NO session-start fallback, and NO
        // persisted daemon_start_epoch, the gauge must NOT export with a 0.0
        // (epoch-zero) value -- that renders downstream as ~56.5 years
        // ("now - 1970"). No anchor => absent series.
        let state = json!({});
        let lines = build_metrics(&state, "x", "y", &LiveCounts::default(), None, None);
        assert!(
            !lines
                .iter()
                .any(|l| l.starts_with("claude_last_context_clear_timestamp_seconds")),
            "gauge must be omitted when no anchor exists, got: {lines:?}"
        );
    }

    #[test]
    fn last_context_clear_falls_back_to_session_start() {
        // No observed clear, but a session-start fallback (container start) is
        // provided: the gauge is emitted with that anchor so the panel renders
        // a real duration instead of "no data".
        let state = json!({});
        let lines = build_metrics(&state, "x", "y", &LiveCounts::default(), None, Some(1000.0));
        let line = lines
            .iter()
            .find(|l| l.starts_with("claude_last_context_clear_timestamp_seconds "))
            .expect("gauge should fall back to the session-start anchor");
        assert!(
            line.contains("1000"),
            "expected session-start epoch in line, got: {line}"
        );
    }

    #[test]
    fn last_context_clear_falls_back_to_daemon_start_epoch() {
        // No observed clear and no session-start fallback, but a persisted
        // daemon_start_epoch exists (the bare-host path): the gauge falls back
        // to it so the panel still renders a duration.
        let state = json!({ "daemon_start_epoch": 1234.5 });
        let lines = build_metrics(&state, "x", "y", &LiveCounts::default(), None, None);
        let line = lines
            .iter()
            .find(|l| l.starts_with("claude_last_context_clear_timestamp_seconds "))
            .expect("gauge should fall back to daemon_start_epoch");
        assert!(
            line.contains("1234.5"),
            "expected daemon_start_epoch in line, got: {line}"
        );
    }

    #[test]
    fn explicit_clear_wins_over_fallbacks() {
        // An observed clear takes priority over both fallbacks.
        let state = json!({
            "last_context_clear": "2026-01-01T00:00:00Z",
            "daemon_start_epoch": 1234.5
        });
        let lines = build_metrics(&state, "x", "y", &LiveCounts::default(), None, Some(1000.0));
        let line = lines
            .iter()
            .find(|l| l.starts_with("claude_last_context_clear_timestamp_seconds "))
            .expect("gauge should be present");
        // 2026-01-01T00:00:00Z == 1767225600 epoch secs (not 1000 or 1234.5).
        assert!(
            line.contains("1767225600"),
            "expected observed-clear epoch to win, got: {line}"
        );
    }

    #[test]
    fn last_context_clear_emitted_when_set() {
        let state = json!({ "last_context_clear": "2026-01-01T00:00:00Z" });
        let lines = build_metrics(&state, "x", "y", &LiveCounts::default(), None, None);
        let line = lines
            .iter()
            .find(|l| l.starts_with("claude_last_context_clear_timestamp_seconds "))
            .expect("gauge should be present when a clear is recorded");
        // 2026-01-01T00:00:00Z == 1767225600 epoch secs.
        assert!(
            line.contains("1767225600"),
            "expected recorded epoch in line, got: {line}"
        );
    }

    #[test]
    fn build_metrics_watcher_health() {
        let state = json!({
            "watcher_health": {
                "alerts-watcher": {"enabled": true, "consecutive_missing": 0},
                "torrent-wait": {"enabled": true, "consecutive_missing": 5},
                "dead-one": {"enabled": false, "consecutive_missing": 10},
            },
            "last_known_tokens": 42,
            "alert_count": 3,
        });
        let lines = build_metrics(&state, "x", "y", &LiveCounts::default(), None, None);
        assert!(lines.iter().any(|l| l == "claude_watchers_total 2"));
        assert!(lines.iter().any(|l| l == "claude_watchers_missing 1"));
        assert!(lines.iter().any(|l| l == "claude_context_tokens 42"));
        assert!(lines.iter().any(|l| l == "claude_alert_count 3"));
    }

    #[test]
    fn down_metrics_format() {
        let lines = down_metrics();
        assert!(lines.iter().any(|l| l == "claude_watch_up 0"));
    }

    #[test]
    fn heartbeat_file_mtime_present_for_fresh_file() {
        let dir = tempfile::tempdir().unwrap();
        let hb = dir.path().join("heartbeat");
        std::fs::write(&hb, b"").unwrap();
        let mtime = heartbeat_file_mtime_secs(&hb).expect("fresh file should yield an mtime");
        // Within a generous window of "now" — file was just written.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs_f64();
        assert!(
            (now - mtime).abs() < 60.0,
            "mtime {mtime} should be near now {now}"
        );
    }

    #[test]
    fn heartbeat_file_mtime_none_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does-not-exist");
        assert!(heartbeat_file_mtime_secs(&missing).is_none());
    }

    #[test]
    fn parse_pid_starttime_after_last_paren() {
        // comm contains a space AND an internal ')': the robust parse must
        // split on the LAST ')'. starttime (field 22) here is 7777.
        let stat = "1 (odd )name) S 0 1 1 0 -1 4194560 0 0 0 0 0 0 0 0 20 0 1 0 7777 12345";
        assert_eq!(parse_pid_starttime_ticks(stat), Some(7777));
    }

    #[test]
    fn parse_btime_extracts_boot_epoch() {
        let proc_stat = "cpu  1 2 3\nbtime 1700000000\nprocesses 42\n";
        assert_eq!(parse_btime(proc_stat), Some(1_700_000_000));
    }

    #[test]
    fn container_start_epoch_combines_btime_and_ticks() {
        // starttime field 22 = 200 ticks; clk_tck = 100 -> 2.0s after btime.
        let stat = "1 (init) S 0 1 1 0 -1 0 0 0 0 0 0 0 0 0 20 0 1 0 200 0";
        assert_eq!(container_start_epoch(stat, "btime 1000\n", 100), Some(1002.0));
    }

    #[test]
    fn container_start_epoch_none_on_zero_clk_tck() {
        assert_eq!(
            container_start_epoch("1 (x) S 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 5", "btime 1\n", 0),
            None
        );
    }

    #[test]
    fn build_metrics_includes_mainloop_heartbeat_when_present() {
        let state = json!({});
        // A fixed, recognizable epoch (2026-01-01T00:00:00Z = 1767225600).
        let lines = build_metrics(
            &state,
            "x",
            "y",
            &LiveCounts::default(),
            Some(1_767_225_600.0),
            None,
        );
        assert!(lines
            .iter()
            .any(|l| l == "claude_mainloop_heartbeat_timestamp_seconds 1767225600.000"));
        assert!(lines
            .iter()
            .any(|l| l == "# TYPE claude_mainloop_heartbeat_timestamp_seconds gauge"));
        // The daemon-check gauge must remain present and untouched.
        assert!(lines
            .iter()
            .any(|l| l.starts_with("claude_heartbeat_timestamp_seconds ")));
    }

    #[test]
    fn build_metrics_omits_mainloop_heartbeat_when_absent() {
        let state = json!({});
        let lines = build_metrics(&state, "x", "y", &LiveCounts::default(), None, None);
        assert!(!lines
            .iter()
            .any(|l| l.contains("claude_mainloop_heartbeat_timestamp_seconds")));
        // Daemon-check gauge still present.
        assert!(lines
            .iter()
            .any(|l| l.starts_with("claude_heartbeat_timestamp_seconds ")));
    }

    #[test]
    fn write_and_read_prom() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.prom");
        let lines = vec![
            "a".to_string(),
            "b".to_string(),
            "".to_string(),
            "c".to_string(),
        ];
        write_prom(&lines, &path).unwrap();
        let read = std::fs::read_to_string(&path).unwrap();
        assert_eq!(read, "a\nb\n\nc\n");
    }

    #[test]
    fn build_metrics_includes_fallback_counters() {
        let state = json!({
            "fallback_clear_count": 4,
            "fallback_update_count": 2,
            "reminder_to_clear_latency_secs_sum": 123.5,
            "reminder_to_clear_latency_count": 3,
        });
        let lines = build_metrics(&state, "x", "y", &LiveCounts::default(), None, None);
        let joined = lines.join("\n");
        assert!(joined.contains(
            "claude_watch_fallback_injections_total{type=\"clear\"} 4"
        ));
        assert!(joined.contains(
            "claude_watch_fallback_injections_total{type=\"update\"} 2"
        ));
        assert!(joined.contains(
            "claude_watch_reminder_to_action_latency_seconds_sum{type=\"clear\"} 123.500"
        ));
        assert!(joined.contains(
            "claude_watch_reminder_to_action_latency_seconds_count{type=\"clear\"} 3"
        ));
    }

    #[test]
    fn build_metrics_includes_per_interrupt_kind_counters() {
        // Each kind should render as claude_interrupts_total{kind="..."} <value>
        let state = json!({
            "prolonged_thinking_interrupts_total": 7,
            "foreground_blocking_interrupts_total": 3,
            "context_warning_interrupts_total": 11,
            "watcher_down_interrupts_total": 42,
            "wedged_clear_interrupts_total": 2,
            "auto_update_interrupts_total": 19,
            "reauth_inject_interrupts_total": 1,
            "self_login_autofire_total": 9,
            "post_restart_resume_inject_interrupts_total": 4,
            "fresh_session_inject_interrupts_total": 5,
            "fresh_clear_resume_inject_interrupts_total": 6,
            "restart_claude_interrupts_total": 8,
        });
        let lines = build_metrics(&state, "x", "y", &LiveCounts::default(), None, None);
        let joined = lines.join("\n");

        // # TYPE claude_interrupts_total counter (NOT gauge)
        assert!(
            joined.contains("# TYPE claude_interrupts_total counter"),
            "missing counter type declaration: {}",
            joined
        );

        // Each kind present with expected value
        for (kind, value) in [
            ("prolonged_thinking", 7),
            ("foreground_blocking", 3),
            ("context_warning", 11),
            ("watcher_down", 42),
            ("wedged_clear", 2),
            ("auto_update", 19),
            ("reauth_inject", 1),
            ("self_login_autofire", 9),
            ("post_restart_resume_inject", 4),
            ("fresh_session_inject", 5),
            ("fresh_clear_resume_inject", 6),
            ("restart_claude", 8),
        ] {
            let needle = format!(
                "claude_interrupts_total{{kind=\"{}\"}} {}",
                kind, value
            );
            assert!(
                joined.contains(&needle),
                "missing interrupt line {:?} in:\n{}",
                needle,
                joined
            );
        }
    }

    #[test]
    fn build_metrics_per_interrupt_defaults_to_zero() {
        // Missing fields default to 0 (new counters, state file predates them).
        let state = json!({});
        let lines = build_metrics(&state, "x", "y", &LiveCounts::default(), None, None);
        let joined = lines.join("\n");
        assert!(
            joined.contains("claude_interrupts_total{kind=\"prolonged_thinking\"} 0"),
            "missing zero-default for prolonged_thinking: {}",
            joined
        );
        assert!(
            joined.contains("claude_interrupts_total{kind=\"watcher_down\"} 0"),
            "missing zero-default for watcher_down: {}",
            joined
        );
    }

    #[test]
    fn build_metrics_includes_reminder_fire_labels() {
        // We don't control the marker files here (reminder_fire_lines()
        // reads from the shared dir), but we can at least verify all
        // three label types are present in the output.
        let state = json!({});
        let lines = build_metrics(&state, "x", "y", &LiveCounts::default(), None, None);
        let joined = lines.join("\n");
        for label in ["context_high", "version_update", "pre_compact"] {
            assert!(
                joined.contains(&format!(
                    "claude_watch_reminder_fires_total{{type=\"{}\"}}",
                    label
                )),
                "missing reminder fire line for {}: {}",
                label,
                joined
            );
        }
    }

    #[test]
    fn build_metrics_live_counts_zero_default() {
        // LiveCounts::default() means all five gauges emit 0.
        let state = json!({});
        let lines = build_metrics(&state, "x", "y", &LiveCounts::default(), None, None);
        let joined = lines.join("\n");
        for name in [
            "claude_code_active_agents",
            "claude_code_running_tasks",
            "claude_code_live_watchers",
            "claude_code_enabled_watchers",
            "claude_code_open_bashes",
        ] {
            let needle = format!("{} 0", name);
            assert!(
                joined.lines().any(|l| l == needle),
                "missing zero-default {:?} in:\n{}",
                needle,
                joined
            );
        }
    }

    #[test]
    fn build_metrics_live_counts_populated() {
        // Non-zero LiveCounts values render correctly.
        let state = json!({});
        let live = LiveCounts {
            active_agents: 2,
            running_tasks: 1,
            live_watchers: 3,
            enabled_watchers: 3,
            open_bashes: 4,
        };
        let lines = build_metrics(&state, "x", "y", &live, None, None);
        let joined = lines.join("\n");
        assert!(joined.contains("claude_code_active_agents 2"), "{joined}");
        assert!(joined.contains("claude_code_running_tasks 1"), "{joined}");
        assert!(joined.contains("claude_code_live_watchers 3"), "{joined}");
        assert!(joined.contains("claude_code_enabled_watchers 3"), "{joined}");
        assert!(joined.contains("claude_code_open_bashes 4"), "{joined}");
    }

    #[test]
    fn prom_file_path_honors_env_override() {
        std::env::set_var("CLAUDE_WATCH_PROM_FILE", "/tmp/cw-custom.prom");
        assert_eq!(prom_file_path(), PathBuf::from("/tmp/cw-custom.prom"));
        std::env::remove_var("CLAUDE_WATCH_PROM_FILE");
        assert_eq!(prom_file_path(), PathBuf::from(PROM_FILE));
    }

    #[test]
    fn default_state_file_honors_env_override() {
        // CLAUDE_WATCH_STATE wins and short-circuits before the config /
        // hardcoded-default fallbacks — the metrics reader must be able to be
        // pointed at the daemon's live state file explicitly.
        std::env::set_var("CLAUDE_WATCH_STATE", "/tmp/cw-live-state.json");
        assert_eq!(
            default_state_file(),
            PathBuf::from("/tmp/cw-live-state.json")
        );
        // An empty override is ignored (falls through to config/default rather
        // than pointing the reader at a bogus "" path).
        std::env::set_var("CLAUDE_WATCH_STATE", "");
        assert_ne!(default_state_file(), PathBuf::from(""));
        std::env::remove_var("CLAUDE_WATCH_STATE");
    }
}
