//! Reminder tracking shared by the `hook-fire` subcommand and the daemon
//! fallback gating logic.
//!
//! The hybrid hooks + daemon-fallback model works like this:
//!
//! 1. A Claude Code hook (SessionStart / Stop / PreCompact) calls
//!    `claude-watch hook-fire <type>` on the relevant trigger.
//! 2. `hook-fire` writes a timestamped marker to
//!    `~/.cache/claude-watch/reminders/<type>.json` and emits the reminder
//!    text to stdout (the hook injects the stdout into the conversation).
//! 3. The daemon, before falling back to a heavy-handed injection (e.g.
//!    `/clear` via tmux, or `claude update`), checks whether a matching
//!    reminder was written recently. If so, it skips the injection — Claude
//!    was told and has a chance to self-act. If the reminder is stale
//!    (older than the fallback window), the daemon falls back.
//!
//! This keeps the daemon as the ultimate safety net while letting
//! hook-injected reminders be the primary, low-friction path.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Reminder types tracked by the hybrid system.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReminderType {
    /// Context is high — Claude should `/clear` soon. Fired by the `Stop`
    /// hook when token usage exceeds the threshold.
    ContextHigh,
    /// A newer Claude Code binary is installed than the one currently
    /// running. Fired by the `SessionStart` hook.
    VersionUpdate,
    /// Auto-compaction is about to run. Fired by the `PreCompact` hook
    /// with matcher `auto`.
    PreCompact,
}

impl ReminderType {
    /// Parse a CLI arg into a reminder type.
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "context_high" | "context-high" => Some(Self::ContextHigh),
            "version_update" | "version-update" => Some(Self::VersionUpdate),
            "pre_compact" | "pre-compact" | "precompact" => Some(Self::PreCompact),
            _ => None,
        }
    }

    /// Return the canonical lowercase-with-underscores label used for
    /// file names and Prometheus labels.
    pub fn as_label(self) -> &'static str {
        match self {
            Self::ContextHigh => "context_high",
            Self::VersionUpdate => "version_update",
            Self::PreCompact => "pre_compact",
        }
    }
}

/// Persisted marker file written by `hook-fire` and consumed by the daemon.
///
/// The marker records when the hook last fired for this reminder type,
/// plus a per-type counter that the metrics subcommand can export.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ReminderMarker {
    /// RFC3339 timestamp of the most recent hook fire.
    pub last_fired: Option<String>,
    /// Total times this reminder has fired (across daemon restarts — the
    /// file persists in ~/.cache/).
    #[serde(default)]
    pub fire_count: u64,
    /// Snapshot of status at fire time, for debugging.
    #[serde(default)]
    pub last_context: Option<serde_json::Value>,
}

/// Directory where reminder marker files live.
///
/// Honours `CLAUDE_WATCH_REMINDER_DIR` for test isolation. Otherwise
/// defaults to `$HOME/.cache/claude-watch/reminders/`.
pub fn reminder_dir() -> PathBuf {
    if let Ok(p) = std::env::var("CLAUDE_WATCH_REMINDER_DIR") {
        return PathBuf::from(p);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home).join(".cache/claude-watch/reminders")
}

/// Path to the marker file for a reminder type.
pub fn marker_path(kind: ReminderType) -> PathBuf {
    reminder_dir().join(format!("{}.json", kind.as_label()))
}

/// Read the current marker for a type, returning an empty default if the
/// file does not exist or cannot be parsed.
pub fn read_marker(kind: ReminderType) -> ReminderMarker {
    let path = marker_path(kind);
    match std::fs::read_to_string(&path) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
        Err(_) => ReminderMarker::default(),
    }
}

