//! Persistent state: serialization, deserialization, load/save.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use tracing::error;

#[derive(Debug, Serialize, Deserialize, Default, Clone)]
pub struct State {
    pub last_check: Option<String>,
    pub consecutive_failures: u32,
    pub consecutive_dead_checks: u32,
    pub consecutive_fast_detections: u32,
    pub alert_count: u32,
    pub last_alert: Option<String>,
    pub last_fast_path_alert: Option<String>,
    pub last_restart: Option<String>,
    pub restart_count: u32,
    pub pending_resume_inject: bool,
    pub last_failure: Option<String>,
    pub last_failure_detail: Option<FailureDetail>,
    pub last_status: Option<StatusSnapshot>,
    // Foreground monitor
    pub foreground_start: Option<String>,
    pub foreground_alerted: bool,
    // Thinking duration monitor
    #[serde(default)]
    pub thinking_start: Option<String>,
    #[serde(default)]
    pub thinking_alerted: bool,
    /// Count of consecutive thinking interrupts (for exponential backoff)
    #[serde(default)]
    pub thinking_interrupt_count: u32,
    /// Status-bar token count recorded when the current thinking episode
    /// started (i.e. when `thinking_start` was set). Used by the
    /// token-progress guard: at fire time, a token delta below
    /// `foreground_monitor.min_tokens_delta` marks the episode as an idle
    /// open turn (background shells keep the turn open) and suppresses the
    /// interrupt. `None` when the token count was unavailable at episode
    /// start — the guard then fails open (fire allowed).
    #[serde(default)]
    pub thinking_episode_start_tokens: Option<u64>,
    /// Timestamp of the last interrupt fired (across all fire paths:
    /// prolonged-thinking, watcher-down, context-warning). Used as the
    /// global post-interrupt cooldown gate so any one interrupt suppresses
    /// re-fires from the other paths for a short window.
    #[serde(default)]
    pub last_interrupt_at: Option<String>,
    // Last known pane/status for foreground polling (not persisted meaningfully)
    #[serde(default)]
    pub last_known_pane: String,
    #[serde(default)]
    pub last_known_tokens: u64,
    #[serde(default)]
    pub last_known_bashes: u64,
    // Context monitoring
    #[serde(default)]
    pub context_clear_triggered: bool,
    #[serde(default)]
    pub last_context_clear: Option<String>,
    #[serde(default)]
    pub context_clear_child_pid: Option<u32>,
    /// Last observed token count (for detecting external clears)
    #[serde(default)]
    pub last_seen_tokens: Option<u64>,
    /// Number of consecutive check cycles where the pane has shown a "wedged"
    /// pattern (context limit reached / persistent rate limit). When this
    /// reaches `context_monitor.wedged_consecutive`, claude-watch runs
    /// `self-clear` itself rather than waiting for the agent to do it.
    #[serde(default)]
    pub wedged_consecutive: u32,
    /// Timestamp of the last wedged-triggered self-clear (cooldown gate).
    #[serde(default)]
    pub last_wedged_clear: Option<String>,
    /// Total wedged-triggered self-clears (for metrics).
    #[serde(default)]
    pub wedged_clear_count: u32,
    /// Number of consecutive check cycles where the pane has shown a MALFORMED
    /// tool-call signature (raw non-namespaced `<invoke>` / `<parameter>` tags
    /// rendered as assistant text). When this reaches
    /// `malformed_tool_call.consecutive`, claude-watch injects a short
    /// corrective nudge. Reset to 0 when the signature clears.
    #[serde(default)]
    pub malformed_tool_call_consecutive: u32,
    /// Timestamp (RFC3339) of the last malformed-tool-call corrective nudge
    /// (cooldown gate).
    #[serde(default)]
    pub last_malformed_nudge: Option<String>,
    /// Cumulative count of malformed-tool-call corrective nudges (for metrics).
    #[serde(default)]
    pub malformed_tool_call_nudge_count: u64,
    // Watcher health
    pub watcher_health: HashMap<String, WatcherState>,
    /// Per-watcher RFC3339 timestamp of when the watcher was FIRST observed
    /// `DOWN` by `watcher-status --unhealthy-only` (the health predicate the
    /// obligations gate consults). Used to implement the health grace window
    /// (`watcher_monitor.health_grace_secs`): a watcher DOWN for less than the
    /// grace is suppressed from the unhealthy report so the one-shot waiters'
    /// brief print-and-exit gap doesn't trip the gate. An entry is set when a
    /// watcher transitions ok/DUPLICATE -> DOWN and CLEARED when it returns to
    /// non-DOWN, so each fresh outage gets its own grace window. Persisted
    /// across `watcher-status` invocations (which are one-shot CLI calls) via
    /// the state file. NOT cleared on daemon load — a stale value is harmless
    /// (it only widens the elapsed-down measurement, which fails safe toward
    /// surfacing a genuinely-stuck watcher).
    #[serde(default)]
    pub watcher_down_since: HashMap<String, String>,
    #[serde(default)]
    pub last_watcher_inject: Option<String>,
    /// Count of watcher inject events (for metrics)
    #[serde(default)]
    pub watcher_inject_count: u32,
    /// Count of auto-update events (for metrics)
    #[serde(default)]
    pub auto_update_count: u32,
    /// Count of heartbeat stale alert events (for metrics)
    #[serde(default)]
    pub heartbeat_stale_count: u32,
    /// Cumulative count of prolonged-thinking interrupts (for metrics).
    /// Separate from `thinking_interrupt_count` which is a per-episode
    /// backoff index that resets when Claude exits the thinking state.
    #[serde(default)]
    pub prolonged_thinking_interrupts_total: u64,
    /// Cumulative count of foreground-blocking interrupts (for metrics).
    #[serde(default)]
    pub foreground_blocking_interrupts_total: u64,
    /// Cumulative count of context-warning interrupts (for metrics).
    /// The `fallback_clear_count` field shares the same fire site; this
    /// field is the canonical per-interrupt counter name.
    #[serde(default)]
    pub context_warning_interrupts_total: u64,
    /// Cumulative count of watcher-down interrupts (for metrics).
    /// The `watcher_inject_count` field shares the same fire site; this
    /// field is the canonical per-interrupt counter name.
    #[serde(default)]
    pub watcher_down_interrupts_total: u64,
    /// Cumulative count of wedged-pane self-clear interrupts (for metrics).
    #[serde(default)]
    pub wedged_clear_interrupts_total: u64,
    /// Cumulative count of auto-update interrupts (for metrics).
    /// The `auto_update_count` field shares the same fire site; this
    /// field is the canonical per-interrupt counter name.
    #[serde(default)]
    pub auto_update_interrupts_total: u64,
    /// Cumulative count of reauth `/login` injections (for metrics).
    #[serde(default)]
    pub reauth_inject_interrupts_total: u64,
    /// Cumulative count of post-restart resume injections (for metrics).
    #[serde(default)]
    pub post_restart_resume_inject_interrupts_total: u64,
    /// Cumulative count of fresh-external-session resume injections.
    #[serde(default)]
    pub fresh_session_inject_interrupts_total: u64,
    /// Cumulative count of fresh-/clear resume injections.
    #[serde(default)]
    pub fresh_clear_resume_inject_interrupts_total: u64,
    /// Cumulative count of restart-claude events (for metrics).
    /// The `restart_count` field shares the same fire site; this is the
    /// canonical per-interrupt counter name.
    #[serde(default)]
    pub restart_claude_interrupts_total: u64,
    /// Count of context-clear fallback injections (daemon injected `/clear`
    /// because the context_high hook fire was stale or absent).
    #[serde(default)]
    pub fallback_clear_count: u32,
    /// Count of version-update fallback injections (daemon ran `claude update`
    /// because the version_update hook fire was stale or absent).
    #[serde(default)]
    pub fallback_update_count: u32,
    /// Sum of reminder-to-action latency samples (seconds) for the context_high
    /// reminder. Used to emit a histogram-style rate via Prometheus counters.
    #[serde(default)]
    pub reminder_to_clear_latency_secs_sum: f64,
    /// Number of reminder-to-action latency samples collected for context_high.
    #[serde(default)]
    pub reminder_to_clear_latency_count: u64,
    /// Sum of reminder-to-action latency samples (seconds) for the version_update
    /// reminder.
    #[serde(default)]
    pub reminder_to_update_latency_secs_sum: f64,
    /// Number of reminder-to-action latency samples collected for version_update.
    #[serde(default)]
    pub reminder_to_update_latency_count: u64,
    // Auto-update tracking
    #[serde(default)]
    pub last_update_check: Option<String>,
    #[serde(default)]
    pub last_update_attempt: Option<String>,
    #[serde(default)]
    pub update_in_progress: bool,
    // Reauth detection
    #[serde(default)]
    pub reauth_detected: bool,
    #[serde(default)]
    pub last_reauth_alert: Option<String>,
    #[serde(default)]
    pub login_injected: bool,
    /// Tracks whether we've already injected "resume" for a fresh external session
    /// (tokens=0 with Claude idle prompt visible). Reset when tokens become non-zero.
    #[serde(default)]
    pub fresh_session_injected: bool,
    /// Tracks whether Claude was ever alive (tokens > 0) since the last fresh inject.
    /// Prevents the inject loop: inject → startup (tokens=0) → "dead" reset → re-inject.
    /// Only set to true when tokens > 0 while fresh_session_injected is true.
    #[serde(default)]
    pub was_alive_since_inject: bool,
    /// Timestamp of the last fresh session inject. Used as a fallback timeout: if Claude
    /// never becomes active within N minutes after inject, allow resetting the flag.
    #[serde(default)]
    pub last_fresh_inject: Option<String>,
    /// Timestamp of the last check where the main loop was observed actively
    /// running a tool call (`bashes > 0`). Used by the watcher-down inject
    /// suppression gate so we don't preempt an in-flight turn with a
    /// `WATCHER(S) DOWN` prompt. Updated on every check that sees
    /// `bashes > 0`. Not cleared on daemon restart — a stale value just
    /// suppresses one inject cycle, which is the safer side to err on.
    #[serde(default)]
    pub last_active_at: Option<String>,
    /// Number of consecutive cycles where ANY of the three suppression
    /// gates (watcher-down, fresh-/clear, dead-process) suppressed an
    /// inject because the main loop was actively turning. When this
    /// reaches `[suppression] max_consecutive_suppressions` OR the
    /// wall-clock since `first_suppression_at` exceeds
    /// `max_suppression_window_secs`, the next gate fire force-injects
    /// regardless of `actively_turning`.
    ///
    /// Reset to 0 when an actual inject lands at any of the three gates
    /// (a force-inject or a non-suppressed inject — either way the gate
    /// has demonstrably "made progress"). The wall-clock backstop is
    /// what catches the slow-drip case where progress is never made;
    /// trying to reset on per-gate "predicate stopped matching" would
    /// be incorrect for a counter shared across three independent gates.
    /// Transient — cleared on daemon restart so a long-stale daemon
    /// doesn't escalate immediately on the first suppression after
    /// coming back up.
    #[serde(default)]
    pub consecutive_suppressions: u32,
    /// Wall-clock timestamp of the first suppression in the current run.
    /// Set the first time `consecutive_suppressions` increments from 0
    /// to 1; cleared whenever `consecutive_suppressions` resets to 0.
    /// Used by the wall-clock backstop in the escalation predicate.
    /// Transient — cleared on daemon restart for the same reason as
    /// `consecutive_suppressions`.
    #[serde(default)]
    pub first_suppression_at: Option<String>,
    /// Number of consecutive check cycles where the pane has shown an
    /// upstream-API retry banner ("Retrying in Ns / attempt N/M" with a 5xx
    /// or "Overloaded" cue). Once this reaches `api_retry.consecutive`,
    /// claude-watch suppresses all inject sites until the retry resolves.
    /// Transient — reset on daemon load.
    #[serde(default)]
    pub api_retry_consecutive: u32,
    /// Timestamp of the first cycle in the current api_retrying episode.
    /// Used as the `max_stuck_secs` guard so a hung retry banner can't
    /// suppress monitoring forever. Cleared when the pane no longer shows
    /// a retry banner. Transient — reset on daemon load.
    #[serde(default)]
    pub api_retry_first_seen: Option<String>,
    /// Cumulative count of cycles where claude-watch suppressed an interrupt
    /// fire because api_retry was active. Persisted across daemon restarts
    /// so Prometheus metrics can graph the suppression rate.
    #[serde(default)]
    pub api_retry_suppressions_total: u64,

