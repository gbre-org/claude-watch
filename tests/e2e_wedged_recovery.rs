//! E2e tests for wedged-pane recovery on the `cs.is_none()` path.
//!
//! Regression guard for the 2026-06-19 incident: when Claude Code hits the
//! context wall, it renders an error banner OVER the status bar. The status-bar
//! parse then misses and `get_claude_status()` returns `None` — making the
//! session look "not running" even though it is WEDGED. Before the fix,
//! `check_cycle` took the `cs.is_none()` early return and skipped the
//! wedged-detection block entirely, so the auto-clear-at-limit recovery never
//! fired (the daemon logged "claude-status returned None -- not running" every
//! cycle for 83 minutes). The fix runs wedged-recovery on a fallback pane
//! BEFORE concluding "not running".

mod common;

use common::{TestEnv, TestEnvOptions};

/// A `None` status read whose pane shows a context-limit banner must be
/// recovered as WEDGED (self-clear), NOT treated as "not running" / dead.
#[test]
fn wedged_context_limit_with_unparseable_status_runs_self_clear() {
    let env = TestEnv::new(
        "wedged-none-recovery",
        TestEnvOptions {
            check_interval: 1,
            // Fire after 2 consecutive wedged observations so the test runs fast.
            context_monitor_enabled: true,
            wedged_consecutive: 2,
            ..Default::default()
        },
    );

    // Status parse returns None (banner covers the status bar) ...
    env.set_status_unparseable();
    // ... but the pane clearly shows the context-limit wedge banner.
    env.set_pane_content("Context limit reached. /compact or /clear to continue");

    // Run enough cycles to clear the consecutive gate plus action.
    let run = env.run_daemon_cycles(5, 2000);

    // The wedged-recovery path must have fired self-clear.
    let self_clear_log = env.read_self_clear_log();
    assert!(
        !self_clear_log.trim().is_empty(),
        "wedged recovery should have invoked self-clear. self-clear log: {:?}, stderr: {}",
        self_clear_log,
        run.stderr
    );

    // It must NOT have been misclassified as a dead / not-running process.
    let legacy_log = env.read_legacy_log();
    assert!(
        !legacy_log.contains("Dead process detected"),
        "wedged pane must not be flagged as dead. Legacy log: {}",
        legacy_log
    );

    // The wedged_clear event should be in the JSONL log.
    let entries = env.read_log_entries();
    assert!(
        entries
            .iter()
            .any(|e| e["event"] == "wedged_clear"),
        "expected a wedged_clear JSONL event. Entries: {:?}",
        entries
    );
}

/// A `None` status read whose pane is a normal shell prompt (Claude genuinely
/// exited, no wedge banner) must still take the dead/not-running path — the fix
/// must not swallow real exits.
#[test]
fn non_wedged_none_status_still_treated_as_not_running() {
    let env = TestEnv::new(
        "notwedged-none",
        TestEnvOptions {
            check_interval: 1,
            context_monitor_enabled: true,
            wedged_consecutive: 2,
            ..Default::default()
        },
    );

    env.set_status_unparseable();
    // A plain shell prompt — no wedge banner.
    env.set_pane_content("user@testhost:~$ ");

    let run = env.run_daemon_cycles(4, 2000);

    // self-clear must NOT have fired (no wedge present).
    let self_clear_log = env.read_self_clear_log();
    assert!(
        self_clear_log.trim().is_empty(),
        "self-clear must not fire when no wedge banner is present. log: {:?}, stderr: {}",
        self_clear_log,
        run.stderr
    );

    // The daemon should still recognize this as not-running.
    let legacy_log = env.read_legacy_log();
    assert!(
        legacy_log.contains("claude-status returned None -- not running"),
        "non-wedged None status should log 'not running'. Legacy log: {}",
        legacy_log
    );
}
