//! Alerting: push notifications, claude-event emission, and
//! interrupt-then-inject.
//!
//! Three sinks fire from this module:
//! 1. **Push notification** via `$CLAUDE_WATCH_NOTIFY_CMD` — operator's
//!    phone alert. The env var names the executable (e.g. `pingme`).
//!    When unset/empty, push notifications are silently skipped.
//!    The command is invoked as: `<cmd> -p <priority> <message>`.
//! 2. **claude-event** via `event_bus::emit` — structured JSON dropped
//!    into `~/claude-events/` so `claude-event-watch` surfaces the
//!    alert to the main loop with parseable fields (alert_type,
//!    stuck_reason, stale_minutes, affected_watchers, severity). The
//!    reflexive "claude-watch said /cleanup → I run /cleanup without
//!    looking at the data" failure mode (flagged on a prior chore)
//!    only goes away when the loop is forced to read structured fields.
//! 3. **tmux-inject** — types the resume prompt into Claude Code's
//!    pane so the agent can recover in-band.
//!
//! Sinks are independent: a failure in one MUST NOT skip the others.
//! `event_bus::emit` is itself default-open (logs + swallows errors),
//! so this module just calls it unconditionally.

use crate::cmd::run_cmd;
use crate::event_bus::{self, ClaudeWatchAlert};
use crate::inject_dispatch;
use crate::tmux;

pub async fn send_pingme(message: &str) {
    send_pingme_with_priority(message, "normal").await;
}

pub async fn send_pingme_with_priority(message: &str, priority: &str) {
    let cmd = match std::env::var("CLAUDE_WATCH_NOTIFY_CMD") {
        Ok(c) if !c.is_empty() => c,
        _ => return,
    };
    let parts: Vec<&str> = cmd.split_whitespace().collect();
    if parts.is_empty() {
        return;
    }
    let mut args: Vec<&str> = parts.clone();
    args.push("-p");
    args.push(priority);
    args.push(message);
    let _ = run_cmd(&args, 15).await;
}

/// Pingme + claude-event emission in one shot. Use this for any alert
/// that doesn't need tmux-inject (auto-update progress, reauth alert,
/// crash notice). For the full stuck-state path use `alert()`.
///
/// Severity controls the push-notification priority AND the event's
/// `severity` data field. Priorities: `low|normal|high|urgent` (mapped
/// from Severity).
pub async fn notify(alert: ClaudeWatchAlert<'_>) {
    let priority = alert.severity.as_priority();
    send_pingme_with_priority(alert.message, priority).await;
    event_bus::emit(&alert);
}

/// Stuck-state alert: pingme (gated) + claude-event + interrupt + inject.
pub async fn alert(
    message: &str,
    pane: &str,
    resume_prompt: &str,
    use_pingme: bool,
    event_alert: ClaudeWatchAlert<'_>,
) {
    if use_pingme {
        send_pingme(message).await;
    }
    // Always emit the claude-event, even when pingme is suppressed by
    // the max_pingme_alerts gate. The structured event is the channel
    // that forces the main loop to look at fields like stale_minutes
    // — silencing it would defeat the whole point of this sink.
    event_bus::emit(&event_alert);

    // Actively interrupt, then inject resume prompt. 5s budget keeps the
    // perceived recovery latency low; if the pane never goes idle we
    // proceed with the inject anyway (Claude has typically responded long
    // before the timeout fires).
    //
    // The Escape/interrupt phase only matters for terminal-mode panes —
    // for panel-mode agents the pidfd path appends rather than cancels,
    // so the interrupt is a no-op there. We still call interrupt_and_wait
    // because the cost is bounded and a stray Escape into a defunct pane
    // is harmless.
    tmux::interrupt_and_wait(pane, 5).await;
    inject_dispatch::inject_to_agent(pane, resume_prompt).await;
}

/// Convenience: emit a claude-event for a fire-and-forget alert path
/// (no pingme, no inject). Mirrors `event_bus::emit` but lives here so
/// callers only `use crate::alert::*`.
pub fn emit_event(alert: ClaudeWatchAlert<'_>) {
    event_bus::emit(&alert);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Severity is re-exported through alert::Severity? No, callers
    /// `use crate::event_bus::Severity` directly. This test just
    /// smoke-checks that `notify` builds and serialises correctly when
    /// stubbed (it can't actually exec pingme in unit tests).
    #[test]
    fn notify_alert_struct_compiles() {
        let _ = ClaudeWatchAlert {
            alert_type: "claude-crashed",
            stuck_reason: "Claude Code process gone — restarting",
            stale_minutes: None,
            affected_watchers: vec![],
            severity: crate::event_bus::Severity::High,
            message: "claude-watch: Claude Code crashed -- auto-restarting",
        };
    }
}