    // --- Auto-respawn-on-hang -------------------------------------------
    /// Sliding-window observation history of "Claude Code is hung" signals.
    /// Multiple independent signals must fire within
    /// `auto_respawn_on_hang.signal_window_secs` for the auto-respawn
    /// decision to fire. See `crate::respawn`.
    #[serde(default)]
    pub hang_signal_history: crate::respawn::HangSignalHistory,
    /// Timestamp of the last auto-respawn fire (for the cooldown gate).
    #[serde(default)]
    pub last_respawn_at: Option<String>,
    /// Cumulative count of auto-respawn fires (for metrics).
    #[serde(default)]
    pub auto_respawn_count: u32,
    /// Cumulative count of auto-respawn fires emitted as interrupts (for
    /// metrics — mirrors the `*_interrupts_total` naming convention).
    #[serde(default)]
    pub auto_respawn_interrupts_total: u64,
    /// Hash of the last pane capture (for the PaneCaptureUnchanged signal).
    /// Stored as a u64 of the FxHash digest. Resets to None when the pane
    /// content changes.
    #[serde(default)]
    pub pane_content_hash: Option<u64>,
    /// Timestamp of the first cycle the pane content hash matched the
    /// current value (`pane_content_hash`). When the pane changes this
    /// resets to None / now. The PaneCaptureUnchanged signal fires when
    /// (now - pane_content_unchanged_since) >= pane_unchanged_secs.
    #[serde(default)]
    pub pane_content_unchanged_since: Option<String>,