/// Write a marker file atomically (temp + rename). Creates parent dirs if
/// needed. Silently returns on I/O error — the hook must not block a
/// Claude Code session on a cache-dir issue.
pub fn write_marker(kind: ReminderType, marker: &ReminderMarker) -> std::io::Result<()> {
    let path = marker_path(kind);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let content = serde_json::to_string_pretty(marker)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let tmp_path = path.with_extension("json.tmp");
    std::fs::write(&tmp_path, content)?;
    std::fs::rename(&tmp_path, &path)?;
    Ok(())
}

/// Record a new hook fire: bump count, update timestamp, write atomically.
/// Returns the updated marker so callers can emit metrics / log it.
pub fn record_fire(kind: ReminderType, context: Option<serde_json::Value>) -> ReminderMarker {
    let mut marker = read_marker(kind);
    marker.last_fired = Some(Utc::now().to_rfc3339());
    marker.fire_count = marker.fire_count.saturating_add(1);
    marker.last_context = context;
    if let Err(e) = write_marker(kind, &marker) {
        // Best-effort — hooks must not fail a session.
        tracing::warn!(error = %e, kind = kind.as_label(), "failed to persist reminder marker");
    }
    marker
}

/// Return seconds since the last fire of this reminder, or None if it has
/// never fired (or the timestamp is unparseable).
pub fn seconds_since_fire(kind: ReminderType) -> Option<f64> {
    let marker = read_marker(kind);
    let last = marker.last_fired?;
    let dt = DateTime::parse_from_rfc3339(&last).ok()?;
    let elapsed = Utc::now() - dt.with_timezone(&Utc);
    Some(elapsed.num_milliseconds() as f64 / 1000.0)
}

/// Fallback gate: return true if the daemon should SKIP its injection
/// because a hook-fired reminder is still within the grace window.
///
/// `grace_secs` is how long after a hook fire we defer to Claude's own
/// self-action (e.g. `/clear`, `/restart`). If the reminder fired within
/// that window, the daemon holds off; otherwise the fallback proceeds.
pub fn should_defer_to_hook(kind: ReminderType, grace_secs: f64) -> bool {
    match seconds_since_fire(kind) {
        Some(s) if s >= 0.0 && s < grace_secs => true,
        _ => false,
    }
}

