//! Claude-event bus emitter.
//!
//! Writes structured JSON events into `~/claude-events/` so that
//! `claude-event-watch` surfaces them to the main loop. This is an
//! ADDITIVE third alert sink alongside the push-notification path
//! (`pingme`) and the tmux-inject prompt — those paths must keep firing
//! whether or not event emission succeeds.
//!
//! Field shape mirrors what `~/bin/claude-event` (Python helper) writes
//! so the consumer (`claude-event-watch`) needs no special-case logic.
//! The full event JSON is:
//!
//! ```json
//! {
//!   "timestamp": <unix float>,
//!   "timestamp_iso": "<RFC 3339 local>",
//!   "hostname": "...",
//!   "source": "claude-watch",
//!   "source_name": "claude-watch",
//!   "tag": "claude-watch-alert",
//!   "priority": "low|normal|high|urgent",
//!   "message": "<full human-readable, same as push-notification body>",
//!   "data": {
//!       "alert_type":        "<heartbeat-stale|prolonged-thinking|...>",
//!       "stuck_reason":      "<short human-readable>",
//!       "stale_minutes":     <int|null>,
//!       "affected_watchers": ["<name>", ...],
//!       "severity":          "<low|medium|high|critical>"
//!   },
//!   "pid":  <int>,
//!   "user": "..."
//! }
//! ```
//!
//! Note: `source` is **`claude-watch`**, which is outside the canonical
//! source enum used by the Python helper (`cron|alertmanager|queue|...`).
//! `claude-event-watch` itself doesn't validate `source` — it dispatches
//! by `tag`. The new tag is `claude-watch-alert`.
//!
//! Writes are atomic (tmp file in same dir + rename). Filename:
//! `<unix_ns>_claude-watch-alert.json` (matches Python helper convention).
//!
//! On any error the function logs and returns — never panics, never
//! propagates failure to the caller. Same default-open principle as the
//! obligations PreToolUse hook: a broken alert sink must not blackhole
//! the push-notification path or tmux-inject.

use serde::Serialize;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

/// Severity levels that map cleanly onto push-notification priority and
/// downstream triage decisions in the routing table.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Low,
    Medium,
    High,
    Critical,
}

impl Severity {
    /// Map severity → claude-event `priority` field. Mirrors what the
    /// Python `claude-event` helper accepts (`low|normal|high|urgent`).
    pub fn as_priority(self) -> &'static str {
        match self {
            Severity::Low => "low",
            Severity::Medium => "normal",
            Severity::High => "high",
            Severity::Critical => "urgent",
        }
    }
}

/// One claude-watch alert. Caller fills the fields it knows; missing
/// optional fields default to None / empty.
#[derive(Debug, Clone)]
pub struct ClaudeWatchAlert<'a> {
    /// Discriminator string matching the codebase alert paths:
    ///   `heartbeat-stale`, `prolonged-thinking`, `watcher-down`,
    ///   `fresh-clear-stuck`, `claude-crashed`, `auto-update-failed`,
    ///   `auto-update-complete`, `reauth-needed`, `wedged-pane`.
    pub alert_type: &'a str,
    /// Short human-readable reason. For heartbeat-path alerts this is
    /// the same `stuck_reason` already threaded through `policy.rs`.
    pub stuck_reason: &'a str,
    /// Heartbeat staleness in minutes (only meaningful for
    /// `heartbeat-stale`; None elsewhere).
    pub stale_minutes: Option<u64>,
    /// Names of watchers known to be missing (only meaningful for
    /// `watcher-down`; empty elsewhere).
    pub affected_watchers: Vec<String>,
    /// Severity tier driving push-notification priority + dispatch routing.
    pub severity: Severity,
    /// Full human-readable message, byte-for-byte the same string sent
    /// to the push-notification shim so log/event/push all agree.
    pub message: &'a str,
}

