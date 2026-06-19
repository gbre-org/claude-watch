//! E2e tests for `tmux::pane_exists`.
//!
//! Regression coverage for the container auto-recovery gap: the inject
//! dispatcher's "pane present?" signal used to be derived from
//! `get_pane_pid` -> `find_claude_pid_in_tree`, the SAME probe used to
//! classify the deployment mode. When the in-pane claude loop wedged, that
//! probe stalled/returned `None`, collapsing both `mode` (=> Unknown) and
//! `pane_present` (=> false) together, so the dispatcher emitted an
//! unactionable `inject-dispatch-unknown-mode` event instead of attempting
//! the `tmux send-keys` that recovers the pane.
//!
//! `pane_exists` decouples pane-presence from the process-tree probe: it asks
//! tmux a question independent of the pane's process state ("does a pane with
//! this id exist?"). These tests assert it reports a live pane present (even
//! though no claude binary runs under it) and reports absent panes / empty
//! specs as not present.

use claude_watch::tmux::pane_exists;
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

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
    format!("cw-pane-exists-{}-{}-{}", prefix, std::process::id(), n)
}

fn create_session(name: &str) {
    let status = Command::new("tmux")
        .args(["new-session", "-d", "-s", name, "-x", "120", "-y", "40"])
        .status()
        .expect("create tmux session");
    assert!(status.success(), "failed to create tmux session");
    std::thread::sleep(Duration::from_millis(300));
}

/// A live tmux pane is reported present even though NO claude binary runs
/// under it. This is the load-bearing decouple: the recovery decision no
/// longer depends on resolving the agent process tree (which wedges when the
/// loop wedges). A bare `bash` pane has no claude descendant, so the old
/// `get_pane_pid` + `find_claude_pid_in_tree` derivation could miss it.
#[tokio::test]
async fn pane_exists_true_for_live_pane_without_claude() {
    let session = unique_session_name("live");
    let _guard = TmuxSession { name: session.clone() };
    create_session(&session);

    let pane = format!("{}:0.0", session);
    assert!(
        pane_exists(&pane).await,
        "live tmux pane should be reported present"
    );
}

/// An empty pane spec is genuinely "no pane" -- preserves historical
/// no-pane behavior so the dispatcher still falls back to claude-event
/// escalation when there truly is no in-band channel.
#[tokio::test]
async fn pane_exists_false_for_empty_spec() {
    assert!(
        !pane_exists("").await,
        "empty pane spec should be reported absent"
    );
}

/// A pane spec pointing at a session that does not exist is reported absent.
#[tokio::test]
async fn pane_exists_false_for_missing_session() {
    let bogus = format!("cw-pane-exists-nonesuch-{}:0.0", std::process::id());
    assert!(
        !pane_exists(&bogus).await,
        "missing tmux session should be reported absent"
    );
}
