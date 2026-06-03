//! metrics — write Prometheus textfile metrics for node-exporter.
//!
//! Rust port of `claude-watch-metrics` (Python). Reads
//! `~/.config/claude-watch/state.json` and writes
//! `/var/lib/node-exporter/textfile/claude_watch.prom` atomically.
//!
//! Run from cron every minute:
//!     * * * * * /home/hndrewaall/bin/claude-watch metrics

use crate::reminders::all_fire_counts;
use crate::status::get_version_info;
use chrono::DateTime;
use serde_json::Value;
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

const PROM_FILE: &str = "/var/lib/node-exporter/textfile/claude_watch.prom";

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

fn build_metrics(
    state: &Value,
    current_version: &str,
    latest_version: &str,
    live: &LiveCounts,
) -> Vec<String> {
    let last_check = state
        .get("last_check")
        .and_then(|v| v.as_str())
        .map(parse_iso_timestamp)
        .unwrap_or(0.0);
    let last_context_clear = state
        .get("last_context_clear")
        .and_then(|v| v.as_str())
        .map(parse_iso_timestamp)
        .unwrap_or(0.0);

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

    vec![
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
        "# HELP claude_last_context_clear_timestamp_seconds Epoch of last context clear"
            .to_string(),
        "# TYPE claude_last_context_clear_timestamp_seconds gauge".to_string(),
        format!(
            "claude_last_context_clear_timestamp_seconds {:.3}",
            last_context_clear
        ),
        "".to_string(),
        "# HELP claude_version_info Claude Code version info".to_string(),
        "# TYPE claude_version_info gauge".to_string(),
        format!(
            "claude_version_info{{current=\"{}\",latest=\"{}\"}} 1",
            current_version, latest_version
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
    ]
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
        active_agents: agents.subagents.len() as u32,
        running_tasks: agents.workloads.len() as u32,
        live_watchers,
        enabled_watchers,
        open_bashes,
    }
}

/// CLI entry point: `claude-watch metrics`.
pub async fn cmd_metrics() -> i32 {
    let state_path = default_state_file();
    let prom_path = PathBuf::from(PROM_FILE);

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
    let lines = build_metrics(&state, &cur, &latest, &live);
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
        let lines = build_metrics(&state, "1.2.3", "1.2.4", &LiveCounts::default());
        // Key lines present
        assert!(lines.iter().any(|l| l == "claude_watch_up 1"));
        assert!(lines.iter().any(|l| l == "claude_context_tokens 0"));
        assert!(lines
            .iter()
            .any(|l| l.contains("claude_version_info{current=\"1.2.3\",latest=\"1.2.4\"} 1")));
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
        let lines = build_metrics(&state, "x", "y", &LiveCounts::default());
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
        let lines = build_metrics(&state, "x", "y", &LiveCounts::default());
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
        let lines = build_metrics(&state, "x", "y", &LiveCounts::default());
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
        let lines = build_metrics(&state, "x", "y", &LiveCounts::default());
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
        let lines = build_metrics(&state, "x", "y", &LiveCounts::default());
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
        let lines = build_metrics(&state, "x", "y", &LiveCounts::default());
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
        let lines = build_metrics(&state, "x", "y", &live);
        let joined = lines.join("\n");
        assert!(joined.contains("claude_code_active_agents 2"), "{joined}");
        assert!(joined.contains("claude_code_running_tasks 1"), "{joined}");
        assert!(joined.contains("claude_code_live_watchers 3"), "{joined}");
        assert!(joined.contains("claude_code_enabled_watchers 3"), "{joined}");
        assert!(joined.contains("claude_code_open_bashes 4"), "{joined}");
    }
}
