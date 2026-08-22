//! Persistent state: serialization, deserialization, load/save.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use tracing::{error, warn};

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
    /// RFC3339 timestamp of when the prolonged-thinking OBLIGATION was armed
    /// (pending-alert written, event emitted) WITHOUT yet interrupting. The
    /// two-phase escalation gate (BUG 1 fix) requires the obligation to be
    /// armed and the dwell window to elapse before escalating to a tmux
    /// interrupt. Cleared when the condition clears or after the interrupt
    /// fires. Transient — daemon downtime breaks the dwell semantics (same
    /// rationale as `last_interrupt_at`).
    #[serde(default)]
    pub thinking_obligation_armed_at: Option<String>,
    /// RFC3339 timestamp of when the context-low OBLIGATION was armed. See
    /// `thinking_obligation_armed_at`. Transient.
    #[serde(default)]
    pub context_obligation_armed_at: Option<String>,
    /// RFC3339 timestamp of when the heartbeat-stale OBLIGATION was armed.
    /// See `thinking_obligation_armed_at`. Transient.
    #[serde(default)]
    pub heartbeat_obligation_armed_at: Option<String>,
    /// RFC3339 timestamp of when the watcher-down OBLIGATION was armed.
    /// The watcher-down inject was the 4th interrupt fire site that #424's
    /// obligation-precedence gate did NOT cover (its commit message scoped
    /// the gate to prolonged-thinking / context-low / heartbeat-stale only,
    /// leaving "watcher-down ... unchanged"). It now arms an obligation
    /// (pending alert + event) on first detection and only escalates to the
    /// turn-cancelling tmux interrupt after the dwell elapses with no live
    /// subagents — same two-phase shape as the other three.
    /// See `thinking_obligation_armed_at`. Transient.
    #[serde(default)]
    pub watcher_down_obligation_armed_at: Option<String>,
    /// Count of consecutive global interrupts within the rolling cooldown
    /// window — drives the exponential global-cooldown backoff. Incremented
    /// (saturating) on each successful `try_claim_global_interrupt`; reset to
    /// 0 when a full effective cooldown window elapses with no interrupt.
    /// Transient — daemon downtime breaks the streak semantics (same
    /// rationale as `last_interrupt_at`).
    #[serde(default)]
    pub global_interrupt_streak: u32,
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
    /// Epoch (float secs) at which THIS daemon process started. Set once at
    /// daemon startup (`run_daemon`); persisted so the short-lived
    /// `claude-watch metrics` scraper can use it as a platform-independent
    /// fallback anchor for the "time since last clear" panel when no explicit
    /// /clear has been observed yet (e.g. right after a deploy/recreate wipes
    /// the observed-clear state). NOT reset on load -- `run_daemon` overwrites
    /// it each start.
    #[serde(default)]
    pub daemon_start_epoch: Option<f64>,
    #[serde(default)]
    pub context_clear_child_pid: Option<u32>,
    /// Last observed token count (for detecting external clears)
    #[serde(default)]
    pub last_seen_tokens: Option<u64>,
    /// RFC3339 timestamp of the FIRST check cycle on which the context
    /// threshold was seen crossed in the current episode. Anchors the hard
    /// ceiling on hook-deferral (`hybrid.context_fallback_max_secs`): the
    /// per-fire grace window is measured against the last hook fire, which
    /// the hook itself refreshes on every turn, so it can never expire on a
    /// loop that keeps working. Cleared whenever the context-low condition
    /// clears. Transient.
    #[serde(default)]
    pub context_threshold_first_seen_at: Option<String>,
    /// `last_context_clear` value we have ALREADY injected a post-clear
    /// resume for. Latches the post-clear resume gate to one inject per
    /// observed clear (a `/clear` sits idle for many check cycles, and the
    /// gate deliberately does not consult the background-shell count, so it
    /// would otherwise re-fire every cycle). Transient.
    #[serde(default)]
    pub post_clear_resume_injected_for: Option<String>,
    /// Consecutive check cycles the pane has been observed idle inside the
    /// post-clear window. Debounces the post-clear resume gate the same way
    /// `consecutive_fast_detections` debounces the fresh-/clear gate.
    #[serde(default)]
    pub post_clear_idle_checks: u32,
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
    /// True while a wedged-triggered `self-clear` has been fired but the wedge
    /// has NOT yet been observed to clear on a subsequent cycle. Set when
    /// `spawn_immediate_clear` succeeds; cleared when `detect_wedged` later
    /// returns `None` (recovery confirmed). While set, a further wedge fire is
    /// treated as a RETRY (the prior clear did not stick) and escalated with a
    /// louder alert — the daemon no longer treats a fire-and-forget spawn as
    /// instant success (native-installer regression, PR #473 / c7ee999, made
    /// `self-clear` silently no-op on comm-name pane misses).
    #[serde(default)]
    pub wedged_clear_unverified: bool,
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
    /// Number of corrective nudges (phase-1 soft) fired so far in the CURRENT
    /// unbroken malform episode. Drives escalation to the phase-2 hard block:
    /// once this reaches `malformed_tool_call.escalate_after`, claude-watch
    /// switches to the relentless per-cycle hard-block injection. Reset to 0
    /// when a clean (non-malformed) cycle is observed.
    #[serde(default)]
    pub malformed_tool_call_episode_nudges: u32,
    /// Cumulative count of phase-2 (hard-block) malformed re-injections (metric).
    #[serde(default)]
    pub malformed_tool_call_hard_block_count: u64,
    /// Fingerprint of the malformed block that the LAST corrective inject fired
    /// on (see `tmux::malformed_tool_call_fingerprint`). Used to suppress
    /// re-injecting on the SAME malformed block when it merely lingers in pane
    /// scrollback after the model has already recovered with a well-formed call
    /// below it — the tight self-perpetuating interruption loop documented in
    /// the 2026-06-20 incident (the interrupter false-positiving on stale
    /// scrollback). A genuinely NEW malform produces a DIFFERENT fingerprint and
    /// is acted on immediately. Cleared when a clean (non-malformed) cycle is
    /// observed so a recurrence of the identical text after recovery still
    /// fires.
    #[serde(default)]
    pub last_malformed_fingerprint: Option<String>,
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
    /// Latched while Claude Code's in-TUI "Please run /login · API Error: 401
    /// OAuth access token has expired" banner is standing on the pane. Used
    /// only to log the first sighting and the resolution once each; the
    /// decision to act is re-made against the credential store every cycle.
    #[serde(default)]
    pub reauth_banner_detected: bool,

    // Proactive login-expiry tracking (the forward-looking half of reauth).
    /// Latched while Claude Code's "your login expires in N days" warning is
    /// standing. Cleared when the credentials are renewed, which is also what
    /// resets the auto-fire attempt budget.
    #[serde(default)]
    pub login_expiry_detected: bool,
    /// Days-left reported by the most recent detection, for the log/alert.
    #[serde(default)]
    pub login_expiry_days_left: Option<u32>,
    /// Last time the expiry warning was alerted on (rate limiting).
    #[serde(default)]
    pub last_login_expiry_alert: Option<String>,
    /// Last time `self-login` was auto-fired (retry spacing).
    #[serde(default)]
    pub last_self_login_attempt: Option<String>,
    /// Auto-fire attempts spent in the CURRENT expiry window. Reset when the
    /// warning clears — never on a timer, or the budget is not a budget.
    #[serde(default)]
    pub self_login_attempts_this_window: u32,
    /// When auto-fire last put a login dialog on the pane. Set BEFORE the
    /// flow runs, not after it succeeds: a `self-login start` that fails
    /// partway can still have left a modal up, and that modal has to be
    /// cleaned up by the same watchdog. Cleared once the watchdog has handed
    /// the pane back, or when the credentials are renewed.
    #[serde(default)]
    pub self_login_dialog_opened_at: Option<String>,
    /// Cumulative count of auto-fired `self-login` runs (for metrics).
    #[serde(default)]
    pub self_login_autofire_total: u64,
    /// Last observed `refreshTokenExpiresAt`. Watched for MOVEMENT, not
    /// position: a value that jumps forward is the credentials being renewed,
    /// which is the one unambiguous "this is resolved" signal available. It
    /// works even for a short-lived rolling token that never leaves the
    /// warning window and therefore never "resolves" by position alone.
    #[serde(default)]
    pub last_seen_refresh_expiry_ms: Option<i64>,
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
    /// RFC3339 timestamp when this watcher began its CURRENT continuous
    /// past-grace down run: stamped on the 0 -> 1 `consecutive_missing`
    /// transition and cleared the moment the watcher is seen running again.
    ///
    /// Distinct from the SHARED cross-gate suppression clock
    /// (`State.first_suppression_at`): this measures how long THIS specific
    /// watcher has itself been continuously down, independent of the shared
    /// suppression counter (which is polluted by the other gates and is tuned
    /// very high — `[suppression].max_suppression_window_secs` = 86400 — to
    /// tolerate the chronically-flapping surface-and-exit event consumer).
    /// The per-watcher watcher-down force-inject cap
    /// (`[watcher_monitor].max_suppress_secs`) reads this so an honest
    /// down comms watcher (e.g. `botchat-wait`) can never be silently
    /// suppressed for longer than the cap while the main loop is busy.
    #[serde(default)]
    pub down_since: Option<String>,
    // NOTE: `last_auto_restart_at` was removed 2026-05-01 along with the
    // daemon-side auto-restart path (cardinal rule: watchers must be
    // spawned by the main loop). Older state files containing the field
    // still deserialize cleanly — serde ignores unknown fields by default.
}

