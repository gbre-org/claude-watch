//! E2e test for the inject-targeting fix (focus-follows-inject bug).
//!
//! Bug: the daemon resolved the inject pane via `status::find_claude_pane()`,
//! which scans `tmux list-panes -a` and returns the FIRST pane whose
//! `pane_current_command == "claude"`. When the operator focuses a Claude Code
//! TUI agent-view (a running SUBAGENT) — which spawns a second `claude` process
//! in its own pane — that scan can resolve the SUBAGENT's pane, so the daemon's
//! MAIN-LOOP-SCOPED injects (watcher-down restart, heartbeat-stale nudge,
//! resume) land in the subagent's context, where nothing can act on them.
//!
//! Fix: `status::find_claude_pane_with_config()` prefers the explicitly
//! configured `[tmux] dashboard_pane` / `dashboard_session` (the FIXED
//! main-loop pane) over the unconstrained scan. `send-keys` to a specific pane
//! id is focus-independent, so the inject always reaches the main loop.
//!
//! This test stands up a real tmux session matching a configured
//! `dashboard_session` and asserts the resolver returns the configured pane —
//! the fixed main-loop target — rather than auto-detecting.

use claude_watch::config::TmuxConfig;
use claude_watch::status::find_claude_pane_with_config;
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

static TEST_COUNTER: AtomicU32 = AtomicU32::new(0);

/// RAII guard that kills the tmux session on drop.
struct TmuxSession {
    name: String,
}

impl Drop for TmuxSession {
    fn drop(&mut self) {
        let _ = Command::new("tmux")
            .args(["kill-session", "-t", &self.name])
            .output();
    }
}

fn unique_session_name(prefix: &str) -> String {
    let n = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
    format!("cw-target-{}-{}-{}", prefix, std::process::id(), n)
}

fn tmux(args: &[&str]) -> bool {
    Command::new("tmux")
        .args(args)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Resolve a positional pane spec to its immutable `#{pane_id}` (`%N`) via tmux.
fn pane_id_of(target: &str) -> Option<String> {
    let out = Command::new("tmux")
        .args(["display-message", "-t", target, "-p", "#{pane_id}"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let id = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if id.is_empty() {
        None
    } else {
        Some(id)
    }
}

/// The configured main-loop pane wins even when another tmux session also
/// exists, and resolution returns the pane's STABLE `#{pane_id}` (not the
/// positional `session:window.pane` spec, which can drift onto an agent pane
/// when tmux renumbers indices).
#[tokio::test]
async fn configured_pane_is_resolved_as_main_loop_target() {
    let main_session = unique_session_name("main");
    let agent_session = unique_session_name("agentview");

    // The main-loop session (what the config points at) and a SEPARATE session
    // standing in for an operator-focused TUI agent-view subagent pane.
    if !tmux(&["new-session", "-d", "-s", &main_session, "-x", "200", "-y", "50"]) {
        eprintln!("skipping: tmux not available");
        return;
    }
    let _main_guard = TmuxSession {
        name: main_session.clone(),
    };
    assert!(tmux(&[
        "new-session",
        "-d",
        "-s",
        &agent_session,
        "-x",
        "200",
        "-y",
        "50",
    ]));
    let _agent_guard = TmuxSession {
        name: agent_session.clone(),
    };

    let configured_pane = format!("{}:0.0", main_session);
    let expected_id =
        pane_id_of(&configured_pane).expect("configured pane must resolve to a pane_id");
    let cfg = TmuxConfig {
        dashboard_pane: configured_pane.clone(),
        dashboard_session: main_session.clone(),
        post_escape_settle_ms: 0,
        ..Default::default()
    };

    // With the main-loop session configured, resolution MUST return the
    // configured pane's STABLE pane_id — regardless of any other (agent-view)
    // session present, and never the positional spec.
    let resolved = find_claude_pane_with_config(&cfg).await;
    assert_eq!(
        resolved.as_deref(),
        Some(expected_id.as_str()),
        "configured dashboard_pane must resolve to the main-loop pane's stable \
         pane_id, not the positional spec or an auto-detected/active pane"
    );
}

/// Regression for the inject-into-the-wrong-pane bug: when the operator
/// SELECTS an agent pane in the TUI (the agent pane becomes tmux's active
/// pane), the main-loop inject target must STILL resolve to the configured
/// main-loop pane — never the active/selected pane. We split the configured
/// window so a second ("agent-view") pane exists and make IT the active pane,
/// then assert the resolver hands back the MAIN pane's stable id, not the
/// active agent pane's.
#[tokio::test]
async fn agent_pane_active_still_resolves_to_main_pane() {
    let session = unique_session_name("split");

    if !tmux(&["new-session", "-d", "-s", &session, "-x", "200", "-y", "50"]) {
        eprintln!("skipping: tmux not available");
        return;
    }
    let _guard = TmuxSession {
        name: session.clone(),
    };

    let main_pane = format!("{}:0.0", session);
    let main_id = pane_id_of(&main_pane).expect("main pane must resolve to a pane_id");

    // Split the window: a SECOND pane (the stand-in TUI agent-view pane) appears
    // in the same window and becomes the active pane.
    assert!(tmux(&["split-window", "-t", &main_pane]));
    // Make the agent pane unambiguously the active/selected one.
    assert!(tmux(&["select-pane", "-t", &format!("{}:0.1", session)]));

    let active_id = pane_id_of(&format!("{}:0.1", session))
        .expect("agent pane must resolve to a pane_id");
    assert_ne!(active_id, main_id, "test setup: panes must be distinct");

    let cfg = TmuxConfig {
        dashboard_pane: main_pane.clone(),
        dashboard_session: session.clone(),
        post_escape_settle_ms: 0,
        ..Default::default()
    };

    let resolved = find_claude_pane_with_config(&cfg).await;
    assert_eq!(
        resolved.as_deref(),
        Some(main_id.as_str()),
        "with an agent pane SELECTED/active, the inject target must stay pinned \
         to the configured main-loop pane's stable id, never the active pane"
    );
    assert_ne!(
        resolved.as_deref(),
        Some(active_id.as_str()),
        "inject must NEVER resolve to the active/selected agent pane"
    );
}

/// Sanity: a configured-but-nonexistent session falls through. The resolver
/// must NOT return the dead configured pane (find_dashboard_pane's has-session
/// check fails); it falls back to the auto-detect scan, which finds no real
/// `claude` pane in this test and returns None. The load-bearing assertion is
/// that it does NOT hand back the configured-but-gone pane.
#[tokio::test]
async fn configured_session_absent_does_not_return_dead_pane() {
    let absent = unique_session_name("absent");
    // Ensure it really doesn't exist.
    let _ = tmux(&["kill-session", "-t", &absent]);

    let cfg = TmuxConfig {
        dashboard_pane: format!("{}:0.0", absent),
        dashboard_session: absent.clone(),
        post_escape_settle_ms: 0,
        ..Default::default()
    };

    let resolved = find_claude_pane_with_config(&cfg).await;
    assert_ne!(
        resolved.as_deref(),
        Some(format!("{}:0.0", absent).as_str()),
        "a configured-but-absent session must not be returned as a live pane"
    );
}
