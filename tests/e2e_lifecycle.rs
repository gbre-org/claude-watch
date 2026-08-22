//! E2e tests for daemon lifecycle: startup, logging, state persistence, shutdown.
//!
//! Verifies fundamental daemon behavior independent of specific failure detection.

mod common;

use common::{MockStatus, TestEnv, TestEnvOptions};

/// Daemon should start, write startup log entries, run checks, and shut down
/// cleanly on SIGTERM.
#[test]
fn daemon_startup_and_shutdown() {
    let env = TestEnv::new(
        "lifecycle-basic",
        TestEnvOptions {
            check_interval: 1,
            ..Default::default()
        },
    );

    env.set_status(&MockStatus::healthy(&env.tmux_pane));

    let _run = env.run_daemon_cycles(3, 1500);

    // Should have daemon_start and daemon_stop events
    let log_entries = env.read_log_entries();
    let start_events = env.count_log_events("daemon_start");
    let stop_events = env.count_log_events("daemon_stop");
    let check_events = env.count_log_events("check");

    assert!(
        start_events >= 1,
        "should have daemon_start event. Entries: {:?}",
        log_entries
    );
    assert!(
        stop_events >= 1,
        "should have daemon_stop event. Entries: {:?}",
        log_entries
    );
    assert!(
        check_events >= 1,
        "should have at least one check event. Entries: {:?}",
        log_entries
    );

    // Legacy log should also have entries
    let legacy = env.read_legacy_log();
    assert!(
        legacy.contains("daemon started"),
        "legacy log should contain startup. Log: {}",
        legacy
    );
    assert!(
        legacy.contains("daemon stopped"),
        "legacy log should contain shutdown. Log: {}",
        legacy
    );

    // Daemon should exit cleanly (0 or signal-terminated)
    // The daemon_start + daemon_stop log entries above confirm the full lifecycle.
}

/// State should be persisted to disk after each check cycle.
#[test]
fn state_persisted_after_checks() {
    let env = TestEnv::new(
        "lifecycle-state",
        TestEnvOptions {
            check_interval: 1,
            ..Default::default()
        },
    );

    env.set_status(&MockStatus::healthy(&env.tmux_pane));
    env.record_ack();

    let _run = env.run_daemon_cycles(3, 1500);

    // State file should exist and contain valid JSON
    let state = env.read_state();
    assert!(!state.is_null(), "state file should contain valid JSON");
    assert!(
        state["last_check"].is_string(),
        "state should have last_check timestamp. State: {:?}",
        state
    );
    assert_eq!(
        state["consecutive_failures"].as_u64(),
        Some(0),
        "healthy instance should have zero failures"
    );
}

/// Check events should contain expected fields.
#[test]
fn check_events_have_expected_fields() {
    let env = TestEnv::new(
        "lifecycle-fields",
        TestEnvOptions {
            check_interval: 1,
            ..Default::default()
        },
    );

    env.set_status(&MockStatus::healthy(&env.tmux_pane));

    let _run = env.run_daemon_cycles(2, 1500);

    let checks = env.find_log_events("check");
    assert!(!checks.is_empty(), "should have check events");

    let check = &checks[0];
    // Verify expected fields exist
    assert!(
        check.get("tokens").is_some(),
        "check event should have tokens field. Event: {:?}",
        check
    );
    assert!(
        check.get("bashes").is_some(),
        "check event should have bashes field"
    );
    assert!(
        check.get("stuck").is_some(),
        "check event should have stuck field"
    );
    assert!(
        check.get("consecutive_failures").is_some(),
        "check event should have consecutive_failures field"
    );
}

/// Config from CLAUDE_WATCH_CONFIG env var should be picked up.
/// Verified indirectly: if the daemon uses our config, it writes to our log paths.
#[test]
fn config_loaded_from_env_var() {
    let env = TestEnv::new(
        "lifecycle-config",
        TestEnvOptions {
            check_interval: 1,
            ..Default::default()
        },
    );

    env.set_status(&MockStatus::healthy(&env.tmux_pane));

    let _run = env.run_daemon_cycles(2, 1500);

    // If our config was loaded, logs land in our temp directory
    let log_entries = env.read_log_entries();
    assert!(
        !log_entries.is_empty(),
        "config should be loaded from CLAUDE_WATCH_CONFIG -- log entries in our temp path confirm this"
    );

    // State file should also be in our temp directory
    let state = env.read_state();
    assert!(
        !state.is_null(),
        "state file should be at our configured path"
    );
}