// ---------------------------------------------------------------------------
// Watcher-health reconciliation
//
// `State.watcher_health` is a persisted map keyed by watcher name, and it is
// only ever GROWN: the watcher monitor inserts an entry the first time it sees
// a configured+enabled watcher, and nothing ever removes one. That made the map
// an append-only record of every watcher the daemon has EVER monitored, so a
// watcher later RETIRED (deleted from the watchers config) or TURNED OFF (its
// config line flipped to disabled) kept its last-known entry forever —
// including `enabled: true` and a `consecutive_missing` counter that climbs
// without bound, because the monitor's per-cycle loop skips absent/disabled
// watchers entirely and therefore never touches those entries again.
//
// The stale belief is load-bearing downstream:
//   * `claude_watchers_missing` counts entries with `enabled: true` AND
//     `consecutive_missing > 3`, so a retired watcher pins the gauge above
//     zero permanently and any alert built on it fires forever;
//   * `collect_non_pane_signals` reads the same two fields to decide whether a
//     watcher outage counts as a hang signal, so retired entries can feed a
//     permanently-true input into respawn decisions.
//
// The fix is RECONCILIATION rather than a one-off prune of today's offenders:
// the map is brought back into agreement with the CURRENT config every time the
// config is read, so the divergence cannot reappear the next time a watcher is
// retired or toggled.
// ---------------------------------------------------------------------------