/// Build the JSON event body. Public for testability — production
/// callers should use `emit()` which also performs the atomic write.
pub fn build_event_json(alert: &ClaudeWatchAlert<'_>) -> serde_json::Value {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    let now_iso = chrono::Local::now().to_rfc3339();
    let hostname = hostname_string();
    let user = std::env::var("USER").unwrap_or_default();
    let pid = std::process::id();

    serde_json::json!({
        "timestamp": now,
        "timestamp_iso": now_iso,
        "hostname": hostname,
        "source": "claude-watch",
        "source_name": "claude-watch",
        "tag": "claude-watch-alert",
        "priority": alert.severity.as_priority(),
        "message": alert.message,
        "data": {
            "alert_type": alert.alert_type,
            "stuck_reason": alert.stuck_reason,
            "stale_minutes": alert.stale_minutes,
            "affected_watchers": alert.affected_watchers,
            "severity": alert.severity,
        },
        "pid": pid,
        "user": user,
    })
}

/// Resolve the queue dir. Honors `CLAUDE_EVENT_QUEUE` (preferred) and
/// the legacy `CRON_EVENT_QUEUE`, matching the Python helper. Falls
/// back to `~/claude-events/`.
pub fn queue_dir() -> PathBuf {
    if let Ok(p) = std::env::var("CLAUDE_EVENT_QUEUE") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    if let Ok(p) = std::env::var("CRON_EVENT_QUEUE") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home).join("claude-events")
}

/// Emit a claude-event JSON file into the queue dir. Default-open: any
/// I/O failure is logged at warn level and swallowed. The caller's
/// push-notification + tmux-inject paths must remain unaffected.
pub fn emit(alert: &ClaudeWatchAlert<'_>) {
    let dir = queue_dir();
    if let Err(e) = std::fs::create_dir_all(&dir) {
        tracing::warn!(error = %e, dir = %dir.display(),
            "claude-event emit: failed to create queue dir, skipping");
        return;
    }

    let event = build_event_json(alert);
    let body = match serde_json::to_string_pretty(&event) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "claude-event emit: failed to serialize event");
            return;
        }
    };

    // Atomic write: tmp file in same dir + rename. Filename matches
    // the Python helper's <ts_ns>_<safe_tag>.json convention so any
    // tooling that parses filenames stays compatible.
    let ts_ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let final_name = format!("{}_claude-watch-alert.json", ts_ns);
    let final_path = dir.join(&final_name);
    let tmp_path = dir.join(format!(".{}.tmp", final_name));

    if let Err(e) = std::fs::write(&tmp_path, body.as_bytes()) {
        tracing::warn!(error = %e, path = %tmp_path.display(),
            "claude-event emit: failed to write tmp file");
        return;
    }
    if let Err(e) = std::fs::rename(&tmp_path, &final_path) {
        tracing::warn!(error = %e, src = %tmp_path.display(), dst = %final_path.display(),
            "claude-event emit: failed to rename tmp into place");
        // best-effort cleanup
        let _ = std::fs::remove_file(&tmp_path);
        return;
    }

    tracing::info!(
        path = %final_path.display(),
        alert_type = %alert.alert_type,
        severity = ?alert.severity,
        "claude-event emitted"
    );
}

/// A generic daemon-emitted cadence event (`heartbeat-tick`,
/// `memory-reminder`). Unlike [`ClaudeWatchAlert`], these carry no
/// alert/stuck semantics — they are plain periodic signals the daemon
/// produces on its monotonic clock (see [`crate::cadence`]). The body
/// matches the same JSON shape the Python `claude-event` helper writes so
/// `claude-event-watch` dispatches purely on `tag`.
#[derive(Debug, Clone)]
pub struct CadenceEvent<'a> {
    /// Event tag (also the dispatch key). E.g. `heartbeat-tick`.
    pub tag: &'a str,
    /// `source` / `source_name` fields. `claude-watch` for these.
    pub source: &'a str,
    /// Human-readable message — for `heartbeat-tick` a short string, for
    /// `memory-reminder` the full action checklist.
    pub message: &'a str,
    /// Priority field (`low|normal|high|urgent`).
    pub priority: &'a str,
    /// Extra `data` fields merged into the event. Pass
    /// `serde_json::json!({})` for none.
    pub data: serde_json::Value,
}

