//! E2e tests for heartbeat stale detection.
//!
//! Verifies that when the heartbeat file's mtime exceeds the configured
//! stale threshold, the daemon detects a stuck state.

mod common;

use common::{MockStatus, TestEnv, TestEnvOptions};

/// Stale heartbeat should be detected when file mtime exceeds threshold.
#[test]
fn stale_heartbeat_detected() {
    let env = TestEnv::new(
        "hb-stale",
        TestEnvOptions {
            check_interval: 1,
            heartbeat_stale_minutes: 1, // 60 seconds
            show_idle_prompt: true,
            ..Default::default()
        },
    );

    // Set status showing a live process but NO recent bash activity,
    // so the actively_turning proof-of-life check doesn't suppress
    // heartbeat-stale detection (the ack-driven redesign suppresses
    // when bashes > 0 within the active window).
    env.set_status(&MockStatus {
        pane: env.tmux_pane.clone(), tokens: 50000, bashes: 0,
        compact_remaining: None, version: Some("1.0.0".to_string()) });

    // Create heartbeat file aged 120 seconds (past the 60s threshold)
    env.age_heartbeat(120);

    let _run = env.run_daemon_cycles(4, 2000);

    // Check for stuck detection in logs
    let log_entries = env.read_log_entries();
    let stuck_checks: Vec<_> = log_entries
        .iter()
        .filter(|e| e["event"].as_str() == Some("check") && e["stuck"].as_bool() == Some(true))
        .collect();

    assert!(
        !stuck_checks.is_empty(),
        "should detect stale heartbeat as stuck. Entries: {:?}\nStderr: {}",
        log_entries,
        _run.stderr
    );

    // Verify the stuck reason mentions heartbeat
    let has_heartbeat_reason = stuck_checks.iter().any(|e| {
        e["stuck_reason"]
            .as_str()
            .map(|r| r.contains("heartbeat"))
            .unwrap_or(false)
    });
    assert!(
        has_heartbeat_reason,
        "stuck reason should mention heartbeat. Stuck checks: {:?}",
        stuck_checks
    );
}

/// Fresh heartbeat should NOT trigger stuck detection.
#[test]
fn fresh_heartbeat_not_stuck() {
    let env = TestEnv::new(
        "hb-fresh",
        TestEnvOptions {
            check_interval: 1,
            heartbeat_stale_minutes: 1,
            ..Default::default()
        },
    );

    env.set_status(&MockStatus::healthy(&env.tmux_pane));

    // Touch heartbeat to make it fresh
    env.touch_heartbeat();

    let _run = env.run_daemon_cycles(3, 1000);

    let log_entries = env.read_log_entries();
    let stuck_checks: Vec<_> = log_entries
        .iter()
        .filter(|e| {
            e["event"].as_str() == Some("check")
                && e["stuck"].as_bool() == Some(true)
                && e["stuck_reason"]
                    .as_str()
                    .map(|r| r.contains("heartbeat"))
                    .unwrap_or(false)
        })
        .collect();

    assert!(
        stuck_checks.is_empty(),
        "fresh heartbeat should NOT trigger stuck. Stuck: {:?}",
        stuck_checks
    );
}

/// Missing heartbeat file should NOT trigger stuck detection
/// (gives Claude time to start up).
#[test]
fn missing_heartbeat_not_stuck() {
    let env = TestEnv::new(
        "hb-missing",
        TestEnvOptions {
            check_interval: 1,
            heartbeat_stale_minutes: 1,
            ..Default::default()
        },
    );

    env.set_status(&MockStatus::healthy(&env.tmux_pane));

    // Don't create heartbeat file at all

    let _run = env.run_daemon_cycles(3, 1000);

    let log_entries = env.read_log_entries();
    let stuck_checks: Vec<_> = log_entries
        .iter()
        .filter(|e| {
            e["event"].as_str() == Some("check")
                && e["stuck"].as_bool() == Some(true)
                && e["stuck_reason"]
                    .as_str()
                    .map(|r| r.contains("heartbeat"))
                    .unwrap_or(false)
        })
        .collect();

    assert!(
        stuck_checks.is_empty(),
        "missing heartbeat should NOT trigger stuck. Stuck: {:?}",
        stuck_checks
    );
}