/// What a [`reconcile_watcher_health`] pass changed. Returned (rather than only
/// logged) so callers can log it in their own format and so the behaviour is
/// directly assertable in tests.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct WatcherHealthReconciliation {
    /// Watchers dropped from `watcher_health` because the config no longer
    /// mentions them at all.
    pub removed: Vec<String>,
    /// Watchers whose stored entry was flipped to `enabled: false` because the
    /// config disables them.
    pub disabled: Vec<String>,
    /// Watchers whose stored entry was flipped back to `enabled: true` because
    /// the config enables them again (the mirror case — without this a
    /// disable/re-enable cycle would leave the entry stuck at `false`, since
    /// the monitor only ever sets `enabled` at insert time).
    pub re_enabled: Vec<String>,
    /// True when the pass was SKIPPED because the parsed config yielded no
    /// entries at all. A missing/unreadable config is indistinguishable from a
    /// genuinely empty one at this layer, and pruning the whole map on a
    /// transient read failure would silently zero the watcher gauges. Failing
    /// closed (change nothing) keeps a stale entry, which is strictly less
    /// harmful than deleting live health for every watcher.
    pub skipped_empty_config: bool,
}

impl WatcherHealthReconciliation {
    /// True when the pass actually mutated the map.
    pub fn changed(&self) -> bool {
        !self.removed.is_empty() || !self.disabled.is_empty() || !self.re_enabled.is_empty()
    }
}