/// Build the JSON body for a cadence event. Public for testability;
/// production callers use [`emit_cadence`].
pub fn build_cadence_json(ev: &CadenceEvent<'_>) -> serde_json::Value {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    let now_iso = chrono::Local::now().to_rfc3339();
    let hostname = hostname_string();
    let user = std::env::var("USER").unwrap_or_default();
    let pid = std::process::id();

    serde_json::json!({
        "timestamp": now,
        "timestamp_iso": now_iso,
        "hostname": hostname,
        "source": ev.source,
        "source_name": ev.source,
        "tag": ev.tag,
        "priority": ev.priority,
        "message": ev.message,
        "data": ev.data,
        "pid": pid,
        "user": user,
    })
}

/// Build the `data` body for a `heartbeat-tick` cadence event.
///
/// Carries the configured host heartbeat-file `path` — the file the main
/// loop is reminded to touch on each tick — plus the emit `interval_secs`.
/// The path is sourced from the daemon's existing `[claude].heartbeat_file`
/// config, which is the SAME path the daemon monitors for staleness, so the
/// "touch this file" instruction and the "this file went stale" detector can
/// never drift to different paths. Kept as a named builder so the daemon
/// call site and the unit tests agree on the body shape.
pub fn heartbeat_tick_data(heartbeat_path: &str, interval_secs: u64) -> serde_json::Value {
    serde_json::json!({
        "path": heartbeat_path,
        "interval_secs": interval_secs,
    })
}

/// Emit a cadence event JSON file into the queue dir. Default-open: any
/// I/O failure is logged at warn level and swallowed (a missed cadence
/// tick is harmless — the next interval re-fires).
pub fn emit_cadence(ev: &CadenceEvent<'_>) {
    let dir = queue_dir();
    if let Err(e) = std::fs::create_dir_all(&dir) {
        tracing::warn!(error = %e, dir = %dir.display(),
            "cadence emit: failed to create queue dir, skipping");
        return;
    }

    let event = build_cadence_json(ev);
    let body = match serde_json::to_string_pretty(&event) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "cadence emit: failed to serialize event");
            return;
        }
    };

    let ts_ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    // Sanitize the tag for the filename (matches the Python helper's
    // <ts_ns>_<safe_tag>.json convention).
    let safe_tag: String = ev
        .tag
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
        .collect();
    let final_name = format!("{}_{}.json", ts_ns, safe_tag);
    let final_path = dir.join(&final_name);
    let tmp_path = dir.join(format!(".{}.tmp", final_name));

    if let Err(e) = std::fs::write(&tmp_path, body.as_bytes()) {
        tracing::warn!(error = %e, path = %tmp_path.display(),
            "cadence emit: failed to write tmp file");
        return;
    }
    if let Err(e) = std::fs::rename(&tmp_path, &final_path) {
        tracing::warn!(error = %e, src = %tmp_path.display(), dst = %final_path.display(),
            "cadence emit: failed to rename tmp into place");
        let _ = std::fs::remove_file(&tmp_path);
        return;
    }

    tracing::info!(
        path = %final_path.display(),
        tag = %ev.tag,
        "cadence event emitted"
    );
}

/// One workload-done event. Emitted exactly once per workload run when
/// the underlying tmux-pane wrapper script finishes (or `workload kill`
/// terminates it). Surfaced to the main loop via `claude-event-watch`
/// as `EVENT[workload/workload-done] ...`, replacing the "fire a
/// `workload wait` background task and poll" pattern.
///
/// First-class workload model (Andrew DM 2026-05-03 05:23 ET): when
/// the workload was launched with `workload run --queue-id q-X`, the
/// queue id is carried into ``data.queue_id`` AND `cmd_emit_done`
/// transitions the queue item to done/abandoned in the same step.
/// Workload completion IS queue completion; no separate respawn-
/// obligation handshake.
#[derive(Debug, Clone)]
pub struct WorkloadDoneEvent<'a> {
    pub label: &'a str,
    /// Exit code as reported by the wrapper script. Negative values
    /// indicate non-natural termination — `-15` for `workload kill`
    /// (SIGTERM marker), other negative for future kill modes.
    pub exit_code: i32,
    /// True iff the wrapper script did not write its own exit code
    /// (i.e. `workload kill` raced ahead and synthesised this event).
    pub killed: bool,
    /// Path to the workload's output log so the main loop can `Read`
    /// the tail without re-deriving paths.
    pub log_path: &'a str,
    /// Optional queue id the workload was tied to (`workload run
    /// --queue-id q-X`). When present, included in the event's
    /// ``data.queue_id`` field so consumers can correlate without
    /// round-tripping through the workload state file.
    pub queue_id: Option<&'a str>,
}

