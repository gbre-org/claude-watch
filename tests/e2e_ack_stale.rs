//! E2e tests for ack-stale detection — the daemon's ONE liveness check.
//!
//! The main loop acks every event batch it handles (`event-ack ack-batch`),
//! which stamps `<ack state dir>/last-ack-timestamp`. When that stamp is older
//! than `[ack] stale_minutes` the daemon flags the loop as stuck. (Before
//! 2026-08-22 this keyed on a separate host heartbeat FILE the loop had to
//! `touch`; two signals for one fact, now collapsed to one.)

mod common;

use common::{MockStatus, TestEnv, TestEnvOptions};

/// A stale ack stamp should be detected as stuck.
#[test]
fn stale_ack_detected() {
    let env = TestEnv::new(
        "ack-stale",
        TestEnvOptions {
            check_interval: 1,
            ack_stale_minutes: 1, // 60 seconds
            show_idle_prompt: true,
            ..Default::default()
        },
    );

    // Set status showing a live process but NO recent bash activity, so the
    // actively_turning proof-of-life check doesn't suppress the detection
    // (it suppresses when bashes > 0 within the active window).
    env.set_status(&MockStatus {
        pane: env.tmux_pane.clone(),
        tokens: 50000,
        bashes: 0,
        compact_remaining: None,
        version: Some("1.0.0".to_string()),
    });

    // Last ack 120 seconds ago — past the 60s threshold.
    env.age_ack(120);

    let _run = env.run_daemon_cycles(4, 2000);

    let log_entries = env.read_log_entries();
    let stuck_checks: Vec<_> = log_entries
        .iter()
        .filter(|e| e["event"].as_str() == Some("check") && e["stuck"].as_bool() == Some(true))
        .collect();

    assert!(
        !stuck_checks.is_empty(),
        "should detect a stale ack as stuck. Entries: {:?}\nStderr: {}",
        log_entries,
        _run.stderr
    );

    // The reason must name the ack — an operator reading it has to know WHICH
    // signal went quiet and therefore what to do about it.
    let has_ack_reason = stuck_checks.iter().any(|e| {
        e["stuck_reason"]
            .as_str()
            .map(|r| r.contains("no event ack"))
            .unwrap_or(false)
    });
    assert!(
        has_ack_reason,
        "stuck reason should name the missing ack. Stuck checks: {:?}",
        stuck_checks
    );
}

/// A fresh ack should NOT trigger stuck detection.
#[test]
fn fresh_ack_not_stuck() {
    let env = TestEnv::new(
        "ack-fresh",
        TestEnvOptions {
            check_interval: 1,
            ack_stale_minutes: 1,
            ..Default::default()
        },
    );

    env.set_status(&MockStatus::healthy(&env.tmux_pane));

    // Ack now — this is what the main loop does per event batch.
    env.record_ack();

    let _run = env.run_daemon_cycles(3, 1000);

    let log_entries = env.read_log_entries();
    let stuck_checks: Vec<_> = log_entries
        .iter()
        .filter(|e| {
            e["event"].as_str() == Some("check")
                && e["stuck"].as_bool() == Some(true)
                && e["stuck_reason"]
                    .as_str()
                    .map(|r| r.contains("no event ack"))
                    .unwrap_or(false)
        })
        .collect();

    assert!(
        stuck_checks.is_empty(),
        "a fresh ack should NOT trigger stuck. Stuck: {:?}",
        stuck_checks
    );
}

/// No ack stamp at all should NOT trigger stuck detection (gives a
/// fresh session, or a host without event-must-act, time to start up).
/// Absence is not staleness: the clock starts at the FIRST ack.
#[test]
fn missing_ack_stamp_not_stuck() {
    let env = TestEnv::new(
        "ack-missing",
        TestEnvOptions {
            check_interval: 1,
            ack_stale_minutes: 1,
            ..Default::default()
        },
    );

    env.set_status(&MockStatus::healthy(&env.tmux_pane));

    // Deliberately never call record_ack().

    let _run = env.run_daemon_cycles(3, 1000);

    let log_entries = env.read_log_entries();
    let stuck_checks: Vec<_> = log_entries
        .iter()
        .filter(|e| {
            e["event"].as_str() == Some("check")
                && e["stuck"].as_bool() == Some(true)
                && e["stuck_reason"]
                    .as_str()
                    .map(|r| r.contains("no event ack"))
                    .unwrap_or(false)
        })
        .collect();

    assert!(
        stuck_checks.is_empty(),
        "a missing ack stamp should NOT trigger stuck. Stuck: {:?}",
        stuck_checks
    );
}