/// Bring `state.watcher_health` back into agreement with the watcher config.
///
/// Two distinct shapes are corrected, and they are different code paths:
///
/// 1. **Absent from config** — the watcher was retired; its entry is REMOVED
///    (along with any dangling `watcher_down_since` key), because there is no
///    such watcher left to hold health for.
/// 2. **Present but disabled** — the watcher still exists but is switched off;
///    its entry is KEPT (so re-enabling it does not lose the last-seen history)
///    but forced to `enabled: false`, and the missing-run bookkeeping
///    (`consecutive_missing` / `down_since` / `event_emitted_at`) is cleared,
///    since a watcher that is not supposed to be running cannot meaningfully be
///    "missing".
///
/// The `enabled` flag is synced in BOTH directions, so a watcher re-enabled in
/// the config gets its stored flag restored too.
///
/// `entries` is the fully-merged watcher list (primary config plus any extra
/// config), exactly as the watcher monitor builds it. When the same name
/// appears more than once, enabled-anywhere wins.
pub fn reconcile_watcher_health(
    state: &mut State,
    entries: &[crate::status::WatcherEntry],
) -> WatcherHealthReconciliation {
    let mut outcome = WatcherHealthReconciliation::default();

    if entries.is_empty() {
        outcome.skipped_empty_config = true;
        if !state.watcher_health.is_empty() {
            warn!(
                tracked = state.watcher_health.len(),
                "watcher config parsed to zero entries — skipping watcher_health reconciliation \
                 (cannot distinguish an unreadable config from an empty one)"
            );
        }
        return outcome;
    }

    let mut configured: HashMap<&str, bool> = HashMap::new();
    for entry in entries {
        configured
            .entry(entry.name.as_str())
            .and_modify(|e| *e |= entry.enabled)
            .or_insert(entry.enabled);
    }

    // Shape 1: retired watchers — drop the entry entirely.
    let mut removed: Vec<String> = Vec::new();
    state.watcher_health.retain(|name, _| {
        let keep = configured.contains_key(name.as_str());
        if !keep {
            removed.push(name.clone());
        }
        keep
    });
    removed.sort();

    // Shape 2: watchers the config disables (and the mirror re-enable case).
    // Names whose separate `watcher_down_since` key must go too, collected here
    // because that map cannot be touched while `watcher_health` is borrowed.
    let mut clear_down_since: Vec<String> = removed.clone();
    for (name, health) in state.watcher_health.iter_mut() {
        let enabled = configured
            .get(name.as_str())
            .copied()
            .unwrap_or(health.enabled);
        if health.enabled != enabled {
            health.enabled = enabled;
            if enabled {
                outcome.re_enabled.push(name.clone());
            } else {
                outcome.disabled.push(name.clone());
            }
        }
        if !enabled {
            // Not expected to run -> not meaningfully missing. Clearing the
            // counter is what actually takes the entry out of the
            // missing-watcher count and out of the hang-signal predicate; the
            // `enabled` flag alone only covers the readers that check it.
            health.consecutive_missing = 0;
            health.down_since = None;
            health.event_emitted_at = None;
            clear_down_since.push(name.clone());
        }
    }
    for name in &clear_down_since {
        state.watcher_down_since.remove(name);
    }
    outcome.removed = removed;
    outcome.disabled.sort();
    outcome.re_enabled.sort();

    outcome
}

pub fn load_state(path: &str) -> State {
    load_state_with_now(path, &chrono::Utc::now().to_rfc3339())
}