/// Build the JSON event body for a workload-done event. Public for
/// testability; production callers should use `emit_workload_done()`.
pub fn build_workload_done_json(ev: &WorkloadDoneEvent<'_>) -> serde_json::Value {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    let now_iso = chrono::Local::now().to_rfc3339();
    let hostname = hostname_string();
    let user = std::env::var("USER").unwrap_or_default();
    let pid = std::process::id();

    // Human-readable message — same string the main loop sees in the
    // `EVENT[workload/workload-done] <preview>` one-liner.
    let message = if ev.killed {
        format!(
            "workload {} killed (rc={}, log={})",
            ev.label, ev.exit_code, ev.log_path
        )
    } else {
        format!(
            "workload {} done rc={} log={}",
            ev.label, ev.exit_code, ev.log_path
        )
    };

    // Priority: success = low (informational), failure/kill = normal
    // (still not urgent — the main loop should react but it's not an
    // alert).
    let priority = if ev.exit_code == 0 { "low" } else { "normal" };

    let mut data = serde_json::json!({
        "label": ev.label,
        "exit_code": ev.exit_code,
        "killed": ev.killed,
        "log_path": ev.log_path,
    });
    if let Some(qid) = ev.queue_id {
        data["queue_id"] = serde_json::Value::String(qid.to_string());
    }

    serde_json::json!({
        "timestamp": now,
        "timestamp_iso": now_iso,
        "hostname": hostname,
        "source": "workload",
        "source_name": ev.label,
        "tag": "workload-done",
        "priority": priority,
        "message": message,
        "data": data,
        "pid": pid,
        "user": user,
    })
}

/// Emit a workload-done event into the queue dir. Idempotency is the
/// caller's responsibility — this function unconditionally writes one
/// event file per call. Default-open: I/O failure is logged at warn
/// level and swallowed (the wrapper script's exit-file write already
/// happened; losing the event is recoverable via `workload list`).
pub fn emit_workload_done(ev: &WorkloadDoneEvent<'_>) {
    let dir = queue_dir();
    if let Err(e) = std::fs::create_dir_all(&dir) {
        tracing::warn!(error = %e, dir = %dir.display(),
            "workload-done emit: failed to create queue dir, skipping");
        return;
    }

    let event = build_workload_done_json(ev);
    let body = match serde_json::to_string_pretty(&event) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "workload-done emit: failed to serialize event");
            return;
        }
    };

    let ts_ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let final_name = format!("{}_workload-done.json", ts_ns);
    let final_path = dir.join(&final_name);
    let tmp_path = dir.join(format!(".{}.tmp", final_name));

    if let Err(e) = std::fs::write(&tmp_path, body.as_bytes()) {
        tracing::warn!(error = %e, path = %tmp_path.display(),
            "workload-done emit: failed to write tmp file");
        return;
    }
    if let Err(e) = std::fs::rename(&tmp_path, &final_path) {
        tracing::warn!(error = %e, src = %tmp_path.display(), dst = %final_path.display(),
            "workload-done emit: failed to rename tmp into place");
        let _ = std::fs::remove_file(&tmp_path);
        return;
    }

    tracing::info!(
        path = %final_path.display(),
        label = %ev.label,
        exit_code = ev.exit_code,
        killed = ev.killed,
        "workload-done event emitted"
    );
}

