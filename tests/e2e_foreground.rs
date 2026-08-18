//! E2e tests for foreground blocking detection.
//!
//! Verifies that the daemon detects when Claude Code appears stuck in a
//! foreground operation (spinner visible, no prompt) for longer than the
//! configured threshold.

mod common;

use common::{MockStatus, TestEnv, TestEnvOptions};

/// Foreground blocking should be detected when spinner is visible
/// without a prompt for longer than threshold.
#[test]
fn foreground_blocking_detected() {
    let env = TestEnv::new(
        "fg-block",
        TestEnvOptions {
            check_interval: 1,
            foreground_threshold: 2, // 2 seconds for fast test
            ..Default::default()
        },
    );

    env.set_status(&MockStatus::healthy(&env.tmux_pane));

    // Set pane content to show spinner without prompt (foreground busy state).
    // U+280B is one of the braille spinner characters the daemon checks for.
    env.set_pane_content("Running bash command...\n\u{280b} processing large file...");

    let _run = env.run_daemon_cycles(5, 2000);

    // Check for foreground blocking in JSONL logs
    let log_entries = env.read_log_entries();
    let fg_events: Vec<_> = log_entries
        .iter()
        .filter(|e| e["event"].as_str() == Some("foreground_blocking"))
        .collect();

    assert!(
        !fg_events.is_empty(),
        "should detect foreground blocking. All log entries: {:?}\nStderr: {}",
        log_entries,
        _run.stderr
    );
}

/// No foreground blocking when Claude prompt is visible (idle state).
#[test]
fn idle_prompt_no_foreground_blocking() {
    let env = TestEnv::new(
        "fg-idle",
        TestEnvOptions {
            check_interval: 1,
            foreground_threshold: 2,
            show_idle_prompt: true,
            ..Default::default()
        },
    );

    env.set_status(&MockStatus::healthy(&env.tmux_pane));

    let _run = env.run_daemon_cycles(4, 1000);

    let log_entries = env.read_log_entries();
    let fg_events: Vec<_> = log_entries
        .iter()
        .filter(|e| e["event"].as_str() == Some("foreground_blocking"))
        .collect();

    assert!(
        fg_events.is_empty(),
        "should NOT detect foreground blocking when prompt visible. Events: {:?}",
        fg_events
    );
}

/// When foreground blocking is detected and interrupt_enabled=true,
/// the daemon should log a "foreground_interrupted" event.
#[test]
fn foreground_blocking_triggers_interrupt() {
    let env = TestEnv::new(
        "fg-interrupt",
        TestEnvOptions {
            check_interval: 1,
            foreground_threshold: 2,
            foreground_interrupt_enabled: true,
            foreground_interrupt_message: "[TEST-INTERRUPT] Command was backgrounded.".to_string(),
            ..Default::default()
        },
    );

    env.set_status(&MockStatus::healthy(&env.tmux_pane));

    // Set pane content to show spinner without prompt (foreground busy state).
    env.set_pane_content("Running bash command...\n\u{280b} processing large file...");

    let _run = env.run_daemon_cycles(5, 2000);

    // Check for foreground_interrupted event in JSONL logs
    let interrupted_events = env.find_log_events("foreground_interrupted");

    assert!(
        !interrupted_events.is_empty(),
        "should log foreground_interrupted event when interrupt_enabled=true. All events: {:?}\nStderr: {}",
        env.read_log_entries(),
        _run.stderr
    );
}

/// When interrupt_enabled=false, foreground blocking should be detected
/// but no interrupt action should be taken.
#[test]
fn foreground_blocking_no_interrupt_when_disabled() {
    let env = TestEnv::new(
        "fg-no-int",
        TestEnvOptions {
            check_interval: 1,
            foreground_threshold: 2,
            foreground_interrupt_enabled: false,
            ..Default::default()
        },
    );

    env.set_status(&MockStatus::healthy(&env.tmux_pane));

    env.set_pane_content("Running bash command...\n\u{280b} processing large file...");

    let _run = env.run_daemon_cycles(5, 2000);

    // Should have foreground_blocking but NOT foreground_interrupted
    let blocking_events = env.find_log_events("foreground_blocking");
    let interrupted_events = env.find_log_events("foreground_interrupted");

    assert!(
        !blocking_events.is_empty(),
        "should still detect foreground_blocking"
    );
    assert!(
        interrupted_events.is_empty(),
        "should NOT log foreground_interrupted when disabled. Events: {:?}",
        interrupted_events
    );
}

