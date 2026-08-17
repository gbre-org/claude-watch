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
use chrono::{DateTime, Local};
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
        return PathBuf::from(s);
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

fn build_metrics(
    state: &Value,
    current_version: &str,
    latest_version: &str,
    live: &LiveCounts,
    mainloop_heartbeat_mtime: Option<f64>,
) -> Vec<String> {
    let last_check = state
        .get("last_check")
        .and_then(|v| v.as_str())
        .map(parse_iso_timestamp)
        .unwrap_or(0.0);
    // Epoch (float secs) of the last context clear, or `None` when the daemon
    // has not yet recorded a clear this session. Crucially we DO NOT default a
    // missing/unparseable value to `0.0`: a zero epoch makes a downstream
    // "now - last_clear" panel render ~56.5 years (2026 - 1970), the classic
    // epoch-zero bug. Treating "no clear recorded" as an absent series (gauge
    // omitted) matches how `mainloop_heartbeat_mtime` handles a missing file.
    let last_context_clear = state
        .get("last_context_clear")
        .and_then(|v| v.as_str())
        .map(parse_iso_timestamp)
        .filter(|&t| t > 0.0);

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

    // Last context-clear timestamp. Omitted entirely when the daemon has not
    // recorded a clear this session (see `last_context_clear` derivation
    // above) so a downstream "time since last clear" panel shows "no data"
    // rather than a bogus ~56-year duration computed from epoch zero.
    if let Some(ts) = last_context_clear {
        lines.push("".to_string());
        lines.push(
            "# HELP claude_last_context_clear_timestamp_seconds Epoch of last context clear"
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
    let current = info.running.unwrap_or_else(|| "unknown".to_string());
    let latest = info.installed.unwrap_or_else(|| "unknown".to_string());
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
    /// Longest continuous-present run observed TODAY (seconds). Persisted, so
    /// it survives restarts; scoped to the local calendar day via `max_date`.
    max_streak_secs: f64,
    /// Local calendar date ("%Y-%m-%d") the `max_streak_secs` belongs to.
    /// When a sample's local date differs, the day has rolled over and the
    /// max restarts. `None` on a fresh/legacy sidecar (treated as "no day yet"
    /// -> the first sample seeds today's max).
    max_date: Option<String>,
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
    StreakState {
        run_start: v.get("run_start").and_then(|x| x.as_f64()),
        max_streak_secs: v
            .get("max_streak_secs")
            .and_then(|x| x.as_f64())
            .unwrap_or(0.0)
            .max(0.0),
        max_date: v
            .get("max_date")
            .and_then(|x| x.as_str())
            .map(|s| s.to_string()),
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
        "max_streak_secs": state.max_streak_secs,
        "max_date": state.max_date,
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
/// - While present, the current run accumulates as `now - run_start`; `max`
///   ratchets up to the longest run seen TODAY.
/// - On present->away the current run resets to 0 (run_start cleared); `max`
///   is untouched (already captured while present).
/// - Gap handling: if the operator was present at the last sample AND the
///   elapsed time since that sample exceeds `max_gap_secs`, continuity across
///   the unobserved gap can't be asserted (cron/daemon was down, or the laptop
///   slept and the carrier re-freshened between samples) -> the run restarts
///   at `now` rather than over-counting the gap.
/// - Daily scoping: `max` is scoped to the local calendar day `today`
///   ("%Y-%m-%d"). If the stored `max_date` differs from `today` the day has
///   rolled over, so the day's max restarts from the current run (0 when away)
///   -- i.e. `max` resets at local midnight. A same-day sample carries the
///   stored max forward and ratchets it. The returned state's `max_date` is
///   always stamped to `today`.
///
/// Returns the new state and the current-run length in seconds.
fn advance_streak(
    prev: &StreakState,
    present: bool,
    now: f64,
    max_gap_secs: f64,
    today: &str,
) -> (StreakState, f64) {
    // Daily-scoped baseline: carry the stored max forward only when it belongs
    // to `today`; otherwise the day rolled over and today's max starts at 0.
    let prev_max_today = match &prev.max_date {
        Some(d) if d == today => prev.max_streak_secs,
        _ => 0.0,
    };
    if !present {
        return (
            StreakState {
                run_start: None,
                max_streak_secs: prev_max_today,
                max_date: Some(today.to_string()),
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
    let max = prev_max_today.max(current);
    (
        StreakState {
            run_start: Some(run_start),
            max_streak_secs: max,
            max_date: Some(today.to_string()),
            last_sample: Some(now),
            last_present: true,
        },
        current,
    )
}

/// Render the `claude_operator_desk_streak_seconds` gauge block.
fn desk_streak_lines(current: f64, max: f64) -> Vec<String> {
    vec![
        "# HELP claude_operator_desk_streak_seconds Continuous operator at-desk presence streak in seconds (kind=current: trailing run ending now, resets to 0 on away; kind=max: longest continuous run TODAY, resets at local midnight). BOTH rehydrated from the Prometheus claude_operator_present series each emit, so they survive container/cw restarts.".to_string(),
        "# TYPE claude_operator_desk_streak_seconds gauge".to_string(),
        format!(
            "claude_operator_desk_streak_seconds{{kind=\"current\"}} {:.3}",
            current
        ),
        format!(
            "claude_operator_desk_streak_seconds{{kind=\"max\"}} {:.3}",
            max
        ),
    ]
}

/// Local calendar date ("%Y-%m-%d") used to scope the daily max. Uses the
/// host's LOCAL timezone -- the same local-wall-clock convention the rest of
/// the presence pipeline uses (`datetime.now()` in the Python bridge) -- so
/// "midnight" is the operator's local midnight, matching the Grafana panels'
/// local-day boundaries.
fn local_date_string() -> String {
    Local::now().format("%Y-%m-%d").to_string()
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
/// rehydrating. Must comfortably cover today (for the daily max) plus a current
/// run that began before local midnight. `CW_DESK_STREAK_LOOKBACK_SECS`
/// overrides; defaults to 48h.
fn desk_streak_lookback_secs() -> f64 {
    std::env::var("CW_DESK_STREAK_LOOKBACK_SECS")
        .ok()
        .and_then(|s| s.trim().parse::<f64>().ok())
        .filter(|v| *v > 0.0)
        .unwrap_or(172_800.0)
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

/// LOCAL calendar date ("%Y-%m-%d") of an epoch second, in the host's local
/// timezone (same convention as `local_date_string`). Scopes each rehydrated
/// Prometheus sample to its day for the daily-max midnight reset.
fn local_date_of(epoch: f64) -> String {
    use chrono::TimeZone;
    match Local.timestamp_opt(epoch as i64, 0).single() {
        Some(dt) => dt.format("%Y-%m-%d").to_string(),
        None => local_date_string(),
    }
}

/// One presence sample rehydrated from Prometheus: epoch second, present flag,
/// and the LOCAL calendar date that second falls in (for daily-max scoping).
#[derive(Debug, Clone, PartialEq)]
struct PresenceSample {
    ts: f64,
    present: bool,
    date: String,
}

/// Parse a Prometheus `query_range` matrix response into ordered presence
/// samples. Pure + testable: `date_fn` maps an epoch second to a LOCAL calendar
/// date so tests stay timezone-independent. Returns:
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
    date_fn: &dyn Fn(f64) -> String,
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
            date: date_fn(ts),
        });
    }
    samples.sort_by(|a, b| a.ts.partial_cmp(&b.ts).unwrap_or(std::cmp::Ordering::Equal));
    Some(samples)
}

/// Reconstruct the streak by folding the tested `advance_streak` over the
/// rehydrated Prometheus samples (each carrying its own local date, so
/// daily-max midnight resets AND scrape-gap breaks reuse the exact logic the
/// sidecar path uses), then over the LIVE "now" sample last so the emitted
/// `current` reflects this instant. Historical samples at or after `now` are
/// ignored. Returns the reconstructed state and the current-run length (secs).
fn compute_streak_from_samples(
    samples: &[PresenceSample],
    now: f64,
    present_now: bool,
    now_date: &str,
    max_gap_secs: f64,
) -> (StreakState, f64) {
    let mut state = StreakState::default();
    for s in samples {
        if s.ts >= now {
            continue;
        }
        let (next, _) = advance_streak(&state, s.present, s.ts, max_gap_secs, &s.date);
        state = next;
    }
    advance_streak(&state, present_now, now, max_gap_secs, now_date)
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
    let max_gap = presence_max_age() * 2.0;
    let today = local_date_string();
    let path = default_streak_state_file();

    // Preferred: rehydrate from Prometheus (survives restarts, no local state).
    if let Some(base) = prometheus_base_url() {
        if let Some(body) = fetch_prom_presence_range(
            &base,
            now,
            desk_streak_lookback_secs(),
            desk_streak_step_secs(),
        ) {
            if let Some(samples) = parse_prom_presence(&body, &|ts| local_date_of(ts)) {
                let (next, current) =
                    compute_streak_from_samples(&samples, now, present, &today, max_gap);
                // Keep the sidecar warm so a later Prometheus outage degrades
                // gracefully rather than restarting from zero.
                let _ = save_streak_state(&path, &next);
                return desk_streak_lines(current, next.max_streak_secs);
            }
        }
    }

    // Fallback: single-sample accumulation persisted to the sidecar.
    let prev = load_streak_state(&path);
    let (next, current) = advance_streak(&prev, present, now, max_gap, &today);
    let _ = save_streak_state(&path, &next);
    desk_streak_lines(current, next.max_streak_secs)
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

    let mut lines = build_metrics(&state, &cur, &latest, &live, mainloop_heartbeat_mtime);

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

    // A fixed local date used across the same-day streak tests.
    const D0: &str = "2026-01-01";

    #[test]
    fn streak_accumulates_while_present() {
        let s0 = StreakState::default();
        // First present sample starts a run at now; current is 0.
        let (s1, c1) = advance_streak(&s0, true, 1000.0, 200.0, D0);
        assert_eq!(c1, 0.0);
        assert_eq!(s1.run_start, Some(1000.0));
        assert_eq!(s1.max_date.as_deref(), Some(D0));
        // Still present 30s later: current accumulates, run_start unchanged.
        let (s2, c2) = advance_streak(&s1, true, 1030.0, 200.0, D0);
        assert_eq!(c2, 30.0);
        assert_eq!(s2.run_start, Some(1000.0));
        assert_eq!(s2.max_streak_secs, 30.0);
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
        assert_eq!(s3.max_streak_secs, 50.0);
        // Present again: brand-new run from now, current 0.
        let (s4, c4) = advance_streak(&s3, true, 1100.0, 200.0, D0);
        assert_eq!(c4, 0.0);
        assert_eq!(s4.run_start, Some(1100.0));
        assert_eq!(s4.max_streak_secs, 50.0);
    }

    #[test]
    fn streak_max_tracks_longest_run() {
        // A 100s run.
        let (s, _) = advance_streak(&StreakState::default(), true, 0.0, 200.0, D0);
        let (s, c) = advance_streak(&s, true, 100.0, 200.0, D0);
        assert_eq!(c, 100.0);
        assert_eq!(s.max_streak_secs, 100.0);
        // Away, then a shorter 20s run -- max must retain the longer 100s.
        let (s, _) = advance_streak(&s, false, 110.0, 200.0, D0);
        let (s, _) = advance_streak(&s, true, 120.0, 200.0, D0);
        let (s, c) = advance_streak(&s, true, 140.0, 200.0, D0);
        assert_eq!(c, 20.0);
        assert_eq!(s.max_streak_secs, 100.0);
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
        assert_eq!(s.max_streak_secs, 50.0);
    }

    #[test]
    fn streak_max_resets_at_local_midnight() {
        // Build up a 100s max on day 0.
        let (s, _) = advance_streak(&StreakState::default(), true, 0.0, 200.0, D0);
        let (s, c) = advance_streak(&s, true, 100.0, 200.0, D0);
        assert_eq!(c, 100.0);
        assert_eq!(s.max_streak_secs, 100.0);
        assert_eq!(s.max_date.as_deref(), Some(D0));

        // Day rolls over. A shorter run today must NOT inherit yesterday's 100s:
        // the daily max starts fresh from today's current run.
        const D1: &str = "2026-01-02";
        let (s, c) = advance_streak(&s, true, 1000.0, 200.0, D1);
        assert_eq!(c, 0.0, "new run starts at 0 across the day boundary");
        assert_eq!(
            s.max_streak_secs, 0.0,
            "yesterday's 100s max must not carry into the new day"
        );
        assert_eq!(s.max_date.as_deref(), Some(D1));
        let (s, c) = advance_streak(&s, true, 1030.0, 200.0, D1);
        assert_eq!(c, 30.0);
        assert_eq!(s.max_streak_secs, 30.0, "today's max is today's longest run");
        assert_eq!(s.max_date.as_deref(), Some(D1));
    }

    #[test]
    fn streak_max_persists_across_restart_same_day() {
        // Persistence regression: a sidecar carrying a same-day max must be
        // preserved even though the current run restarts (process/container
        // restart clears run_start/last_present but the sidecar survives).
        let prev = StreakState {
            run_start: None,
            max_streak_secs: 600.0,
            max_date: Some(D0.to_string()),
            last_sample: Some(500.0),
            last_present: false,
        };
        // Fresh present sample same day: current is 0 (new run) but the day's
        // max is retained from the sidecar.
        let (s, c) = advance_streak(&prev, true, 1000.0, 200.0, D0);
        assert_eq!(c, 0.0);
        assert_eq!(s.max_streak_secs, 600.0);
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
                date: "2026-01-01".to_string()
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
                date: D0.to_string(),
            })
            .collect();
        // now = 60s after the last sample, still present.
        let (state, current) = compute_streak_from_samples(&samples, 1360.0, true, D0, 200.0);
        assert_eq!(
            current, 360.0,
            "current run reconstructed from the first present sample's ts"
        );
        assert_eq!(state.max_streak_secs, 360.0);
    }

    #[test]
    fn rehydrate_breaks_continuity_on_scrape_gap() {
        let samples = vec![
            PresenceSample { ts: 0.0, present: true, date: D0.to_string() },
            PresenceSample { ts: 60.0, present: true, date: D0.to_string() },
            // 300s gap > max_gap 100 (cron/scrape outage): continuity broken.
            PresenceSample { ts: 360.0, present: true, date: D0.to_string() },
            PresenceSample { ts: 420.0, present: true, date: D0.to_string() },
        ];
        let (state, current) = compute_streak_from_samples(&samples, 480.0, true, D0, 100.0);
        assert_eq!(current, 120.0, "trailing run restarts after the gap (360..480)");
        assert_eq!(state.max_streak_secs, 120.0);
    }

    #[test]
    fn rehydrate_current_zero_when_away_now() {
        let samples = vec![
            PresenceSample { ts: 0.0, present: true, date: D0.to_string() },
            PresenceSample { ts: 60.0, present: true, date: D0.to_string() },
        ];
        // Live sample = away: current resets, today's max is preserved.
        let (state, current) = compute_streak_from_samples(&samples, 120.0, false, D0, 200.0);
        assert_eq!(current, 0.0);
        assert_eq!(state.max_streak_secs, 60.0);
    }

    #[test]
    fn rehydrate_daily_max_resets_across_midnight() {
        let samples = vec![
            PresenceSample { ts: 0.0, present: true, date: "2026-01-01".to_string() },
            PresenceSample { ts: 60.0, present: true, date: "2026-01-01".to_string() },
            PresenceSample { ts: 120.0, present: true, date: "2026-01-01".to_string() },
            // Next local day, after a gap.
            PresenceSample { ts: 1000.0, present: true, date: "2026-01-02".to_string() },
        ];
        let (state, current) =
            compute_streak_from_samples(&samples, 1060.0, true, "2026-01-02", 200.0);
        assert_eq!(current, 60.0);
        assert_eq!(
            state.max_streak_secs, 60.0,
            "yesterday's 120s max must not carry into the new day"
        );
        assert_eq!(state.max_date.as_deref(), Some("2026-01-02"));
    }

    #[test]
    fn rehydrate_ignores_samples_at_or_after_now() {
        // A stray sample >= now (clock skew) must not corrupt the fold.
        let samples = vec![
            PresenceSample { ts: 1000.0, present: true, date: D0.to_string() },
            PresenceSample { ts: 2000.0, present: true, date: D0.to_string() }, // == now, skipped
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
        let lines = desk_streak_lines(42.0, 100.0);
        let joined = lines.join("\n");
        assert!(joined.contains("# TYPE claude_operator_desk_streak_seconds gauge"));
        assert!(joined.contains("claude_operator_desk_streak_seconds{kind=\"current\"} 42.000"));
        assert!(joined.contains("claude_operator_desk_streak_seconds{kind=\"max\"} 100.000"));
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
            max_streak_secs: 456.0,
            max_date: Some("2026-01-01".to_string()),
            last_sample: Some(789.0),
            last_present: true,
        };
        save_streak_state(&p, &st).unwrap();
        let loaded = load_streak_state(&p);
        assert_eq!(loaded.run_start, Some(123.0));
        assert_eq!(loaded.max_streak_secs, 456.0);
        assert_eq!(loaded.max_date.as_deref(), Some("2026-01-01"));
        assert_eq!(loaded.last_sample, Some(789.0));
        assert!(loaded.last_present);
        // Missing file -> default (away, zero max, no day stamp).
        let d = load_streak_state(&dir.join("does-not-exist.json"));
        assert_eq!(d.run_start, None);
        assert_eq!(d.max_streak_secs, 0.0);
        assert_eq!(d.max_date, None);
        assert!(!d.last_present);
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
        let lines = build_metrics(&state, "1.2.3", "1.2.4", &LiveCounts::default(), None);
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
    fn last_context_clear_omitted_when_unset() {
        // Regression: a missing `last_context_clear` must NOT export the gauge
        // with a 0.0 (epoch-zero) value — that renders downstream as ~56.5
        // years ("now - 1970"). Absent clear => absent series.
        let state = json!({});
        let lines = build_metrics(&state, "x", "y", &LiveCounts::default(), None);
        assert!(
            !lines
                .iter()
                .any(|l| l.starts_with("claude_last_context_clear_timestamp_seconds")),
            "gauge must be omitted when no clear recorded, got: {lines:?}"
        );
    }

    #[test]
    fn last_context_clear_emitted_when_set() {
        let state = json!({ "last_context_clear": "2026-01-01T00:00:00Z" });
        let lines = build_metrics(&state, "x", "y", &LiveCounts::default(), None);
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
        let lines = build_metrics(&state, "x", "y", &LiveCounts::default(), None);
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
    fn build_metrics_includes_mainloop_heartbeat_when_present() {
        let state = json!({});
        // A fixed, recognizable epoch (2026-01-01T00:00:00Z = 1767225600).
        let lines = build_metrics(
            &state,
            "x",
            "y",
            &LiveCounts::default(),
            Some(1_767_225_600.0),
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
        let lines = build_metrics(&state, "x", "y", &LiveCounts::default(), None);
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
        let lines = build_metrics(&state, "x", "y", &LiveCounts::default(), None);
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
            "post_restart_resume_inject_interrupts_total": 4,
            "fresh_session_inject_interrupts_total": 5,
            "fresh_clear_resume_inject_interrupts_total": 6,
            "restart_claude_interrupts_total": 8,
        });
        let lines = build_metrics(&state, "x", "y", &LiveCounts::default(), None);
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
        let lines = build_metrics(&state, "x", "y", &LiveCounts::default(), None);
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
        let lines = build_metrics(&state, "x", "y", &LiveCounts::default(), None);
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
        let lines = build_metrics(&state, "x", "y", &LiveCounts::default(), None);
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
        let lines = build_metrics(&state, "x", "y", &live, None);
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
}