/// Cheap, no-deps hostname lookup. Falls back to `gethostname`'s
/// failure mode (empty string) — the event still emits, the field is
/// just blank.
fn hostname_string() -> String {
    // Try /etc/hostname first (cheap, no syscall), then `uname -n`,
    // then env. nix could supply this but we already pull libc; keep
    // this dep-free.
    if let Ok(s) = std::fs::read_to_string("/etc/hostname") {
        let trimmed = s.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    if let Ok(s) = std::env::var("HOSTNAME") {
        if !s.is_empty() {
            return s;
        }
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn severity_maps_to_priority() {
        assert_eq!(Severity::Low.as_priority(), "low");
        assert_eq!(Severity::Medium.as_priority(), "normal");
        assert_eq!(Severity::High.as_priority(), "high");
        assert_eq!(Severity::Critical.as_priority(), "urgent");
    }

    #[test]
    fn build_event_json_has_required_fields() {
        let alert = ClaudeWatchAlert {
            alert_type: "heartbeat-stale",
            stuck_reason: "heartbeat stale (574min, threshold=10min, watchmen=8)",
            stale_minutes: Some(574),
            affected_watchers: vec![],
            severity: Severity::High,
            message: "Claude stuck: heartbeat stale (574min, threshold=10min, watchmen=8). 2 consecutive checks failed.",
        };

        let v = build_event_json(&alert);

        assert_eq!(v["tag"], "claude-watch-alert");
        assert_eq!(v["source"], "claude-watch");
        assert_eq!(v["source_name"], "claude-watch");
        assert_eq!(v["priority"], "high");
        assert_eq!(v["message"], alert.message);
        assert!(v["timestamp"].is_number());
        assert!(v["timestamp_iso"].is_string());
        assert!(v["pid"].is_number());

        let data = &v["data"];
        assert_eq!(data["alert_type"], "heartbeat-stale");
        assert_eq!(data["stuck_reason"], alert.stuck_reason);
        assert_eq!(data["stale_minutes"], 574);
        assert_eq!(data["severity"], "high");
        assert!(data["affected_watchers"].is_array());
        assert_eq!(data["affected_watchers"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn build_event_json_handles_watcher_down() {
        let alert = ClaudeWatchAlert {
            alert_type: "watcher-down",
            stuck_reason: "2 watcher(s) missing: alerts-watcher, torrent-wait",
            stale_minutes: None,
            affected_watchers: vec!["alerts-watcher".to_string(), "torrent-wait".to_string()],
            severity: Severity::Medium,
            message: "watchers down: alerts-watcher, torrent-wait",
        };

        let v = build_event_json(&alert);

        assert_eq!(v["data"]["alert_type"], "watcher-down");
        assert!(v["data"]["stale_minutes"].is_null());
        let watchers = v["data"]["affected_watchers"].as_array().unwrap();
        assert_eq!(watchers.len(), 2);
        assert_eq!(watchers[0], "alerts-watcher");
        assert_eq!(watchers[1], "torrent-wait");
        assert_eq!(v["priority"], "normal");
    }

    #[test]
    fn build_event_json_handles_minimal_alert() {
        // Push-notification-only paths (e.g. auto-update-complete) carry no
        // structured stale/watcher data — verify the optional fields
        // serialise cleanly as null/empty.
        let alert = ClaudeWatchAlert {
            alert_type: "auto-update-complete",
            stuck_reason: "claude-watch: auto-update complete (1.0.0 → 1.0.1)",
            stale_minutes: None,
            affected_watchers: vec![],
            severity: Severity::Low,
            message: "claude-watch: auto-update complete (1.0.0 → 1.0.1)",
        };

        let v = build_event_json(&alert);
        assert_eq!(v["data"]["alert_type"], "auto-update-complete");
        assert!(v["data"]["stale_minutes"].is_null());
        assert!(v["data"]["affected_watchers"]
            .as_array()
            .unwrap()
            .is_empty());
        assert_eq!(v["priority"], "low");
    }

    #[test]
    fn emit_writes_a_file_in_temp_queue_dir() {
        // Point CLAUDE_EVENT_QUEUE at a tempdir, emit, verify file lands
        // with valid JSON. Doesn't touch ~/claude-events/.
        let tmp = tempfile::tempdir().expect("tempdir");
        // Save & restore env; tests in this binary run in parallel
        // so be careful — we set + read + clean up in one shot.
        let prev = std::env::var("CLAUDE_EVENT_QUEUE").ok();
        // Safety: tests in this module are not racy w.r.t. each other
        // because each call to emit() reads queue_dir() up-front before
        // any sub-task spawns. Other tests in this module don't touch
        // the env var.
        // SAFETY: setting an env var in a test module — Rust 1.85+ flags
        // this unsafe but our test threads do not concurrently mutate
        // CLAUDE_EVENT_QUEUE.
        unsafe {
            std::env::set_var("CLAUDE_EVENT_QUEUE", tmp.path());
        }

        let alert = ClaudeWatchAlert {
            alert_type: "prolonged-thinking",
            stuck_reason: "prolonged thinking (>300s)",
            stale_minutes: None,
            affected_watchers: vec![],
            severity: Severity::Medium,
            message: "prolonged thinking interrupt #1 fired",
        };
        emit(&alert);

        // Restore env first so a panic below doesn't leak state.
        unsafe {
            match prev {
                Some(v) => std::env::set_var("CLAUDE_EVENT_QUEUE", v),
                None => std::env::remove_var("CLAUDE_EVENT_QUEUE"),
            }
        }

        // Verify exactly one file was created with the right tag and
        // that it parses as valid JSON with our expected fields.
        let entries: Vec<_> = std::fs::read_dir(tmp.path())
            .expect("read tempdir")
            .filter_map(Result::ok)
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .ends_with("_claude-watch-alert.json")
            })
            .collect();
        assert_eq!(entries.len(), 1, "expected exactly one event file");

        let content = std::fs::read_to_string(entries[0].path()).expect("read event");
        let parsed: serde_json::Value =
            serde_json::from_str(&content).expect("event is valid JSON");
        assert_eq!(parsed["tag"], "claude-watch-alert");
        assert_eq!(parsed["data"]["alert_type"], "prolonged-thinking");
    }

    #[test]
    fn build_workload_done_natural_exit() {
        let ev = WorkloadDoneEvent {
            label: "ebook-twilight",
            exit_code: 0,
            killed: false,
            log_path: "/tmp/claude-workloads/ebook-twilight.output",
            queue_id: None,
        };
        let v = build_workload_done_json(&ev);

        assert_eq!(v["tag"], "workload-done");
        assert_eq!(v["source"], "workload");
        assert_eq!(v["source_name"], "ebook-twilight");
        assert_eq!(v["priority"], "low"); // exit 0 → low
        assert!(v["message"]
            .as_str()
            .unwrap()
            .contains("workload ebook-twilight done rc=0"));
        let data = &v["data"];
        assert_eq!(data["label"], "ebook-twilight");
        assert_eq!(data["exit_code"], 0);
        assert_eq!(data["killed"], false);
        assert_eq!(
            data["log_path"],
            "/tmp/claude-workloads/ebook-twilight.output"
        );
        // Without --queue-id, no queue_id key appears in data.
        assert!(data.get("queue_id").is_none(),
            "queue_id must be absent when not bound");
    }

    #[test]
    fn build_workload_done_failure_exit() {
        let ev = WorkloadDoneEvent {
            label: "broken-task",
            exit_code: 2,
            killed: false,
            log_path: "/tmp/claude-workloads/broken-task.output",
            queue_id: None,
        };
        let v = build_workload_done_json(&ev);
        assert_eq!(v["priority"], "normal"); // non-zero exit → normal
        assert_eq!(v["data"]["exit_code"], 2);
        assert_eq!(v["data"]["killed"], false);
    }

    #[test]
    fn build_workload_done_killed() {
        let ev = WorkloadDoneEvent {
            label: "dead-task",
            exit_code: -15,
            killed: true,
            log_path: "/tmp/claude-workloads/dead-task.output",
            queue_id: None,
        };
        let v = build_workload_done_json(&ev);
        assert_eq!(v["priority"], "normal");
        assert_eq!(v["data"]["killed"], true);
        assert_eq!(v["data"]["exit_code"], -15);
        assert!(v["message"]
            .as_str()
            .unwrap()
            .contains("workload dead-task killed"));
    }

    #[test]
    fn build_workload_done_with_queue_id_includes_field() {
        // First-class workload model: when --queue-id was passed at
        // `workload run`, the event's data.queue_id mirrors it so the
        // main loop can correlate the workload exit with the queue
        // item without round-tripping state.json.
        let ev = WorkloadDoneEvent {
            label: "stv-promote-Akudama",
            exit_code: 0,
            killed: false,
            log_path: "/tmp/claude-workloads/stv-promote-Akudama.output",
            queue_id: Some("q-2026-05-03-test"),
        };
        let v = build_workload_done_json(&ev);
        assert_eq!(v["tag"], "workload-done");
        assert_eq!(v["data"]["queue_id"], "q-2026-05-03-test");
    }

    #[test]
    fn emit_workload_done_writes_file_with_correct_shape() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let prev = std::env::var("CLAUDE_EVENT_QUEUE").ok();
        // SAFETY: tests in this module don't concurrently mutate this var
        // beyond the single set/restore window per test.
        unsafe {
            std::env::set_var("CLAUDE_EVENT_QUEUE", tmp.path());
        }

        let ev = WorkloadDoneEvent {
            label: "translate-book",
            exit_code: 0,
            killed: false,
            log_path: "/tmp/claude-workloads/translate-book.output",
            queue_id: None,
        };
        emit_workload_done(&ev);

        unsafe {
            match prev {
                Some(v) => std::env::set_var("CLAUDE_EVENT_QUEUE", v),
                None => std::env::remove_var("CLAUDE_EVENT_QUEUE"),
            }
        }

        let entries: Vec<_> = std::fs::read_dir(tmp.path())
            .expect("read tempdir")
            .filter_map(Result::ok)
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .ends_with("_workload-done.json")
            })
            .collect();
        assert_eq!(
            entries.len(),
            1,
            "expected exactly one workload-done event file"
        );

        let content = std::fs::read_to_string(entries[0].path()).expect("read event");
        let parsed: serde_json::Value =
            serde_json::from_str(&content).expect("event is valid JSON");
        assert_eq!(parsed["tag"], "workload-done");
        assert_eq!(parsed["source"], "workload");
        assert_eq!(parsed["source_name"], "translate-book");
        assert_eq!(parsed["data"]["label"], "translate-book");
        assert_eq!(parsed["data"]["exit_code"], 0);
        assert_eq!(parsed["data"]["killed"], false);
    }

    #[test]
    fn build_cadence_json_has_required_fields() {
        let ev = CadenceEvent {
            tag: "heartbeat-tick",
            source: "claude-watch",
            message: "heartbeat tick",
            priority: "low",
            data: serde_json::json!({"interval_secs": 60}),
        };
        let v = build_cadence_json(&ev);
        assert_eq!(v["tag"], "heartbeat-tick");
        assert_eq!(v["source"], "claude-watch");
        assert_eq!(v["source_name"], "claude-watch");
        assert_eq!(v["priority"], "low");
        assert_eq!(v["message"], "heartbeat tick");
        assert_eq!(v["data"]["interval_secs"], 60);
        assert!(v["timestamp"].is_number());
        assert!(v["timestamp_iso"].is_string());
        assert!(v["pid"].is_number());
    }

    #[test]
    fn emit_cadence_writes_file_with_sanitized_name() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let prev = std::env::var("CLAUDE_EVENT_QUEUE").ok();
        // SAFETY: cadence tests in this module don't concurrently mutate
        // this var beyond the single set/restore window per test.
        unsafe {
            std::env::set_var("CLAUDE_EVENT_QUEUE", tmp.path());
        }

        let ev = CadenceEvent {
            tag: "memory-reminder",
            source: "claude-watch",
            message: "=== MEMORY REMINDER ===",
            priority: "high",
            data: serde_json::json!({"interval_secs": 900}),
        };
        emit_cadence(&ev);

        unsafe {
            match prev {
                Some(v) => std::env::set_var("CLAUDE_EVENT_QUEUE", v),
                None => std::env::remove_var("CLAUDE_EVENT_QUEUE"),
            }
        }

        let entries: Vec<_> = std::fs::read_dir(tmp.path())
            .expect("read tempdir")
            .filter_map(Result::ok)
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .ends_with("_memory-reminder.json")
            })
            .collect();
        assert_eq!(entries.len(), 1, "expected exactly one cadence event file");

        let content = std::fs::read_to_string(entries[0].path()).expect("read event");
        let parsed: serde_json::Value =
            serde_json::from_str(&content).expect("event is valid JSON");
        assert_eq!(parsed["tag"], "memory-reminder");
        assert_eq!(parsed["priority"], "high");
        assert_eq!(parsed["data"]["interval_secs"], 900);
    }

    /// Regression guard: heartbeat-tick must reach the event queue.
    ///
    /// An earlier change made the daemon's heartbeat-tick a no-op (logged
    /// only, never delivered), so the main loop stopped getting its 5-min
    /// reminder to touch the host heartbeat file → the heartbeat went stale
    /// and the daemon fired spurious "heartbeat stale" alerts. This test
    /// builds the heartbeat-tick cadence event exactly as `run_daemon` does
    /// (using the `cadence` constants) and asserts an event file lands in the
    /// queue with the right tag/source/priority.
    #[test]
    fn emit_cadence_heartbeat_tick_writes_event() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let prev = std::env::var("CLAUDE_EVENT_QUEUE").ok();
        // SAFETY: nextest runs each test in its own process; the single
        // set/restore window per test keeps this env mutation isolated.
        unsafe {
            std::env::set_var("CLAUDE_EVENT_QUEUE", tmp.path());
        }

        // Mirror the production call site in `main::run_daemon`, including a
        // NON-default configured heartbeat path so we prove the configured
        // value flows into the event body (not a hardcoded constant).
        let configured_path = "/custom/run/claude/heartbeat";
        let ev = CadenceEvent {
            tag: crate::cadence::HEARTBEAT_TICK_TAG,
            source: crate::cadence::CADENCE_SOURCE,
            message: "heartbeat tick",
            priority: "low",
            data: heartbeat_tick_data(configured_path, 300),
        };
        emit_cadence(&ev);

        unsafe {
            match prev {
                Some(v) => std::env::set_var("CLAUDE_EVENT_QUEUE", v),
                None => std::env::remove_var("CLAUDE_EVENT_QUEUE"),
            }
        }

        let entries: Vec<_> = std::fs::read_dir(tmp.path())
            .expect("read tempdir")
            .filter_map(Result::ok)
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .ends_with("_heartbeat-tick.json")
            })
            .collect();
        assert_eq!(
            entries.len(),
            1,
            "heartbeat-tick must produce exactly one event-queue file"
        );

        let content = std::fs::read_to_string(entries[0].path()).expect("read event");
        let parsed: serde_json::Value =
            serde_json::from_str(&content).expect("event is valid JSON");
        assert_eq!(parsed["tag"], "heartbeat-tick");
        assert_eq!(parsed["source"], "claude-watch");
        assert_eq!(parsed["priority"], "low");
        assert_eq!(parsed["data"]["interval_secs"], 300);
        // The configured heartbeat-file path must be carried in the body and
        // must reflect the configured (non-default) value.
        assert_eq!(parsed["data"]["path"], configured_path);
    }

    #[test]
    fn heartbeat_tick_data_carries_configured_path() {
        // Default-shaped path.
        let data = heartbeat_tick_data("/var/run/claude/heartbeat", 300);
        assert_eq!(data["path"], "/var/run/claude/heartbeat");
        assert_eq!(data["interval_secs"], 300);

        // A user-configured override must surface verbatim in the body — the
        // path is NOT hardcoded.
        let data = heartbeat_tick_data("/tmp/claude-heartbeat", 60);
        assert_eq!(data["path"], "/tmp/claude-heartbeat");
        assert_eq!(data["interval_secs"], 60);
    }
}
