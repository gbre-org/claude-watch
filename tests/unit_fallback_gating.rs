//! Unit tests for the daemon fallback-gating logic that sits between the
//! Claude Code hooks and the tmux-injecting code paths.
//!
//! The pure decision function under test is
//! `reminders::should_defer_to_hook(kind, grace_secs)` — tests here
//! exercise the end-to-end behaviour that the policy module relies on,
//! without having to spin up a full e2e tmux session.

use chrono::Utc;
use claude_watch::reminders::{
    all_fire_counts, marker_path, read_marker, record_fire, seconds_since_fire,
    should_defer_to_hook, write_marker, ReminderMarker, ReminderType,
};
use std::path::PathBuf;
use std::sync::Mutex;

// Reminder dir is process-global via env var, so these tests must run
// serially within this binary.
static TEST_LOCK: Mutex<()> = Mutex::new(());

struct ScopedDir(PathBuf);
impl ScopedDir {
    fn new(name: &str) -> Self {
        let p = PathBuf::from(format!(
            "/tmp/claude-watch-fallback-test-{}-{}",
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
fn daemon_fallback_skipped_when_hook_fired_recently() {
    let _g = TEST_LOCK.lock().unwrap();
    let _dir = ScopedDir::new("skip_when_recent");

    // Simulate the hook firing
    record_fire(ReminderType::ContextHigh, None);

    // 5-minute grace window -> daemon should defer
    assert!(should_defer_to_hook(ReminderType::ContextHigh, 300.0));

    // 0-second grace -> daemon should NOT defer
    assert!(!should_defer_to_hook(ReminderType::ContextHigh, 0.0));
}

#[test]
fn daemon_fallback_proceeds_when_hook_never_fired() {
    let _g = TEST_LOCK.lock().unwrap();
    let _dir = ScopedDir::new("no_fire");

    // No hook fire ever -> daemon must proceed with fallback
    assert!(!should_defer_to_hook(ReminderType::ContextHigh, 300.0));
    assert!(!should_defer_to_hook(ReminderType::VersionUpdate, 900.0));
}

#[test]
fn daemon_fallback_proceeds_when_hook_marker_is_stale() {
    let _g = TEST_LOCK.lock().unwrap();
    let _dir = ScopedDir::new("stale_marker");

    // Marker fired an hour ago
    let stale = ReminderMarker {
        last_fired: Some((Utc::now() - chrono::Duration::seconds(3600)).to_rfc3339()),
        fire_count: 5,
        last_context: None,
    };
    write_marker(ReminderType::ContextHigh, &stale).unwrap();

    // 5-min grace -> stale marker should NOT defer
    assert!(!should_defer_to_hook(ReminderType::ContextHigh, 300.0));

    // 2-hour grace -> would defer
    assert!(should_defer_to_hook(ReminderType::ContextHigh, 7200.0));
}

#[test]
fn fire_count_survives_across_simulated_daemon_restart() {
    let _g = TEST_LOCK.lock().unwrap();
    let dir_struct = ScopedDir::new("restart_count");

    record_fire(ReminderType::ContextHigh, None);
    record_fire(ReminderType::ContextHigh, None);

    // Simulate daemon restart: the marker file is on disk, so a new
    // read_marker call should observe the previous count.
    let m = read_marker(ReminderType::ContextHigh);
    assert_eq!(m.fire_count, 2);

    // Ensure marker path matches what the daemon will look at
    assert!(
        marker_path(ReminderType::ContextHigh).starts_with(&dir_struct.0),
        "marker should be inside our scoped dir"
    );
}

#[test]
fn per_type_markers_are_independent() {
    let _g = TEST_LOCK.lock().unwrap();
    let _dir = ScopedDir::new("per_type");

    record_fire(ReminderType::ContextHigh, None);

    assert!(should_defer_to_hook(ReminderType::ContextHigh, 300.0));
    // VersionUpdate has no fire — daemon should NOT defer on that side
    assert!(!should_defer_to_hook(ReminderType::VersionUpdate, 300.0));
    assert!(!should_defer_to_hook(ReminderType::PreCompact, 300.0));
}

#[test]
fn seconds_since_fire_returns_small_positive_value() {
    let _g = TEST_LOCK.lock().unwrap();
    let _dir = ScopedDir::new("seconds_sanity");

    record_fire(ReminderType::VersionUpdate, None);
    let s = seconds_since_fire(ReminderType::VersionUpdate).unwrap();
    assert!(
        s >= 0.0 && s < 10.0,
        "seconds since fire should be small: got {}",
        s
    );
}

#[test]
fn all_fire_counts_reflects_recorded_fires() {
    let _g = TEST_LOCK.lock().unwrap();
    let _dir = ScopedDir::new("all_counts_reflect");

    for _ in 0..4 {
        record_fire(ReminderType::ContextHigh, None);
    }
    record_fire(ReminderType::PreCompact, None);

    let counts: std::collections::HashMap<_, _> = all_fire_counts().into_iter().collect();
    assert_eq!(counts["context_high"], 4);
    assert_eq!(counts["pre_compact"], 1);
    assert_eq!(counts["version_update"], 0);
}