/// `load_state` with an injectable "startup now" timestamp so the cold-start
/// cooldown-seeding behavior is unit-testable without mocking the clock.
/// Production callers use [`load_state`], which passes the real wall clock.
pub fn load_state_with_now(path: &str, startup_now: &str) -> State {
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
    // last_interrupt_at is the global post-interrupt cooldown gate. On a
    // fresh daemon/container start it must INITIALIZE AT MAX COOLDOWN, NOT
    // be cleared. Clearing it to `None` makes `elapsed_since` read as
    // "never injected" → the cooldown check sees the daemon as infinitely
    // overdue → the very first cycle can fire an interrupt IMMEDIATELY,
    // before any real condition has had time to warrant it (the cold-start
    // injection storm). Seeding it to startup-time instead makes
    // `interrupt_in_global_cooldown` measure elapsed ≈ 0 on the first
    // cycle, so a cold-started daemon waits a FULL `post_interrupt_cooldown_secs`
    // window before the first interrupt is even eligible — "as if we just
    // injected at startup". A genuinely-stuck condition still fires once
    // the cooldown elapses; this only suppresses the spurious cold-start
    // fire in the first cooldown window. (Was: `= None`.)
    state.last_interrupt_at = Some(startup_now.to_string());
    // The two-phase escalation obligation timers are transient — daemon
    // downtime makes the dwell measurement meaningless — so they reset to
    // None on load. That's the SAFE direction here (independent of the
    // cooldown seeding above): `obligation_escalation_decision` treats a
    // `None` armed-at as `ArmObligation` (arm the obligation, DON'T
    // interrupt) on the first detection cycle, so a fresh start can never
    // escalate-to-interrupt on cycle 1 regardless of the cooldown.
    state.thinking_obligation_armed_at = None;
    state.context_obligation_armed_at = None;
    state.heartbeat_obligation_armed_at = None;
    state.watcher_down_obligation_armed_at = None;
    // The global-interrupt backoff streak is transient for the same reason:
    // daemon downtime makes the streak measurement meaningless. Reset to 0
    // so the cold-start cooldown above is the BASE `post_interrupt_cooldown_secs`
    // (not an inflated exponential window), which is the intended
    // "one full base cooldown after startup" behavior.
    state.global_interrupt_streak = 0;
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
                down_since: None,
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
        // last_interrupt_at is no longer cleared to None — it is SEEDED to
        // startup-time so the global cooldown initializes at max on a cold
        // start (cold-start injection-storm fix). It must be Some(...), not
        // the stale persisted 2026-01-01 value, and not None.
        assert!(loaded.last_interrupt_at.is_some());
        assert_ne!(
            loaded.last_interrupt_at.as_deref(),
            Some("2026-01-01T00:00:00+00:00"),
            "stale persisted timestamp must be replaced by startup-time seed"
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn test_cold_start_seeds_last_interrupt_at_to_startup_now() {
        // Cold-start injection-storm fix: on a fresh start, the global
        // post-interrupt cooldown gate (`last_interrupt_at`) must INITIALIZE
        // AT MAX COOLDOWN — seeded to the daemon's startup-time — NOT cleared
        // to None. None would read as "infinitely overdue" and let the first
        // cycle fire an interrupt immediately. With the seed, elapsed-since
        // ≈ 0 at startup, so the daemon waits a full cooldown window first.
        let startup_now = "2026-06-24T12:00:00+00:00";

        // (a) missing state file (brand-new daemon) -> seeded, not None.
        let loaded = load_state_with_now(
            "/tmp/nonexistent-claude-watch-cold-start.json",
            startup_now,
        );
        assert_eq!(
            loaded.last_interrupt_at.as_deref(),
            Some(startup_now),
            "fresh daemon must seed last_interrupt_at to startup_now"
        );
        // The exponential backoff streak resets so the seeded cooldown is the
        // BASE window, not an inflated one.
        assert_eq!(loaded.global_interrupt_streak, 0);

        // (b) existing state file with a stale timestamp -> overwritten with
        // startup_now (the persisted value is meaningless across downtime).
        let path = "/tmp/claude-watch-test-cold-start-seed.json";
        let mut state = State::default();
        state.last_interrupt_at = Some("2020-01-01T00:00:00+00:00".to_string());
        state.global_interrupt_streak = 9;
        save_state(path, &state);
        let loaded = load_state_with_now(path, startup_now);
        assert_eq!(loaded.last_interrupt_at.as_deref(), Some(startup_now));
        assert_eq!(loaded.global_interrupt_streak, 0);
        let _ = std::fs::remove_file(path);

        // (c) the seed actually engages the cooldown predicate: a freshly
        // loaded state is IN the global cooldown for any positive window
        // (elapsed ≈ 0 < cooldown), so the first-cycle interrupt is gated.
        let loaded = load_state_with_now(
            "/tmp/nonexistent-claude-watch-cold-start-2.json",
            &chrono::Utc::now().to_rfc3339(),
        );
        assert!(
            crate::policy::interrupt_in_global_cooldown(&loaded, 300),
            "cold-started state must be inside the global cooldown so the \
             first cycle cannot fire immediately"
        );
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
    fn test_obligation_arm_timers_cleared_on_load() {
        // The two-phase escalation obligation timers and the global-interrupt
        // backoff streak are transient — daemon downtime breaks the
        // dwell/streak semantics, so load_state() must clear all four
        // (mirrors thinking_start / last_interrupt_at).
        let path = "/tmp/claude-watch-test-obligation-arm-timers.json";
        let mut state = State::default();
        state.thinking_obligation_armed_at = Some("2026-06-24T00:00:00+00:00".to_string());
        state.context_obligation_armed_at = Some("2026-06-24T00:00:00+00:00".to_string());
        state.heartbeat_obligation_armed_at = Some("2026-06-24T00:00:00+00:00".to_string());
        state.watcher_down_obligation_armed_at = Some("2026-06-24T00:00:00+00:00".to_string());
        state.global_interrupt_streak = 7;
        save_state(path, &state);

        let loaded = load_state(path);
        assert!(loaded.thinking_obligation_armed_at.is_none());
        assert!(loaded.context_obligation_armed_at.is_none());
        assert!(loaded.heartbeat_obligation_armed_at.is_none());
        assert!(loaded.watcher_down_obligation_armed_at.is_none());
        assert_eq!(loaded.global_interrupt_streak, 0);
        let _ = std::fs::remove_file(path);

        // Old state file (no fields) -> defaults, not an error.
        let path2 = "/tmp/claude-watch-test-obligation-arm-timers-default.json";
        std::fs::write(path2, "{}").unwrap();
        let loaded2 = load_state(path2);
        assert!(loaded2.thinking_obligation_armed_at.is_none());
        assert!(loaded2.context_obligation_armed_at.is_none());
        assert!(loaded2.heartbeat_obligation_armed_at.is_none());
        assert!(loaded2.watcher_down_obligation_armed_at.is_none());
        assert_eq!(loaded2.global_interrupt_streak, 0);
        let _ = std::fs::remove_file(path2);
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


    // -----------------------------------------------------------------------
    // watcher_health reconciliation
    // -----------------------------------------------------------------------

    fn watcher_entry(name: &str, enabled: bool) -> crate::status::WatcherEntry {
        crate::status::WatcherEntry {
            name: name.to_string(),
            pattern: format!("/usr/local/bin/{name}"),
            min_count: 1,
            enabled,
            start_cmd: None,
            on_restart_cmd: None,
            ..Default::default()
        }
    }

    fn health(enabled: bool, consecutive_missing: u32) -> WatcherState {
        WatcherState {
            last_seen_running: Some("2026-06-01T00:00:00Z".to_string()),
            consecutive_missing,
            enabled,
            event_emitted_at: Some("2026-06-01T00:05:00Z".to_string()),
            down_since: Some("2026-06-01T00:05:00Z".to_string()),
        }
    }

    /// Count of watchers the `claude_watchers_missing` gauge would report:
    /// entries that are `enabled` AND past the missing threshold. Mirrors the
    /// predicate in `metrics::build_metrics` so the tests assert the actual
    /// user-visible consequence, not just the map contents.
    fn missing_gauge(state: &State) -> usize {
        state
            .watcher_health
            .values()
            .filter(|w| w.enabled && w.consecutive_missing > 3)
            .count()
    }

    #[test]
    fn reconcile_watcher_health_drops_watchers_absent_from_config() {
        // Shape 1: the watcher was RETIRED (its line deleted from the config).
        // The monitor loop never iterates a name it cannot see, so the entry
        // would otherwise keep `enabled: true` and its climbing miss counter
        // forever.
        let mut state = State::default();
        state
            .watcher_health
            .insert("retired-watcher".to_string(), health(true, 71));
        state
            .watcher_health
            .insert("live-watcher".to_string(), health(true, 0));
        state
            .watcher_down_since
            .insert("retired-watcher".to_string(), "2026-06-01T00:05:00Z".into());

        let outcome = reconcile_watcher_health(&mut state, &[watcher_entry("live-watcher", true)]);

        assert_eq!(outcome.removed, vec!["retired-watcher".to_string()]);
        assert!(outcome.disabled.is_empty());
        assert!(outcome.changed());
        assert!(!state.watcher_health.contains_key("retired-watcher"));
        assert!(state.watcher_health.contains_key("live-watcher"));
        // The parallel down-since map is keyed by the same names; a dangling
        // key there would outlive the watcher too.
        assert!(!state.watcher_down_since.contains_key("retired-watcher"));
        assert_eq!(missing_gauge(&state), 0);
    }

    #[test]
    fn reconcile_watcher_health_disables_watchers_the_config_disables() {
        // Shape 2 — a DIFFERENT code path from shape 1: the watcher is still
        // listed, but its config line is switched off. The entry is kept (so a
        // later re-enable does not lose history) but must stop counting as an
        // enabled, missing watcher.
        let mut state = State::default();
        state
            .watcher_health
            .insert("switched-off".to_string(), health(true, 84));
        state
            .watcher_down_since
            .insert("switched-off".to_string(), "2026-06-01T00:05:00Z".into());
        assert_eq!(missing_gauge(&state), 1);

        let outcome = reconcile_watcher_health(&mut state, &[watcher_entry("switched-off", false)]);

        assert_eq!(outcome.disabled, vec!["switched-off".to_string()]);
        assert!(outcome.removed.is_empty());
        let entry = &state.watcher_health["switched-off"];
        assert!(!entry.enabled);
        // The miss bookkeeping is cleared too: a watcher that is not supposed
        // to run cannot be "missing", and the hang-signal predicate reads the
        // counter directly.
        assert_eq!(entry.consecutive_missing, 0);
        assert!(entry.down_since.is_none());
        assert!(entry.event_emitted_at.is_none());
        assert!(!state.watcher_down_since.contains_key("switched-off"));
        assert_eq!(missing_gauge(&state), 0);
    }

    #[test]
    fn reconcile_watcher_health_re_enables_when_config_turns_a_watcher_back_on() {
        // Mirror of shape 2. The monitor only ever sets `enabled` when it
        // INSERTS an entry, so without a re-enable pass a disable/enable cycle
        // would leave the stored flag stuck at false and the watcher invisible
        // to every reader that gates on it.
        let mut state = State::default();
        state
            .watcher_health
            .insert("back-on".to_string(), health(false, 0));

        let outcome = reconcile_watcher_health(&mut state, &[watcher_entry("back-on", true)]);

        assert_eq!(outcome.re_enabled, vec!["back-on".to_string()]);
        assert!(state.watcher_health["back-on"].enabled);
    }

    #[test]
    fn reconcile_watcher_health_clears_both_stale_shapes_together() {
        // The observed failure: one retired watcher plus one config-disabled
        // watcher, each with a large miss counter, pinning the missing gauge at
        // 2 indefinitely. Both shapes must clear in a single pass.
        let mut state = State::default();
        state
            .watcher_health
            .insert("retired".to_string(), health(true, 71));
        state
            .watcher_health
            .insert("disabled-in-config".to_string(), health(true, 84));
        state
            .watcher_health
            .insert("healthy".to_string(), health(true, 0));
        assert_eq!(missing_gauge(&state), 2);

        let outcome = reconcile_watcher_health(
            &mut state,
            &[
                watcher_entry("disabled-in-config", false),
                watcher_entry("healthy", true),
            ],
        );

        assert_eq!(outcome.removed, vec!["retired".to_string()]);
        assert_eq!(outcome.disabled, vec!["disabled-in-config".to_string()]);
        assert_eq!(missing_gauge(&state), 0);
        // A genuinely enabled watcher is untouched.
        assert!(state.watcher_health["healthy"].enabled);
    }

    #[test]
    fn reconcile_watcher_health_leaves_enabled_watchers_untouched() {
        // Reconciliation must not disturb live health: a real outage on a
        // configured+enabled watcher still counts.
        let mut state = State::default();
        state
            .watcher_health
            .insert("down-for-real".to_string(), health(true, 9));

        let outcome = reconcile_watcher_health(&mut state, &[watcher_entry("down-for-real", true)]);

        assert!(!outcome.changed());
        assert_eq!(state.watcher_health["down-for-real"].consecutive_missing, 9);
        assert_eq!(missing_gauge(&state), 1);
    }

    #[test]
    fn reconcile_watcher_health_skips_when_config_parses_to_nothing() {
        // A missing/unreadable config parses to zero entries, which is
        // indistinguishable from a genuinely empty one. Pruning on that would
        // wipe every watcher's health (and silently zero the gauges) on a
        // transient read failure, so the pass fails closed instead.
        let mut state = State::default();
        state
            .watcher_health
            .insert("still-configured".to_string(), health(true, 2));

        let outcome = reconcile_watcher_health(&mut state, &[]);

        assert!(outcome.skipped_empty_config);
        assert!(!outcome.changed());
        assert!(state.watcher_health.contains_key("still-configured"));
    }

    #[test]
    fn reconcile_watcher_health_enabled_anywhere_wins_across_merged_configs() {
        // The daemon merges the primary watchers config with an optional extra
        // one, so the same name can appear twice. A watcher enabled in either
        // file is enabled.
        let mut state = State::default();
        state
            .watcher_health
            .insert("dual-listed".to_string(), health(false, 0));

        let outcome = reconcile_watcher_health(
            &mut state,
            &[
                watcher_entry("dual-listed", false),
                watcher_entry("dual-listed", true),
            ],
        );

        assert_eq!(outcome.re_enabled, vec!["dual-listed".to_string()]);
        assert!(state.watcher_health["dual-listed"].enabled);
    }

    #[test]
    fn reconcile_watcher_health_survives_a_save_load_roundtrip() {
        // The reconciled map is what gets persisted, so the stale entry is gone
        // from the state file the metrics exporter reads (it reads the file,
        // not the daemon's in-memory state).
        let path = "/tmp/claude-watch-test-watcher-health-reconcile.json";
        let mut state = State::default();
        state
            .watcher_health
            .insert("retired".to_string(), health(true, 71));
        state
            .watcher_health
            .insert("kept".to_string(), health(true, 0));

        reconcile_watcher_health(&mut state, &[watcher_entry("kept", true)]);
        save_state(path, &state);

        let loaded = load_state(path);
        assert!(!loaded.watcher_health.contains_key("retired"));
        assert!(loaded.watcher_health.contains_key("kept"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn reconcile_watcher_health_reads_both_shapes_from_a_real_watchers_config() {
        // Cover the seam both call sites actually use: parse the config file,
        // then reconcile. The disabled shape depends on the parser's fourth
        // column, so asserting it through real config text (rather than a
        // hand-built entry) is what proves the disabled case is wired up.
        let path = "/tmp/claude-watch-test-watchers-reconcile.conf";
        std::fs::write(
            path,
            "# name|pattern|min_count|enabled\n\
             live-watcher|/usr/local/bin/live-watcher|1|true\n\
             off-watcher|/usr/local/bin/off-watcher|1|false\n",
        )
        .unwrap();

        let mut state = State::default();
        // Retired: no longer in the file at all.
        state
            .watcher_health
            .insert("gone-watcher".to_string(), health(true, 71));
        // Present but switched off in the file.
        state
            .watcher_health
            .insert("off-watcher".to_string(), health(true, 84));
        state
            .watcher_health
            .insert("live-watcher".to_string(), health(true, 0));
        assert_eq!(missing_gauge(&state), 2);

        let entries = crate::status::parse_watchers_config(path);
        let outcome = reconcile_watcher_health(&mut state, &entries);

        assert_eq!(outcome.removed, vec!["gone-watcher".to_string()]);
        assert_eq!(outcome.disabled, vec!["off-watcher".to_string()]);
        assert!(!state.watcher_health["off-watcher"].enabled);
        assert!(state.watcher_health["live-watcher"].enabled);
        assert_eq!(missing_gauge(&state), 0);
        let _ = std::fs::remove_file(path);
    }
}