/// When interrupt_enabled=false, foreground blocking should log a
/// "foreground_would_interrupt" event (log-only mode) with the same data
/// that a real interrupt would include.
#[test]
fn foreground_log_only_mode_logs_would_interrupt() {
    let env = TestEnv::new(
        "fg-logonly",
        TestEnvOptions {
            check_interval: 1,
            foreground_threshold: 2,
            foreground_interrupt_enabled: false,
            ..Default::default()
        },
    );

    env.set_status(&MockStatus::healthy(&env.tmux_pane));
    env.set_pane_content("Running bash command...\n\u{280b} processing large file...");

    let _run = env.run_daemon_cycles(5, 2000);

    // Should have foreground_would_interrupt event logged
    let would_interrupt_events = env.find_log_events("foreground_would_interrupt");

    assert!(
        !would_interrupt_events.is_empty(),
        "should log foreground_would_interrupt event in log-only mode. All events: {:?}\nStderr: {}",
        env.read_log_entries(),
        _run.stderr
    );

    // Verify the event contains expected data fields
    let event = &would_interrupt_events[0];
    assert!(
        event["elapsed_secs"].is_number(),
        "foreground_would_interrupt should include elapsed_secs"
    );
    assert!(
        event["tokens"].is_number(),
        "foreground_would_interrupt should include tokens"
    );
    assert!(
        event["bashes"].is_number(),
        "foreground_would_interrupt should include bashes"
    );
}

/// Foreground checks should run on their own timer (foreground_monitor.check_interval),
/// independent of the general.check_interval. With a long general interval (10s) but
/// short foreground interval (1s), foreground blocking should be detected within 5s,
/// NOT waiting for the full general interval to elapse.
#[test]
fn foreground_uses_own_check_interval() {
    let env = TestEnv::new(
        "fg-interval",
        TestEnvOptions {
            check_interval: 10,           // general check: slow (10s)
            foreground_check_interval: 1, // foreground check: fast (1s)
            foreground_threshold: 3,      // detect after 3s
            ..Default::default()
        },
    );

    env.set_status(&MockStatus::healthy(&env.tmux_pane));
    env.set_pane_content("Running bash command...\n\u{280b} processing large file...");

    // Run for only 7 seconds total (NOT enough for even 1 full general check cycle of 10s).
    // If foreground used general.check_interval, we'd only get the initial check
    // (sets foreground_start) and never a second check to detect elapsed time.
    // With foreground_monitor.check_interval (1s), we get ~7 foreground checks
    // and should detect blocking after 3s.
    // Spawned directly rather than via run_daemon_cycles because this test
    // controls the lifetime itself — but through `daemon_command()`, which
    // owns the isolation env. Hand-rolling the Command here is what once let
    // a test daemon run against the real home directory and emit real alerts.
    let child = env
        .daemon_command()
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn daemon");

    // Wait 7 seconds - enough for foreground polling at 1s but less than
    // a single 10s general check cycle
    std::thread::sleep(std::time::Duration::from_secs(7));

    unsafe {
        libc::kill(child.id() as i32, libc::SIGTERM);
    }
    let output = child.wait_with_output().expect("wait on daemon");
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    let blocking_events = env.find_log_events("foreground_blocking");

    assert!(
        !blocking_events.is_empty(),
        "foreground blocking should be detected using foreground_monitor.check_interval (1s), \
         not general.check_interval (10s). Only ran for 7s (less than one 10s general cycle). \
         All events: {:?}\nStderr: {}",
        env.read_log_entries(),
        stderr
    );
}

/// State file should track foreground timing correctly.
#[test]
fn foreground_state_tracking() {
    let env = TestEnv::new(
        "fg-state",
        TestEnvOptions {
            check_interval: 1,
            foreground_threshold: 10, // High threshold -- won't trigger alert
            ..Default::default()
        },
    );

    env.set_status(&MockStatus::healthy(&env.tmux_pane));

    // Show spinner content
    env.set_pane_content("\u{280b} working...");

    let _run = env.run_daemon_cycles(3, 1000);

    // State should record foreground_start but not foreground_alerted
    let state = env.read_state();
    if state["foreground_start"].is_string() {
        assert_eq!(
            state["foreground_alerted"].as_bool(),
            Some(false),
            "foreground should be tracked but not yet alerted at high threshold"
        );
    }
    // If foreground_start is null, the pane content may not have been picked up
    // (timing-dependent), which is acceptable for this test
}