    // --- AskUserQuestion stale monitor (Phase 1: detect + alarm) -------
    /// RFC3339 timestamp of when an interactive `AskUserQuestion` prompt
    /// was first observed pending (main loop blocked, reads as Idle).
    /// `None` when no interactive prompt is currently up. Set on the first
    /// cycle the prompt is seen; cleared when the prompt clears. Mirrors
    /// the `thinking_start` timer lifecycle. Transient — cleared on daemon
    /// load (daemon downtime makes the elapsed measurement unreliable).
    #[serde(default)]
    pub ask_question_pending_since: Option<String>,
    /// Whether the stale-question alarm has already fired for the CURRENT
    /// pending question. Set true when the alarm fires so it fires exactly
    /// once per pending question; reset to false when the prompt clears.
    /// Transient — cleared on daemon load alongside the timer.
    #[serde(default)]
    pub ask_question_alerted: bool,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct FailureDetail {
    pub bashes: u64,
    pub watchmen: u32,
    pub stuck_reason: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct StatusSnapshot {
    pub bashes: u64,
    pub watchmen: u32,
}

#[derive(Debug, Serialize, Deserialize, Clone, Default)]
pub struct WatcherState {
    pub last_seen_running: Option<String>,
    pub consecutive_missing: u32,
    pub enabled: bool,
    /// RFC3339 timestamp of the last `watcher-down` claude-event emission for
    /// this watcher (the "quiet path", PR #48). When set, subsequent
    /// watcher-monitor cycles suppress re-emission within the configured
    /// grace window AND suppress the heavyweight tmux-inject path entirely
    /// until the grace window expires (at which point we fall through to
    /// inject as a fallback). Cleared on recovery (count >= min_count).
    #[serde(default)]
    pub event_emitted_at: Option<String>,
    // NOTE: `last_auto_restart_at` was removed 2026-05-01 along with the
    // daemon-side auto-restart path (cardinal rule: watchers must be
    // spawned by the main loop). Older state files containing the field
    // still deserialize cleanly — serde ignores unknown fields by default.
}

pub fn load_state(path: &str) -> State {
    let mut state: State = match std::fs::read_to_string(path) {
        Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
        Err(_) => State::default(),
    };
    // Transient timers are meaningless across daemon restarts — daemon
    // downtime makes the elapsed measurement unreliable and can trigger
    // spurious "prolonged thinking" interrupts within seconds of startup.
    // Clear them on load so tracking starts fresh.
    state.thinking_start = None;
    state.thinking_alerted = false;
    state.thinking_interrupt_count = 0;
    state.thinking_episode_start_tokens = None;
    // last_interrupt_at is a short-lived global cooldown gate — daemon
    // downtime makes any persisted value meaningless (either stale or
    // indefinitely-suppressive). Clear on load so the next interrupt is
    // allowed to fire immediately.
    state.last_interrupt_at = None;
    state.foreground_start = None;
    state.foreground_alerted = false;
    // wedged_consecutive is transient — daemon downtime breaks the
    // "consecutive" semantics. Reset on load. (last_wedged_clear and
    // wedged_clear_count persist for cooldown + metrics.)
    state.wedged_consecutive = 0;
    // Suppression-escalation counter and first-suppression timestamp are
    // transient for the same reason: a daemon that's been down for an
    // hour shouldn't escalate immediately on the first suppression
    // after coming back up. The escalation re-builds from scratch.
    state.consecutive_suppressions = 0;
    state.first_suppression_at = None;
    // api_retry tracking is transient — daemon downtime makes the
    // "current episode" timestamp meaningless and the consecutive count
    // unreliable. Reset on load. (api_retry_suppressions_total persists
    // for metrics.)
    state.api_retry_consecutive = 0;
    state.api_retry_first_seen = None;
    // AskUserQuestion stale-monitor timer is transient — daemon downtime
    // makes the elapsed measurement unreliable. Reset on load so tracking
    // starts fresh (mirrors thinking_start).
    state.ask_question_pending_since = None;
    state.ask_question_alerted = false;
    state
}

pub fn save_state(path: &str, state: &State) {
    if let Some(parent) = Path::new(path).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match serde_json::to_string_pretty(state) {
        Ok(json) => {
            if let Err(e) = std::fs::write(path, json) {
                error!(error = %e, "failed to save state");
            }
        }
        Err(e) => error!(error = %e, "failed to serialize state"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_state() {
        let state = State::default();
        assert_eq!(state.consecutive_failures, 0);
        assert_eq!(state.consecutive_dead_checks, 0);
        assert_eq!(state.alert_count, 0);
        assert_eq!(state.restart_count, 0);
        assert!(!state.pending_resume_inject);
        assert!(state.last_check.is_none());
        assert!(state.watcher_health.is_empty());
    }

    #[test]
    fn test_serialize_deserialize_roundtrip() {
        let mut state = State::default();
        state.consecutive_failures = 5;
        state.alert_count = 2;
        state.last_check = Some("2026-03-16T12:00:00-05:00".to_string());
        state.pending_resume_inject = true;
        state.last_failure_detail = Some(FailureDetail {
            bashes: 45,
            watchmen: 3,
            stuck_reason: "heartbeat stale".to_string(),
        });
        state.last_status = Some(StatusSnapshot {
            bashes: 45,
            watchmen: 3,
        });
        state.watcher_health.insert(
            "alerts-watcher".to_string(),
            WatcherState {
                last_seen_running: Some("2026-03-16T12:00:00-05:00".to_string()),
                consecutive_missing: 0,
                enabled: true,
                event_emitted_at: None,
            },
        );

        let json = serde_json::to_string_pretty(&state).expect("serialize");
        let restored: State = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(restored.consecutive_failures, 5);
        assert_eq!(restored.alert_count, 2);
        assert_eq!(restored.last_check, state.last_check);
        assert!(restored.pending_resume_inject);
        assert!(restored.last_failure_detail.is_some());
        assert!(restored.last_status.is_some());
        assert_eq!(restored.watcher_health.len(), 1);
        assert!(restored.watcher_health.contains_key("alerts-watcher"));
    }

    #[test]
    fn test_watcher_down_since_roundtrip_and_default() {
        // The health-grace DOWN-since map must round-trip through save/load
        // AND default to empty on a state file written before the field
        // existed (serde default).
        let path = "/tmp/claude-watch-test-down-since.json";
        let mut state = State::default();
        state
            .watcher_down_since
            .insert("signal-wait-dm".to_string(), "2026-05-30T17:00:00Z".to_string());
        save_state(path, &state);

        let loaded = load_state(path);
        assert_eq!(
            loaded.watcher_down_since.get("signal-wait-dm").map(String::as_str),
            Some("2026-05-30T17:00:00Z")
        );
        let _ = std::fs::remove_file(path);

        // Old state file (no field) -> empty map, not an error.
        let path2 = "/tmp/claude-watch-test-down-since-default.json";
        std::fs::write(path2, "{}").unwrap();
        let loaded2 = load_state(path2);
        assert!(loaded2.watcher_down_since.is_empty());
        let _ = std::fs::remove_file(path2);
    }

    #[test]
    fn test_load_state_missing_file() {
        let state = load_state("/tmp/nonexistent-claude-watch-test-state.json");
        assert_eq!(state.consecutive_failures, 0);
    }

    #[test]
    fn test_load_state_invalid_json() {
        let path = "/tmp/claude-watch-test-invalid-state.json";
        std::fs::write(path, "not json").unwrap();
        let state = load_state(path);
        assert_eq!(state.consecutive_failures, 0);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn test_save_and_load_roundtrip() {
        let path = "/tmp/claude-watch-test-state-roundtrip.json";
        let mut state = State::default();
        state.alert_count = 7;
        state.restart_count = 2;
        save_state(path, &state);

        let loaded = load_state(path);
        assert_eq!(loaded.alert_count, 7);
        assert_eq!(loaded.restart_count, 2);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn test_interrupt_counters_roundtrip() {
        let path = "/tmp/claude-watch-test-interrupt-counters.json";
        let mut state = State::default();
        state.prolonged_thinking_interrupts_total = 7;
        state.foreground_blocking_interrupts_total = 3;
        state.context_warning_interrupts_total = 11;
        state.watcher_down_interrupts_total = 42;
        state.wedged_clear_interrupts_total = 2;
        state.auto_update_interrupts_total = 19;
        state.reauth_inject_interrupts_total = 1;
        state.post_restart_resume_inject_interrupts_total = 4;
        state.fresh_session_inject_interrupts_total = 5;
        state.fresh_clear_resume_inject_interrupts_total = 6;
        state.restart_claude_interrupts_total = 8;
        save_state(path, &state);

        let loaded = load_state(path);
        assert_eq!(loaded.prolonged_thinking_interrupts_total, 7);
        assert_eq!(loaded.foreground_blocking_interrupts_total, 3);
        assert_eq!(loaded.context_warning_interrupts_total, 11);
        assert_eq!(loaded.watcher_down_interrupts_total, 42);
        assert_eq!(loaded.wedged_clear_interrupts_total, 2);
        assert_eq!(loaded.auto_update_interrupts_total, 19);
        assert_eq!(loaded.reauth_inject_interrupts_total, 1);
        assert_eq!(loaded.post_restart_resume_inject_interrupts_total, 4);
        assert_eq!(loaded.fresh_session_inject_interrupts_total, 5);
        assert_eq!(loaded.fresh_clear_resume_inject_interrupts_total, 6);
        assert_eq!(loaded.restart_claude_interrupts_total, 8);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn test_interrupt_counters_default_to_zero_on_missing_fields() {
        // State files written before these fields existed should still
        // deserialize — counters default to 0 (serde default).
        let path = "/tmp/claude-watch-test-interrupt-counters-default.json";
        std::fs::write(path, "{}").unwrap();
        let loaded = load_state(path);
        assert_eq!(loaded.prolonged_thinking_interrupts_total, 0);
        assert_eq!(loaded.foreground_blocking_interrupts_total, 0);
        assert_eq!(loaded.context_warning_interrupts_total, 0);
        assert_eq!(loaded.watcher_down_interrupts_total, 0);
        assert_eq!(loaded.wedged_clear_interrupts_total, 0);
        assert_eq!(loaded.auto_update_interrupts_total, 0);
        assert_eq!(loaded.reauth_inject_interrupts_total, 0);
        assert_eq!(loaded.post_restart_resume_inject_interrupts_total, 0);
        assert_eq!(loaded.fresh_session_inject_interrupts_total, 0);
        assert_eq!(loaded.fresh_clear_resume_inject_interrupts_total, 0);
        assert_eq!(loaded.restart_claude_interrupts_total, 0);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn test_interrupt_counters_preserved_across_load() {
        // load_state() explicitly resets some transient fields (thinking_start,
        // last_interrupt_at, etc.) but must NOT reset cumulative counters.
        let path = "/tmp/claude-watch-test-interrupt-counters-preserve.json";
        let mut state = State::default();
        state.prolonged_thinking_interrupts_total = 100;
        state.watcher_down_interrupts_total = 200;
        state.thinking_interrupt_count = 5; // transient (gets cleared on load)
        state.thinking_episode_start_tokens = Some(123_456); // transient
        state.last_interrupt_at = Some("2026-01-01T00:00:00+00:00".to_string()); // transient
        save_state(path, &state);

        let loaded = load_state(path);
        // Cumulative counters preserved
        assert_eq!(loaded.prolonged_thinking_interrupts_total, 100);
        assert_eq!(loaded.watcher_down_interrupts_total, 200);
        // Transient state cleared (guarded by existing behavior in load_state)
        assert_eq!(loaded.thinking_interrupt_count, 0);
        assert!(loaded.thinking_episode_start_tokens.is_none());
        assert!(loaded.last_interrupt_at.is_none());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn test_suppression_counters_cleared_on_load() {
        // consecutive_suppressions and first_suppression_at are
        // transient — daemon downtime breaks the "consecutive" semantics
        // (watcher conditions could have churned during downtime) and a
        // stale persisted timestamp would cause the wall-clock backstop
        // to escalate immediately on the first suppression after restart.
        // load_state() must clear both fields, alongside the other
        // transient timers (thinking_start, last_interrupt_at, etc.).
        let path = "/tmp/claude-watch-test-suppression-counters.json";
        let mut state = State::default();
        state.consecutive_suppressions = 5;
        state.first_suppression_at = Some("2026-04-28T00:00:00+00:00".to_string());
        save_state(path, &state);

        let loaded = load_state(path);
        assert_eq!(loaded.consecutive_suppressions, 0);
        assert!(loaded.first_suppression_at.is_none());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn test_api_retry_state_transient_reset_on_load() {
        // api_retry_consecutive and api_retry_first_seen are transient and
        // must reset on load. The cumulative counter (suppressions_total)
        // must persist.
        let path = "/tmp/claude-watch-test-api-retry-transient.json";
        let mut state = State::default();
        state.api_retry_consecutive = 5;
        state.api_retry_first_seen = Some("2026-04-28T18:00:00+00:00".to_string());
        state.api_retry_suppressions_total = 42;
        save_state(path, &state);

        let loaded = load_state(path);
        // Transient cleared
        assert_eq!(loaded.api_retry_consecutive, 0);
        assert!(loaded.api_retry_first_seen.is_none());
        // Cumulative preserved
        assert_eq!(loaded.api_retry_suppressions_total, 42);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn test_api_retry_suppressions_total_default_to_zero() {
        // Old state files (written before this field existed) deserialize
        // cleanly with the counter at 0.
        let path = "/tmp/claude-watch-test-api-retry-default.json";
        std::fs::write(path, "{}").unwrap();
        let loaded = load_state(path);
        assert_eq!(loaded.api_retry_suppressions_total, 0);
        assert_eq!(loaded.api_retry_consecutive, 0);
        assert!(loaded.api_retry_first_seen.is_none());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn test_hybrid_fallback_counters_roundtrip() {
        let path = "/tmp/claude-watch-test-hybrid-roundtrip.json";
        let mut state = State::default();
        state.fallback_clear_count = 11;
        state.fallback_update_count = 3;
        state.reminder_to_clear_latency_secs_sum = 123.45;
        state.reminder_to_clear_latency_count = 5;
        state.reminder_to_update_latency_secs_sum = 600.0;
        state.reminder_to_update_latency_count = 2;
        save_state(path, &state);

        let loaded = load_state(path);
        assert_eq!(loaded.fallback_clear_count, 11);
        assert_eq!(loaded.fallback_update_count, 3);
        assert!((loaded.reminder_to_clear_latency_secs_sum - 123.45).abs() < 1e-6);
        assert_eq!(loaded.reminder_to_clear_latency_count, 5);
        assert!((loaded.reminder_to_update_latency_secs_sum - 600.0).abs() < 1e-6);
        assert_eq!(loaded.reminder_to_update_latency_count, 2);
        let _ = std::fs::remove_file(path);
    }


    #[test]
    fn test_ask_question_fields_roundtrip_and_default() {
        // The AskUserQuestion stale-monitor timer field round-trips through
        // save, and old state files (without the field) default to None.
        // The timer is transient, so load_state() clears it — assert the
        // CLEAR-on-load behavior (mirrors thinking_start).
        let path = "/tmp/claude-watch-test-ask-question.json";
        let mut state = State::default();
        state.ask_question_pending_since = Some("2026-06-17T12:00:00-05:00".to_string());
        state.ask_question_alerted = true;
        save_state(path, &state);

        // load_state resets the transient timer.
        let loaded = load_state(path);
        assert!(loaded.ask_question_pending_since.is_none());
        assert!(!loaded.ask_question_alerted);
        let _ = std::fs::remove_file(path);

        // Old state file (no field) -> default None / false, not an error.
        let path2 = "/tmp/claude-watch-test-ask-question-default.json";
        std::fs::write(path2, "{}").unwrap();
        let loaded2 = load_state(path2);
        assert!(loaded2.ask_question_pending_since.is_none());
        assert!(!loaded2.ask_question_alerted);
        let _ = std::fs::remove_file(path2);
    }

}