/// Load fire counts for ALL tracked reminder types. Used by the metrics
/// subcommand to export `claude_watch_reminder_fires_total{type=...}`.
pub fn all_fire_counts() -> Vec<(&'static str, u64)> {
    let types = [
        ReminderType::ContextHigh,
        ReminderType::VersionUpdate,
        ReminderType::PreCompact,
    ];
    types
        .iter()
        .map(|t| (t.as_label(), read_marker(*t).fire_count))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // Tests that touch the reminder dir need serial access because the
    // dir is process-global via the env var.
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    struct ScopedDir(PathBuf);
    impl ScopedDir {
        fn new(name: &str) -> Self {
            let p = PathBuf::from(format!(
                "/tmp/claude-watch-reminder-test-{}-{}",
                name,
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&p);
            std::fs::create_dir_all(&p).unwrap();
            std::env::set_var("CLAUDE_WATCH_REMINDER_DIR", &p);
            Self(p)
        }
    }
    impl Drop for ScopedDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
            std::env::remove_var("CLAUDE_WATCH_REMINDER_DIR");
        }
    }

    #[test]
    fn from_str_canonical_forms() {
        assert_eq!(
            ReminderType::from_str("context_high"),
            Some(ReminderType::ContextHigh)
        );
        assert_eq!(
            ReminderType::from_str("context-high"),
            Some(ReminderType::ContextHigh)
        );
        assert_eq!(
            ReminderType::from_str("version_update"),
            Some(ReminderType::VersionUpdate)
        );
        assert_eq!(
            ReminderType::from_str("pre-compact"),
            Some(ReminderType::PreCompact)
        );
        assert_eq!(ReminderType::from_str("bogus"), None);
    }

    #[test]
    fn record_fire_increments_counter() {
        let _guard = TEST_LOCK.lock().unwrap();
        let _dir = ScopedDir::new("record_fire");

        let m1 = record_fire(ReminderType::ContextHigh, None);
        assert_eq!(m1.fire_count, 1);
        assert!(m1.last_fired.is_some());

        let m2 = record_fire(ReminderType::ContextHigh, None);
        assert_eq!(m2.fire_count, 2);

        // Different type keeps its own counter
        let m3 = record_fire(ReminderType::VersionUpdate, None);
        assert_eq!(m3.fire_count, 1);
    }

    #[test]
    fn read_marker_missing_file_is_default() {
        let _guard = TEST_LOCK.lock().unwrap();
        let _dir = ScopedDir::new("read_missing");

        let m = read_marker(ReminderType::ContextHigh);
        assert_eq!(m.fire_count, 0);
        assert!(m.last_fired.is_none());
    }

    #[test]
    fn seconds_since_fire_after_record() {
        let _guard = TEST_LOCK.lock().unwrap();
        let _dir = ScopedDir::new("seconds_since");

        let _ = record_fire(ReminderType::ContextHigh, None);
        let s = seconds_since_fire(ReminderType::ContextHigh).unwrap();
        assert!(s >= 0.0 && s < 5.0, "elapsed should be small: {}", s);
    }

    #[test]
    fn should_defer_to_hook_respects_grace_window() {
        let _guard = TEST_LOCK.lock().unwrap();
        let _dir = ScopedDir::new("defer_grace");

        // No fire yet -> don't defer (daemon should inject)
        assert!(!should_defer_to_hook(ReminderType::ContextHigh, 300.0));

        // Fresh fire -> defer
        let _ = record_fire(ReminderType::ContextHigh, None);
        assert!(should_defer_to_hook(ReminderType::ContextHigh, 300.0));

        // Zero grace -> never defer
        assert!(!should_defer_to_hook(ReminderType::ContextHigh, 0.0));
    }

    #[test]
    fn should_defer_false_for_stale_marker() {
        let _guard = TEST_LOCK.lock().unwrap();
        let _dir = ScopedDir::new("stale_marker");

        // Write a marker with a timestamp 1 hour ago
        let stale = ReminderMarker {
            last_fired: Some((Utc::now() - chrono::Duration::seconds(3600)).to_rfc3339()),
            fire_count: 1,
            last_context: None,
        };
        write_marker(ReminderType::ContextHigh, &stale).unwrap();

        // 5-minute grace -> stale marker should not defer
        assert!(!should_defer_to_hook(ReminderType::ContextHigh, 300.0));
        // But a 2-hour grace would still defer
        assert!(should_defer_to_hook(ReminderType::ContextHigh, 7200.0));
    }

    #[test]
    fn all_fire_counts_covers_all_types() {
        let _guard = TEST_LOCK.lock().unwrap();
        let _dir = ScopedDir::new("all_counts");

        let _ = record_fire(ReminderType::ContextHigh, None);
        let _ = record_fire(ReminderType::ContextHigh, None);
        let _ = record_fire(ReminderType::VersionUpdate, None);

        let counts = all_fire_counts();
        assert_eq!(counts.len(), 3);
        let as_map: std::collections::HashMap<_, _> = counts.into_iter().collect();
        assert_eq!(as_map["context_high"], 2);
        assert_eq!(as_map["version_update"], 1);
        assert_eq!(as_map["pre_compact"], 0);
    }

    #[test]
    fn marker_roundtrip_preserves_context() {
        let _guard = TEST_LOCK.lock().unwrap();
        let _dir = ScopedDir::new("roundtrip_context");

        let ctx = serde_json::json!({"tokens": 850000, "pct": 85});
        let _ = record_fire(ReminderType::ContextHigh, Some(ctx.clone()));
        let m = read_marker(ReminderType::ContextHigh);
        assert_eq!(m.last_context.as_ref().unwrap()["tokens"], 850000);
    }
}
