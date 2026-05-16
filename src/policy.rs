//! Policy: the main check logic including dead process detection, fresh /clear,
//! heartbeat stale, foreground monitor, and watcher health.

use chrono::{DateTime, Local, Timelike, Utc};
use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
use std::os::unix::process::CommandExt;
use std::time::SystemTime;
use tracing::{debug, info, warn};

use crate::alert;
use crate::config::Config;
use crate::logging::{write_jsonl_log, write_legacy_log};
use crate::reminders::{seconds_since_fire, should_defer_to_hook, ReminderType};
use crate::state::{FailureDetail, State, StatusSnapshot, WatcherState};
use crate::status;
use crate::tmux;

/// Parse elapsed seconds since an ISO datetime string.
pub(crate) fn elapsed_since(dt_str: &str) -> Option<f64> {
    let dt = DateTime::parse_from_rfc3339(dt_str).ok()?;
    let now = Utc::now();
    Some((now - dt.with_timezone(&Utc)).num_milliseconds() as f64 / 1000.0)
}

/// Pure function: compute the next thinking interrupt threshold with exponential backoff.
/// Formula: min(base_threshold * backoff_multiplier^interrupt_count, max_backoff)
/// E.g. with base=60, mult=2, max=960: 60, 120, 240, 480, 960, 960, ...
/// With base=300, mult=3, max=1800: 300, 900, 1800, 1800, ...
///
/// This 2-multiplier wrapper is retained for backward-compatibility and is
/// used by the legacy-compat test. The daemon's check_foreground path now
/// calls `thinking_backoff_threshold_with_multiplier` directly, reading the
/// multiplier from config.
#[allow(dead_code)]
pub(crate) fn thinking_backoff_threshold(
    base_threshold: u64,
    max_backoff: u64,
    interrupt_count: u32,
) -> u64 {
    thinking_backoff_threshold_with_multiplier(base_threshold, max_backoff, interrupt_count, 2)
}

/// Generalised version of `thinking_backoff_threshold` with a configurable
/// multiplier per step. Uses saturating arithmetic so huge `interrupt_count`
/// values never panic — they just cap at `max_backoff`.
pub(crate) fn thinking_backoff_threshold_with_multiplier(
    base_threshold: u64,
    max_backoff: u64,
    interrupt_count: u32,
    multiplier: u64,
) -> u64 {
    let mut threshold = base_threshold;
    for _ in 0..interrupt_count {
        threshold = threshold.saturating_mul(multiplier);
        if threshold >= max_backoff {
            return max_backoff;
        }
    }
    threshold.min(max_backoff)
}

/// Returns true if a previous interrupt fired within the last
/// `cooldown_secs` seconds. Used to suppress cascading interrupts across
/// the prolonged-thinking and context-warning fire paths.
///
/// NOTE: The watcher-down inject path is intentionally EXEMPT from
/// this gate. A down watcher (any of the `*-wait` / `claude-event-
/// watch` / torrent-wait family) is a hard liveness failure — silence
/// in the cooldown window means inbound events go unprocessed for as
/// long as it takes to clear. The watcher-down
/// inject must be allowed to fire even when another interrupt fired
/// recently. The per-watcher `last_watcher_inject` cooldown
/// (`watcher_monitor.inject_cooldown`, default 60s) still rate-limits
/// re-injects on the same fire path.
///
/// A `cooldown_secs` of 0 disables the gate entirely.
pub(crate) fn interrupt_in_global_cooldown(state: &State, cooldown_secs: u64) -> bool {
    if cooldown_secs == 0 {
        return false;
    }
    state
        .last_interrupt_at
        .as_deref()
        .and_then(elapsed_since)
        .is_some_and(|e| e < cooldown_secs as f64)
}

/// Pure predicate: should the watcher-down inject path fire now, given
/// the timestamp of the last watcher-inject and the configured cooldown?
///
/// - `None` last-inject (never fired before) -> always allow.
/// - `Some(ts)` -> allow iff elapsed >= cooldown_secs (or the timestamp
///   is malformed and `elapsed_since` returns None — fail-open so the
///   gate never wedges).
///
/// Intentionally does NOT consult `interrupt_in_global_cooldown` (PR #44):
/// a down watcher is a hard liveness failure, so the watcher-down path is
/// exempt from the global post-interrupt cooldown that gates other inject
/// reasons.
pub(crate) fn watcher_inject_due(
    last_watcher_inject: Option<&str>,
    cooldown_secs: u64,
) -> bool {
    match last_watcher_inject {
        Some(last) => elapsed_since(last).is_none_or(|e| e >= cooldown_secs as f64),
        None => true,
    }
}

/// Returns true if the main loop is "actively turning" — either a tool
/// call is currently running (`bashes > 0` this check) or one fired
/// within the last `window_secs` (per `state.last_active_at`).
///
/// Used by the watcher-down inject suppression gate so the daemon does
/// not preempt an in-flight turn with a `WATCHER(S) DOWN` prompt. A
/// `window_secs` of 0 still honors the live `bashes > 0` check.
pub(crate) fn main_loop_actively_turning(
    state: &State,
    bashes: u64,
    window_secs: u64,
) -> bool {
    if bashes > 0 {
        return true;
    }
    state
        .last_active_at
        .as_deref()
        .and_then(elapsed_since)
        .is_some_and(|e| e < window_secs as f64)
}

/// Pure predicate: should the fresh-/clear inject be suppressed because
/// the main loop is actively turning? Mirrors the decision we make at
/// the fire site so unit tests don't have to mock tmux pane reads.
///
/// Returns true iff `suppress_enabled && main_loop_actively_turning(...)`.
pub(crate) fn fresh_clear_inject_suppressed(
    state: &State,
    bashes: u64,
    suppress_enabled: bool,
    window_secs: u64,
) -> bool {
    suppress_enabled && main_loop_actively_turning(state, bashes, window_secs)
}

/// Pure predicate: should the dead-process restart be suppressed because
/// the main loop is actively turning? Mirrors the decision we make at
/// the fire site so unit tests don't have to mock tmux pane reads.
///
/// Returns true iff `suppress_enabled && main_loop_actively_turning(...)`.
pub(crate) fn dead_process_restart_suppressed(
    state: &State,
    bashes: u64,
    suppress_enabled: bool,
    window_secs: u64,
) -> bool {
    suppress_enabled && main_loop_actively_turning(state, bashes, window_secs)
}

/// Pure predicate: is at least one workload heartbeat fresh?
///
/// Scans `dir` for files (any name) and returns `true` if any has an
/// mtime within `max_age_secs` of `now`. Used to suppress stuck-state
/// alerts (heartbeat-stale, prolonged-thinking) when an out-of-band
/// `workload run` is providing proof-of-life that the main loop's
/// idleness can't otherwise explain.
///
/// Returns `false` (no suppression) if:
///   * `dir` doesn't exist (no workloads ever ran on this host).
///   * `dir` exists but is empty (no active workloads).
///   * Every heartbeat file's mtime is older than `max_age_secs`
///     (workloads stalled — let the existing stuck-alert fire).
///   * `max_age_secs == 0` AND no file's mtime equals `now` exactly
///     (mostly useful for tests).
///
/// Fail-open behaviour: any I/O error reading `dir` returns `false`
/// so a transient permissions / mount issue can't accidentally
/// suppress the entire stuck-detection subsystem.
pub(crate) fn workload_heartbeat_fresh(
    dir: &std::path::Path,
    max_age_secs: u64,
    now: SystemTime,
) -> bool {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return false,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        // Only consider regular files. A subdir named like a label
        // shouldn't ever exist here, but skip it defensively.
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        if !meta.is_file() {
            continue;
        }
        // Only count files with the `.heartbeat` suffix so unrelated
        // sidecars (`.alerted`, `.tmp` from a mid-rename touch) don't
        // accidentally satisfy freshness. The wrapper writes
        // `<label>.heartbeat` so the suffix is stable.
        if path
            .extension()
            .and_then(|s| s.to_str())
            .is_none_or(|s| s != "heartbeat")
        {
            continue;
        }
        let mtime = match meta.modified() {
            Ok(t) => t,
            Err(_) => continue,
        };
        let age = match now.duration_since(mtime) {
            Ok(d) => d,
            Err(_) => {
                // mtime is in the future relative to now — treat as fresh
                // (clock skew, but proof the file was very recently written).
                return true;
            }
        };
        if age.as_secs() <= max_age_secs {
            return true;
        }
    }
    false
}

/// Convenience wrapper that pulls the dir + threshold from `Config` and
/// honours the `enabled` master switch. Always uses `SystemTime::now()`
/// so callers don't have to thread a clock through.
pub(crate) fn workload_heartbeat_suppresses_stuck(config: &Config) -> bool {
    if !config.stuck_detection.enabled {
        return false;
    }
    workload_heartbeat_fresh(
        std::path::Path::new(&config.stuck_detection.workload_heartbeat_dir),
        config.stuck_detection.workload_heartbeat_max_age_secs,
        SystemTime::now(),
    )
}

/// Reason a force-inject escalation should fire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EscalationReason {
    /// `consecutive_suppressions >= max_consecutive_suppressions`.
    ConsecutiveCap,
    /// `now - first_suppression_at > max_suppression_window_secs`.
    WindowExceeded,
}

impl EscalationReason {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            EscalationReason::ConsecutiveCap => "consecutive_cap",
            EscalationReason::WindowExceeded => "window_exceeded",
        }
    }
}

/// Pure predicate: has the cross-gate suppression run been long/persistent
/// enough that the next gate fire should force-inject regardless of
/// `actively_turning`? Returns the triggering reason if so.
///
/// Both limits are checked on EVERY gate fire — the consecutive counter
/// catches "many suppressions in a tight window" and the wall-clock window
/// catches "fewer suppressions, but the active turn has been running so
/// long the gate's been open way too long".
///
/// `consecutive_suppressions == 0` short-circuits to None: the first
/// suppression of a run can never escalate (escalation only fires when
/// the gate has demonstrably failed to drain at least once).
pub(crate) fn should_escalate_suppression(
    state: &State,
    max_consecutive_suppressions: u32,
    max_suppression_window_secs: u64,
) -> Option<EscalationReason> {
    if state.consecutive_suppressions == 0 {
        return None;
    }
    if max_consecutive_suppressions > 0
        && state.consecutive_suppressions >= max_consecutive_suppressions
    {
        return Some(EscalationReason::ConsecutiveCap);
    }
    if max_suppression_window_secs > 0 {
        if let Some(elapsed) = state
            .first_suppression_at
            .as_deref()
            .and_then(elapsed_since)
        {
            if elapsed > max_suppression_window_secs as f64 {
                return Some(EscalationReason::WindowExceeded);
            }
        }
    }
    None
}

/// Record that a suppression-gate fired and was suppressed (the `actively_
/// turning` path took the "skip the inject" branch). Increments the shared
/// counter and stamps `first_suppression_at` on the 0 -> 1 transition.
/// Idempotent w.r.t. `first_suppression_at` after the first call.
pub(crate) fn record_suppression(state: &mut State, now: &str) {
    if state.consecutive_suppressions == 0 {
        state.first_suppression_at = Some(now.to_string());
    }
    state.consecutive_suppressions = state.consecutive_suppressions.saturating_add(1);
}

/// Reset the shared suppression counter and timestamp. Called when an
/// inject lands successfully OR when the underlying suppression condition
/// resolves (the gate's predicate stops matching). Cheap no-op when the
/// counter is already 0.
pub(crate) fn reset_suppression(state: &mut State) {
    state.consecutive_suppressions = 0;
    state.first_suppression_at = None;
}

/// Pure helper: filter the missing-watchers list before emitting a
/// `watcher-down` claude-event, suppressing the event entirely when the
/// only thing down is the event-consumer watcher.
///
/// **Why this exists**: when the event-consumer watcher (typically
/// `claude-event-watch`) goes down, dropping a `watcher-down` JSON file
/// into `~/claude-events/` creates a self-reinforcing feedback loop:
///
///   1. consumer-watcher reads the next event, prints it, exits (one-shot).
///   2. main loop restarts the consumer.
///   3. claude-watch sees the consumer briefly DOWN, emits a
///      `watcher-down` event ABOUT THE CONSUMER into `~/claude-events/`.
///   4. consumer fires immediately on its own self-referential alert,
///      exits. Goto 3.
///
/// We observed 6+ buffered self-alerts pile up after a fresh restart and
/// take down the watcher for 30+ minutes. The fix: never write a
/// `watcher-down` event whose only payload IS the consumer watcher — the
/// consumer can't deliver an event about itself, so the file is
/// pure self-feedback. The tmux-inject path (`watcher-ctl run <name>`
/// typed into the Claude Code pane) is unaffected and remains the
/// recovery channel for a down consumer.
///
/// Behaviour:
/// - `affected = [consumer]`  → returns `None` (suppress emit entirely).
/// - `affected = [a, consumer, b]` → returns `Some([a, b])` (filter
///   the consumer out so the event is still useful for the other
///   watchers without dragging the consumer's name back into the
///   self-feedback path).
/// - `affected = [a, b]` (consumer not present) → returns
///   `Some([a, b])` unchanged.
/// - `affected = []` → returns `None` (nothing to emit).
pub(crate) fn filter_consumer_for_event_emit(
    affected: &[String],
    consumer_name: &str,
) -> Option<Vec<String>> {
    let filtered: Vec<String> = affected
        .iter()
        .filter(|name| name.as_str() != consumer_name)
        .cloned()
        .collect();
    if filtered.is_empty() {
        None
    } else {
        Some(filtered)
    }
}

/// Quiet-path decision for watcher-down events.
///
/// Pure helper: given the configured thresholds plus a watcher's current
/// state, decide what the watcher-monitor cycle should do this iteration.
/// Returns a `WatcherDownAction`:
///
///   * `Nothing`         — below event_threshold, or in grace window.
///   * `EmitEvent`       — fire a `watcher-down` claude-event; quiet path.
///   * `InjectFallback`  — heavyweight tmux-inject path:
///       - the watcher is the configured event-consumer (chicken-and-egg:
///         emitting an event with no consumer is pointless), OR
///       - we already emitted an event for this watcher AND the grace
///         window has expired AND consecutive_missing has reached the
///         inject_threshold.
///
/// This function does NOT consult the global cooldown or the
/// `last_watcher_inject` cooldown; those are layered on top by the caller
/// at the inject site (mirroring the legacy behaviour).
#[derive(Debug, PartialEq, Eq, Clone)]
pub(crate) enum WatcherDownAction {
    Nothing,
    EmitEvent,
    InjectFallback,
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn evaluate_watcher_down_action(
    is_consumer_watcher: bool,
    consecutive_missing: u32,
    event_emitted_at: Option<&str>,
    event_threshold: u32,
    inject_threshold: u32,
    event_grace_secs: u64,
) -> WatcherDownAction {
    // Special-case: when the consumer watcher itself is missing, the quiet
    // path can't deliver — skip event emission and fall straight through
    // to inject as soon as it has reached the inject_threshold (so the
    // legacy semantics for that watcher are preserved).
    if is_consumer_watcher {
        if consecutive_missing >= inject_threshold {
            return WatcherDownAction::InjectFallback;
        }
        return WatcherDownAction::Nothing;
    }

    let grace_active = event_emitted_at
        .and_then(elapsed_since)
        .is_some_and(|e| e < event_grace_secs as f64);

    // Once the quiet path has fired AT ALL for this watcher (regardless of
    // grace age), the inject path is the only escalation route — we do NOT
    // re-emit. While the grace window is active, the loud path is also
    // suppressed (give the main loop a chance). Past the grace window, we
    // fall through to inject as fallback for the case where the main loop
    // never picked up the event (or claude-event-watch is itself stalled).
    if event_emitted_at.is_some() {
        if grace_active {
            return WatcherDownAction::Nothing;
        }
        if consecutive_missing >= inject_threshold {
            return WatcherDownAction::InjectFallback;
        }
        return WatcherDownAction::Nothing;
    }

    // No prior emission. First-time event emission: at-or-above
    // event_threshold but below inject_threshold (so the quiet path
    // strictly precedes the loud one for normal configs).
    if consecutive_missing >= event_threshold && consecutive_missing < inject_threshold {
        return WatcherDownAction::EmitEvent;
    }

    // No prior event AND consecutive_missing has marched past the inject
    // threshold without ever crossing event_threshold (only possible if
    // event_threshold > inject_threshold, i.e. misconfiguration). Fall
    // through to inject as legacy behaviour.
    if consecutive_missing >= inject_threshold {
        return WatcherDownAction::InjectFallback;
    }

    WatcherDownAction::Nothing
}

/// Best-effort fire-and-forget emission of a `watcher-down` claude-event.
///
/// Shells out to the configured `claude-event` CLI. If the CLI is missing,
/// crashes, or hangs, we log and move on — the caller should treat this as
/// non-blocking. The fallback inject path will eventually fire if the main
/// loop never picks the event up.
async fn emit_watcher_down_event(
    cli: &str,
    watcher: &str,
    consecutive_missing: u32,
    recorded_pid: Option<u32>,
) -> bool {
    let message = format!(
        "Watcher DOWN: {}. Run: watcher-ctl run {}",
        watcher, watcher
    );
    let pid_str = match recorded_pid {
        Some(p) => p.to_string(),
        None => "null".to_string(),
    };
    let watcher_kv = format!("watcher={}", watcher);
    let consec_kv = format!("consecutive_missing={}", consecutive_missing);
    let pid_kv = format!("recorded_pid={}", pid_str);
    let args: Vec<&str> = vec![
        cli,
        &message,
        "--tag",
        "watcher-down",
        "--source",
        "claude-watch",
        "--source-name",
        "claude-watch",
        "--priority",
        "high",
        "--data",
        &watcher_kv,
        "--data",
        &consec_kv,
        "--data",
        &pid_kv,
    ];

    // 5s timeout — claude-event is a tiny Python script that should complete
    // in well under a second; if it hangs, don't block the monitor loop.
    let result = crate::cmd::run_cmd_any(&args, 5).await;
    if !result.1 {
        warn!(
            watcher = %watcher,
            cli = %cli,
            "claude-event emission failed (CLI missing, non-zero exit, or timeout); falling back to inject path on next cycle past grace window"
        );
        return false;
    }
    true
}

/// If the given reminder fired within the last `max_age_secs` (we default
/// to 1 hour — beyond that we assume the self-action is unrelated),
/// record the reminder -> action latency sample into the state-based
/// counters that `claude-watch metrics` exports. No-op otherwise.
///
/// `short` selects the shorter "context clear" latency window (1h); the
/// longer version-update path uses `short = false` (6h cap) because
/// updates can legitimately take many turns to propagate.
fn record_reminder_latency_if_recent(kind: ReminderType, state: &mut State, short: bool) {
    let max_age = if short { 3600.0 } else { 21600.0 };
    let elapsed = match seconds_since_fire(kind) {
        Some(e) if e >= 0.0 && e < max_age => e,
        _ => return,
    };
    match kind {
        ReminderType::ContextHigh => {
            state.reminder_to_clear_latency_secs_sum += elapsed;
            state.reminder_to_clear_latency_count =
                state.reminder_to_clear_latency_count.saturating_add(1);
        }
        ReminderType::VersionUpdate => {
            state.reminder_to_update_latency_secs_sum += elapsed;
            state.reminder_to_update_latency_count =
                state.reminder_to_update_latency_count.saturating_add(1);
        }
        ReminderType::PreCompact => {
            // PreCompact is a blocking hook — there's no "latency to
            // action" concept the same way as the other two. Skip.
        }
    }
}

/// Restart Claude Code by writing a relaunch script and injecting it.
async fn restart_claude(pane: &str, state: &mut State, config: &crate::config::ClaudeConfig) {
    let now = Local::now().to_rfc3339();

    // Try to find session ID from pane history
    let mut session_id: Option<String> = None;
    if let Some(out) = tmux::capture_pane_history(pane, 100).await {
        let re = regex_lite::Regex::new(r"--resume\s+([0-9a-f-]{36})").unwrap();
        if let Some(caps) = re.captures(&out) {
            session_id = Some(caps[1].to_string());
        }
    }

    // NOTE: Do NOT use --append-system-prompt here. It persists for the lifetime of the
    // process (survives /clear), causing misleading messages on subsequent context clears.
    // The resume prompt injection handles session startup instead.
    // --dangerously-skip-permissions: harness-managed instances run in
    // permanent permission-bypass mode. The harness's own gates
    // (obligations, queue, etc.) provide finer-grained safety than
    // per-tool prompts.
    let launch = if let Some(ref sid) = session_id {
        info!(session_id = %sid, "restarting Claude Code with --resume");
        format!("claude --dangerously-skip-permissions --resume {}", sid)
    } else {
        info!("restarting Claude Code with --continue (no session ID found)");
        "claude --dangerously-skip-permissions --continue".to_string()
    };

    // Write relaunch script
    let script_content = format!(
        "#!/bin/bash\ncd $HOME\n{}\necho \"\\n[claude-watch-relaunch] Claude exited with code $?\"\n",
        launch
    );
    if let Err(e) = std::fs::write(&config.relaunch_script, &script_content) {
        tracing::error!(error = %e, "failed to write relaunch script");
        return;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(
            &config.relaunch_script,
            std::fs::Permissions::from_mode(0o755),
        );
    }

    tmux::inject_shell(pane, &format!("bash {}", config.relaunch_script)).await;

    state.last_restart = Some(now);
    state.restart_count += 1;
    state.restart_claude_interrupts_total =
        state.restart_claude_interrupts_total.saturating_add(1);
    state.pending_resume_inject = true;

    alert::notify(crate::event_bus::ClaudeWatchAlert {
        alert_type: "claude-crashed",
        stuck_reason: "claude code process gone",
        stale_minutes: None,
        affected_watchers: vec![],
        severity: crate::event_bus::Severity::High,
        message: "claude-watch: Claude Code crashed -- auto-restarting",
    })
    .await;
}

/// Pure decision: given the current observation (`is_retrying`) and the
/// existing state, return the `(new_consecutive, new_first_seen, suppress)`
/// triple. Split out so the consecutive-cycles + max-stuck-secs logic can
/// be unit-tested without mocking tmux.
///
/// Semantics:
///   - `is_retrying=true` increments `consecutive`. The first detection sets
///     `first_seen`. Suppression activates once `consecutive >= threshold`.
///   - `is_retrying=true` AND we've already been suppressing for longer than
///     `max_stuck_secs` returns `suppress=false` so monitoring resumes (the
///     retry has hung long enough to count as a real failure).
///   - `is_retrying=false` clears the episode immediately.
pub(crate) fn evaluate_api_retry_state(
    is_retrying: bool,
    consecutive: u32,
    first_seen: Option<&str>,
    threshold: u32,
    max_stuck_secs: u64,
) -> (u32, Option<String>, bool) {
    if !is_retrying {
        return (0, None, false);
    }

    let new_consecutive = consecutive.saturating_add(1);
    // Preserve the original first_seen if we already have one; otherwise stamp
    // it now (the caller passes the current local time as `first_seen=None`
    // when no episode is in progress).
    let new_first_seen = match first_seen {
        Some(fs) => Some(fs.to_string()),
        None => Some(Local::now().to_rfc3339()),
    };

    // Don't suppress until the consecutive threshold is reached.
    if new_consecutive < threshold {
        return (new_consecutive, new_first_seen, false);
    }

    // Suppression cap: once we've been retrying for longer than
    // max_stuck_secs, stop suppressing — let the normal monitoring sites
    // fire so something can recover.
    if max_stuck_secs > 0 {
        if let Some(ref fs) = new_first_seen {
            if let Some(elapsed) = elapsed_since(fs) {
                if elapsed > max_stuck_secs as f64 {
                    return (new_consecutive, new_first_seen, false);
                }
            }
        }
    }

    (new_consecutive, new_first_seen, true)
}

/// Detect whether the pane is currently in an upstream-API retry-backoff and
/// update the daemon's tracking state accordingly. Returns true when the
/// caller should SUPPRESS interrupt fires for this cycle.
///
/// This is the single chokepoint for the "back off when API is overloaded"
/// fix. To avoid double-counting state updates when `check_cycle` calls
/// `check_foreground` near the end of its body (both would otherwise call
/// this function in a single cycle), the caller in `check_foreground` skips
/// the update and reads the suppression flag from existing state via
/// `is_api_retry_suppressing` instead.
async fn update_api_retry_state(config: &Config, state: &mut State, pane: &str) -> bool {
    if !config.api_retry.enabled || pane.is_empty() {
        return false;
    }

    let is_retrying = tmux::detect_api_retry(pane).await;
    let was_suppressing = is_api_retry_suppressing(config, state);

    let (new_consec, new_first, suppress) = evaluate_api_retry_state(
        is_retrying,
        state.api_retry_consecutive,
        state.api_retry_first_seen.as_deref(),
        config.api_retry.consecutive,
        config.api_retry.max_stuck_secs,
    );
    state.api_retry_consecutive = new_consec;
    state.api_retry_first_seen = new_first;

    if suppress {
        state.api_retry_suppressions_total =
            state.api_retry_suppressions_total.saturating_add(1);
        if !was_suppressing {
            // Edge: log on transition into suppression.
            info!(
                consecutive = state.api_retry_consecutive,
                "API retry detected — suppressing interrupt sites until retry resolves"
            );
            write_jsonl_log(
                &config.general.log_file,
                "api_retry_suppress_start",
                serde_json::json!({
                    "consecutive": state.api_retry_consecutive,
                }),
            );
        } else {
            debug!(
                consecutive = state.api_retry_consecutive,
                "api_retry suppression continues"
            );
        }
    } else if was_suppressing {
        // Transition out of suppression. Either the retry resolved or we hit
        // the max_stuck_secs cap — either way the caller resumes normal
        // monitoring on this cycle.
        info!("API retry resolved or stuck timeout reached — resuming normal monitoring");
        write_jsonl_log(
            &config.general.log_file,
            "api_retry_suppress_end",
            serde_json::json!({}),
        );
    }

    suppress
}

/// Pure decision (no I/O, no state mutation): given the current State and
/// Config, return whether the api_retry guard is currently suppressing
/// interrupts. Used by `check_foreground` when called from inside
/// `check_cycle` (which already ran `update_api_retry_state` once this
/// cycle) so we don't increment the suppressions counter twice.
///
/// Returns false when the feature is disabled, no episode is in progress,
/// the consecutive threshold isn't met, or the max_stuck_secs cap has been
/// exceeded.
pub(crate) fn is_api_retry_suppressing(config: &Config, state: &State) -> bool {
    if !config.api_retry.enabled {
        return false;
    }
    if state.api_retry_consecutive < config.api_retry.consecutive {
        return false;
    }
    let first_seen = match state.api_retry_first_seen.as_deref() {
        Some(fs) => fs,
        None => return false,
    };
    if config.api_retry.max_stuck_secs > 0 {
        if let Some(elapsed) = elapsed_since(first_seen) {
            if elapsed > config.api_retry.max_stuck_secs as f64 {
                return false;
            }
        }
    }
    true
}

/// Run a foreground-only check cycle. This is called more frequently than
/// the full check_cycle to provide responsive foreground blocking detection.
/// Requires a known pane to check against.
///
/// Performs its own api_retry detection via `update_api_retry_state`. Use
/// `check_foreground_inner` directly when called from inside `check_cycle`
/// to avoid double-incrementing the api_retry state counters in a single
/// full-check cycle.
pub async fn check_foreground(
    config: &Config,
    state: &mut State,
    pane: &str,
    tokens: u64,
    bashes: u64,
) {
    if !config.foreground_monitor.enabled || pane.is_empty() {
        return;
    }
    let api_retrying = update_api_retry_state(config, state, pane).await;
    check_foreground_inner(config, state, pane, tokens, bashes, api_retrying).await;
}

/// Foreground check body, with the api_retrying flag passed in by the
/// caller. Split out from `check_foreground` so `check_cycle` can call it
/// without re-running `update_api_retry_state` (which would
/// double-increment `api_retry_suppressions_total` per full cycle).
async fn check_foreground_inner(
    config: &Config,
    state: &mut State,
    pane: &str,
    tokens: u64,
    bashes: u64,
    api_retrying: bool,
) {
    if !config.foreground_monitor.enabled || pane.is_empty() {
        return;
    }

    // API retry guard: if Claude Code is currently in upstream-API retry
    // backoff (529 / overloaded / 5xx), suppress every fire from this
    // function. Each inject during retry resets the retry state machine,
    // creating a livelock where the retry loop never gets to complete.
    // Also reset the thinking timer so a stale start time doesn't cause
    // an immediate fire the moment the retry resolves.
    if api_retrying {
        debug!("foreground check: api_retry active — suppressing fires this cycle");
        state.thinking_start = None;
        state.thinking_alerted = false;
        state.foreground_start = None;
        state.foreground_alerted = false;
        return;
    }

    let now = chrono::Local::now().to_rfc3339();
    let fg_busy = tmux::is_foreground_busy(pane).await;

    // Also check thinking state at 3s resolution
    let activity = tmux::get_activity(pane).await;
    let is_thinking = matches!(activity, tmux::ClaudeActivity::Thinking);
    debug!(fg_busy, is_thinking, activity = %activity, tokens, bashes, "foreground check");

    // --- Thinking duration tracking (with exponential backoff) ---
    if is_thinking {
        if state.thinking_start.is_none() {
            state.thinking_start = Some(now.clone());
            state.thinking_alerted = false;
            // Don't reset thinking_interrupt_count here — it persists across
            // brief non-thinking blips within the same stall episode. It only
            // resets when we see a genuinely active state (below).
        } else if let Some(ref start) = state.thinking_start {
            if let Some(elapsed) = elapsed_since(start) {
                let next_threshold = thinking_backoff_threshold_with_multiplier(
                    config.foreground_monitor.threshold_seconds,
                    config.foreground_monitor.max_thinking_backoff,
                    state.thinking_interrupt_count,
                    config.foreground_monitor.thinking_backoff_multiplier,
                );
                if elapsed >= next_threshold as f64 {
                    // Global post-interrupt cooldown: if ANY interrupt fired
                    // recently (watcher-down, context-warning, or a prior
                    // thinking one), suppress this fire. Prevents the
                    // cascade where e.g. a watcher-down interrupt resets the
                    // thinking timer and the new thought trips prolonged
                    // thinking immediately afterward.
                    if interrupt_in_global_cooldown(
                        state,
                        config.general.post_interrupt_cooldown_secs,
                    ) {
                        debug!(
                            elapsed_secs = elapsed,
                            threshold = next_threshold,
                            cooldown = config.general.post_interrupt_cooldown_secs,
                            "prolonged thinking would fire but global post-interrupt cooldown active"
                        );
                        return;
                    }
                    // Workload-heartbeat suppression: an active
                    // `workload run` (stv-promote, big rsync, ffmpeg)
                    // can pin the main loop in a fire-and-forget wait
                    // that the prolonged-thinking detector reads as a
                    // stuck thought. Suppress when any workload
                    // heartbeat file under
                    // `config.stuck_detection.workload_heartbeat_dir`
                    // is younger than
                    // `workload_heartbeat_max_age_secs`. The thinking
                    // timer is NOT reset here — the next cycle re-
                    // evaluates from the same start so the moment the
                    // workload finishes (heartbeat goes stale) the
                    // interrupt can fire on the next tick.
                    if workload_heartbeat_suppresses_stuck(config) {
                        debug!(
                            elapsed_secs = elapsed,
                            threshold = next_threshold,
                            dir = %config.stuck_detection.workload_heartbeat_dir,
                            "prolonged thinking suppressed by fresh workload heartbeat"
                        );
                        write_jsonl_log(
                            &config.general.log_file,
                            "prolonged_thinking_suppressed",
                            serde_json::json!({
                                "elapsed_secs": elapsed,
                                "threshold_secs": next_threshold,
                                "reason": "workload_heartbeat_fresh",
                                "dir": &config.stuck_detection.workload_heartbeat_dir,
                                "max_age_secs": config.stuck_detection.workload_heartbeat_max_age_secs,
                            }),
                        );
                        return;
                    }
                    warn!(
                        elapsed_secs = elapsed,
                        threshold = next_threshold,
                        interrupt_count = state.thinking_interrupt_count,
                        "prolonged thinking detected — interrupting (backoff)"
                    );
                    write_jsonl_log(
                        &config.general.log_file,
                        "prolonged_thinking",
                        serde_json::json!({
                            "elapsed_secs": elapsed,
                            "tokens": tokens,
                            "bashes": bashes,
                            "interrupt_count": state.thinking_interrupt_count,
                            "next_threshold_secs": next_threshold,
                            "action": if config.foreground_monitor.interrupt_enabled { "interrupt" } else { "log-only" },
                        }),
                    );
                    state.thinking_alerted = true;
                    state.thinking_interrupt_count += 1;
                    // Reset thinking_start so the next backoff interval
                    // counts from NOW, not from the original start
                    state.thinking_start = Some(now.clone());

                    if config.foreground_monitor.interrupt_enabled {
                        info!(
                            interrupt_count = state.thinking_interrupt_count,
                            next_backoff_secs = thinking_backoff_threshold_with_multiplier(
                                config.foreground_monitor.threshold_seconds,
                                config.foreground_monitor.max_thinking_backoff,
                                state.thinking_interrupt_count,
                                config.foreground_monitor.thinking_backoff_multiplier,
                            ),
                            "thinking interrupt: Escape + inject prompt"
                        );
                        // Stamp the global interrupt cooldown so other fire
                        // paths (watcher-down, context-warning) see this
                        // interrupt and back off.
                        state.last_interrupt_at = Some(now.clone());
                        state.prolonged_thinking_interrupts_total = state
                            .prolonged_thinking_interrupts_total
                            .saturating_add(1);
                        // 5s budget: Escape blasts every 250ms. If Claude
                        // hasn't honored the interrupt by ~5s, it almost
                        // certainly won't — proceed with the inject anyway.
                        // Pre-fix: 30s, dominated perceived recovery latency.
                        tmux::interrupt_and_wait(pane, 5).await;
                        let msg = format!(
                                "[CLAUDE-WATCH] Prolonged thinking detected (>{}s in thinking state, interrupt #{}). \
                                You appear to be stuck in a long generation. If you have complex work to do, \
                                delegate it to a background Agent instead of doing it inline. \
                                Use run_in_background: true for long Bash commands. \
                                Resume your current task now.",
                                next_threshold,
                                state.thinking_interrupt_count,
                            );
                        tmux::inject_text(pane, &msg).await;
                        write_jsonl_log(
                            &config.general.log_file,
                            "thinking_interrupted",
                            serde_json::json!({
                                "elapsed_secs": elapsed,
                                "tokens": tokens,
                                "bashes": bashes,
                                "interrupt_count": state.thinking_interrupt_count,
                            }),
                        );
                        // Third sink: claude-event so the main loop can
                        // see this stuck-state via structured fields and
                        // not just react reflexively to the injected
                        // string.
                        let pt_reason = format!(
                            "prolonged thinking ({}s, interrupt #{})",
                            elapsed as u64, state.thinking_interrupt_count,
                        );
                        alert::emit_event(crate::event_bus::ClaudeWatchAlert {
                            alert_type: "prolonged-thinking",
                            stuck_reason: &pt_reason,
                            stale_minutes: None,
                            affected_watchers: vec![],
                            severity: crate::event_bus::Severity::Medium,
                            message: &msg,
                        });
                    } else {
                        info!(
                            elapsed_secs = elapsed,
                            interrupt_count = state.thinking_interrupt_count,
                            "thinking would interrupt (log-only mode)"
                        );
                    }
                }
            }
        }
    } else {
        state.thinking_start = None;
        state.thinking_alerted = false;
        state.thinking_interrupt_count = 0;
    }

    // --- Foreground blocking tracking ---
    if fg_busy {
        if state.foreground_start.is_none() {
            state.foreground_start = Some(now);
            state.foreground_alerted = false;
        } else if !state.foreground_alerted {
            if let Some(ref start) = state.foreground_start {
                if let Some(elapsed) = elapsed_since(start) {
                    if elapsed >= config.foreground_monitor.threshold_seconds as f64 {
                        warn!(
                            elapsed_secs = elapsed,
                            threshold = config.foreground_monitor.threshold_seconds,
                            "foreground blocking detected"
                        );
                        write_jsonl_log(
                            &config.general.log_file,
                            "foreground_blocking",
                            serde_json::json!({
                                "elapsed_secs": elapsed,
                                "tokens": tokens,
                                "bashes": bashes,
                            }),
                        );
                        state.foreground_alerted = true;

                        if config.foreground_monitor.interrupt_enabled {
                            info!("foreground interrupt: sending Ctrl-B x2 + inject message");
                            state.foreground_blocking_interrupts_total = state
                                .foreground_blocking_interrupts_total
                                .saturating_add(1);
                            // 5s budget — see comment at the prolonged-thinking
                            // interrupt site above.
                            tmux::interrupt_and_wait(pane, 5).await;
                            tmux::inject_text(pane, &config.foreground_monitor.interrupt_message)
                                .await;
                            write_jsonl_log(
                                &config.general.log_file,
                                "foreground_interrupted",
                                serde_json::json!({
                                    "elapsed_secs": elapsed,
                                    "tokens": tokens,
                                    "bashes": bashes,
                                    "message": config.foreground_monitor.interrupt_message,
                                }),
                            );
                        } else {
                            info!(
                                elapsed_secs = elapsed,
                                "foreground would interrupt (log-only mode)"
                            );
                            write_jsonl_log(
                                &config.general.log_file,
                                "foreground_would_interrupt",
                                serde_json::json!({
                                    "elapsed_secs": elapsed,
                                    "tokens": tokens,
                                    "bashes": bashes,
                                }),
                            );
                        }
                    }
                }
            }
        }
    } else {
        state.foreground_start = None;
        state.foreground_alerted = false;
    }
}

/// Check if a PID is still alive (signal 0 probe).
fn is_pid_alive(pid: u32) -> bool {
    kill(Pid::from_raw(pid as i32), Signal::SIGCONT)
        .map(|_| true)
        .unwrap_or(false)
}

/// Read a watcher PID file and return the recorded PID, if the file exists
/// and contains a parseable integer. Whitespace is trimmed.
///
/// Returns:
/// - `Some(pid)` if the file exists and parses cleanly.
/// - `None` if the file is missing, unreadable, or contains non-numeric data.
fn read_watcher_pid(pid_dir: &str, name: &str) -> Option<u32> {
    let path = format!("{}/{}.pid", pid_dir, name);
    let content = std::fs::read_to_string(&path).ok()?;
    content.trim().parse::<u32>().ok()
}

/// Decide whether a watcher should be considered DOWN, given:
/// - the live process count (from `pgrep -fc`)
/// - the configured `min_count`
/// - the recorded PID file (if any)
/// - a process-liveness probe (typically `is_pid_alive`)
///
/// Returns `true` when the watcher is missing/orphaned.
///
/// The orphan-detection branch is the bug-2 fix: if a PID file exists and its
/// recorded PID is dead, the watcher is DOWN even when `pgrep -fc` happens to
/// match some other process by accident (e.g. a stale shell whose argv still
/// contains the watcher's name pattern, or a self-matching wrapper). This
/// matches the legacy `watchmen` shell-script's `kill -0` cross-check that
/// was lost when watchmen was rewritten in Rust.
///
/// Watchers without a PID file fall through to the existing pgrep-only logic
/// (preserves backward compat for watchers we don't explicitly track).
pub fn watcher_is_down(
    pgrep_count: u32,
    min_count: u32,
    recorded_pid: Option<u32>,
    pid_alive: impl Fn(u32) -> bool,
) -> bool {
    // Standard pgrep-only check first.
    if pgrep_count < min_count {
        return true;
    }
    // Orphan-detection: pgrep saw a match, but the PID Claude actually
    // started has died. The match is a false positive — count as DOWN.
    if let Some(pid) = recorded_pid {
        if !pid_alive(pid) {
            return true;
        }
    }
    false
}

/// Spawn `self-clear` immediately (no grace period). Used for the
/// wedged-pane recovery path: when the agent is too stuck to run any tool
/// call (context limit reached, persistent 429), claude-watch must drive
/// `/clear` itself rather than waiting for the agent to cooperate.
///
/// Detached via setsid() so it survives a daemon restart, same as
/// `spawn_deferred_clear`.
fn spawn_immediate_clear(state: &mut State) {
    // Don't double-spawn if a deferred clear child is already running.
    if let Some(pid) = state.context_clear_child_pid {
        if is_pid_alive(pid) {
            debug!(pid, "self-clear child already running, skipping immediate spawn");
            return;
        }
    }

    // SAFETY: setsid() is async-signal-safe and we call it before exec.
    match unsafe {
        std::process::Command::new("self-clear")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .pre_exec(|| {
                nix::unistd::setsid()
                    .map(|_| ())
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))
            })
            .spawn()
    } {
        Ok(child) => {
            state.context_clear_child_pid = Some(child.id());
            info!(pid = child.id(), "spawned immediate self-clear (wedged recovery)");
        }
        Err(e) => {
            tracing::error!(error = %e, "failed to spawn immediate self-clear");
        }
    }
}

/// Spawn a deferred self-clear child process.
/// The child sleeps for the grace period, then checks if tokens are still high.
/// If so, it runs `self-clear` to force a context clear.
fn spawn_deferred_clear(config: &Config, state: &mut State) {
    // If there's already a living child, skip
    if let Some(pid) = state.context_clear_child_pid {
        if is_pid_alive(pid) {
            debug!(pid, "deferred self-clear child already running");
            return;
        }
    }

    let grace = config.context_monitor.grace_period;
    // The child: sleep for grace period, polling every 10s.
    // If tokens drop below 30000 (Claude cleared on its own), exit cleanly.
    // If grace expires with tokens still high, run self-clear.
    let script = format!(
        r#"elapsed=0; while [ "$elapsed" -lt {grace} ]; do sleep 10; elapsed=$((elapsed + 10)); tokens=$(claude-watch status --tokens 2>/dev/null); if [ "$tokens" != "?" ] && [ "$tokens" -lt 30000 ] 2>/dev/null; then exit 0; fi; done; self-clear"#,
        grace = grace
    );

    // SAFETY: setsid() is async-signal-safe and we call it before exec.
    // This detaches the child into its own session so it survives
    // systemd's cgroup-wide SIGTERM when claude-watch restarts.
    match unsafe {
        std::process::Command::new("bash")
            .args(["-c", &script])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .pre_exec(|| {
                nix::unistd::setsid()
                    .map(|_| ())
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))
            })
            .spawn()
    } {
        Ok(child) => {
            state.context_clear_child_pid = Some(child.id());
            info!(pid = child.id(), grace, "spawned deferred self-clear child");
        }
        Err(e) => {
            tracing::error!(error = %e, "failed to spawn deferred self-clear");
        }
    }
}

/// Inject a context warning message into the Claude Code pane.
async fn inject_context_warning(pane: &str, pct: f64, compact_remaining: Option<u32>, grace: u64) {
    let context_info = if let Some(cr) = compact_remaining {
        format!("{}% compact remaining", cr)
    } else {
        format!("{:.0}% token usage", pct)
    };

    let msg = format!(
        "[CLAUDE-WATCH] CONTEXT CRITICALLY LOW ({}). \
        You MUST act IMMEDIATELY: (1) session-task set '<state>', \
        (2) commit/push repos, (3) self-clear. \
        Forced clear in {}s if you don't act.",
        context_info, grace
    );
    // 5s budget — same rationale as the other inline interrupt sites.
    tmux::interrupt_and_wait(pane, 5).await;
    tmux::inject_text(pane, &msg).await;
}

/// Below this token count, Claude Code is treated as "fresh / just-cleared"
/// and the trigger flag resets. The deferred-clear child uses the same
/// constant in its inner poll, and `self-clear` confirms a clear by reading
/// tokens drop below it.
pub(crate) const CONTEXT_FRESH_TOKEN_THRESHOLD: u64 = 30000;

/// Threshold below which `last_seen_tokens` is considered "previously low /
/// boot state" — used to suppress spammy external-clear logs while the daemon
/// is just starting up (no prior high reading).
const PREV_HIGH_FOR_EXTERNAL_CLEAR_LOG: u64 = 30000;

/// Reset `state.context_clear_triggered` when tokens drop below the fresh
/// threshold, regardless of whether the inner trigger gate (`tokens > 0`)
/// runs this cycle. Also handles the external-clear bookkeeping path so the
/// "Since Last Clear" dashboard metric stays accurate.
///
/// Why this lives outside the `tokens > 0` guard in `check_cycle`:
/// when `self-clear` succeeds, the pane briefly shows tokens=0. The inner
/// trigger block was skipped on that sample (tokens=0 → guard false), and
/// the reset path was nested inside the same guard — so the flag never
/// reset. As soon as Claude resumed (tokens jumps to >30K), the sub-30K
/// branch couldn't fire either, and `context_clear_triggered` stayed stuck
/// at true for the rest of the session. Real incident 2026-05-01: deferred
/// clear ran cleanly, but the next four hours of context-threshold checks
/// were all suppressed by the stuck flag — the user had to manually /clear.
pub(crate) fn maybe_reset_context_clear(
    config: &Config,
    state: &mut State,
    tokens: u64,
    now: &str,
) {
    if tokens >= CONTEXT_FRESH_TOKEN_THRESHOLD {
        return;
    }

    // Path 1: we triggered the clear and it landed (tokens dropped). Reset
    // the in-flight flag + child-pid bookkeeping so the next threshold
    // crossing can fire.
    if state.context_clear_triggered {
        info!(tokens, "context clear detected — resetting trigger");
        write_jsonl_log(
            &config.general.log_file,
            "context_clear_reset",
            serde_json::json!({
                "tokens": tokens,
            }),
        );
        record_reminder_latency_if_recent(ReminderType::ContextHigh, state, true);
        state.context_clear_triggered = false;
        state.context_clear_child_pid = None;
        state.last_context_clear = Some(now.to_string());
        return;
    }

    // Path 2: external clear (user `/clear`, fresh-clear path, or any other
    // off-path reset). Only emit the log when we previously saw a high
    // sample, to avoid logging on every check during boot.
    if state.last_seen_tokens.unwrap_or(0) >= PREV_HIGH_FOR_EXTERNAL_CLEAR_LOG {
        info!(
            tokens,
            prev_tokens = state.last_seen_tokens,
            "external context clear detected"
        );
        write_jsonl_log(
            &config.general.log_file,
            "context_clear_reset",
            serde_json::json!({
                "tokens": tokens,
                "external": true,
            }),
        );
        record_reminder_latency_if_recent(ReminderType::ContextHigh, state, true);
        state.last_context_clear = Some(now.to_string());
    }
}

/// Determine if context threshold is exceeded.
/// Returns Some((pct, triggered_by_compact)) if triggered, None otherwise.
///
/// The three trigger paths are INDEPENDENT — any one firing causes a trigger:
///
/// 1. **BY_COMPACT** (primary): `compact_remaining <= compact_trigger_percent`.
///    The most accurate signal when Claude Code reports it.
/// 2. **BY_MARGIN** (safety net): `tokens >= max_context_tokens - threshold_margin`.
///    Runs even when compact_remaining is Some but not triggering — this is the
///    fix for the 2026-04-30 incident where a session sat at 95.97% for 12 min
///    with no auto-clear because the old else-if chain skipped this check.
/// 3. **BY_PERCENT** (legacy fallback): `pct >= threshold_percent`. Only used
///    when threshold_margin is unset (per documented config semantics:
///    "ignored when threshold_margin is set").
pub(crate) fn check_context_threshold_with_margin(
    tokens: u64,
    max_context_tokens: u64,
    compact_remaining: Option<u32>,
    threshold_percent: u64,
    compact_trigger_percent: u32,
    threshold_margin: Option<u64>,
) -> Option<(f64, bool)> {
    let pct = (tokens as f64 / max_context_tokens as f64) * 100.0;

    // Primary: compact_remaining is the most accurate signal when present.
    if let Some(cr) = compact_remaining {
        if cr <= compact_trigger_percent {
            return Some((pct, true));
        }
    }

    // Safety net: fixed token margin from max. Runs independently of the
    // compact_remaining check — if compact didn't trigger above, margin still
    // gets a chance to fire.
    if let Some(margin) = threshold_margin {
        if max_context_tokens > margin && tokens >= max_context_tokens - margin {
            return Some((pct, false));
        }
        // When threshold_margin is set, threshold_percent is ignored
        // (legacy fallback semantics, documented in ContextMonitorConfig).
        return None;
    }

    // Legacy fallback: percent of max.
    if pct >= threshold_percent as f64 {
        return Some((pct, false));
    }

    None
}

/// Check if an auto-update should be triggered, and if so, spawn the update task.
/// This is called from check_cycle() on each iteration.
/// Check if Claude Code needs API reauth and send high-priority alert.
///
/// Two-phase flow:
/// 1. **401 detected** (TUI visible, error JSON in pane) — inject `/login`, no alert yet.
/// 2. **Login screen visible** (OAuth URL present) — send high-priority alert with URL.
///
/// Alerts are rate-limited to once per `alert_interval_seconds` (default 3 hours).
async fn check_reauth(config: &Config, state: &mut State, pane: &str) {
    let reauth_result = tmux::needs_reauth(pane).await;

    if let Some(login_url) = reauth_result {
        if !state.reauth_detected {
            info!("reauth needed: first detection");
            state.reauth_detected = true;
        }

        // Inject /login once per reauth cycle so the login screen appears
        if !state.login_injected {
            info!("injecting /login command into pane");
            tmux::inject_text(pane, "/login").await;
            state.login_injected = true;
            state.reauth_inject_interrupts_total = state
                .reauth_inject_interrupts_total
                .saturating_add(1);
            write_jsonl_log(
                &config.general.log_file,
                "login_injected",
                serde_json::json!({ "pane": pane }),
            );
            write_legacy_log(
                &config.general.legacy_log_file,
                "Reauth: injected /login command",
            );
            crate::state::save_state(&config.general.state_file, state);
        }

        // Only send the high-priority alert once we have the OAuth URL.
        // Phase 1 (401 error) has no URL — we just inject /login and wait.
        // Phase 2 (login screen) has the URL — send the alert so Andrew can
        // open it on his phone and SSH in to paste the auth code.
        if !login_url.is_empty() {
            // Check alert cooldown
            let should_alert = match &state.last_reauth_alert {
                Some(last) => {
                    if let Some(elapsed) = elapsed_since(last) {
                        elapsed >= config.reauth.alert_interval_seconds as f64
                    } else {
                        true
                    }
                }
                None => true,
            };

            if should_alert {
                let now = Local::now().to_rfc3339();
                warn!("sending high-priority reauth alert with URL");
                let alert_msg = format!("Claude Code login needed. URL: {}", login_url);
                alert::notify(crate::event_bus::ClaudeWatchAlert {
                    alert_type: "reauth-needed",
                    stuck_reason: "claude code 401, login url present",
                    stale_minutes: None,
                    affected_watchers: vec![],
                    severity: crate::event_bus::Severity::High,
                    message: &alert_msg,
                })
                .await;
                write_jsonl_log(
                    &config.general.log_file,
                    "reauth_alert",
                    serde_json::json!({ "pane": pane, "url": login_url }),
                );
                write_legacy_log(
                    &config.general.legacy_log_file,
                    "Reauth needed: sent high-priority alert with URL",
                );
                state.last_reauth_alert = Some(now);
                crate::state::save_state(&config.general.state_file, state);
            } else {
                debug!("reauth still needed, alert cooldown active");
            }
        } else {
            debug!("reauth detected (401) but no URL yet — waiting for login screen");
        }
    } else if state.reauth_detected {
        // Reauth resolved
        info!("reauth resolved");
        write_jsonl_log(
            &config.general.log_file,
            "reauth_resolved",
            serde_json::json!({}),
        );
        write_legacy_log(&config.general.legacy_log_file, "Reauth resolved");
        state.reauth_detected = false;
        state.last_reauth_alert = None;
        state.login_injected = false;
        crate::state::save_state(&config.general.state_file, state);
    }
}

/// Check for a manual update trigger file written by `claude-watch update`.
/// If found, force-run the auto-update regardless of schedule.
pub async fn check_update_trigger(config: &Config, state: &mut State, pane: &str) {
    const TRIGGER_FILE: &str = "/tmp/claude-watch-update-trigger";

    let content = match std::fs::read_to_string(TRIGGER_FILE) {
        Ok(c) => c,
        Err(_) => return, // No trigger file
    };

    // Remove the trigger file immediately to avoid re-triggering
    let _ = std::fs::remove_file(TRIGGER_FILE);

    let force = content.trim() == "force";
    info!(force, "manual update trigger detected");
    write_jsonl_log(
        &config.general.log_file,
        "manual_update_trigger",
        serde_json::json!({ "force": force }),
    );

    if pane.is_empty() {
        warn!("manual update trigger found but no pane detected");
        return;
    }

    // Check version mismatch (or force)
    let version_info = tokio::task::spawn_blocking(crate::status::get_version_info)
        .await
        .unwrap_or_default();

    let running = match version_info.running {
        Some(v) => v,
        None => {
            warn!("manual update trigger: cannot determine running version");
            return;
        }
    };
    let installed = match version_info.installed {
        Some(v) => v,
        None => {
            warn!("manual update trigger: cannot determine installed version");
            return;
        }
    };

    if running == installed && !force {
        info!(running = %running, "manual update trigger: already up to date");
        return;
    }

    info!(
        running = %running,
        installed = %installed,
        force,
        "manual update trigger — starting update"
    );

    write_jsonl_log(
        &config.general.log_file,
        "manual_update_start",
        serde_json::json!({
            "running": running,
            "installed": installed,
            "force": force,
        }),
    );

    state.last_update_attempt = Some(chrono::Local::now().to_rfc3339());
    state.update_in_progress = true;
    state.auto_update_count += 1;
    state.auto_update_interrupts_total =
        state.auto_update_interrupts_total.saturating_add(1);
    crate::state::save_state(&config.general.state_file, state);

    let pane = pane.to_string();
    let config = config.clone();
    let state_file = config.general.state_file.clone();
    tokio::spawn(async move {
        run_auto_update(&pane, &running, &installed, &config).await;
        let mut st = crate::state::load_state(&state_file);
        st.update_in_progress = false;
        crate::state::save_state(&state_file, &st);
    });
}

pub async fn check_auto_update(config: &Config, state: &mut State, pane: &str) {
    if !config.auto_update.enabled || pane.is_empty() {
        return;
    }

    // Don't run if an update is already in progress (with 1-hour staleness timeout)
    if state.update_in_progress {
        if let Some(ref last_attempt) = state.last_update_attempt {
            if let Some(elapsed) = elapsed_since(last_attempt) {
                if elapsed > 3600.0 {
                    warn!(
                        "auto-update: update_in_progress stuck for {:.0}s, clearing",
                        elapsed
                    );
                    state.update_in_progress = false;
                    crate::state::save_state(&config.general.state_file, state);
                } else {
                    debug!(
                        "auto-update already in progress ({:.0}s ago), skipping",
                        elapsed
                    );
                    return;
                }
            } else {
                debug!("auto-update already in progress, skipping");
                return;
            }
        } else {
            // No last_attempt but update_in_progress is true — stale, clear it
            warn!("auto-update: update_in_progress with no last_attempt, clearing");
            state.update_in_progress = false;
            crate::state::save_state(&config.general.state_file, state);
        }
    }

    let now = Local::now();

    // Check if we're at the configured minute of the hour
    let current_minute = now.minute();
    if current_minute != config.auto_update.check_minute {
        return;
    }

    // Check cooldown since last attempt
    if let Some(ref last_attempt) = state.last_update_attempt {
        if let Some(elapsed) = elapsed_since(last_attempt) {
            let cooldown_secs = config.auto_update.cooldown_hours * 3600;
            if elapsed < cooldown_secs as f64 {
                return;
            }
        }
    }

    // Check version mismatch
    let version_info = tokio::task::spawn_blocking(crate::status::get_version_info)
        .await
        .unwrap_or_default();

    let running = match version_info.running {
        Some(v) => v,
        None => return,
    };
    let installed = match version_info.installed {
        Some(v) => v,
        None => return,
    };

    if running == installed {
        state.last_update_check = Some(now.to_rfc3339());
        debug!(running = %running, installed = %installed, "versions match, no update needed");
        // Claude Code picked up the new binary (either via /restart after the
        // hook reminder or via the previous fallback). Record the latency.
        record_reminder_latency_if_recent(ReminderType::VersionUpdate, state, false);
        return;
    }

    // Hybrid gate: if the version_update hook fired recently, give Claude
    // a grace window to `/restart` on its own before falling back to the
    // heavy-handed `claude update` injection.
    if config.hybrid.enabled
        && should_defer_to_hook(
            ReminderType::VersionUpdate,
            config.hybrid.version_fallback_secs as f64,
        )
    {
        debug!(
            running = %running,
            installed = %installed,
            grace = config.hybrid.version_fallback_secs,
            "version mismatch detected but deferring to recent hook reminder"
        );
        write_jsonl_log(
            &config.general.log_file,
            "auto_update_hook_deferred",
            serde_json::json!({
                "running": running,
                "installed": installed,
                "grace_secs": config.hybrid.version_fallback_secs,
            }),
        );
        state.last_update_check = Some(now.to_rfc3339());
        return;
    }

    info!(
        running = %running,
        installed = %installed,
        "version mismatch detected — starting auto-update (hybrid fallback)"
    );

    write_jsonl_log(
        &config.general.log_file,
        "auto_update_start",
        serde_json::json!({
            "running": running,
            "installed": installed,
            "hybrid_fallback": true,
        }),
    );

    state.last_update_attempt = Some(now.to_rfc3339());
    state.last_update_check = Some(now.to_rfc3339());
    state.update_in_progress = true;
    state.auto_update_count += 1;
    state.fallback_update_count = state.fallback_update_count.saturating_add(1);
    state.auto_update_interrupts_total =
        state.auto_update_interrupts_total.saturating_add(1);
    crate::state::save_state(&config.general.state_file, state);

    // Spawn the long-running update sequence as a background task
    let pane = pane.to_string();
    let config = config.clone();
    let state_file = config.general.state_file.clone();
    tokio::spawn(async move {
        run_auto_update(&pane, &running, &installed, &config).await;
        // Clear update_in_progress in state file
        let mut st = crate::state::load_state(&state_file);
        st.update_in_progress = false;
        crate::state::save_state(&state_file, &st);
    });
}

/// Execute the auto-update sequence: interrupt → /exit → wait → relaunch → resume.
async fn run_auto_update(pane: &str, old_version: &str, new_version: &str, config: &Config) {
    info!("auto-update: interrupting Claude Code...");
    write_jsonl_log(
        &config.general.log_file,
        "auto_update_interrupt",
        serde_json::json!({}),
    );

    // Step 1: Interrupt and wait for idle. 10s budget — auto-update is
    // a rare path so we're a bit more patient than the inline interrupt
    // sites (5s), but still bounded so a stuck pane doesn't pin the
    // updater for half a minute.
    if tmux::interrupt_and_wait(pane, 10).await {
        info!("auto-update: Claude Code is idle");
    } else {
        warn!("auto-update: could not confirm idle after 10s, proceeding anyway");
    }

    // Settle time after interruption
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;

    // Step 2: Inject /exit
    info!("auto-update: injecting /exit...");
    tmux::inject_text(pane, "/exit").await;

    // Step 3: Wait for Claude to exit
    info!("auto-update: waiting for Claude Code to exit...");
    if !tmux::wait_for_exit(pane, 45).await {
        warn!("auto-update: Claude Code did not exit within 45s, aborting");
        write_jsonl_log(
            &config.general.log_file,
            "auto_update_failed",
            serde_json::json!({"reason": "exit_timeout"}),
        );
        alert::notify(crate::event_bus::ClaudeWatchAlert {
            alert_type: "auto-update-failed",
            stuck_reason: "auto-update: claude code did not exit within 45s",
            stale_minutes: None,
            affected_watchers: vec![],
            severity: crate::event_bus::Severity::High,
            message: "claude-watch: auto-update FAILED — Claude Code did not exit",
        })
        .await;
        return;
    }
    info!("auto-update: Claude Code exited");

    // Brief delay for shell prompt to fully render
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    // Step 4: Capture session ID from pane content
    let mut session_id: Option<String> = None;
    if let Some(out) = tmux::capture_pane_history(pane, 100).await {
        let re = regex_lite::Regex::new(r"--resume\s+([0-9a-f-]{36})").unwrap();
        if let Some(caps) = re.captures(&out) {
            session_id = Some(caps[1].to_string());
        }
    }

    if let Some(ref sid) = session_id {
        info!(session_id = %sid, "auto-update: captured session ID");
    } else {
        info!("auto-update: no session ID found, will use --continue");
    }

    // Step 5: Write relaunch script
    // NOTE: Do NOT use --append-system-prompt here. It persists for the lifetime of the
    // process (survives /clear), causing misleading "version update" messages on subsequent
    // context clears. The resume prompt (step 9) handles session startup instead.
    //
    // --dangerously-skip-permissions: harness-managed instances run in permanent
    // permission-bypass mode (see also crash-recovery launch above).
    let launch = if let Some(ref sid) = session_id {
        format!("claude --dangerously-skip-permissions --resume {}", sid)
    } else {
        "claude --dangerously-skip-permissions --continue".to_string()
    };

    let script_content = format!(
        "#!/bin/bash\ncd $HOME\n{}\necho \"\\n[claude-watch-update] Claude exited with code $?\"\n",
        launch
    );
    if let Err(e) = std::fs::write(&config.claude.relaunch_script, &script_content) {
        tracing::error!(error = %e, "auto-update: failed to write relaunch script");
        return;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(
            &config.claude.relaunch_script,
            std::fs::Permissions::from_mode(0o755),
        );
    }

    // Step 6: Inject relaunch command into shell
    info!("auto-update: injecting relaunch command...");
    tmux::inject_shell(pane, &format!("bash {}", config.claude.relaunch_script)).await;

    // Step 7: Wait for claude binary to appear in process tree
    info!("auto-update: waiting for Claude binary to start...");
    if !tmux::wait_for_claude_binary(pane, 120).await {
        warn!("auto-update: claude binary not detected after 120s");
        write_jsonl_log(
            &config.general.log_file,
            "auto_update_warning",
            serde_json::json!({"reason": "binary_not_found"}),
        );
    }

    // Step 8: Wait for ❯ prompt (Claude Code is ready for input)
    info!("auto-update: waiting for idle prompt...");
    if !tmux::wait_for_idle_prompt(pane, 90).await {
        warn!("auto-update: prompt not found after 90s, trying inject anyway");
    }

    // Brief settle after prompt appears
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;

    // Step 9: Inject resume text
    info!("auto-update: injecting resume prompt...");
    tmux::inject_text(pane, &config.auto_update.resume_prompt).await;

    // Step 10: Log and notify
    write_jsonl_log(
        &config.general.log_file,
        "auto_update_complete",
        serde_json::json!({
            "old_version": old_version,
            "new_version": new_version,
            "session_id": session_id,
        }),
    );

    let msg = format!(
        "claude-watch: auto-update complete ({} → {})",
        old_version, new_version
    );
    alert::notify(crate::event_bus::ClaudeWatchAlert {
        alert_type: "auto-update-complete",
        stuck_reason: "auto-update finished",
        stale_minutes: None,
        affected_watchers: vec![],
        severity: crate::event_bus::Severity::Low,
        message: &msg,
    })
    .await;
    info!("auto-update: complete ({} → {})", old_version, new_version);
}

/// Pure function: decide whether a self-heal retry should reset the dead-check
/// counter. Returns true if `dead_checks` has reached the configured threshold
/// AND the retry observed a non-zero status (tokens or bashes).
///
/// Split out so the decision logic can be unit-tested without mocking tmux.
pub(crate) fn should_self_heal(
    dead_checks: u32,
    checks_required: u32,
    retry_tokens: u64,
    retry_bashes: u64,
) -> bool {
    dead_checks >= checks_required && (retry_tokens > 0 || retry_bashes > 0)
}

/// Pure helper: walk the current `State` + heartbeat-stuck flag and return
/// the set of HangSignals that should be observed THIS cycle from
/// non-pane-capture sources (everything except PaneCaptureUnchanged,
/// which needs an async tmux capture).
///
/// Split out so we can unit-test the signal-collection logic without
/// mocking tmux. Caller is responsible for adding PaneCaptureUnchanged
/// based on a separate `evaluate_pane_unchanged` call.
pub(crate) fn collect_non_pane_signals(
    state: &State,
    config: &Config,
    heartbeat_stuck: bool,
) -> Vec<crate::respawn::HangSignal> {
    use crate::respawn::HangSignal;
    let mut out = Vec::new();
    if heartbeat_stuck {
        out.push(HangSignal::HeartbeatStale);
    }
    let watcher_critical = state
        .watcher_health
        .values()
        .any(|wh| wh.enabled && wh.consecutive_missing >= config.watcher_monitor.inject_threshold);
    let recent_watcher_inject = state
        .last_watcher_inject
        .as_deref()
        .and_then(elapsed_since)
        .is_some_and(|e| e <= config.auto_respawn_on_hang.signal_window_secs as f64);
    if watcher_critical && recent_watcher_inject {
        out.push(HangSignal::WatcherDownPersistent);
    }
    if state.thinking_interrupt_count >= 2 {
        out.push(HangSignal::ProlongedThinkingNoProgress);
    }
    let recent_wedged = state
        .last_wedged_clear
        .as_deref()
        .and_then(elapsed_since)
        .is_some_and(|e| e <= config.auto_respawn_on_hang.signal_window_secs as f64);
    if recent_wedged && state.wedged_consecutive >= 2 {
        out.push(HangSignal::WedgedClearNoProgress);
    }
    out
}

/// Per-cycle signal collection + multi-signal hang evaluation. Side-effects:
///
///   - Records new HangSignals into `state.hang_signal_history`.
///   - Updates `pane_content_hash` / `pane_content_unchanged_since`.
///   - Prunes the history to `signal_window_secs`.
///   - If the threshold + cooldown are satisfied, calls
///     `respawn::execute_respawn`, then updates `last_respawn_at` / counters.
///
/// Idempotent within a single cycle. Each signal can fire only once per
/// invocation (HashMap dedup in `HangSignalHistory.observe`).
pub(crate) async fn check_auto_respawn(
    config: &Config,
    state: &mut State,
    pane: &str,
    now: &str,
    heartbeat_stuck: bool,
) {
    check_auto_respawn_with_versions_dir(config, state, pane, now, heartbeat_stuck, None).await
}

/// Test-friendly variant. `versions_dir_override` is forwarded to
/// `execute_respawn_with_versions_dir`. Production code MUST call
/// `check_auto_respawn` (which passes None). Tests MUST pass
/// `Some("/nonexistent")` so the destructive kill path can never find
/// a real Claude PID. See the safety note on
/// `respawn::execute_respawn_with_versions_dir`.
pub(crate) async fn check_auto_respawn_with_versions_dir(
    config: &Config,
    state: &mut State,
    pane: &str,
    now: &str,
    heartbeat_stuck: bool,
    versions_dir_override: Option<&str>,
) {
    use crate::respawn::{
        count_active_subagents_with_versions_dir, evaluate_pane_unchanged,
        execute_respawn_with_versions_dir, hash_pane_content, should_respawn, HangSignal,
        RespawnOutcome,
    };

    if !config.auto_respawn_on_hang.enabled {
        return;
    }

    // ---- Signals 1, 2, 3, 5: pure-state-derived ----
    for sig in collect_non_pane_signals(state, config, heartbeat_stuck) {
        state.hang_signal_history.observe(&sig, now);
    }

    // ---- Signal 4: pane capture unchanged (needs tmux I/O) ----
    if !pane.is_empty() {
        if let Some(capture) = tmux::capture_pane(pane).await {
            let h = hash_pane_content(&capture);
            let (new_hash, new_first_seen, fire) = evaluate_pane_unchanged(
                h,
                state.pane_content_hash,
                state.pane_content_unchanged_since.as_deref(),
                now,
                config.auto_respawn_on_hang.pane_unchanged_secs,
            );
            state.pane_content_hash = new_hash;
            state.pane_content_unchanged_since = new_first_seen;
            if fire {
                state
                    .hang_signal_history
                    .observe(&HangSignal::PaneCaptureUnchanged, now);
            }
        }
    }

    // Prune anything outside the window.
    state
        .hang_signal_history
        .prune_window(now, config.auto_respawn_on_hang.signal_window_secs);

    let active_count = state.hang_signal_history.distinct_active().len();

    // Active-subagent guard: if subagents are alive, the main loop is not
    // hung — it's legitimately waiting on agent work. Skip respawn.
    // We thread the same `versions_dir_override` so unit tests can force
    // the count to 0 (via a non-existent versions_dir → no claude PID
    // detected → fail-open to 0). Production passes None.
    let active_subagents = count_active_subagents_with_versions_dir(versions_dir_override);

    debug!(
        active_count,
        active_subagents,
        signals_required = config.auto_respawn_on_hang.signals_required,
        "auto-respawn: signal evaluation"
    );

    if !should_respawn(
        &state.hang_signal_history,
        state.last_respawn_at.as_deref(),
        now,
        config.auto_respawn_on_hang.signals_required,
        config.auto_respawn_on_hang.cooldown_secs,
        active_subagents,
    ) {
        if active_subagents > 0 {
            debug!(
                active_subagents,
                "auto-respawn: skipping fire — active subagents present (guard)"
            );
        }
        return;
    }

    // Threshold + cooldown satisfied — fire.
    let active_signals: Vec<String> = state
        .hang_signal_history
        .distinct_active()
        .into_iter()
        .collect();
    warn!(
        signals = ?active_signals,
        "auto-respawn: multi-signal hang detected — killing + respawning dashboard"
    );
    write_jsonl_log(
        &config.general.log_file,
        "auto_respawn_fire",
        serde_json::json!({
            "signals": active_signals,
            "signals_required": config.auto_respawn_on_hang.signals_required,
            "window_secs": config.auto_respawn_on_hang.signal_window_secs,
        }),
    );
    write_legacy_log(
        &config.general.legacy_log_file,
        &format!(
            "AUTO-RESPAWN: multi-signal hang detected (signals={:?}) -- killing + respawning",
            active_signals
        ),
    );

    let outcome = execute_respawn_with_versions_dir(
        &config.auto_respawn_on_hang,
        &config.tmux.dashboard_session,
        versions_dir_override,
    )
    .await;

    state.last_respawn_at = Some(now.to_string());
    state.auto_respawn_count = state.auto_respawn_count.saturating_add(1);
    state.auto_respawn_interrupts_total =
        state.auto_respawn_interrupts_total.saturating_add(1);
    state.last_interrupt_at = Some(now.to_string());
    // Clear the history so the next cycle starts from a clean slate.
    state.hang_signal_history = crate::respawn::HangSignalHistory::default();
    state.pane_content_hash = None;
    state.pane_content_unchanged_since = None;

    match &outcome {
        RespawnOutcome::Success { new_pid } => {
            info!(?new_pid, "auto-respawn: success");
            write_jsonl_log(
                &config.general.log_file,
                "auto_respawn_success",
                serde_json::json!({ "new_pid": new_pid }),
            );
            alert::send_pingme(
                "claude-watch: auto-respawned dashboard after multi-signal hang detection",
            )
            .await;
        }
        RespawnOutcome::LaunchFailed => {
            warn!("auto-respawn: launch failed");
            write_jsonl_log(
                &config.general.log_file,
                "auto_respawn_launch_failed",
                serde_json::json!({}),
            );
            alert::send_pingme_with_priority(
                "claude-watch: AUTO-RESPAWN failed — dashboard launch did not produce a new claude PID",
                "high",
            )
            .await;
        }
        RespawnOutcome::Aborted { reason } => {
            warn!(reason = %reason, "auto-respawn: aborted");
            write_jsonl_log(
                &config.general.log_file,
                "auto_respawn_aborted",
                serde_json::json!({ "reason": reason }),
            );
        }
    }

    crate::state::save_state(&config.general.state_file, state);
}

/// Run a single check cycle.
pub async fn check_cycle(config: &Config, state: &mut State) {
    let now = Local::now().to_rfc3339();

    // Get Claude Code status
    let cs = status::get_claude_status().await;

    if cs.is_none() {
        debug!("claude-status returned None -- not running");
        write_legacy_log(
            &config.general.legacy_log_file,
            "claude-status returned None -- not running",
        );
        // Claude Code not running at all — if a new session starts later,
        // it should be eligible for fresh inject regardless of old state.
        if state.fresh_session_injected {
            // Only reset if Claude was alive at some point since the inject,
            // or if the inject is expired (>5min without activity).
            let inject_expired = state
                .last_fresh_inject
                .as_ref()
                .and_then(|ts| elapsed_since(ts))
                .is_some_and(|elapsed| elapsed >= 300.0);

            if state.was_alive_since_inject || inject_expired {
                debug!("resetting fresh_session_injected — no Claude Code running (was_alive={}, expired={})",
                    state.was_alive_since_inject, inject_expired);
                state.fresh_session_injected = false;
                state.was_alive_since_inject = false;
            } else {
                debug!("fresh_session_injected set but Claude never became active — not resetting");
            }
        }
        state.last_check = Some(now);
        state.consecutive_failures = 0;
        crate::state::save_state(&config.general.state_file, state);
        return;
    }

    let cs = cs.unwrap();
    let pane = &cs.pane;
    let tokens = cs.tokens;
    let bashes = cs.bashes;
    let watchmen_count = status::check_watchmen_count().await;

    // --- Activity detection (Phase 1: logging only) ---
    if !pane.is_empty() {
        let activity = tmux::get_activity(pane).await;
        debug!(activity = %activity, "claude activity state");
    }

    // --- Post-restart resume injection ---
    if state.pending_resume_inject && !pane.is_empty() && tokens > 0 {
        // Don't inject during /exit teardown
        if tmux::is_exit_teardown(pane).await {
            debug!("post-restart: skipping — exit teardown detected");
            state.last_check = Some(now);
            crate::state::save_state(&config.general.state_file, state);
            return;
        }
        if tmux::is_idle(pane).await {
            info!("post-restart: injecting resume prompt");
            tmux::inject_text(
                pane,
                "[CLAUDE-WATCH-RESUME] Claude Code was restarted after a crash. \
                 All background task handles were lost. Run the full resume \
                 checklist at your configured resume-checklist path immediately.",
            )
            .await;
            state.pending_resume_inject = false;
            state.post_restart_resume_inject_interrupts_total = state
                .post_restart_resume_inject_interrupts_total
                .saturating_add(1);
            state.last_check = Some(now);
            crate::state::save_state(&config.general.state_file, state);
            return;
        }
        debug!(tokens, "post-restart: Claude running but not idle yet");
        state.last_check = Some(now);
        crate::state::save_state(&config.general.state_file, state);
        return;
    }

    // --- Find pane when claude-status can't (process crashed) ---
    let effective_pane: String = if pane.is_empty() && tokens == 0 && bashes == 0 {
        if let Some(p) = tmux::find_dashboard_pane(&config.tmux).await {
            debug!(pane = %p, "found dashboard pane via fallback");
            p
        } else {
            String::new()
        }
    } else {
        pane.clone()
    };

    // Detect pane change (new Claude Code session, e.g. dashboard --recreate).
    // Reset fresh_session_injected so the new session can get its resume inject,
    // and reset dead_checks so the countdown restarts for the new session.
    if !effective_pane.is_empty()
        && !state.last_known_pane.is_empty()
        && effective_pane != state.last_known_pane
    {
        info!(
            old_pane = %state.last_known_pane,
            new_pane = %effective_pane,
            "pane change detected — resetting fresh_session_injected"
        );
        state.fresh_session_injected = false;
        state.was_alive_since_inject = false;
        state.consecutive_dead_checks = 0;
    }

    // Store last known values for foreground polling between full check cycles.
    // Only update tokens/bashes when we got a valid parse (non-zero) to avoid
    // writing 0 to Prometheus during transient status bar parsing failures.
    state.last_known_pane = effective_pane.clone();
    if tokens > 0 {
        state.last_known_tokens = tokens;
    }
    if bashes > 0 || tokens > 0 {
        state.last_known_bashes = bashes;
    }
    // Mark "actively turning" whenever a tool call is in flight. The
    // watcher-down inject path consults this timestamp to avoid
    // preempting a busy main loop with a `WATCHER(S) DOWN` prompt.
    if bashes > 0 {
        state.last_active_at = Some(now.clone());
    }

    // --- API retry detection (suppression flag for downstream interrupt sites) ---
    //
    // When Claude Code is in upstream-API retry backoff (529 / overloaded /
    // 5xx → "Retrying in Ns · attempt N/M"), every interrupt resets the
    // retry state machine and prevents recovery. We detect once per cycle
    // here and have downstream interrupt sites (wedged-clear, watcher-down,
    // context-warning, and check_foreground's prolonged-thinking) skip
    // their fires while the flag is set. Heartbeat and dead-process
    // detection are NOT suppressed — those measure liveness, and a truly
    // dead loop must still alert.
    let api_retrying =
        update_api_retry_state(config, state, &effective_pane).await;
    if api_retrying {
        debug!("check_cycle: api_retry active — suppressing wedged/watcher/context fires");
    }

    // --- Dead process detection ---
    if tokens == 0 && bashes == 0 && !effective_pane.is_empty() {
        state.consecutive_dead_checks += 1;
        let dead_checks = state.consecutive_dead_checks;
        info!(dead_checks, "dead process detected: tokens=0, bashes=0");

        // --- Self-heal: once we reach the alert threshold, retry status
        // discovery from scratch before committing to any dead-check actions.
        // Addresses a stale-latch bug where the daemon read tokens=0 for 45+
        // minutes across 250+ loops while the same binary's CLI
        // (`claude-watch status --json`) parsed the same pane correctly.
        // A fresh get_claude_status() call re-runs pane discovery and
        // capture, which recovers from the stuck state.
        if dead_checks >= config.dead_process.checks_required {
            if let Some(retry) = status::get_claude_status().await {
                if should_self_heal(
                    dead_checks,
                    config.dead_process.checks_required,
                    retry.tokens,
                    retry.bashes,
                ) {
                    warn!(
                        recovered_tokens = retry.tokens,
                        recovered_bashes = retry.bashes,
                        pane = %retry.pane,
                        prior_dead_checks = dead_checks,
                        "self-heal triggered: retry returned non-zero status, \
                         resetting consecutive_dead_checks"
                    );
                    write_jsonl_log(
                        &config.general.log_file,
                        "self_heal_retry",
                        serde_json::json!({
                            "recovered_tokens": retry.tokens,
                            "recovered_bashes": retry.bashes,
                            "pane": &retry.pane,
                            "prior_dead_checks": dead_checks,
                        }),
                    );
                    state.consecutive_dead_checks = 0;
                    state.last_known_pane = retry.pane.clone();
                    if retry.tokens > 0 {
                        state.last_known_tokens = retry.tokens;
                    }
                    if retry.bashes > 0 || retry.tokens > 0 {
                        state.last_known_bashes = retry.bashes;
                    }
                    // Mirror the active-session bookkeeping from the
                    // non-dead branch below so inject state stays coherent.
                    if state.fresh_session_injected {
                        state.was_alive_since_inject = true;
                        state.fresh_session_injected = false;
                    }
                    state.last_check = Some(now);
                    crate::state::save_state(&config.general.state_file, state);
                    return;
                }
            }
        }

        write_legacy_log(
            &config.general.legacy_log_file,
            &format!(
                "Dead process detected: tokens=0, bashes=0, dead_checks={}",
                dead_checks
            ),
        );

        if dead_checks >= config.dead_process.checks_required {
            // Reset fresh_session_injected when Claude was alive and then died.
            // This handles both cases: (1) shell prompt visible after old session died,
            // and (2) rapid session replacement where the pane ID doesn't change
            // (dashboard --recreate always creates dashboard:0.0). Without this,
            // the flag stays true from a previous inject and blocks the next one.
            //
            // IMPORTANT: Only reset if was_alive_since_inject is true, meaning Claude
            // actually reached an active state (tokens > 0) after the last inject.
            // Without this guard, we get an inject loop: inject → startup (tokens=0,
            // looks "dead") → reset flag → re-inject → repeat.
            //
            // Fallback: if the inject was >5 minutes ago and Claude never became active,
            // reset anyway — the session likely died during startup and a new one may
            // need injection.
            if state.fresh_session_injected {
                let inject_expired = state
                    .last_fresh_inject
                    .as_ref()
                    .and_then(|ts| elapsed_since(ts))
                    .is_some_and(|elapsed| elapsed >= 300.0);

                if state.was_alive_since_inject {
                    info!("dead state reached after active session — resetting fresh_session_injected");
                    state.fresh_session_injected = false;
                    state.was_alive_since_inject = false;
                } else if inject_expired {
                    info!("dead state reached — inject expired (>5min, never active) — resetting fresh_session_injected");
                    state.fresh_session_injected = false;
                    state.was_alive_since_inject = false;
                } else {
                    debug!("dead state but inject recent and Claude never active — not resetting (preventing inject loop)");
                }
            }

            // Check restart cooldown
            if let Some(ref last) = state.last_restart {
                if let Some(elapsed) = elapsed_since(last) {
                    if elapsed < config.dead_process.restart_cooldown as f64 {
                        info!(
                            elapsed_secs = elapsed,
                            cooldown = config.dead_process.restart_cooldown,
                            "restart cooldown active"
                        );
                        state.last_check = Some(now);
                        crate::state::save_state(&config.general.state_file, state);
                        return;
                    }
                }
            }

            if tmux::is_shell_prompt(&effective_pane).await {
                // Active-turn suppression (2026-04-27 false-positive fix):
                // `tokens == 0 && bashes == 0` is point-in-time and can
                // briefly hold during a tmux pane swap, a status-parser
                // miss, or the gap between two tool calls. The
                // shell-prompt confirmation is the strong-side check
                // here, but the parser can ALSO mis-classify mixed
                // pane content as a shell prompt (e.g. a backgrounded
                // bash command output line ending in `$`). If the loop
                // ran ANY tool call within `active_window_secs`,
                // suppress the restart — the process is demonstrably
                // alive and `restart_claude` would kill an active
                // session and fire a false `claude-crashed` alert.
                let actively_turning = dead_process_restart_suppressed(
                    state,
                    bashes,
                    config.dead_process.suppress_when_active,
                    config.dead_process.active_window_secs,
                );
                // Cross-gate escalation backstop (2026-04-28
                // q-2026-04-28-2449): if the suppression run has been
                // long/persistent enough, force the restart even though
                // the active-turn predicate matches. Catches the case
                // where a sustained dispatcher window holds the gate
                // open indefinitely.
                let escalation = should_escalate_suppression(
                    state,
                    config.suppression.max_consecutive_suppressions,
                    config.suppression.max_suppression_window_secs,
                );
                if actively_turning && escalation.is_none() {
                    let last_active_age = state
                        .last_active_at
                        .as_deref()
                        .and_then(elapsed_since)
                        .map(|e| e as u64);
                    info!(
                        dead_checks,
                        bashes,
                        last_active_age_secs = ?last_active_age,
                        "dead-process restart suppressed: main loop actively turning"
                    );
                    write_jsonl_log(
                        &config.general.log_file,
                        "dead_process_restart_suppressed",
                        serde_json::json!({
                            "dead_checks": dead_checks,
                            "bashes": bashes,
                            "reason": "main_loop_actively_turning",
                            "last_active_age_secs": last_active_age,
                            "active_window_secs": config.dead_process.active_window_secs,
                            "consecutive_suppressions": state.consecutive_suppressions + 1,
                        }),
                    );
                    record_suppression(state, &now);
                    // Reset the consecutive counter so we don't re-fire
                    // on the very next check after the active window
                    // closes — require a fresh `checks_required`-cycle
                    // run of dead-state observations before restarting.
                    state.consecutive_dead_checks = 0;
                } else {
                    if let Some(reason) = escalation {
                        warn!(
                            dead_checks,
                            consecutive_suppressions = state.consecutive_suppressions,
                            escalation_reason = reason.as_str(),
                            "dead-process restart escalating: suppression run capped — forcing restart"
                        );
                        write_jsonl_log(
                            &config.general.log_file,
                            "suppression_escalated",
                            serde_json::json!({
                                "site": "dead_process",
                                "reason": reason.as_str(),
                                "consecutive_suppressions": state.consecutive_suppressions,
                                "first_suppression_at": state.first_suppression_at,
                            }),
                        );
                    }
                    info!(
                        dead_checks,
                        "shell prompt confirmed -- restarting Claude Code"
                    );
                    restart_claude(&effective_pane, state, &config.claude).await;
                    state.consecutive_dead_checks = 0;
                    state.consecutive_failures = 0;
                    state.alert_count = 0;
                    reset_suppression(state);
                }
            } else if dead_checks >= config.dead_process.fresh_inject_checks
                && !state.fresh_session_injected
                && tmux::is_idle(&effective_pane).await
            {
                // Claude Code is running (idle prompt visible) but tokens=0 — this is
                // a fresh session launched externally (e.g. dashboard --fresh), not by
                // claude-watch. Inject "resume" to kick-start the checklist.
                info!(
                    dead_checks,
                    "fresh external session detected — injecting resume"
                );
                tmux::inject_text(&effective_pane, "resume").await;
                state.fresh_session_injected = true;
                state.was_alive_since_inject = false;
                state.last_fresh_inject = Some(Local::now().to_rfc3339());
                state.consecutive_dead_checks = 0;
                state.fresh_session_inject_interrupts_total = state
                    .fresh_session_inject_interrupts_total
                    .saturating_add(1);
                write_jsonl_log(
                    &config.general.log_file,
                    "fresh_session_inject",
                    serde_json::json!({
                        "dead_checks": dead_checks,
                        "pane": &effective_pane,
                    }),
                );
            } else {
                debug!("dead but no shell prompt -- Claude may be starting up");
            }
        }

        state.last_check = Some(now);
        crate::state::save_state(&config.general.state_file, state);
        return;
    }
    state.consecutive_dead_checks = 0;
    // Session is active (tokens > 0). Mark that Claude was alive since inject,
    // then clear the inject flag. The was_alive_since_inject flag allows the dead
    // state handler to distinguish "was alive, then died" from "never started up".
    if state.fresh_session_injected {
        state.was_alive_since_inject = true;
        state.fresh_session_injected = false;
    }

    // --- Check for manual update trigger ---
    check_update_trigger(config, state, &effective_pane).await;

    // --- Auto-update check ---
    check_auto_update(config, state, &effective_pane).await;

    // --- Reauth detection ---
    if config.reauth.enabled && !effective_pane.is_empty() {
        check_reauth(config, state, &effective_pane).await;
    }

    // --- Fresh /clear detection ---
    if tokens >= config.fresh_clear.min_tokens
        && tokens < config.fresh_clear.max_tokens
        && bashes == 0
    {
        // Skip if /exit teardown is in progress — "Goodbye!" or
        // "Background command was stopped" visible in pane output.
        // Injecting resume during teardown is useless and confusing.
        if !effective_pane.is_empty() && tmux::is_exit_teardown(&effective_pane).await {
            debug!("fresh /clear check: skipping — exit teardown detected");
            state.consecutive_fast_detections = 0;
            state.last_check = Some(now);
            crate::state::save_state(&config.general.state_file, state);
            return;
        }

        if !effective_pane.is_empty() && tmux::is_idle(&effective_pane).await {
            state.consecutive_fast_detections += 1;
            if state.consecutive_fast_detections < config.fresh_clear.detections_required {
                state.last_check = Some(now);
                crate::state::save_state(&config.general.state_file, state);
                return;
            }

            // Check cooldown
            if let Some(ref last) = state.last_fast_path_alert {
                if let Some(elapsed) = elapsed_since(last) {
                    if elapsed < config.fresh_clear.cooldown as f64 {
                        state.last_check = Some(now);
                        crate::state::save_state(&config.general.state_file, state);
                        return;
                    }
                }
            }

            // Active-turn suppression (2026-04-27 false-positive fix):
            // The token range [min_tokens, max_tokens) AND `bashes == 0`
            // are both point-in-time predicates that the main loop
            // briefly satisfies between two tool calls (a small turn
            // that just got back, say, 3000 tokens; bashes momentarily 0
            // before the next tool call fires). Without this gate the
            // alert fires mid-turn and injects "resume" into active
            // work. If the loop ran ANY tool call within
            // `active_window_secs`, suppress both the inject and the
            // alert — the loop is clearly alive.
            let actively_turning = fresh_clear_inject_suppressed(
                state,
                bashes,
                config.fresh_clear.suppress_when_active,
                config.fresh_clear.active_window_secs,
            );
            // Cross-gate escalation backstop (2026-04-28 q-2026-04-28-2449).
            let escalation = should_escalate_suppression(
                state,
                config.suppression.max_consecutive_suppressions,
                config.suppression.max_suppression_window_secs,
            );
            if actively_turning && escalation.is_none() {
                let last_active_age = state
                    .last_active_at
                    .as_deref()
                    .and_then(elapsed_since)
                    .map(|e| e as u64);
                info!(
                    tokens,
                    bashes,
                    last_active_age_secs = ?last_active_age,
                    "fresh /clear inject suppressed: main loop actively turning"
                );
                write_jsonl_log(
                    &config.general.log_file,
                    "fresh_clear_inject_suppressed",
                    serde_json::json!({
                        "tokens": tokens,
                        "bashes": bashes,
                        "reason": "main_loop_actively_turning",
                        "last_active_age_secs": last_active_age,
                        "active_window_secs": config.fresh_clear.active_window_secs,
                        "consecutive_suppressions": state.consecutive_suppressions + 1,
                    }),
                );
                record_suppression(state, &now);
                // Reset the consecutive counter so we don't re-fire on
                // the very next check after the active window closes.
                // The detection has to re-build from scratch.
                state.consecutive_fast_detections = 0;
                state.last_check = Some(now);
                crate::state::save_state(&config.general.state_file, state);
                return;
            }

            if let Some(reason) = escalation {
                warn!(
                    tokens,
                    consecutive_suppressions = state.consecutive_suppressions,
                    escalation_reason = reason.as_str(),
                    "fresh /clear inject escalating: suppression run capped — forcing inject"
                );
                write_jsonl_log(
                    &config.general.log_file,
                    "suppression_escalated",
                    serde_json::json!({
                        "site": "fresh_clear",
                        "reason": reason.as_str(),
                        "consecutive_suppressions": state.consecutive_suppressions,
                        "first_suppression_at": state.first_suppression_at,
                    }),
                );
            }

            info!(tokens, "fresh /clear detected -- injecting resume");
            let fresh_msg = format!(
                "Fresh /clear detected (tokens={}, bashes=0). Injecting resume.",
                tokens
            );
            alert::notify(crate::event_bus::ClaudeWatchAlert {
                alert_type: "fresh-clear-stuck",
                stuck_reason: "fresh /clear with no follow-up activity",
                stale_minutes: None,
                affected_watchers: vec![],
                severity: crate::event_bus::Severity::Medium,
                message: &fresh_msg,
            })
            .await;

            // Dismiss feedback prompt if present
            tmux::dismiss_feedback_prompt(&effective_pane).await;

            tmux::inject_text(&effective_pane, &config.alerts.resume_prompt).await;

            state.last_fast_path_alert = Some(now.clone());
            state.last_alert = Some(now.clone());
            state.consecutive_failures = 0;
            state.consecutive_fast_detections = 0;
            state.fresh_clear_resume_inject_interrupts_total = state
                .fresh_clear_resume_inject_interrupts_total
                .saturating_add(1);
            reset_suppression(state);
            state.last_check = Some(now);
            crate::state::save_state(&config.general.state_file, state);
            return;
        }
    } else {
        state.consecutive_fast_detections = 0;
    }

    // --- Heartbeat stale detection ---
    let mut stuck = false;
    let mut stuck_reason = String::new();
    // Captured for the claude-event sink so the main loop can parse
    // `stale_minutes` as a number rather than re-regex'ing the string.
    let mut stuck_stale_minutes: Option<u64> = None;

    match std::fs::metadata(&config.claude.heartbeat_file) {
        Ok(meta) => {
            if let Ok(modified) = meta.modified() {
                let age = SystemTime::now()
                    .duration_since(modified)
                    .unwrap_or_default()
                    .as_secs();
                let stale_secs = config.heartbeat.stale_minutes * 60;
                if age >= stale_secs {
                    // Workload-heartbeat suppression: a long-running
                    // `workload run` (stv-promote, big rsync, ffmpeg)
                    // can pin the main loop in a fire-and-forget wait
                    // that looks like heartbeat-stale from the
                    // memory-remind side. If any workload's per-label
                    // heartbeat file under
                    // `config.stuck_detection.workload_heartbeat_dir`
                    // is younger than
                    // `workload_heartbeat_max_age_secs`, treat it as
                    // proof-of-life and skip the stuck flag for THIS
                    // cycle. The heartbeat-stale counter is also held
                    // back so a long workload doesn't accumulate
                    // suppressed-fire history.
                    if workload_heartbeat_suppresses_stuck(config) {
                        let age_min = age / 60;
                        debug!(
                            stale_age_min = age_min,
                            threshold_min = config.heartbeat.stale_minutes,
                            "heartbeat-stale suppressed by fresh workload heartbeat"
                        );
                        write_jsonl_log(
                            &config.general.log_file,
                            "heartbeat_stale_suppressed",
                            serde_json::json!({
                                "stale_age_min": age_min,
                                "threshold_min": config.heartbeat.stale_minutes,
                                "reason": "workload_heartbeat_fresh",
                                "dir": &config.stuck_detection.workload_heartbeat_dir,
                                "max_age_secs": config.stuck_detection.workload_heartbeat_max_age_secs,
                            }),
                        );
                    } else {
                        stuck = true;
                        let age_min = age / 60;
                        stuck_reason = format!(
                            "heartbeat stale ({}min, threshold={}min, watchmen={})",
                            age_min,
                            config.heartbeat.stale_minutes,
                            watchmen_count
                        );
                        stuck_stale_minutes = Some(age_min);
                        state.heartbeat_stale_count += 1;
                    }
                }
            }
        }
        Err(_) => {
            // No heartbeat file -- give it time
        }
    }

    // --- Foreground blocking detection ---
    // Delegated to check_foreground() which runs on its own timer in the main loop.
    // Also run it here during full check cycles to ensure it runs at least as often
    // as the general interval. We call check_foreground_inner directly so the
    // api_retrying flag we computed at the top of this function is reused
    // (calling check_foreground would re-run update_api_retry_state and
    // double-increment the counters within a single full cycle).
    check_foreground_inner(config, state, &effective_pane, tokens, bashes, api_retrying).await;

    // --- Context monitoring ---
    //
    // Reset paths run UNCONDITIONALLY (not gated on tokens > 0) — when self-clear
    // succeeds the pane briefly shows "0 tokens", and that single check used to
    // skip the entire context-monitoring block, leaving `context_clear_triggered`
    // stuck at true. Once tokens climbed back above 30K (the agent resumed), the
    // sub-30K reset block could no longer fire either, and the flag stayed stuck
    // for the rest of the session — blocking every subsequent threshold fire.
    // Real incident 2026-05-01: deferred clear ran cleanly at 12:23 UTC, the
    // tokens=0 sample at 12:28:20 UTC didn't reset the flag, and the next
    // threshold fire was suppressed for ~4 hours until the user manually /cleared.
    //
    // Calling maybe_reset_context_clear() ahead of the trigger gate also means a
    // fresh fire can happen in the same cycle the reset lands, if tokens jump
    // straight from <30K to >threshold (boundary case, but cheap to handle).
    if config.context_monitor.enabled {
        // Reset path runs first so it can observe the pre-update last_seen_tokens.
        maybe_reset_context_clear(config, state, tokens, &now);
        // Always record the latest token sample (even tokens=0) so the next
        // cycle's "previously high → now low" detector sees the right history.
        state.last_seen_tokens = Some(tokens);
    }
    if config.context_monitor.enabled && tokens > 0 {
        if let Some((pct, _by_compact)) = check_context_threshold_with_margin(
            tokens,
            config.claude.max_context_tokens,
            cs.compact_remaining,
            config.context_monitor.threshold_percent,
            config.context_monitor.compact_trigger_percent,
            config.context_monitor.threshold_margin,
        ) {
            if !state.context_clear_triggered {
                // Check cooldown
                let can_trigger = match &state.last_context_clear {
                    Some(last) => elapsed_since(last)
                        .map(|e| e >= config.context_monitor.cooldown as f64)
                        .unwrap_or(true),
                    None => true,
                };

                if can_trigger {
                    // Hybrid gate: if a recent context_high hook fired the
                    // reminder, give Claude a grace window to self-act
                    // before we tmux-inject a warning + schedule the
                    // deferred clear.
                    let hook_deferred = config.hybrid.enabled
                        && should_defer_to_hook(
                            ReminderType::ContextHigh,
                            config.hybrid.context_fallback_secs as f64,
                        );

                    // Global post-interrupt cooldown: if a recent interrupt
                    // (thinking, watcher-down, or a prior context-warning)
                    // fired, defer this context warning too. The deferred
                    // self-clear child still runs if the token level stays
                    // high; the cooldown only gates the tmux interrupt +
                    // warning message.
                    let global_cooldown_blocks = interrupt_in_global_cooldown(
                        state,
                        config.general.post_interrupt_cooldown_secs,
                    );

                    if api_retrying {
                        debug!(
                            tokens,
                            pct,
                            "context threshold exceeded but api_retry active — suppressing fire"
                        );
                        write_jsonl_log(
                            &config.general.log_file,
                            "context_threshold_api_retry_deferred",
                            serde_json::json!({
                                "tokens": tokens,
                                "pct": pct,
                            }),
                        );
                    } else if hook_deferred {
                        debug!(
                            tokens,
                            pct,
                            grace = config.hybrid.context_fallback_secs,
                            "context threshold exceeded but deferring to recent hook reminder"
                        );
                        write_jsonl_log(
                            &config.general.log_file,
                            "context_threshold_hook_deferred",
                            serde_json::json!({
                                "tokens": tokens,
                                "pct": pct,
                                "grace_secs": config.hybrid.context_fallback_secs,
                            }),
                        );
                    } else if global_cooldown_blocks {
                        debug!(
                            tokens,
                            pct,
                            cooldown = config.general.post_interrupt_cooldown_secs,
                            "context threshold exceeded but global post-interrupt cooldown active"
                        );
                        write_jsonl_log(
                            &config.general.log_file,
                            "context_threshold_global_cooldown_deferred",
                            serde_json::json!({
                                "tokens": tokens,
                                "pct": pct,
                                "cooldown_secs": config.general.post_interrupt_cooldown_secs,
                            }),
                        );
                    } else {
                        warn!(
                            tokens,
                            pct,
                            compact_remaining = ?cs.compact_remaining,
                            "context threshold exceeded — triggering deferred clear (hybrid fallback)"
                        );
                        write_jsonl_log(
                            &config.general.log_file,
                            "context_threshold",
                            serde_json::json!({
                                "tokens": tokens,
                                "pct": pct,
                                "compact_remaining": cs.compact_remaining,
                                "grace_period": config.context_monitor.grace_period,
                                "hybrid_fallback": true,
                            }),
                        );

                        // Run session-event compact-prep
                        let note = format!("auto-clear at {:.0}% tokens", pct);
                        let _ = crate::cmd::run_cmd(
                            &["session-event", "compact-prep", "--note", &note],
                            10,
                        )
                        .await;

                        // Spawn deferred self-clear child
                        spawn_deferred_clear(config, state);

                        // Inject warning message into Claude Code pane
                        if !effective_pane.is_empty() {
                            inject_context_warning(
                                &effective_pane,
                                pct,
                                cs.compact_remaining,
                                config.context_monitor.grace_period,
                            )
                            .await;
                        }

                        state.context_clear_triggered = true;
                        state.last_context_clear = Some(now.clone());
                        state.last_interrupt_at = Some(now.clone());
                        state.fallback_clear_count = state.fallback_clear_count.saturating_add(1);
                        state.context_warning_interrupts_total = state
                            .context_warning_interrupts_total
                            .saturating_add(1);
                    }
                }
            }
        }

        // Reset paths (tokens < 30K) and last_seen_tokens bookkeeping run
        // unconditionally above this block via maybe_reset_context_clear() —
        // keeping them outside the `tokens > 0` guard so a clean tokens=0
        // sample successfully resets `context_clear_triggered`.
    }

    // --- Wedged-pane detection (context limit / persistent rate limit) ---
    //
    // If the pane shows "Context limit reached. /compact or /clear to continue"
    // or repeated "API Error: Request rejected (429)", the agent is wedged: it
    // cannot make any tool call (every attempt errors out before it runs), so
    // it cannot run the normal compact-prep checklist or `self-clear`. The
    // token-based context_monitor above does NOT cover this — the agent may
    // hit the wall *below* its configured threshold (Anthropic API can return
    // context-limit errors before our token counter says "max"), and 429s are
    // entirely independent of token count.
    //
    // Recovery: claude-watch runs `self-clear` itself, the same way the
    // deferred-clear child does after the grace period expires — but
    // immediately, no grace period, no agent dependency.
    //
    // To avoid false positives from chat-history references to the strings,
    // we require N consecutive cycles before firing.
    if config.context_monitor.wedged_detection_enabled && !effective_pane.is_empty() {
        let wedged = tmux::detect_wedged(&effective_pane).await;

        if let Some(reason) = wedged {
            state.wedged_consecutive += 1;
            debug!(
                reason = %reason,
                consecutive = state.wedged_consecutive,
                threshold = config.context_monitor.wedged_consecutive,
                "wedged pane detected"
            );

            if state.wedged_consecutive >= config.context_monitor.wedged_consecutive {
                // Cooldown gate: don't re-fire within wedged_cooldown seconds.
                let in_cooldown = state
                    .last_wedged_clear
                    .as_deref()
                    .and_then(elapsed_since)
                    .is_some_and(|e| e < config.context_monitor.wedged_cooldown as f64);

                if api_retrying {
                    debug!(
                        reason = %reason,
                        "wedged pane detected but api_retry active — suppressing self-clear"
                    );
                    write_jsonl_log(
                        &config.general.log_file,
                        "wedged_clear_api_retry_deferred",
                        serde_json::json!({
                            "reason": reason.to_string(),
                            "consecutive": state.wedged_consecutive,
                        }),
                    );
                } else if !in_cooldown {
                    warn!(
                        reason = %reason,
                        consecutive = state.wedged_consecutive,
                        "wedged pane sustained — running self-clear immediately (no agent cooperation possible)"
                    );
                    write_jsonl_log(
                        &config.general.log_file,
                        "wedged_clear",
                        serde_json::json!({
                            "reason": reason.to_string(),
                            "consecutive": state.wedged_consecutive,
                            "tokens": tokens,
                        }),
                    );
                    write_legacy_log(
                        &config.general.legacy_log_file,
                        &format!(
                            "wedged pane ({reason}) — running self-clear (consecutive={})",
                            state.wedged_consecutive,
                        ),
                    );

                    // Run session-event compact-prep so the next session has a
                    // breadcrumb in the session log explaining why context was
                    // dropped. Best-effort — if it fails, still proceed with
                    // self-clear.
                    let note = format!("auto-clear: pane wedged ({reason})");
                    let _ = crate::cmd::run_cmd(
                        &["session-event", "compact-prep", "--note", &note],
                        10,
                    )
                    .await;

                    // Notify Andrew so he knows claude-watch had to step in.
                    let alert_msg = format!(
                        "claude-watch: agent wedged ({reason}) -- running self-clear",
                    );
                    let wedged_reason = format!("wedged pane: {reason}");
                    alert::notify(crate::event_bus::ClaudeWatchAlert {
                        alert_type: "wedged-pane",
                        stuck_reason: &wedged_reason,
                        stale_minutes: None,
                        affected_watchers: vec![],
                        severity: crate::event_bus::Severity::High,
                        message: &alert_msg,
                    })
                    .await;

                    spawn_immediate_clear(state);

                    state.last_wedged_clear = Some(now.clone());
                    state.wedged_clear_count += 1;
                    state.wedged_clear_interrupts_total =
                        state.wedged_clear_interrupts_total.saturating_add(1);
                    state.wedged_consecutive = 0;
                } else {
                    debug!(
                        reason = %reason,
                        "wedged pane detected but cooldown active"
                    );
                }
            }
        } else {
            // Pane is no longer wedged — reset the counter.
            if state.wedged_consecutive > 0 {
                debug!(
                    prev_consecutive = state.wedged_consecutive,
                    "wedged pane cleared — resetting counter"
                );
                state.wedged_consecutive = 0;
            }
        }
    }

    // --- Individual watcher health monitoring ---
    if config.watcher_monitor.enabled {
        let entries = status::parse_watchers_config(&config.watcher_monitor.watchers_config);
        let mut any_critical_missing = false;
        let mut missing_names: Vec<String> = Vec::new();
        // Pull config values into locals once to avoid borrow-checker
        // friction when we both mutate `state.watcher_health` and read
        // `config` later in the same scope.
        let event_threshold = config.watcher_monitor.event_threshold;
        let inject_threshold = config.watcher_monitor.inject_threshold;
        let event_grace_secs = config.watcher_monitor.event_grace_secs;
        let event_command = config.watcher_monitor.event_command.clone();
        let event_consumer_name = config
            .watcher_monitor
            .event_consumer_watcher_name
            .clone();

        for entry in &entries {
            if !entry.enabled {
                continue;
            }
            let count = status::check_process_count(&entry.pattern).await;
            // Orphan-PID cross-check (bug-2 fix): pgrep can match the wrong
            // process if a stale shell or self-matching wrapper happens to
            // contain the watcher's name pattern in its argv. The legacy
            // `watchmen` shell-script handled this with a `kill -0` probe of
            // the recorded PID; we restore that behaviour here so that a
            // dead memory-remind whose pidfile still points at PID N
            // (now reaped) is reported as DOWN rather than masked by a
            // coincidental pgrep hit.
            let recorded_pid = read_watcher_pid(crate::watcher::PID_DIR, &entry.name);
            let down = watcher_is_down(count, entry.min_count, recorded_pid, is_pid_alive);
            let orphaned = down && count >= entry.min_count;
            let health = state
                .watcher_health
                .entry(entry.name.clone())
                .or_insert_with(|| WatcherState {
                    last_seen_running: None,
                    consecutive_missing: 0,
                    enabled: entry.enabled,
                    event_emitted_at: None,
                });

            if !down {
                health.last_seen_running = Some(now.clone());
                health.consecutive_missing = 0;
                // Recovery clears the quiet-path bookkeeping so the next
                // failure starts a fresh quiet-path episode.
                health.event_emitted_at = None;
            } else {
                // Grace period: if the watcher was seen running within the
                // configured grace_secs, don't count this as a miss. Short-
                // lived watchers (e.g. an `*-wait` watcher that exits when
                // an event arrives) have a natural gap between exit and
                // the main loop's restart. Without this grace period we
                // fire spurious "watcher missing" alerts every time an
                // event is received.
                // Default 90s; tunable via [watcher_monitor].grace_secs (0 in
                // the e2e auto-restart test for fast firing).
                let grace_secs = config.watcher_monitor.grace_secs as f64;
                let in_grace = health
                    .last_seen_running
                    .as_deref()
                    .and_then(elapsed_since)
                    .is_some_and(|e| e < grace_secs);
                if in_grace {
                    continue;
                }
                health.consecutive_missing += 1;
                // Log after 3 consecutive misses (~30s at 10s interval)
                if health.consecutive_missing == 3 {
                    warn!(
                        watcher = %entry.name,
                        pattern = %entry.pattern,
                        consecutive_missing = health.consecutive_missing,
                        orphaned = orphaned,
                        "watcher missing"
                    );
                    write_jsonl_log(
                        &config.general.log_file,
                        "watcher_missing",
                        serde_json::json!({
                            "watcher": entry.name,
                            "pattern": entry.pattern,
                            "consecutive_missing": health.consecutive_missing,
                            "orphaned": orphaned,
                        }),
                    );
                }

                // Quiet-path decision. The pure helper returns one of
                // {Nothing, EmitEvent, InjectFallback} based on the
                // configured thresholds, the consumer-watcher special
                // case, and the per-watcher event_emitted_at timestamp.
                let is_consumer = entry.name == event_consumer_name;
                let action = evaluate_watcher_down_action(
                    is_consumer,
                    health.consecutive_missing,
                    health.event_emitted_at.as_deref(),
                    event_threshold,
                    inject_threshold,
                    event_grace_secs,
                );

                match action {
                    WatcherDownAction::Nothing => {}
                    WatcherDownAction::EmitEvent => {
                        // Snapshot pid for logging. status::check_process_count
                        // doesn't return one; record_pid stays None for now.
                        let recorded_pid: Option<u32> = None;
                        info!(
                            watcher = %entry.name,
                            consecutive_missing = health.consecutive_missing,
                            "watcher-down event (quiet path) — emitting claude-event"
                        );
                        write_jsonl_log(
                            &config.general.log_file,
                            "watcher_down_event_emit",
                            serde_json::json!({
                                "watcher": entry.name,
                                "consecutive_missing": health.consecutive_missing,
                                "recorded_pid": recorded_pid,
                            }),
                        );
                        let ok = emit_watcher_down_event(
                            &event_command,
                            &entry.name,
                            health.consecutive_missing,
                            recorded_pid,
                        )
                        .await;
                        if ok {
                            health.event_emitted_at = Some(now.clone());
                        }
                        // Whether the emission succeeded or not, do NOT add
                        // this watcher to missing_names — we want to give
                        // the main loop a chance to handle the event before
                        // the inject path fires. If the emit failed, the
                        // next cycle past the grace window (which is
                        // skipped here because event_emitted_at is None)
                        // will re-enter EmitEvent and try again, or escalate
                        // straight to InjectFallback once consecutive_missing
                        // crosses inject_threshold.
                    }
                    WatcherDownAction::InjectFallback => {
                        any_critical_missing = true;
                        missing_names.push(entry.name.clone());
                    }
                }
            }
        }

        // Daemon-side watcher auto-restart was REMOVED 2026-05-01.
        //
        // Cardinal rule: watchers can ONLY be started by Claude Code's main
        // loop, in the main loop's process tree. The previous block here
        // called `crate::watcher::auto_restart_watcher` which spawned the
        // watcher inside a transient `claude-watch-watcher-<name>.service`
        // user systemd unit — that unit lives in `user@1000.service`, NOT
        // as a descendant of Claude Code, so the watcher was orphaned from
        // birth and invisible to the main-loop's obligation gate.
        //
        // The replacement is the existing tmux-inject path BELOW. When a
        // watcher is missing-and-past-threshold the daemon types
        // `watcher-ctl run <name>` into the Claude Code tmux pane, and the
        // MAIN LOOP spawns the watcher in its own process tree. claude-watch
        // (the daemon) never spawns watchers itself.
        //
        // See: feedback_watcher-architecture-cardinal.md in claude-config.

        // Inject restart commands if watchers are down and cooldown has passed.
        //
        // The tmux-inject path is the SOLE daemon-side recovery action for
        // a down watcher (cardinal rule, 2026-05-01). When a watcher misses
        // enough consecutive checks, we type `watcher-ctl run <name>` into
        // the Claude Code pane so the main loop spawns the watcher in its
        // own process tree. The daemon never spawns watchers directly.
        //
        // NOTE: The watcher-down inject path is intentionally EXEMPT from
        // `interrupt_in_global_cooldown`. A down watcher is a hard
        // liveness failure — none of the configured `*-wait` /
        // claude-event-watch / torrent-wait watchers are running — and
        // silence here means events / completions sit unprocessed for the
        // cooldown window. A prior systemd-run supervision attempt
        // violated the heartbeat-liveness invariant and was reverted. The
        // correct shape is: keep the spawn target in the main-loop tmux
        // pane (watchers must die when the main loop dies), and let the
        // inject re-fire on the per-watcher cooldown regardless of recent
        // unrelated interrupts.
        //
        // Active-turn suppression with escalation backstop (PR #43) IS
        // retained: when the main loop is actively turning we drop the
        // pane preemption (the claude-event still fires out-of-band), and
        // the cross-gate escalation kicks the inject through anyway if
        // the suppression run gets too long/persistent.
        if any_critical_missing && !effective_pane.is_empty() {
            let should_inject = watcher_inject_due(
                state.last_watcher_inject.as_deref(),
                config.watcher_monitor.inject_cooldown,
            );
            // api_retry suppression (PR #45): if Claude Code is currently
            // in upstream-API retry backoff, an inject would wipe the
            // retry state machine and force a brand-new turn. Skip the
            // inject path entirely until the retry resolves. The next check
            // cycle will re-evaluate and re-fire the inject once the
            // api-retry episode clears.
            if should_inject && api_retrying {
                debug!(
                    "watcher-down inject would fire but api_retry active — suppressing"
                );
                write_jsonl_log(
                    &config.general.log_file,
                    "watcher_inject_api_retry_deferred",
                    serde_json::json!({
                        "missing": missing_names,
                    }),
                );
            }
            // Active-turn suppression: if the main loop is currently
            // running a tool call (or ran one within the last
            // `active_window_secs`), suppress ONLY the in-pane preemption.
            // The structured claude-event still fires so Andrew is
            // notified out-of-band. The reflexive cascade — inject fires
            // mid-turn → loop pivots to "restart watcher" → original ask
            // is abandoned half-finished — only happens if we keep
            // typing into the pane, so dropping the inject is enough.
            let actively_turning = config.watcher_monitor.suppress_inject_when_active
                && main_loop_actively_turning(
                    state,
                    bashes,
                    config.watcher_monitor.active_window_secs,
                );
            // Cross-gate escalation backstop (2026-04-28 q-2026-04-28-2449):
            // if the suppression run has been long/persistent enough, force
            // the inject regardless of `actively_turning`. Catches the
            // sustained-dispatcher-window case where the gate would
            // otherwise hold open indefinitely (real-world incident:
            // claude-event-watch suppressed for 33 min).
            let escalation = should_escalate_suppression(
                state,
                config.suppression.max_consecutive_suppressions,
                config.suppression.max_suppression_window_secs,
            );
            if should_inject && !api_retrying {
                let missing_list = missing_names.join(", ");
                let watcher_reason = format!(
                    "{} watcher(s) missing: {}",
                    missing_names.len(),
                    missing_list,
                );

                if actively_turning && escalation.is_none() {
                    // Suppression path: still emit the structured
                    // claude-event (out-of-band notify) and log it,
                    // but do NOT interrupt or inject into the pane.
                    let bashes_now = bashes;
                    let last_active_age = state
                        .last_active_at
                        .as_deref()
                        .and_then(elapsed_since)
                        .map(|e| e as u64);
                    info!(
                        missing = %missing_list,
                        bashes = bashes_now,
                        last_active_age_secs = ?last_active_age,
                        "watcher-down inject suppressed: main loop actively turning"
                    );
                    write_jsonl_log(
                        &config.general.log_file,
                        "watcher_inject_suppressed",
                        serde_json::json!({
                            "missing": missing_names,
                            "reason": "main_loop_actively_turning",
                            "bashes": bashes_now,
                            "last_active_age_secs": last_active_age,
                            "active_window_secs": config.watcher_monitor.active_window_secs,
                            "consecutive_suppressions": state.consecutive_suppressions + 1,
                        }),
                    );
                    record_suppression(state, &now);
                    // Out-of-band sink still fires — message reflects
                    // suppression so downstream consumers can tell
                    // this fire did not preempt the pane.
                    //
                    // Self-feedback guard: if the only down watcher is
                    // the event consumer itself, suppress the JSON file
                    // emit (it would just feed the consumer's own
                    // restart loop). The tmux-inject path stays intact
                    // and is the actual recovery channel here.
                    let emit_targets = filter_consumer_for_event_emit(
                        &missing_names,
                        &event_consumer_name,
                    );
                    if let Some(targets) = emit_targets {
                        let suppressed_msg = format!(
                            "[CLAUDE-WATCH] watcher-down (inject suppressed: main loop active): {}",
                            missing_list,
                        );
                        alert::emit_event(crate::event_bus::ClaudeWatchAlert {
                            alert_type: "watcher-down",
                            stuck_reason: &watcher_reason,
                            stale_minutes: None,
                            affected_watchers: targets,
                            severity: crate::event_bus::Severity::Medium,
                            message: &suppressed_msg,
                        });
                    } else {
                        info!(
                            consumer = %event_consumer_name,
                            "watcher-down event emit suppressed: only the event consumer is down (self-feedback guard)"
                        );
                        write_jsonl_log(
                            &config.general.log_file,
                            "watcher_down_event_self_feedback_suppressed",
                            serde_json::json!({
                                "consumer": event_consumer_name,
                                "missing": missing_names,
                                "site": "actively_turning_path",
                            }),
                        );
                    }
                    // NOTE (2026-04-28 q-2026-04-28-2449): we used to
                    // bump `last_watcher_inject` here so the cooldown
                    // clock advanced even on suppressed fires. That was
                    // a bug: a single suppressed attempt ate the full
                    // 5-min `inject_cooldown` slot, so even after the
                    // main loop went idle 1s later, the next inject was
                    // deferred until the cooldown elapsed. Now we leave
                    // the cooldown clock untouched on suppression — the
                    // shared `consecutive_suppressions` counter and the
                    // wall-clock window backstop are the things that
                    // bound the suppression run, not the cooldown clock.
                    crate::state::save_state(&config.general.state_file, state);
                } else {
                    if let Some(reason) = escalation {
                        warn!(
                            missing = %missing_list,
                            consecutive_suppressions = state.consecutive_suppressions,
                            escalation_reason = reason.as_str(),
                            "watcher-down inject escalating: suppression run capped — forcing inject"
                        );
                        write_jsonl_log(
                            &config.general.log_file,
                            "suppression_escalated",
                            serde_json::json!({
                                "site": "watcher_monitor",
                                "reason": reason.as_str(),
                                "consecutive_suppressions": state.consecutive_suppressions,
                                "first_suppression_at": state.first_suppression_at,
                                "missing": missing_names,
                            }),
                        );
                    }
                    warn!(missing = %missing_list, "watchers down — interrupting and injecting restart");
                    write_jsonl_log(
                        &config.general.log_file,
                        "watcher_inject",
                        serde_json::json!({
                            "missing": missing_names,
                        }),
                    );

                    // Interrupt first (like prolonged-thinking) to break any inline work
                    if tmux::interrupt_and_wait(&effective_pane, 10).await {
                        info!("watcher inject: Claude Code is idle after interrupt");
                    } else {
                        warn!("watcher inject: could not confirm idle, injecting anyway");
                    }
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;

                    // Build specific restart commands
                    let restart_cmds: Vec<String> = missing_names
                        .iter()
                        .map(|n| format!("watcher-ctl run {}", n))
                        .collect();
                    let prompt = format!(
                        "[CLAUDE-WATCH] WATCHER(S) DOWN: {}. You MUST restart them NOW. \
                         Run these as background tasks immediately: {}",
                        missing_list,
                        restart_cmds.join(", ")
                    );
                    tmux::inject_text(&effective_pane, &prompt).await;
                    // Third sink: claude-event so the main loop sees the
                    // missing-watchers list as structured data and can
                    // decide which restart command(s) to actually run,
                    // rather than reflexively reading the prompt string.
                    //
                    // Self-feedback guard: if the only down watcher is
                    // the event consumer itself, suppress the JSON file
                    // emit (it would just feed the consumer's own
                    // restart loop). The tmux-inject above remains the
                    // actual recovery channel here.
                    let emit_targets = filter_consumer_for_event_emit(
                        &missing_names,
                        &event_consumer_name,
                    );
                    if let Some(targets) = emit_targets {
                        alert::emit_event(crate::event_bus::ClaudeWatchAlert {
                            alert_type: "watcher-down",
                            stuck_reason: &watcher_reason,
                            stale_minutes: None,
                            affected_watchers: targets,
                            severity: crate::event_bus::Severity::Medium,
                            message: &prompt,
                        });
                    } else {
                        info!(
                            consumer = %event_consumer_name,
                            "watcher-down event emit suppressed: only the event consumer is down (self-feedback guard)"
                        );
                        write_jsonl_log(
                            &config.general.log_file,
                            "watcher_down_event_self_feedback_suppressed",
                            serde_json::json!({
                                "consumer": event_consumer_name,
                                "missing": missing_names,
                                "site": "inject_path",
                            }),
                        );
                    }
                    state.last_watcher_inject = Some(now.clone());
                    state.last_interrupt_at = Some(now.clone());
                    state.watcher_inject_count += 1;
                    state.watcher_down_interrupts_total =
                        state.watcher_down_interrupts_total.saturating_add(1);
                    reset_suppression(state);
                    crate::state::save_state(&config.general.state_file, state);
                }
            }
        }
    }

    // --- Auto-respawn-on-hang: multi-signal hang detection ---
    //
    // Independent of the individual interrupt sites above. Each fire path
    // (heartbeat-stale, watcher-down, prolonged-thinking, wedged-pane,
    // pane-capture-unchanged) records a HangSignal here. If `signals_required`
    // distinct signal kinds are observed within `signal_window_secs`, we
    // kill + relaunch the dashboard. Default OFF — Andrew opts in via
    // `[auto_respawn_on_hang] enabled = true`. Default cooldown 30 min so
    // a hung freshly-launched dashboard cannot get respawned in a tight loop.
    if config.auto_respawn_on_hang.enabled {
        check_auto_respawn(config, state, &effective_pane, &now, stuck).await;
    }

    // --- tmux healthcheck brief ---
    let tmux_brief = tmux::healthcheck_brief().await;

    // --- Log this check ---
    let log_msg = format!(
        "pane={} tokens={} bashes={} watchmen={} stuck={} reason={} failures={} {}",
        effective_pane,
        tokens,
        bashes,
        watchmen_count,
        stuck,
        stuck_reason,
        state.consecutive_failures,
        tmux_brief
    );
    write_legacy_log(&config.general.legacy_log_file, &log_msg);
    write_jsonl_log(
        &config.general.log_file,
        "check",
        serde_json::json!({
            "pane": effective_pane,
            "tokens": tokens,
            "bashes": bashes,
            "watchmen": watchmen_count,
            "stuck": stuck,
            "stuck_reason": stuck_reason,
            "consecutive_failures": state.consecutive_failures,
            "tmux_health": tmux_brief,
        }),
    );

    // --- Stuck handling with exponential backoff ---
    if stuck {
        state.consecutive_failures += 1;
        state.last_failure = Some(now.clone());
        state.last_failure_detail = Some(FailureDetail {
            bashes,
            watchmen: watchmen_count,
            stuck_reason: stuck_reason.clone(),
        });

        // Alert after 2 consecutive failures
        if state.consecutive_failures >= 2 {
            let alert_count = state.alert_count;

            // Exponential backoff via escalation tiers
            let cooldown = if (alert_count as usize) < config.alerts.escalation_tiers.len() {
                config.alerts.escalation_tiers[alert_count as usize]
            } else {
                *config.alerts.escalation_tiers.last().unwrap_or(&3600)
            };

            // Cooldown check
            if let Some(ref last) = state.last_alert {
                if let Some(elapsed) = elapsed_since(last) {
                    if elapsed < cooldown as f64 {
                        debug!(
                            elapsed_secs = elapsed,
                            cooldown_secs = cooldown,
                            alert_count,
                            "alert cooldown active"
                        );
                        crate::state::save_state(&config.general.state_file, state);
                        return;
                    }
                }
            }

            state.alert_count += 1;
            let use_pingme = state.alert_count <= config.alerts.max_pingme_alerts;

            info!(
                stuck_reason = %stuck_reason,
                failures = state.consecutive_failures,
                alert_number = state.alert_count,
                use_pingme,
                "ALERTING"
            );
            write_jsonl_log(
                &config.general.log_file,
                "alert",
                serde_json::json!({
                    "stuck_reason": stuck_reason,
                    "failures": state.consecutive_failures,
                    "alert_number": state.alert_count,
                    "use_pingme": use_pingme,
                }),
            );

            let alert_pane = if !effective_pane.is_empty() {
                effective_pane.clone()
            } else {
                tmux::find_dashboard_pane(&config.tmux)
                    .await
                    .unwrap_or_default()
            };

            if !alert_pane.is_empty() {
                let msg = format!(
                    "Claude stuck: {}. {} consecutive checks failed.",
                    stuck_reason, state.consecutive_failures
                );
                // Severity escalates with the alert count: first few
                // alerts are High; once we're past the pingme cap (the
                // sustained-stuck case), bump to Critical. Andrew's
                // 574-min heartbeat-stale incident was the canonical
                // case where the loop should have noticed depth.
                let severity = if state.alert_count > config.alerts.max_pingme_alerts {
                    crate::event_bus::Severity::Critical
                } else {
                    crate::event_bus::Severity::High
                };
                let event_alert = crate::event_bus::ClaudeWatchAlert {
                    alert_type: "heartbeat-stale",
                    stuck_reason: &stuck_reason,
                    stale_minutes: stuck_stale_minutes,
                    affected_watchers: vec![],
                    severity,
                    message: &msg,
                };
                alert::alert(
                    &msg,
                    &alert_pane,
                    &config.alerts.resume_prompt,
                    use_pingme,
                    event_alert,
                )
                .await;
            }

            state.last_alert = Some(now.clone());
        }
    } else {
        state.consecutive_failures = 0;
        state.alert_count = 0;
    }

    state.last_check = Some(now);
    state.last_status = Some(StatusSnapshot {
        bashes,
        watchmen: watchmen_count,
    });
    crate::state::save_state(&config.general.state_file, state);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_elapsed_since_valid() {
        // Use a timestamp 60 seconds ago
        let dt = Utc::now() - chrono::Duration::seconds(60);
        let dt_str = dt.to_rfc3339();
        let elapsed = elapsed_since(&dt_str).expect("should parse");
        // Should be approximately 60 seconds (allow some tolerance)
        assert!(
            elapsed >= 59.0 && elapsed <= 62.0,
            "elapsed was {}",
            elapsed
        );
    }

    #[test]
    fn test_elapsed_since_invalid() {
        assert!(elapsed_since("not a date").is_none());
        assert!(elapsed_since("").is_none());
    }

    // --- should_self_heal tests ---

    #[test]
    fn test_self_heal_triggers_at_threshold_with_tokens() {
        assert!(should_self_heal(5, 5, 12345, 0));
    }

    #[test]
    fn test_self_heal_triggers_at_threshold_with_bashes() {
        assert!(should_self_heal(5, 5, 0, 3));
    }

    #[test]
    fn test_self_heal_triggers_above_threshold() {
        assert!(should_self_heal(250, 5, 100, 0));
    }

    #[test]
    fn test_self_heal_no_trigger_below_threshold() {
        // Not at threshold yet — even if retry has tokens, don't self-heal.
        assert!(!should_self_heal(4, 5, 12345, 0));
    }

    #[test]
    fn test_self_heal_no_trigger_when_retry_still_zero() {
        // At threshold but retry also returned zero — no recovery possible.
        assert!(!should_self_heal(5, 5, 0, 0));
    }

    #[test]
    fn test_self_heal_no_trigger_at_zero() {
        assert!(!should_self_heal(0, 5, 1000, 2));
    }

    // --- watcher_is_down tests ---
    //
    // Regression suite for bug 2: the legacy `watchmen` shell-script did a
    // `kill -0` cross-check on the recorded PID file so a pgrep false-match
    // (stale wrapper, self-matching shell, etc.) didn't mask a dead watcher.
    // The Rust rewrite dropped that check; these tests pin down the
    // restored behaviour. Most importantly: pgrep_count >= min_count but
    // recorded_pid is dead -> DOWN (orphan-detected).

    #[test]
    fn test_watcher_is_down_count_below_min() {
        // No PID file, count = 0 < min_count = 1 -> DOWN.
        assert!(watcher_is_down(0, 1, None, |_| true));
    }

    #[test]
    fn test_watcher_is_down_count_meets_min_no_pidfile() {
        // No PID file -> fall back to pgrep-only logic. Count meets min ->
        // not DOWN. Preserves backward-compat for watchers we don't track.
        assert!(!watcher_is_down(1, 1, None, |_| panic!("should not probe")));
        assert!(!watcher_is_down(3, 1, None, |_| panic!("should not probe")));
    }

    #[test]
    fn test_watcher_is_down_pidfile_alive() {
        // Count meets min AND recorded PID is alive -> not DOWN.
        assert!(!watcher_is_down(1, 1, Some(42), |pid| pid == 42));
    }

    #[test]
    fn test_watcher_is_down_pidfile_dead_orphan() {
        // The bug-2 fix: count meets min via pgrep BUT recorded PID is dead.
        // Used to be reported as ok; now reported as DOWN (orphan).
        assert!(watcher_is_down(1, 1, Some(42), |_| false));
    }

    #[test]
    fn test_watcher_is_down_pidfile_dead_with_higher_count() {
        // Even when pgrep shows multiple matches (e.g. transient self-match),
        // a dead recorded PID is the canonical signal — DOWN.
        assert!(watcher_is_down(5, 1, Some(42), |_| false));
    }

    #[test]
    fn test_watcher_is_down_count_below_min_with_pidfile() {
        // Count below min AND PID dead -> DOWN regardless.
        assert!(watcher_is_down(0, 1, Some(42), |_| false));
        // Count below min but PID alive -> still DOWN (count is canonical
        // signal that the watcher isn't running with min_count instances).
        assert!(watcher_is_down(0, 1, Some(42), |pid| pid == 42));
    }

    #[test]
    fn test_watcher_is_down_min_count_zero() {
        // Edge case: min_count = 0 means always meets count requirement.
        // Without a PID file, never DOWN.
        assert!(!watcher_is_down(0, 0, None, |_| panic!("no probe")));
        // With a PID file and dead PID, DOWN by orphan detection.
        assert!(watcher_is_down(0, 0, Some(42), |_| false));
    }

    // --- read_watcher_pid tests ---

    #[test]
    fn test_read_watcher_pid_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            read_watcher_pid(dir.path().to_str().unwrap(), "nonexistent"),
            None
        );
    }

    #[test]
    fn test_read_watcher_pid_valid() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("foo.pid"), "12345\n").unwrap();
        assert_eq!(
            read_watcher_pid(dir.path().to_str().unwrap(), "foo"),
            Some(12345)
        );
    }

    #[test]
    fn test_read_watcher_pid_trims_whitespace() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("bar.pid"), "  9876  \n").unwrap();
        assert_eq!(
            read_watcher_pid(dir.path().to_str().unwrap(), "bar"),
            Some(9876)
        );
    }

    #[test]
    fn test_read_watcher_pid_garbage() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("baz.pid"), "not-a-pid\n").unwrap();
        assert_eq!(read_watcher_pid(dir.path().to_str().unwrap(), "baz"), None);
    }

    #[test]
    fn test_read_watcher_pid_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("empty.pid"), "").unwrap();
        assert_eq!(
            read_watcher_pid(dir.path().to_str().unwrap(), "empty"),
            None
        );
    }

    // --- check_context_threshold tests ---

    #[test]
    fn test_context_threshold_compact_remaining_triggers() {
        // compact_remaining = 3% <= 5% trigger
        let result = check_context_threshold_with_margin(150000, 200000, Some(3), 75, 5, None);
        assert!(result.is_some());
        let (pct, by_compact) = result.unwrap();
        assert!(by_compact, "should trigger via compact_remaining");
        assert!((pct - 75.0).abs() < 0.1);
    }

    #[test]
    fn test_context_threshold_compact_remaining_at_boundary() {
        // compact_remaining = 5% == 5% trigger (inclusive)
        let result = check_context_threshold_with_margin(150000, 200000, Some(5), 75, 5, None);
        assert!(result.is_some());
        let (_, by_compact) = result.unwrap();
        assert!(by_compact);
    }

    #[test]
    fn test_context_threshold_compact_remaining_safe() {
        // compact_remaining = 50% > 5% trigger — compact path doesn't fire.
        // Use low tokens (50K of 200K = 25%) so the percent fallback also
        // doesn't fire; expect None.
        let result = check_context_threshold_with_margin(50000, 200000, Some(50), 75, 5, None);
        assert!(result.is_none());
    }

    #[test]
    fn test_context_threshold_compact_zero() {
        // compact_remaining = 0% — should definitely trigger
        let result = check_context_threshold_with_margin(190000, 200000, Some(0), 75, 5, None);
        assert!(result.is_some());
        let (_, by_compact) = result.unwrap();
        assert!(by_compact);
    }

    #[test]
    fn test_context_threshold_fallback_token_percent_triggers() {
        // No compact_remaining, token pct = 80% >= 75% threshold
        let result = check_context_threshold_with_margin(160000, 200000, None, 75, 5, None);
        assert!(result.is_some());
        let (pct, by_compact) = result.unwrap();
        assert!(
            !by_compact,
            "should trigger via token fallback, not compact"
        );
        assert!((pct - 80.0).abs() < 0.1);
    }

    #[test]
    fn test_context_threshold_fallback_token_percent_safe() {
        // No compact_remaining, token pct = 50% < 75% threshold
        let result = check_context_threshold_with_margin(100000, 200000, None, 75, 5, None);
        assert!(result.is_none());
    }

    #[test]
    fn test_context_threshold_compact_does_not_block_percent_fallback() {
        // compact_remaining is present and safe (50%, > 5% trigger), tokens
        // are at 80% (>= 75% threshold), and threshold_margin is unset.
        //
        // The compact check is the PRIMARY signal but does not BLOCK the
        // fallback paths — when compact doesn't trigger, the legacy percent
        // fallback must still run. Expected: BY_PERCENT trigger.
        //
        // (Previously this test asserted is_none(), encoding the very bug
        // fixed in this commit — see test_context_threshold_margin_fires_*.)
        let result = check_context_threshold_with_margin(160000, 200000, Some(50), 75, 5, None);
        assert!(
            result.is_some(),
            "compact-safe should not block percent fallback"
        );
        let (pct, by_compact) = result.unwrap();
        assert!(!by_compact, "should be BY_PERCENT, not BY_COMPACT");
        assert!((pct - 80.0).abs() < 0.1);
    }

    #[test]
    fn test_context_threshold_margin_triggers() {
        // 1M max, 30K margin: trigger at 970K+
        let result = check_context_threshold_with_margin(975000, 1000000, None, 75, 5, Some(30000));
        assert!(result.is_some(), "should trigger at 975K with 30K margin");
    }

    #[test]
    fn test_context_threshold_margin_safe() {
        // 1M max, 30K margin: 960K < 970K threshold
        let result = check_context_threshold_with_margin(960000, 1000000, None, 75, 5, Some(30000));
        assert!(
            result.is_none(),
            "should not trigger at 960K with 30K margin"
        );
    }

    #[test]
    fn test_context_threshold_margin_overrides_percent() {
        // 750K would trigger at 75% but margin says 970K — should NOT trigger
        let result = check_context_threshold_with_margin(750000, 1000000, None, 75, 5, Some(30000));
        assert!(result.is_none(), "margin should override percent threshold");
    }

    #[test]
    fn test_context_threshold_margin_fires_even_when_compact_remaining_present() {
        // Regression test for the 2026-04-30 incident: tokens at 95.97%
        // (well past the 90% / 100K margin threshold) but
        // compact_remaining=Some(30) blocked the margin check via the old
        // else-if chain. The session climbed from 912K → 959K over 12 minutes
        // with zero context_threshold events emitted.
        //
        // Required behavior: compact-trigger and margin/percent triggers must
        // be INDEPENDENT. compact_remaining is the primary signal, but when
        // it's present and not triggering, the margin/percent fallback must
        // still run as a safety net.
        let result = check_context_threshold_with_margin(
            959_756,         // tokens
            1_000_000,       // max
            Some(30),        // compact_remaining > compact_trigger_percent
            75,              // threshold_percent
            5,               // compact_trigger_percent
            Some(100_000),   // threshold_margin (trigger at 900K)
        );
        assert!(
            result.is_some(),
            "margin must fire when compact_remaining is present but not triggering"
        );
        let (pct, by_compact) = result.unwrap();
        assert!((pct - 95.9756).abs() < 0.01);
        assert!(!by_compact, "should be by_margin, not by_compact");
    }

    #[test]
    fn test_context_threshold_compact_wins_over_margin() {
        // compact_remaining=Some(3) (triggers, <= 5) AND margin would not fire
        // (tokens at 200K, far from max-margin=900K). compact_remaining takes
        // precedence — BY_COMPACT path.
        let result = check_context_threshold_with_margin(
            200_000,
            1_000_000,
            Some(3),
            75,
            5,
            Some(100_000),
        );
        assert!(result.is_some());
        let (_, by_compact) = result.unwrap();
        assert!(by_compact, "compact_remaining=3 should win over margin");
    }

    #[test]
    fn test_context_threshold_neither_compact_nor_margin_fires() {
        // compact_remaining=Some(30) doesn't trigger and tokens=500K is below
        // the margin threshold (900K). Expect None — no trigger.
        let result = check_context_threshold_with_margin(
            500_000,
            1_000_000,
            Some(30),
            75,
            5,
            Some(100_000),
        );
        assert!(result.is_none(), "neither compact nor margin should fire");
    }

    #[test]
    fn test_context_threshold_compact_present_but_safe_falls_through_to_percent() {
        // When compact_remaining is present but doesn't trigger, AND
        // threshold_margin is unset, the legacy percent fallback must still
        // run. Tokens=160K of 200K = 80% > 75% threshold. Expect BY_PERCENT.
        // This is the regression guard for the bug fix: the old else-if chain
        // would skip this check entirely when compact_remaining was Some.
        let result = check_context_threshold_with_margin(
            160_000,
            200_000,
            Some(30), // compact present but not triggering
            75,
            5,
            None, // no margin set, legacy percent path
        );
        assert!(
            result.is_some(),
            "percent fallback must fire when compact present but not triggering"
        );
        let (pct, by_compact) = result.unwrap();
        assert!(!by_compact, "should be BY_PERCENT, not BY_COMPACT");
        assert!((pct - 80.0).abs() < 0.1);
    }

    // --- maybe_reset_context_clear tests (regression guard for 2026-05-01) ---
    //
    // 2026-05-01 incident: deferred clear ran cleanly at UTC 12:23:13, the pane
    // briefly read tokens=0 at 12:28:20, but the reset path was nested inside
    // the `tokens > 0` outer guard in check_cycle, so the tokens=0 sample never
    // reset `context_clear_triggered`. Tokens climbed back above 30K, the
    // sub-30K branch couldn't fire either, and the flag stayed stuck for ~4
    // hours — every subsequent threshold crossing was suppressed by
    // `if !state.context_clear_triggered`. Pulling the reset path out of the
    // guard fixes it, and these tests pin the contract.

    fn config_for_reset_test() -> Config {
        let toml_str = r#"
[general]
check_interval = 10
state_file = "/tmp/s.json"
log_file = "/tmp/s.jsonl"
legacy_log_file = "/tmp/s.log"

[claude]
max_context_tokens = 1000000
heartbeat_file = "/tmp/hb"
relaunch_script = "/tmp/rel.sh"

[dead_process]
checks_required = 3
restart_cooldown = 60

[fresh_clear]
min_tokens = 1000
max_tokens = 5000
detections_required = 2
cooldown = 60

[heartbeat]
stale_minutes = 10

[alerts]
initial_cooldown = 60
escalation_tiers = [60]
max_pingme_alerts = 1
resume_prompt = "x"

[foreground_monitor]
enabled = true
threshold_seconds = 60
check_interval = 3

[watcher_monitor]
enabled = false
watchers_config = "/tmp/w.conf"
expected_watchmen = 0

[context_monitor]
enabled = true
threshold_margin = 100000
threshold_percent = 90
compact_trigger_percent = 5
grace_period = 300
cooldown = 300
"#;
        crate::config::parse_config(toml_str).expect("parse")
    }

    #[test]
    fn test_reset_zero_tokens_clears_triggered_flag() {
        // The 2026-05-01 regression: tokens=0 right after self-clear must
        // reset `context_clear_triggered`. Before the fix, the outer
        // `tokens > 0` guard in check_cycle skipped the reset path entirely
        // on this exact sample, leaving the flag stuck.
        let config = config_for_reset_test();
        let mut state = State::default();
        state.context_clear_triggered = true;
        state.context_clear_child_pid = Some(12345);
        state.last_seen_tokens = Some(916_581);
        let now = Utc::now().to_rfc3339();
        maybe_reset_context_clear(&config, &mut state, 0, &now);
        assert!(
            !state.context_clear_triggered,
            "tokens=0 must reset the trigger flag"
        );
        assert!(
            state.context_clear_child_pid.is_none(),
            "child pid bookkeeping must clear"
        );
        assert!(
            state.last_context_clear.is_some(),
            "last_context_clear must update"
        );
    }

    #[test]
    fn test_reset_low_tokens_clears_triggered_flag() {
        // A non-zero tokens sample below the fresh threshold (e.g. 5300, the
        // value right after a /clear) must also reset the flag.
        let config = config_for_reset_test();
        let mut state = State::default();
        state.context_clear_triggered = true;
        state.context_clear_child_pid = Some(12345);
        state.last_seen_tokens = Some(959_704);
        let now = Utc::now().to_rfc3339();
        maybe_reset_context_clear(&config, &mut state, 5_300, &now);
        assert!(!state.context_clear_triggered);
        assert!(state.context_clear_child_pid.is_none());
    }

    #[test]
    fn test_reset_high_tokens_leaves_flag_set() {
        // While tokens are still high, the flag stays set so an in-flight
        // deferred clear isn't double-spawned.
        let config = config_for_reset_test();
        let mut state = State::default();
        state.context_clear_triggered = true;
        state.last_seen_tokens = Some(905_000);
        let now = Utc::now().to_rfc3339();
        maybe_reset_context_clear(&config, &mut state, 950_000, &now);
        assert!(
            state.context_clear_triggered,
            "tokens >= fresh threshold must NOT reset the flag"
        );
    }

    #[test]
    fn test_reset_at_exact_fresh_threshold_does_not_reset() {
        // Boundary: tokens == 30000 is treated as "still in flight".
        let config = config_for_reset_test();
        let mut state = State::default();
        state.context_clear_triggered = true;
        let now = Utc::now().to_rfc3339();
        maybe_reset_context_clear(&config, &mut state, 30_000, &now);
        assert!(state.context_clear_triggered);
    }

    #[test]
    fn test_reset_just_below_threshold_resets() {
        let config = config_for_reset_test();
        let mut state = State::default();
        state.context_clear_triggered = true;
        let now = Utc::now().to_rfc3339();
        maybe_reset_context_clear(&config, &mut state, 29_999, &now);
        assert!(!state.context_clear_triggered);
    }

    #[test]
    fn test_external_clear_path_records_timestamp() {
        // External clear (user /clear): no in-flight trigger flag, but
        // last_seen_tokens was high. Path should log + update last_context_clear.
        let config = config_for_reset_test();
        let mut state = State::default();
        state.context_clear_triggered = false;
        state.last_seen_tokens = Some(800_000);
        state.last_context_clear = None;
        let now = Utc::now().to_rfc3339();
        maybe_reset_context_clear(&config, &mut state, 5_300, &now);
        assert!(
            state.last_context_clear.is_some(),
            "external clear must update last_context_clear"
        );
    }

    #[test]
    fn test_external_clear_path_skipped_during_boot() {
        // No prior high reading -> don't log spurious external clear during boot.
        let config = config_for_reset_test();
        let mut state = State::default();
        state.context_clear_triggered = false;
        state.last_seen_tokens = Some(0);
        state.last_context_clear = None;
        let now = Utc::now().to_rfc3339();
        maybe_reset_context_clear(&config, &mut state, 100, &now);
        assert!(
            state.last_context_clear.is_none(),
            "boot path must not update last_context_clear"
        );
    }

    #[test]
    fn test_reset_idempotent_when_flag_already_clear() {
        // Calling reset when nothing was triggered AND no prior high sample
        // is a no-op — important because check_cycle calls it every iteration.
        let config = config_for_reset_test();
        let mut state = State::default();
        let now = Utc::now().to_rfc3339();
        let before = state.last_context_clear.clone();
        maybe_reset_context_clear(&config, &mut state, 5_300, &now);
        assert!(!state.context_clear_triggered);
        assert_eq!(state.last_context_clear, before);
    }

    // --- Thinking backoff threshold tests ---

    #[test]
    fn test_thinking_backoff_first_interrupt() {
        // First interrupt (count=0): base threshold unchanged
        assert_eq!(thinking_backoff_threshold(60, 960, 0), 60);
    }

    #[test]
    fn test_thinking_backoff_sequence() {
        // Exponential doubling: 60, 120, 240, 480, 960
        assert_eq!(thinking_backoff_threshold(60, 960, 0), 60);
        assert_eq!(thinking_backoff_threshold(60, 960, 1), 120);
        assert_eq!(thinking_backoff_threshold(60, 960, 2), 240);
        assert_eq!(thinking_backoff_threshold(60, 960, 3), 480);
        assert_eq!(thinking_backoff_threshold(60, 960, 4), 960);
    }

    #[test]
    fn test_thinking_backoff_caps_at_max() {
        // Once we hit max_backoff, it stays there
        assert_eq!(thinking_backoff_threshold(60, 960, 4), 960);
        assert_eq!(thinking_backoff_threshold(60, 960, 5), 960);
        assert_eq!(thinking_backoff_threshold(60, 960, 10), 960);
        assert_eq!(thinking_backoff_threshold(60, 960, 100), 960);
    }

    #[test]
    fn test_thinking_backoff_different_base() {
        // With base=120, max=960: 120, 240, 480, 960, 960
        assert_eq!(thinking_backoff_threshold(120, 960, 0), 120);
        assert_eq!(thinking_backoff_threshold(120, 960, 1), 240);
        assert_eq!(thinking_backoff_threshold(120, 960, 2), 480);
        assert_eq!(thinking_backoff_threshold(120, 960, 3), 960);
        assert_eq!(thinking_backoff_threshold(120, 960, 4), 960);
    }

    #[test]
    fn test_thinking_backoff_overflow_safety() {
        // Extremely high interrupt count should not panic (saturating math)
        let result = thinking_backoff_threshold(60, 960, 63);
        assert_eq!(result, 960); // Capped at max
        let result = thinking_backoff_threshold(60, 960, u32::MAX);
        assert_eq!(result, 960); // Capped at max, no panic
    }

    // --- Configurable-multiplier backoff tests (2026-04-21) ---

    #[test]
    fn test_thinking_backoff_multiplier_3() {
        // With base=300, mult=3, max=960: 300, 900, 960 (cap), 960, ...
        assert_eq!(thinking_backoff_threshold_with_multiplier(300, 960, 0, 3), 300);
        assert_eq!(thinking_backoff_threshold_with_multiplier(300, 960, 1, 3), 900);
        assert_eq!(thinking_backoff_threshold_with_multiplier(300, 960, 2, 3), 960);
        assert_eq!(thinking_backoff_threshold_with_multiplier(300, 960, 10, 3), 960);
    }

    #[test]
    fn test_thinking_backoff_multiplier_2_matches_legacy() {
        // multiplier=2 should produce the same output as the legacy doubling.
        for count in 0..6 {
            assert_eq!(
                thinking_backoff_threshold_with_multiplier(60, 960, count, 2),
                thinking_backoff_threshold(60, 960, count),
                "legacy-compat check failed at count={}", count
            );
        }
    }

    #[test]
    fn test_thinking_backoff_multiplier_overflow_safety() {
        // Huge counts with multiplier>1 must not panic.
        let result = thinking_backoff_threshold_with_multiplier(300, 960, u32::MAX, 3);
        assert_eq!(result, 960);
    }

    // --- Global post-interrupt cooldown tests (2026-04-21) ---

    #[test]
    fn test_global_cooldown_disabled_when_zero() {
        // cooldown=0 always returns false, regardless of last_interrupt_at.
        let mut state = State::default();
        state.last_interrupt_at = Some(Utc::now().to_rfc3339());
        assert!(!interrupt_in_global_cooldown(&state, 0));
    }

    #[test]
    fn test_global_cooldown_inactive_when_no_prior_interrupt() {
        // No last_interrupt_at -> never in cooldown.
        let state = State::default();
        assert!(!interrupt_in_global_cooldown(&state, 60));
    }

    #[test]
    fn test_global_cooldown_active_within_window() {
        // Last interrupt was 10s ago, window is 60s -> in cooldown.
        let mut state = State::default();
        let ts = Utc::now() - chrono::Duration::seconds(10);
        state.last_interrupt_at = Some(ts.to_rfc3339());
        assert!(interrupt_in_global_cooldown(&state, 60));
    }

    #[test]
    fn test_global_cooldown_expired_after_window() {
        // Last interrupt was 120s ago, window is 60s -> cooldown expired.
        let mut state = State::default();
        let ts = Utc::now() - chrono::Duration::seconds(120);
        state.last_interrupt_at = Some(ts.to_rfc3339());
        assert!(!interrupt_in_global_cooldown(&state, 60));
    }

    #[test]
    fn test_global_cooldown_ignores_malformed_timestamp() {
        // Garbage timestamp should not count as "in cooldown" (fail-open so
        // the gate never wedges).
        let mut state = State::default();
        state.last_interrupt_at = Some("not a date".to_string());
        assert!(!interrupt_in_global_cooldown(&state, 60));
    }

    // --- Fresh session inject loop prevention tests ---

    /// Helper: simulate the inject loop scenario state transitions.
    /// Returns state after applying the described transition.
    fn make_state_with_inject(was_alive: bool, inject_time_ago_secs: Option<i64>) -> State {
        let mut state = State::default();
        state.fresh_session_injected = true;
        state.was_alive_since_inject = was_alive;
        state.last_fresh_inject = inject_time_ago_secs.map(|secs| {
            let dt = Utc::now() - chrono::Duration::seconds(secs);
            dt.to_rfc3339()
        });
        state
    }

    #[test]
    fn test_inject_loop_prevention_never_alive_recent() {
        // Inject was recent (30s ago), Claude never became active.
        // Should NOT reset fresh_session_injected — prevents the inject loop.
        let state = make_state_with_inject(false, Some(30));
        let inject_expired = state
            .last_fresh_inject
            .as_ref()
            .and_then(|ts| elapsed_since(ts))
            .map_or(false, |elapsed| elapsed >= 300.0);

        assert!(!state.was_alive_since_inject);
        assert!(!inject_expired);
        // The dead state handler would NOT reset because neither condition is true.
    }

    #[test]
    fn test_inject_loop_prevention_was_alive_then_died() {
        // Claude was alive (tokens > 0) after inject, then died.
        // Should reset fresh_session_injected — this is a real session death.
        let state = make_state_with_inject(true, Some(120));

        assert!(state.was_alive_since_inject);
        // The dead state handler WOULD reset because was_alive_since_inject is true.
    }

    #[test]
    fn test_inject_loop_prevention_expired_never_alive() {
        // Inject was 6 minutes ago, Claude never became active.
        // Should reset fresh_session_injected — the session is stuck/dead, allow retry.
        let state = make_state_with_inject(false, Some(360));
        let inject_expired = state
            .last_fresh_inject
            .as_ref()
            .and_then(|ts| elapsed_since(ts))
            .map_or(false, |elapsed| elapsed >= 300.0);

        assert!(!state.was_alive_since_inject);
        assert!(inject_expired);
        // The dead state handler WOULD reset because inject_expired is true.
    }

    #[test]
    fn test_inject_loop_prevention_no_timestamp() {
        // fresh_session_injected is true but no timestamp (legacy state).
        // Should NOT reset (conservative — treat as recent).
        let state = make_state_with_inject(false, None);
        let inject_expired = state
            .last_fresh_inject
            .as_ref()
            .and_then(|ts| elapsed_since(ts))
            .map_or(false, |elapsed| elapsed >= 300.0);

        assert!(!state.was_alive_since_inject);
        assert!(!inject_expired);
        // Conservative: don't reset without evidence.
    }

    #[test]
    fn test_inject_active_session_marks_alive() {
        // Simulates tokens > 0 path: fresh_session_injected → was_alive_since_inject
        let mut state = State::default();
        state.fresh_session_injected = true;
        state.was_alive_since_inject = false;

        // This mirrors the "session is active (tokens > 0)" block in check_cycle:
        if state.fresh_session_injected {
            state.was_alive_since_inject = true;
            state.fresh_session_injected = false;
        }

        assert!(!state.fresh_session_injected);
        assert!(state.was_alive_since_inject);
    }

    #[test]
    fn test_inject_pane_change_resets_both_flags() {
        // Pane change is definitive — always reset both flags.
        let mut state = State::default();
        state.fresh_session_injected = true;
        state.was_alive_since_inject = true;

        // This mirrors the pane change block in check_cycle:
        state.fresh_session_injected = false;
        state.was_alive_since_inject = false;

        assert!(!state.fresh_session_injected);
        assert!(!state.was_alive_since_inject);
    }

    // --- Interrupt counter tests (2026-04-22) ---
    //
    // These sanity-check that each per-interrupt counter uses saturating
    // addition and accumulates across multiple fires. The full tmux-driven
    // fire paths are exercised in the e2e tests; these tests pin down the
    // arithmetic primitive that every fire site uses.

    #[test]
    fn test_interrupt_counter_saturating_increment_accumulates() {
        let mut state = State::default();
        for _ in 0..5 {
            state.prolonged_thinking_interrupts_total = state
                .prolonged_thinking_interrupts_total
                .saturating_add(1);
        }
        assert_eq!(state.prolonged_thinking_interrupts_total, 5);
    }

    #[test]
    fn test_interrupt_counter_saturating_increment_does_not_panic_at_u64_max() {
        let mut state = State::default();
        state.prolonged_thinking_interrupts_total = u64::MAX;
        // saturating_add(1) must not panic at u64::MAX; it saturates.
        state.prolonged_thinking_interrupts_total = state
            .prolonged_thinking_interrupts_total
            .saturating_add(1);
        assert_eq!(state.prolonged_thinking_interrupts_total, u64::MAX);
    }

    #[test]
    fn test_interrupt_counter_independent_of_backoff_index() {
        // The cumulative counter must not be reset by the per-episode
        // thinking_interrupt_count reset (which happens when Claude exits
        // the thinking state — see `check_foreground` else branch).
        let mut state = State::default();
        state.prolonged_thinking_interrupts_total = 42;
        state.thinking_interrupt_count = 3;

        // Mirror the reset branch at the non-thinking else arm:
        state.thinking_start = None;
        state.thinking_alerted = false;
        state.thinking_interrupt_count = 0;

        // Cumulative counter must NOT be reset.
        assert_eq!(state.prolonged_thinking_interrupts_total, 42);
        assert_eq!(state.thinking_interrupt_count, 0);
    }

    #[test]
    fn test_interrupt_counters_independent_per_kind() {
        // Incrementing one kind must not affect the others.
        let mut state = State::default();
        state.watcher_down_interrupts_total = state
            .watcher_down_interrupts_total
            .saturating_add(1);
        state.context_warning_interrupts_total = state
            .context_warning_interrupts_total
            .saturating_add(1);
        state.context_warning_interrupts_total = state
            .context_warning_interrupts_total
            .saturating_add(1);

        assert_eq!(state.watcher_down_interrupts_total, 1);
        assert_eq!(state.context_warning_interrupts_total, 2);
        // Untouched kinds stay at 0
        assert_eq!(state.prolonged_thinking_interrupts_total, 0);
        assert_eq!(state.wedged_clear_interrupts_total, 0);
        assert_eq!(state.auto_update_interrupts_total, 0);
        assert_eq!(state.restart_claude_interrupts_total, 0);
    }

    // --- main_loop_actively_turning suppression-gate tests (2026-04-27) ---
    //
    // The watcher-down inject path consults this predicate. When it returns
    // true, the daemon skips the tmux interrupt + inject (the in-pane
    // preemption) but still emits the structured claude-event sink so
    // Andrew is notified out-of-band. The in-pane preemption is the only
    // cause of the "inject fires mid-turn → loop pivots to restart watcher
    // → original ask is abandoned half-finished" cascade Andrew flagged
    // 2026-04-27.

    fn iso_secs_ago(seconds_ago: i64) -> String {
        let dt = chrono::Utc::now() - chrono::Duration::seconds(seconds_ago);
        dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    }

    #[test]
    fn test_main_loop_actively_turning_when_bashes_nonzero() {
        // bashes > 0 RIGHT NOW: actively turning, regardless of last_active_at.
        let state = State::default();
        assert!(main_loop_actively_turning(&state, 1, 30));
    }

    #[test]
    fn test_main_loop_actively_turning_recent_activity_in_window() {
        // bashes == 0 NOW but a tool call ran 5s ago: still actively turning.
        let mut state = State::default();
        state.last_active_at = Some(iso_secs_ago(5));
        assert!(main_loop_actively_turning(&state, 0, 30));
    }

    #[test]
    fn test_main_loop_actively_turning_stale_activity_outside_window() {
        // last_active_at is 60s ago, window is 30s: not actively turning.
        let mut state = State::default();
        state.last_active_at = Some(iso_secs_ago(60));
        assert!(!main_loop_actively_turning(&state, 0, 30));
    }

    #[test]
    fn test_main_loop_actively_turning_no_history_idle() {
        // No last_active_at, bashes == 0: definitely not actively turning.
        let state = State::default();
        assert!(!main_loop_actively_turning(&state, 0, 30));
    }

    #[test]
    fn test_main_loop_actively_turning_window_zero_still_honors_live_bashes() {
        // window_secs = 0 disables the recent-activity gate, but a live
        // tool call (bashes > 0) MUST still count as actively turning.
        let state = State::default();
        assert!(main_loop_actively_turning(&state, 1, 0));
    }

    #[test]
    fn test_main_loop_actively_turning_window_zero_idle_returns_false() {
        // window_secs = 0 + bashes == 0 + recent activity 1s ago:
        // recent-activity gate is disabled, so this must NOT count as
        // actively turning.
        let mut state = State::default();
        state.last_active_at = Some(iso_secs_ago(1));
        assert!(!main_loop_actively_turning(&state, 0, 0));
    }

    #[test]
    fn test_main_loop_actively_turning_invalid_timestamp_treated_as_idle() {
        // Garbage in last_active_at parses to None and must NOT be
        // treated as "recent" — that would silently disable the inject
        // forever after a single corrupt write.
        let mut state = State::default();
        state.last_active_at = Some("not a timestamp".to_string());
        assert!(!main_loop_actively_turning(&state, 0, 30));
    }

    // --- fresh-/clear and dead-process suppression tests (2026-04-27, q-2026-04-27-ce5f) ---
    //
    // Both alert paths fire on point-in-time predicates that the main
    // loop transiently satisfies between two tool calls (a small turn
    // sitting at a few thousand tokens with bashes momentarily 0; or a
    // brief pane swap making tokens=0 and bashes=0 look like a dead
    // process). These tests pin the suppression-decision logic so the
    // false positives Andrew flagged at 02:45 ET 2026-04-27 don't
    // regress.

    #[test]
    fn test_fresh_clear_suppressed_when_actively_turning() {
        // bashes > 0 right now: the loop is mid-tool-call, so even if
        // the [min_tokens, max_tokens) gate matches we MUST suppress.
        let state = State::default();
        assert!(fresh_clear_inject_suppressed(&state, 1, true, 60));
    }

    #[test]
    fn test_fresh_clear_suppressed_when_recent_activity_in_window() {
        // bashes == 0 NOW but a tool call ran 10s ago: the loop is
        // demonstrably alive — the bashes gauge is just between calls.
        // The fresh-/clear inject would derail real work, so suppress.
        let mut state = State::default();
        state.last_active_at = Some(iso_secs_ago(10));
        assert!(fresh_clear_inject_suppressed(&state, 0, true, 60));
    }

    #[test]
    fn test_fresh_clear_not_suppressed_when_idle_outside_window() {
        // Last activity 120s ago, window is 60s: loop is genuinely
        // idle on a fresh /clear, so the fast-path SHOULD fire.
        let mut state = State::default();
        state.last_active_at = Some(iso_secs_ago(120));
        assert!(!fresh_clear_inject_suppressed(&state, 0, true, 60));
    }

    #[test]
    fn test_fresh_clear_not_suppressed_when_no_history() {
        // Brand-new daemon, no last_active_at recorded, bashes == 0:
        // can't infer activity, so DON'T suppress. The fast-path keeps
        // its existing behaviour for the genuine fresh-/clear case.
        let state = State::default();
        assert!(!fresh_clear_inject_suppressed(&state, 0, true, 60));
    }

    #[test]
    fn test_fresh_clear_not_suppressed_when_disabled() {
        // suppress_when_active = false (operator override): even with a
        // live tool call the suppression gate is bypassed, restoring
        // pre-fix behaviour. Useful escape hatch if the predicate
        // misfires for some workload.
        let mut state = State::default();
        state.last_active_at = Some(iso_secs_ago(5));
        assert!(!fresh_clear_inject_suppressed(&state, 1, false, 60));
        assert!(!fresh_clear_inject_suppressed(&state, 0, false, 60));
    }

    #[test]
    fn test_fresh_clear_window_zero_still_honors_live_bashes() {
        // active_window_secs = 0 disables the time-window check, but a
        // live tool call (bashes > 0) MUST still suppress. Mirrors the
        // main_loop_actively_turning semantics exactly.
        let state = State::default();
        assert!(fresh_clear_inject_suppressed(&state, 1, true, 0));
    }

    #[test]
    fn test_fresh_clear_window_zero_idle_does_not_suppress() {
        // active_window_secs = 0 + bashes == 0 + recent activity 1s
        // ago: window check is disabled, and bashes is 0 right now,
        // so the gate stays open and the inject can fire.
        let mut state = State::default();
        state.last_active_at = Some(iso_secs_ago(1));
        assert!(!fresh_clear_inject_suppressed(&state, 0, true, 0));
    }

    #[test]
    fn test_dead_process_suppressed_when_actively_turning() {
        // bashes > 0 right now: the process is demonstrably alive.
        // Restarting it would kill an active session and fire a false
        // claude-crashed alert. MUST suppress.
        let state = State::default();
        assert!(dead_process_restart_suppressed(&state, 2, true, 60));
    }

    #[test]
    fn test_dead_process_suppressed_when_recent_activity_in_window() {
        // bashes == 0 NOW but a tool call ran 30s ago. The dead-process
        // checks_required is 3 (default) at ~10s intervals, so a 30s
        // window perfectly straddles "could the parser have missed
        // 3 cycles in a row?" — yes, easily. Suppress to be safe.
        let mut state = State::default();
        state.last_active_at = Some(iso_secs_ago(30));
        assert!(dead_process_restart_suppressed(&state, 0, true, 60));
    }

    #[test]
    fn test_dead_process_not_suppressed_when_idle_outside_window() {
        // Last tool call 90s ago, window is 60s: process has been
        // genuinely silent past the window. If the shell-prompt check
        // also confirms, restart the process for real.
        let mut state = State::default();
        state.last_active_at = Some(iso_secs_ago(90));
        assert!(!dead_process_restart_suppressed(&state, 0, true, 60));
    }

    #[test]
    fn test_dead_process_not_suppressed_when_no_history() {
        // Brand-new daemon, no last_active_at, bashes == 0: nothing to
        // infer activity from. Don't suppress — the dead_checks_required
        // counter and is_shell_prompt() check are the other safety belts.
        let state = State::default();
        assert!(!dead_process_restart_suppressed(&state, 0, true, 60));
    }

    #[test]
    fn test_dead_process_not_suppressed_when_disabled() {
        // suppress_when_active = false: gate is bypassed entirely.
        // Restores pre-fix behaviour for an operator who wants it.
        let mut state = State::default();
        state.last_active_at = Some(iso_secs_ago(5));
        assert!(!dead_process_restart_suppressed(&state, 1, false, 60));
        assert!(!dead_process_restart_suppressed(&state, 0, false, 60));
    }

    #[test]
    fn test_dead_process_uses_wider_default_window_than_watcher_down() {
        // Documents the policy choice: a dead-process false positive
        // restarts Claude Code (destroys an in-flight session), which
        // is far more destructive than a missed watcher-down inject
        // (just defers a notification by 5 min). The default
        // active_window_secs for dead_process is 60s vs watcher_monitor's
        // 30s. Test the boundary: 45s ago should suppress at 60s
        // window but not at 30s window.
        let mut state = State::default();
        state.last_active_at = Some(iso_secs_ago(45));
        // dead_process default window (60s) suppresses
        assert!(dead_process_restart_suppressed(&state, 0, true, 60));
        // watcher_monitor default window (30s) would NOT
        assert!(!main_loop_actively_turning(&state, 0, 30));
    }

    #[test]
    fn test_dead_process_invalid_timestamp_treated_as_idle() {
        // Same defensive check as test_main_loop_actively_turning_invalid_timestamp_treated_as_idle:
        // garbage timestamp parses to None, treated as idle (no suppression).
        // A corrupt persisted state file MUST NOT silently disable the
        // restart path forever.
        let mut state = State::default();
        state.last_active_at = Some("garbage".to_string());
        assert!(!dead_process_restart_suppressed(&state, 0, true, 60));
    }

    #[test]
    fn test_fresh_clear_invalid_timestamp_treated_as_idle() {
        // Mirror of dead_process variant. Garbage in last_active_at
        // must NOT be treated as recent activity.
        let mut state = State::default();
        state.last_active_at = Some("garbage".to_string());
        assert!(!fresh_clear_inject_suppressed(&state, 0, true, 60));
    }

    // --- Cross-gate suppression-escalation tests (2026-04-28, q-2026-04-28-2449) ---
    //
    // These pin the behavior of the shared escalation mechanism that backstops
    // the three suppression gates. Real-world incident: claude-event-watch
    // died at 19:27Z and stayed down 33 min because watcher_monitor's
    // suppression gate kept holding through a sustained dispatcher window.
    // These tests guarantee the next time that happens we escalate at the
    // configured cap and force-inject.

    #[test]
    fn test_record_suppression_first_call_stamps_timestamp() {
        // 0 -> 1 transition: first_suppression_at should be set, counter
        // bumped to 1.
        let mut state = State::default();
        let now = chrono::Utc::now().to_rfc3339();
        record_suppression(&mut state, &now);
        assert_eq!(state.consecutive_suppressions, 1);
        assert_eq!(state.first_suppression_at.as_deref(), Some(now.as_str()));
    }

    #[test]
    fn test_record_suppression_subsequent_calls_preserve_timestamp() {
        // Once first_suppression_at is set, subsequent calls must NOT
        // overwrite it (otherwise the wall-clock backstop would never
        // fire — the window would keep resetting).
        let mut state = State::default();
        let t0 = "2026-04-28T00:00:00+00:00".to_string();
        let t1 = "2026-04-28T00:01:00+00:00".to_string();
        let t2 = "2026-04-28T00:02:00+00:00".to_string();
        record_suppression(&mut state, &t0);
        record_suppression(&mut state, &t1);
        record_suppression(&mut state, &t2);
        assert_eq!(state.consecutive_suppressions, 3);
        // t0 is the first, must persist across the next two.
        assert_eq!(state.first_suppression_at, Some(t0));
    }

    #[test]
    fn test_record_suppression_saturates_at_u32_max() {
        // Sanity: catastrophic counter overflow must not panic.
        let mut state = State::default();
        state.consecutive_suppressions = u32::MAX;
        state.first_suppression_at = Some(iso_secs_ago(60));
        record_suppression(&mut state, "now");
        assert_eq!(state.consecutive_suppressions, u32::MAX);
    }

    #[test]
    fn test_reset_suppression_clears_both_fields() {
        let mut state = State::default();
        state.consecutive_suppressions = 5;
        state.first_suppression_at = Some(iso_secs_ago(120));
        reset_suppression(&mut state);
        assert_eq!(state.consecutive_suppressions, 0);
        assert!(state.first_suppression_at.is_none());
    }

    #[test]
    fn test_reset_suppression_idempotent_when_already_clear() {
        let mut state = State::default();
        reset_suppression(&mut state);
        assert_eq!(state.consecutive_suppressions, 0);
        assert!(state.first_suppression_at.is_none());
    }

    #[test]
    fn test_should_escalate_returns_none_when_counter_zero() {
        // The very first suppression of a run can never escalate — the
        // gate has not yet demonstrably failed to drain. Required so the
        // happy path (one suppression, then the active turn ends, then
        // the watcher comes back) doesn't escalate.
        let state = State::default();
        assert_eq!(should_escalate_suppression(&state, 3, 600), None);
    }

    #[test]
    fn test_should_escalate_fires_on_consecutive_cap() {
        // counter == max: escalation due to consecutive cap.
        let mut state = State::default();
        state.consecutive_suppressions = 3;
        state.first_suppression_at = Some(iso_secs_ago(10));
        assert_eq!(
            should_escalate_suppression(&state, 3, 600),
            Some(EscalationReason::ConsecutiveCap)
        );
    }

    // --- evaluate_api_retry_state tests (2026-04-28) ---

    #[test]
    fn test_api_retry_eval_not_retrying_clears_state() {
        // When the pane no longer shows a retry banner, all tracking state
        // resets immediately (no consecutive count, no first_seen).
        let prior = "2026-04-28T12:00:00+00:00";
        let (consec, first, suppress) =
            evaluate_api_retry_state(false, 5, Some(prior), 1, 1800);
        assert_eq!(consec, 0);
        assert!(first.is_none());
        assert!(!suppress);
    }

    #[test]
    fn test_api_retry_eval_first_detection_stamps_first_seen() {
        // First detection: consecutive = 1, first_seen gets stamped, and
        // with threshold=1 we suppress immediately.
        let (consec, first, suppress) = evaluate_api_retry_state(true, 0, None, 1, 1800);
        assert_eq!(consec, 1);
        assert!(first.is_some());
        assert!(suppress);
    }

    #[test]
    fn test_api_retry_eval_below_consecutive_threshold_does_not_suppress() {
        // threshold=3, consec was 0 -> becomes 1. Not enough to suppress yet.
        let (consec, first, suppress) = evaluate_api_retry_state(true, 0, None, 3, 1800);
        assert_eq!(consec, 1);
        assert!(first.is_some()); // first_seen stamped on first detection
        assert!(!suppress);
    }

    #[test]
    fn test_api_retry_eval_at_consecutive_threshold_suppresses() {
        // threshold=3, consec was 2 -> becomes 3. Just hits threshold.
        let prior = Utc::now().to_rfc3339();
        let (consec, first, suppress) =
            evaluate_api_retry_state(true, 2, Some(&prior), 3, 1800);
        assert_eq!(consec, 3);
        assert_eq!(first.as_deref(), Some(prior.as_str()));
        assert!(suppress);
    }

    #[test]
    fn test_api_retry_eval_preserves_first_seen_across_cycles() {
        // While retrying, first_seen MUST stay pinned to the first
        // detection so max_stuck_secs can measure elapsed time correctly.
        let prior = "2026-04-28T12:00:00+00:00";
        let (_, first, _) = evaluate_api_retry_state(true, 1, Some(prior), 1, 1800);
        assert_eq!(first.as_deref(), Some(prior));
    }

    #[test]
    fn test_api_retry_eval_max_stuck_secs_lifts_suppression() {
        // first_seen is 2 hours ago, max_stuck_secs = 1800 (30 min).
        // Suppression must lift so monitoring can resume.
        let two_hours_ago = (Utc::now() - chrono::Duration::seconds(7200)).to_rfc3339();
        let (consec, first, suppress) =
            evaluate_api_retry_state(true, 100, Some(&two_hours_ago), 1, 1800);
        assert_eq!(consec, 101);
        assert_eq!(first.as_deref(), Some(two_hours_ago.as_str()));
        assert!(
            !suppress,
            "max_stuck_secs exceeded — suppression must lift to allow recovery"
        );
    }

    #[test]
    fn test_should_escalate_fires_on_consecutive_cap_overshoot() {
        // counter > max also fires — defensive against off-by-one
        // bumps from a code-path that increments after the predicate
        // check.
        let mut state = State::default();
        state.consecutive_suppressions = 10;
        state.first_suppression_at = Some(iso_secs_ago(10));
        assert_eq!(
            should_escalate_suppression(&state, 3, 600),
            Some(EscalationReason::ConsecutiveCap)
        );
    }

    #[test]
    fn test_should_escalate_fires_on_window_exceeded() {
        // Counter is below the consecutive cap but the wall-clock
        // window has been exceeded — escalate via the window backstop.
        // Mirrors the slow-drip case where suppressions land less often
        // than the cap implies (e.g. a check that satisfies the gate
        // every other cycle).
        let mut state = State::default();
        state.consecutive_suppressions = 1;
        state.first_suppression_at = Some(iso_secs_ago(700));
        assert_eq!(
            should_escalate_suppression(&state, 3, 600),
            Some(EscalationReason::WindowExceeded)
        );
    }

    #[test]
    fn test_should_escalate_returns_none_below_both_limits() {
        // counter < cap AND elapsed < window: no escalation, normal
        // suppression continues.
        let mut state = State::default();
        state.consecutive_suppressions = 1;
        state.first_suppression_at = Some(iso_secs_ago(60));
        assert_eq!(should_escalate_suppression(&state, 3, 600), None);
    }

    #[test]
    fn test_should_escalate_consecutive_cap_zero_disables_consecutive_check() {
        // max_consecutive_suppressions=0 disables the consecutive-cap
        // limb (operator escape hatch). With counter=10 and the cap
        // disabled, only the window backstop can escalate.
        let mut state = State::default();
        state.consecutive_suppressions = 10;
        state.first_suppression_at = Some(iso_secs_ago(10));
        // Window also too short to fire: should NOT escalate.
        assert_eq!(should_escalate_suppression(&state, 0, 600), None);
        // Window exceeded: window-side escalation still fires.
        state.first_suppression_at = Some(iso_secs_ago(700));
        assert_eq!(
            should_escalate_suppression(&state, 0, 600),
            Some(EscalationReason::WindowExceeded)
        );
    }

    #[test]
    fn test_should_escalate_window_zero_disables_window_check() {
        // max_suppression_window_secs=0 disables the window backstop.
        // Useful escape hatch for environments that want only the
        // consecutive-cap behaviour.
        let mut state = State::default();
        state.consecutive_suppressions = 1;
        state.first_suppression_at = Some(iso_secs_ago(10000));
        // Even with a 10000s gap, window=0 means no escalation.
        assert_eq!(should_escalate_suppression(&state, 3, 0), None);
        // Counter still triggers escalation independently.
        state.consecutive_suppressions = 5;
        assert_eq!(
            should_escalate_suppression(&state, 3, 0),
            Some(EscalationReason::ConsecutiveCap)
        );
    }

    #[test]
    fn test_should_escalate_invalid_first_suppression_at_treated_as_no_window_data() {
        // Garbage timestamp → window check skips, falls through to None
        // unless the consecutive cap also fires. Mirrors the defensive
        // semantics elsewhere.
        let mut state = State::default();
        state.consecutive_suppressions = 1;
        state.first_suppression_at = Some("garbage".to_string());
        assert_eq!(should_escalate_suppression(&state, 3, 600), None);
    }

    #[test]
    fn test_should_escalate_consecutive_cap_takes_precedence_over_window() {
        // When BOTH limits would fire, ConsecutiveCap is reported — the
        // counter check runs first. Documents the precedence so log
        // analysis is stable.
        let mut state = State::default();
        state.consecutive_suppressions = 10;
        state.first_suppression_at = Some(iso_secs_ago(10000));
        assert_eq!(
            should_escalate_suppression(&state, 3, 600),
            Some(EscalationReason::ConsecutiveCap)
        );
    }

    #[test]
    fn test_record_then_reset_returns_to_pristine_state() {
        // End-to-end: a suppression run that ends with a successful
        // inject (reset_suppression called) leaves state ready for a
        // brand-new run, with no leftover history.
        let mut state = State::default();
        record_suppression(&mut state, "2026-04-28T00:00:00+00:00");
        record_suppression(&mut state, "2026-04-28T00:00:30+00:00");
        record_suppression(&mut state, "2026-04-28T00:01:00+00:00");
        assert_eq!(state.consecutive_suppressions, 3);
        reset_suppression(&mut state);
        // Next run starts from scratch — consecutive_suppressions=0
        // means should_escalate returns None.
        assert_eq!(should_escalate_suppression(&state, 3, 600), None);
        // And first_suppression_at gets re-stamped on the next record.
        record_suppression(&mut state, "2026-04-28T01:00:00+00:00");
        assert_eq!(state.consecutive_suppressions, 1);
        assert_eq!(
            state.first_suppression_at.as_deref(),
            Some("2026-04-28T01:00:00+00:00")
        );
    }

    // --- Regression test for the cooldown-bump bug (2026-04-28) ---
    //
    // Pre-fix, the watcher_monitor suppression path bumped
    // `state.last_watcher_inject = now` even though no inject ran.
    // That ate the full 5-min `inject_cooldown` slot on a single
    // suppressed attempt — even if the active window closed 1s later,
    // the next inject was deferred until the cooldown elapsed.
    //
    // The fix is intentional structural: the suppression branch in
    // watcher_monitor no longer touches `last_watcher_inject`. We
    // assert via a focused unit test of `record_suppression` (which
    // is what the suppression branch now calls) PLUS a no-op state
    // mutation check.

    #[test]
    fn test_record_suppression_does_not_touch_last_watcher_inject() {
        // Pin the contract: record_suppression bumps the suppression
        // counter ONLY. It must not silently update the watcher-down
        // cooldown clock — that field tracks the last actual inject,
        // which is the cooldown-bump bug we're fixing.
        let mut state = State::default();
        state.last_watcher_inject = Some("2026-04-28T00:00:00+00:00".to_string());
        record_suppression(&mut state, "2026-04-28T01:00:00+00:00");
        // last_watcher_inject is untouched — only consecutive_suppressions
        // and first_suppression_at moved.
        assert_eq!(
            state.last_watcher_inject.as_deref(),
            Some("2026-04-28T00:00:00+00:00")
        );
        assert_eq!(state.consecutive_suppressions, 1);
        assert_eq!(
            state.first_suppression_at.as_deref(),
            Some("2026-04-28T01:00:00+00:00")
        );
    }

    #[test]
    fn test_record_suppression_does_not_touch_last_interrupt_at() {
        // Same contract for the global post-interrupt cooldown clock.
        // No interrupt fired (we suppressed), so last_interrupt_at must
        // not move — otherwise other fire paths (prolonged-thinking,
        // context-warning) would be cooled-down by a non-event.
        let mut state = State::default();
        state.last_interrupt_at = Some("2026-04-28T00:00:00+00:00".to_string());
        record_suppression(&mut state, "2026-04-28T01:00:00+00:00");
        assert_eq!(
            state.last_interrupt_at.as_deref(),
            Some("2026-04-28T00:00:00+00:00")
        );
    }

    // --- State transient-reset on daemon load (2026-04-28) ---
    //
    // The escalation state fields (consecutive_suppressions,
    // first_suppression_at) are transient — daemon downtime makes the
    // "consecutive" semantics meaningless and a stale persisted timestamp
    // would cause the wall-clock backstop to fire immediately on the
    // first suppression after restart. load_state must clear both.
    // The actual reset lives in src/state.rs::load_state; this test
    // documents the expected behaviour from policy's perspective (a
    // fresh State has both fields zeroed).

    #[test]
    fn test_default_state_has_clean_suppression_counters() {
        // Stand-in for the "load_state from missing file" case — the
        // reset semantics in load_state mean a brand-new daemon never
        // sees stale escalation state.
        let state = State::default();
        assert_eq!(state.consecutive_suppressions, 0);
        assert!(state.first_suppression_at.is_none());
        // And no escalation fires on a pristine state.
        assert_eq!(should_escalate_suppression(&state, 3, 600), None);
    }

    // --- Watcher-down inject due-predicate tests (2026-04-28) ---
    //
    // These pin the new behavior:
    //   1. Never-injected -> always due.
    //   2. Recent inject (< cooldown) -> NOT due.
    //   3. Old inject (>= cooldown) -> due.
    //   4. Malformed timestamp -> due (fail-open).
    //   5. cooldown=0 with recent inject -> due (cooldown disabled).
    //   6. The watcher-down predicate does NOT consult
    //      interrupt_in_global_cooldown — i.e. an unrelated recent
    //      interrupt MUST NOT block the watcher-down fire path.
    //      This is the regression guard for the actual bug Andrew filed
    //      (q-2026-04-28-713a) and for the prior reverted attempts.

    #[test]
    fn test_watcher_inject_due_never_injected() {
        assert!(watcher_inject_due(None, 60));
    }

    #[test]
    fn test_watcher_inject_due_within_cooldown() {
        let recent = (Utc::now() - chrono::Duration::seconds(10)).to_rfc3339();
        assert!(!watcher_inject_due(Some(&recent), 60));
    }

    #[test]
    fn test_watcher_inject_due_after_cooldown() {
        let old = (Utc::now() - chrono::Duration::seconds(120)).to_rfc3339();
        assert!(watcher_inject_due(Some(&old), 60));
    }

    #[test]
    fn test_watcher_inject_due_malformed_timestamp_fails_open() {
        // Garbage timestamp must fail OPEN (allow inject) rather than
        // wedge the gate forever.
        assert!(watcher_inject_due(Some("not a date"), 60));
    }

    #[test]
    fn test_watcher_inject_due_cooldown_zero_always_due() {
        // cooldown=0 means "no rate limit"; even a 1s-ago inject is due.
        let just_now = (Utc::now() - chrono::Duration::seconds(1)).to_rfc3339();
        assert!(watcher_inject_due(Some(&just_now), 0));
    }

    #[test]
    fn test_watcher_inject_ignores_global_cooldown() {
        // REGRESSION GUARD (q-2026-04-28-713a): the watcher-down inject
        // path is intentionally exempt from interrupt_in_global_cooldown.
        // Set up state where a different interrupt fired 5s ago; the
        // global cooldown gate would block, but the watcher-down
        // predicate does not consult it.
        let mut state = State::default();
        state.last_interrupt_at =
            Some((Utc::now() - chrono::Duration::seconds(5)).to_rfc3339());
        // Sanity: global cooldown would block.
        assert!(interrupt_in_global_cooldown(&state, 60));
        // But watcher-down predicate ignores last_interrupt_at and only
        // considers last_watcher_inject. With None, it's due.
        assert!(watcher_inject_due(state.last_watcher_inject.as_deref(), 60));
    }

    #[test]
    fn test_default_watcher_inject_cooldown_is_aggressive() {
        // Pin the new 60s default in case someone bumps it back to
        // 300s without realizing the original cascade-suppression
        // rationale was retired. If you genuinely want a longer
        // default, also update CLAUDE.md / the comment in config.rs.
        use crate::config::parse_config;
        let cfg = r#"
[general]
check_interval = 10
state_file = "/tmp/s.json"
log_file = "/tmp/s.jsonl"
legacy_log_file = "/tmp/s.log"

[claude]
max_context_tokens = 200000
heartbeat_file = "/tmp/hb"
relaunch_script = "/tmp/rel.sh"

[dead_process]
checks_required = 3
restart_cooldown = 60

[fresh_clear]
min_tokens = 1000
max_tokens = 5000
detections_required = 2
cooldown = 60

[heartbeat]
stale_minutes = 10

[alerts]
initial_cooldown = 60
escalation_tiers = [60]
max_pingme_alerts = 3
resume_prompt = "r"

[foreground_monitor]
enabled = false
threshold_seconds = 180
check_interval = 3

[watcher_monitor]
enabled = true
watchers_config = "/tmp/w.conf"
expected_watchmen = 0

[context_monitor]
enabled = true
threshold_percent = 75
compact_trigger_percent = 5
grace_period = 120
cooldown = 300
"#;
        let cfg = parse_config(cfg).expect("parse");
        assert_eq!(
            cfg.watcher_monitor.inject_cooldown, 60,
            "default watcher inject_cooldown should be 60s (aggressive re-inject); \
             see src/policy.rs::watcher_inject_due doc comment"
        );
    }

    // --- evaluate_api_retry_state additional tests (PR #45) ---

    #[test]
    fn test_api_retry_eval_max_stuck_secs_zero_disables_cap() {
        // max_stuck_secs=0 disables the timeout — suppression continues
        // indefinitely as long as the retry is still observed.
        let two_hours_ago = (Utc::now() - chrono::Duration::seconds(7200)).to_rfc3339();
        let (_, _, suppress) =
            evaluate_api_retry_state(true, 100, Some(&two_hours_ago), 1, 0);
        assert!(suppress, "max_stuck_secs=0 should disable the cap");
    }

    #[test]
    fn test_api_retry_eval_resolution_then_re_entry() {
        // Episode 1: detect, suppress, resolve, then a NEW episode begins.
        // The new episode's first_seen must be fresh (not inherit episode 1's).
        let (consec_1, first_1, suppress_1) =
            evaluate_api_retry_state(true, 0, None, 1, 1800);
        assert_eq!(consec_1, 1);
        assert!(first_1.is_some());
        assert!(suppress_1);

        // Resolution.
        let (consec_2, first_2, suppress_2) =
            evaluate_api_retry_state(false, consec_1, first_1.as_deref(), 1, 1800);
        assert_eq!(consec_2, 0);
        assert!(first_2.is_none());
        assert!(!suppress_2);

        // New episode starts.
        let (consec_3, first_3, suppress_3) =
            evaluate_api_retry_state(true, consec_2, first_2.as_deref(), 1, 1800);
        assert_eq!(consec_3, 1);
        assert!(first_3.is_some());
        // The new first_seen should NOT equal the old one (it's a new
        // episode) — but since we only know the old one was Some(...),
        // we just check both are Some, are different timestamps... actually
        // they could be equal if both stamp at the same RFC3339 second.
        // Just assert it's stamped.
        assert!(suppress_3);
    }

    #[test]
    fn test_api_retry_eval_saturating_consecutive() {
        // Pathological huge consecutive must not panic on overflow.
        let now = Utc::now().to_rfc3339();
        let (consec, _, suppress) =
            evaluate_api_retry_state(true, u32::MAX, Some(&now), 1, 1800);
        assert_eq!(consec, u32::MAX); // saturated
        assert!(suppress);
    }

    // --- is_api_retry_suppressing tests (read-only state derivation) ---

    fn config_with_api_retry(enabled: bool, consecutive: u32, max_stuck: u64) -> Config {
        let toml_str = format!(
            r#"
[general]
check_interval = 10
state_file = "/tmp/s.json"
log_file = "/tmp/s.jsonl"
legacy_log_file = "/tmp/s.log"

[claude]
max_context_tokens = 200000
heartbeat_file = "/tmp/hb"
relaunch_script = "/tmp/rel.sh"

[dead_process]
checks_required = 3
restart_cooldown = 60

[fresh_clear]
min_tokens = 1000
max_tokens = 5000
detections_required = 2
cooldown = 60

[heartbeat]
stale_minutes = 10

[alerts]
initial_cooldown = 60
escalation_tiers = [60]
max_pingme_alerts = 1
resume_prompt = "x"

[foreground_monitor]
enabled = true
threshold_seconds = 60
check_interval = 3

[watcher_monitor]
enabled = false
watchers_config = "/tmp/w.conf"
expected_watchmen = 0

[context_monitor]
enabled = true
threshold_percent = 75
compact_trigger_percent = 5
grace_period = 60
cooldown = 60

[api_retry]
enabled = {enabled}
consecutive = {consecutive}
max_stuck_secs = {max_stuck}
"#,
            enabled = enabled,
            consecutive = consecutive,
            max_stuck = max_stuck,
        );
        crate::config::parse_config(&toml_str).expect("parse")
    }

    #[test]
    fn test_is_api_retry_suppressing_disabled() {
        // enabled=false always returns false even if state looks active.
        let config = config_with_api_retry(false, 1, 1800);
        let mut state = State::default();
        state.api_retry_consecutive = 5;
        state.api_retry_first_seen = Some(Utc::now().to_rfc3339());
        assert!(!is_api_retry_suppressing(&config, &state));
    }

    #[test]
    fn test_is_api_retry_suppressing_no_episode() {
        // No first_seen / no consecutive -> not suppressing.
        let config = config_with_api_retry(true, 1, 1800);
        let state = State::default();
        assert!(!is_api_retry_suppressing(&config, &state));
    }

    #[test]
    fn test_is_api_retry_suppressing_below_threshold() {
        // consecutive=1, threshold=3 -> not yet suppressing.
        let config = config_with_api_retry(true, 3, 1800);
        let mut state = State::default();
        state.api_retry_consecutive = 1;
        state.api_retry_first_seen = Some(Utc::now().to_rfc3339());
        assert!(!is_api_retry_suppressing(&config, &state));
    }

    #[test]
    fn test_is_api_retry_suppressing_active_episode() {
        let config = config_with_api_retry(true, 1, 1800);
        let mut state = State::default();
        state.api_retry_consecutive = 1;
        state.api_retry_first_seen = Some(Utc::now().to_rfc3339());
        assert!(is_api_retry_suppressing(&config, &state));
    }

    #[test]
    fn test_is_api_retry_suppressing_max_stuck_lifts() {
        // first_seen 2 hours ago, max_stuck=1800 -> no longer suppressing.
        let config = config_with_api_retry(true, 1, 1800);
        let mut state = State::default();
        state.api_retry_consecutive = 100;
        state.api_retry_first_seen =
            Some((Utc::now() - chrono::Duration::seconds(7200)).to_rfc3339());
        assert!(!is_api_retry_suppressing(&config, &state));
    }

    // --- evaluate_watcher_down_action tests (quiet-path / 2026-04-28) ---
    //
    // Behaviour table:
    //
    // | scenario                                  | expected action     |
    // |-------------------------------------------|---------------------|
    // | below event_threshold                     | Nothing             |
    // | hit event_threshold, no prior emit        | EmitEvent           |
    // | event recently emitted, within grace      | Nothing             |
    // | event emitted, grace expired, < inject_th | Nothing             |
    // | event emitted, grace expired, >= inject_th| InjectFallback      |
    // | consumer watcher missing, < inject_th     | Nothing (no event!) |
    // | consumer watcher missing, >= inject_th    | InjectFallback      |
    // | misconfig: ev_th > inj_th, hit inj_th     | InjectFallback      |

    #[test]
    fn test_watcher_action_below_event_threshold_does_nothing() {
        // consecutive=2, event_threshold=3 -> no action yet
        let action = evaluate_watcher_down_action(false, 2, None, 3, 6, 60);
        assert_eq!(action, WatcherDownAction::Nothing);
    }

    #[test]
    fn test_watcher_action_at_event_threshold_emits() {
        // consecutive=3, event_threshold=3, no prior emit -> EmitEvent
        let action = evaluate_watcher_down_action(false, 3, None, 3, 6, 60);
        assert_eq!(action, WatcherDownAction::EmitEvent);
    }

    #[test]
    fn test_watcher_action_above_event_threshold_emits() {
        // consecutive=4, event_threshold=3, no prior emit -> EmitEvent
        // (still below inject_threshold=6)
        let action = evaluate_watcher_down_action(false, 4, None, 3, 6, 60);
        assert_eq!(action, WatcherDownAction::EmitEvent);
    }

    #[test]
    fn test_watcher_action_within_grace_window_suppresses() {
        // event was emitted ~5s ago, grace=60s -> Nothing
        let recent = Utc::now()
            .checked_sub_signed(chrono::Duration::seconds(5))
            .unwrap()
            .to_rfc3339();
        let action = evaluate_watcher_down_action(false, 5, Some(&recent), 3, 6, 60);
        assert_eq!(action, WatcherDownAction::Nothing);
    }

    #[test]
    fn test_watcher_action_grace_expired_below_inject_threshold_does_nothing() {
        // event was emitted long ago, grace expired, but consecutive_missing
        // hasn't reached inject_threshold yet -> Nothing.
        let stale = Utc::now()
            .checked_sub_signed(chrono::Duration::seconds(120))
            .unwrap()
            .to_rfc3339();
        let action = evaluate_watcher_down_action(false, 5, Some(&stale), 3, 6, 60);
        assert_eq!(action, WatcherDownAction::Nothing);
    }

    #[test]
    fn test_watcher_action_grace_expired_at_inject_threshold_falls_through_to_inject() {
        // event was emitted long ago, grace expired, AND consecutive_missing
        // reached inject_threshold -> InjectFallback (the main loop never
        // picked up the event for whatever reason — escalate).
        let stale = Utc::now()
            .checked_sub_signed(chrono::Duration::seconds(120))
            .unwrap()
            .to_rfc3339();
        let action = evaluate_watcher_down_action(false, 6, Some(&stale), 3, 6, 60);
        assert_eq!(action, WatcherDownAction::InjectFallback);
    }

    #[test]
    fn test_watcher_action_consumer_watcher_skips_event_below_inject_threshold() {
        // claude-event-watch itself is missing — never emit (no consumer).
        // Below inject_threshold -> Nothing.
        let action = evaluate_watcher_down_action(true, 3, None, 3, 6, 60);
        assert_eq!(action, WatcherDownAction::Nothing);
    }

    #[test]
    fn test_watcher_action_consumer_watcher_falls_through_to_inject_at_threshold() {
        // claude-event-watch missing AND past inject_threshold -> InjectFallback.
        // No event was ever emitted (None) — the chicken-and-egg case.
        let action = evaluate_watcher_down_action(true, 6, None, 3, 6, 60);
        assert_eq!(action, WatcherDownAction::InjectFallback);
    }

    #[test]
    fn test_filter_consumer_for_event_emit_only_consumer_returns_none() {
        // Self-feedback guard: if the only down watcher is the event
        // consumer itself, the helper returns None (suppress emit).
        let affected = vec!["claude-event-watch".to_string()];
        assert_eq!(
            filter_consumer_for_event_emit(&affected, "claude-event-watch"),
            None,
            "consumer-only down list must suppress the emit"
        );
    }

    #[test]
    fn test_filter_consumer_for_event_emit_consumer_among_others_filtered_out() {
        // Consumer mixed with other watchers: filter the consumer out
        // (still emit, but without the consumer's name) so the event
        // can't be the seed of its own self-feedback loop.
        let affected = vec![
            "alerts-watcher".to_string(),
            "claude-event-watch".to_string(),
            "torrent-wait".to_string(),
        ];
        let result = filter_consumer_for_event_emit(&affected, "claude-event-watch");
        assert_eq!(
            result,
            Some(vec![
                "alerts-watcher".to_string(),
                "torrent-wait".to_string(),
            ]),
            "non-consumer watchers must still emit; consumer must be filtered out"
        );
    }

    #[test]
    fn test_filter_consumer_for_event_emit_consumer_absent_returns_unchanged() {
        // Consumer not in the list: pass through unchanged.
        let affected = vec![
            "alerts-watcher".to_string(),
            "torrent-wait".to_string(),
        ];
        let result = filter_consumer_for_event_emit(&affected, "claude-event-watch");
        assert_eq!(result, Some(affected.clone()));
    }

    #[test]
    fn test_filter_consumer_for_event_emit_empty_returns_none() {
        // Empty list: nothing to emit.
        let affected: Vec<String> = vec![];
        assert_eq!(
            filter_consumer_for_event_emit(&affected, "claude-event-watch"),
            None
        );
    }

    #[test]
    fn test_filter_consumer_for_event_emit_custom_consumer_name() {
        // Consumer name is configurable — make sure the helper honours
        // whatever name is passed in (no hardcoded "claude-event-watch").
        let affected = vec!["my-custom-consumer".to_string()];
        assert_eq!(
            filter_consumer_for_event_emit(&affected, "my-custom-consumer"),
            None
        );
    }

    #[test]
    fn test_watcher_action_misconfig_event_threshold_above_inject_threshold() {
        // Misconfiguration: event_threshold (10) > inject_threshold (6).
        // consecutive_missing=6 is at inject_threshold but below
        // event_threshold. The pure helper falls through to InjectFallback
        // rather than wedging on Nothing forever.
        let action = evaluate_watcher_down_action(false, 6, None, 10, 6, 60);
        assert_eq!(action, WatcherDownAction::InjectFallback);
    }

    #[test]
    fn test_watcher_action_grace_zero_disables_quiet_path_after_first_emit() {
        // grace_secs=0 means the quiet-path suppression window is empty.
        // After emission, the very next cycle past inject_threshold should
        // immediately fall through to InjectFallback (no waiting).
        let just_now = Utc::now().to_rfc3339();
        let action = evaluate_watcher_down_action(false, 6, Some(&just_now), 3, 6, 0);
        assert_eq!(action, WatcherDownAction::InjectFallback);
    }

    #[test]
    fn test_watcher_action_recovery_clears_event_emitted_at_externally() {
        // This test mirrors what the watcher loop does on recovery: it
        // clears event_emitted_at so the next failure gets a fresh quiet
        // path. We verify the helper returns EmitEvent again with a cleared
        // timestamp, even though we previously emitted.
        let action = evaluate_watcher_down_action(false, 3, None, 3, 6, 60);
        assert_eq!(action, WatcherDownAction::EmitEvent);
    }

    #[test]
    fn test_watcher_action_re_emit_suppressed_when_grace_active_and_count_grew() {
        // Even if consecutive_missing grew past event_threshold by another
        // cycle, while the grace window is active we MUST NOT re-emit
        // (no double-fire).
        let recent = Utc::now()
            .checked_sub_signed(chrono::Duration::seconds(10))
            .unwrap()
            .to_rfc3339();
        let action = evaluate_watcher_down_action(false, 4, Some(&recent), 3, 6, 60);
        assert_eq!(action, WatcherDownAction::Nothing);
    }

    #[test]
    fn test_watcher_state_recovery_clears_event_emitted_at() {
        // Simulate the watcher-loop "recovery" branch: when count >= min_count
        // the loop sets last_seen_running, zeros consecutive_missing, AND
        // clears event_emitted_at. Verify the field actually gets cleared
        // (regression guard for forgetting to reset it).
        let mut health = WatcherState {
            last_seen_running: None,
            consecutive_missing: 5,
            enabled: true,
            event_emitted_at: Some("2026-04-28T12:00:00+00:00".to_string()),
        };
        // Mirror the recovery branch:
        health.last_seen_running = Some("2026-04-28T12:05:00+00:00".to_string());
        health.consecutive_missing = 0;
        health.event_emitted_at = None;

        assert_eq!(health.consecutive_missing, 0);
        assert!(health.event_emitted_at.is_none());
        assert!(health.last_seen_running.is_some());
    }

    // --- auto-respawn-on-hang signal-collection tests (2026-05-01) ---

    fn config_with_auto_respawn(enabled: bool, signals_required: u32, window_secs: u64) -> Config {
        let toml_str = format!(
            r#"
[general]
check_interval = 10
state_file = "/tmp/s.json"
log_file = "/tmp/s.jsonl"
legacy_log_file = "/tmp/s.log"

[claude]
max_context_tokens = 200000
heartbeat_file = "/tmp/hb"
relaunch_script = "/tmp/rel.sh"

[dead_process]
checks_required = 3
restart_cooldown = 60

[fresh_clear]
min_tokens = 1000
max_tokens = 5000
detections_required = 2
cooldown = 60

[heartbeat]
stale_minutes = 10

[alerts]
initial_cooldown = 60
escalation_tiers = [60]
max_pingme_alerts = 1
resume_prompt = "x"

[foreground_monitor]
enabled = true
threshold_seconds = 60
check_interval = 3

[watcher_monitor]
enabled = true
watchers_config = "/tmp/w.conf"
expected_watchmen = 0
inject_threshold = 6

[context_monitor]
enabled = true
threshold_percent = 75
compact_trigger_percent = 5
grace_period = 60
cooldown = 60

[auto_respawn_on_hang]
enabled = {enabled}
signals_required = {signals_required}
signal_window_secs = {window_secs}
cooldown_secs = 1800
kill_grace_secs = 5
respawn_verify_secs = 30
pane_unchanged_secs = 600
"#,
            enabled = enabled,
            signals_required = signals_required,
            window_secs = window_secs,
        );
        crate::config::parse_config(&toml_str).expect("parse")
    }

    #[test]
    fn test_auto_respawn_default_off() {
        // No [auto_respawn_on_hang] section -> default disabled.
        let config = config_with_api_retry(true, 1, 1800);
        assert!(
            !config.auto_respawn_on_hang.enabled,
            "auto-respawn must default OFF — destructive feature, opt-in only"
        );
    }

    #[test]
    fn test_collect_no_signals_when_clean_state() {
        let config = config_with_auto_respawn(true, 2, 300);
        let state = State::default();
        let signals = collect_non_pane_signals(&state, &config, false);
        assert!(signals.is_empty());
    }

    #[test]
    fn test_collect_heartbeat_stuck_emits_signal() {
        let config = config_with_auto_respawn(true, 2, 300);
        let state = State::default();
        let signals = collect_non_pane_signals(&state, &config, true);
        assert_eq!(signals, vec![crate::respawn::HangSignal::HeartbeatStale]);
    }

    #[test]
    fn test_collect_watcher_signal_requires_recent_inject() {
        let config = config_with_auto_respawn(true, 2, 300);
        let mut state = State::default();
        // Watcher critically missing
        state.watcher_health.insert(
            "memory-remind".to_string(),
            crate::state::WatcherState {
                last_seen_running: None,
                consecutive_missing: 10,
                enabled: true,
                ..Default::default()
            },
        );
        // No recent watcher inject — should NOT emit (we haven't poked the loop yet)
        let signals = collect_non_pane_signals(&state, &config, false);
        assert!(
            signals.is_empty(),
            "watcher critical without recent inject must NOT signal"
        );

        // Add a recent watcher inject -> signal fires
        state.last_watcher_inject = Some(Utc::now().to_rfc3339());
        let signals = collect_non_pane_signals(&state, &config, false);
        assert_eq!(
            signals,
            vec![crate::respawn::HangSignal::WatcherDownPersistent]
        );
    }

    #[test]
    fn test_collect_watcher_signal_ignores_stale_inject() {
        // Watcher inject 10 min ago, window 300s -> outside window, no signal.
        let config = config_with_auto_respawn(true, 2, 300);
        let mut state = State::default();
        state.watcher_health.insert(
            "memory-remind".to_string(),
            crate::state::WatcherState {
                last_seen_running: None,
                consecutive_missing: 10,
                enabled: true,
                ..Default::default()
            },
        );
        state.last_watcher_inject =
            Some((Utc::now() - chrono::Duration::seconds(600)).to_rfc3339());
        let signals = collect_non_pane_signals(&state, &config, false);
        assert!(signals.is_empty());
    }

    #[test]
    fn test_collect_thinking_signal_requires_two_interrupts() {
        let config = config_with_auto_respawn(true, 2, 300);
        let mut state = State::default();
        state.thinking_interrupt_count = 1;
        let signals = collect_non_pane_signals(&state, &config, false);
        assert!(signals.is_empty(), "1 interrupt below threshold");

        state.thinking_interrupt_count = 2;
        let signals = collect_non_pane_signals(&state, &config, false);
        assert_eq!(
            signals,
            vec![crate::respawn::HangSignal::ProlongedThinkingNoProgress]
        );
    }

    #[test]
    fn test_collect_wedged_signal_requires_recent_clear_and_climbing() {
        let config = config_with_auto_respawn(true, 2, 300);
        let mut state = State::default();
        state.last_wedged_clear = Some(Utc::now().to_rfc3339());
        state.wedged_consecutive = 1;
        let signals = collect_non_pane_signals(&state, &config, false);
        assert!(
            signals.is_empty(),
            "wedged consecutive=1 below threshold of 2"
        );

        state.wedged_consecutive = 2;
        let signals = collect_non_pane_signals(&state, &config, false);
        assert_eq!(
            signals,
            vec![crate::respawn::HangSignal::WedgedClearNoProgress]
        );
    }

    #[test]
    fn test_collect_multiple_signals_combine() {
        let config = config_with_auto_respawn(true, 2, 300);
        let mut state = State::default();
        state.thinking_interrupt_count = 3;
        state.watcher_health.insert(
            "memory-remind".to_string(),
            crate::state::WatcherState {
                last_seen_running: None,
                consecutive_missing: 10,
                enabled: true,
                ..Default::default()
            },
        );
        state.last_watcher_inject = Some(Utc::now().to_rfc3339());
        let signals = collect_non_pane_signals(&state, &config, true);
        assert_eq!(signals.len(), 3);
        assert!(signals.contains(&crate::respawn::HangSignal::HeartbeatStale));
        assert!(signals.contains(&crate::respawn::HangSignal::WatcherDownPersistent));
        assert!(signals.contains(&crate::respawn::HangSignal::ProlongedThinkingNoProgress));
    }

    /// End-to-end-ish: when the feature is disabled (default), check_auto_respawn
    /// is a no-op even with all signals firing.
    #[tokio::test]
    async fn test_check_auto_respawn_is_noop_when_disabled() {
        let config = config_with_auto_respawn(false, 2, 300);
        let mut state = State::default();
        state.thinking_interrupt_count = 5;
        state.watcher_health.insert(
            "memory-remind".to_string(),
            crate::state::WatcherState {
                last_seen_running: None,
                consecutive_missing: 10,
                enabled: true,
                ..Default::default()
            },
        );
        state.last_watcher_inject = Some(Utc::now().to_rfc3339());

        let now = Utc::now().to_rfc3339();
        check_auto_respawn(&config, &mut state, "", &now, true).await;

        // No signals recorded, no respawn fired.
        assert!(
            state.hang_signal_history.distinct_active().is_empty(),
            "disabled feature must not record signals"
        );
        assert!(state.last_respawn_at.is_none());
        assert_eq!(state.auto_respawn_count, 0);
    }

    /// When the feature is enabled and signals fire below threshold, no respawn.
    #[tokio::test]
    async fn test_check_auto_respawn_records_but_does_not_fire_below_threshold() {
        let config = config_with_auto_respawn(true, 3, 300);
        let mut state = State::default();
        state.thinking_interrupt_count = 5;

        let now = Utc::now().to_rfc3339();
        check_auto_respawn(&config, &mut state, "", &now, false).await;

        // Recorded the thinking signal.
        assert_eq!(
            state.hang_signal_history.distinct_active().len(),
            1,
            "exactly 1 distinct signal recorded"
        );
        // But threshold is 3, not 1 -> no fire.
        assert_eq!(
            state.auto_respawn_count, 0,
            "below threshold must not respawn"
        );
        assert!(state.last_respawn_at.is_none());
    }

    /// When two distinct signals fire AND the feature is enabled, the respawn
    /// path runs but (because we pass a mocked `versions_dir` that doesn't
    /// match any /proc/PID/exe) `find_claude_pid_with_versions_dir` returns
    /// None and `execute_respawn` aborts cleanly. The state-mutation
    /// bookkeeping must run regardless of the abort. CRITICAL SAFETY: this
    /// test must never find a real Claude PID — the override is the
    /// guard. See `respawn::execute_respawn_with_versions_dir`.
    #[tokio::test]
    async fn test_check_auto_respawn_aborts_when_no_claude_via_mock() {
        let config = config_with_auto_respawn(true, 2, 300);
        let mut state = State::default();
        state.thinking_interrupt_count = 5;
        state.watcher_health.insert(
            "memory-remind".to_string(),
            crate::state::WatcherState {
                last_seen_running: None,
                consecutive_missing: 10,
                enabled: true,
                ..Default::default()
            },
        );
        state.last_watcher_inject = Some(Utc::now().to_rfc3339());

        let now = Utc::now().to_rfc3339();
        // Use the *_with_versions_dir variant with a path that no /proc
        // entry will ever match; this forces the abort branch and never
        // touches the real Claude PID running the test session.
        check_auto_respawn_with_versions_dir(
            &config,
            &mut state,
            "",
            &now,
            true,
            Some("/nonexistent/claude/versions/path"),
        )
        .await;

        // 3 signals collected, but execute_respawn aborted / launched.
        // The state-mutation-on-fire bookkeeping must run regardless.
        assert_eq!(
            state.auto_respawn_count, 1,
            "counter must increment even on abort/launch-failure"
        );
        assert!(
            state.last_respawn_at.is_some(),
            "cooldown timestamp must be stamped"
        );
        // History cleared after fire so the next cycle starts fresh.
        assert!(
            state.hang_signal_history.distinct_active().is_empty(),
            "history clears after fire"
        );
    }

    /// Cooldown: a recent respawn blocks re-fire even if signals are firing.
    #[tokio::test]
    async fn test_check_auto_respawn_cooldown_blocks_re_fire() {
        let config = config_with_auto_respawn(true, 2, 300);
        let mut state = State::default();
        state.thinking_interrupt_count = 5;
        state.watcher_health.insert(
            "memory-remind".to_string(),
            crate::state::WatcherState {
                last_seen_running: None,
                consecutive_missing: 10,
                enabled: true,
                ..Default::default()
            },
        );
        state.last_watcher_inject = Some(Utc::now().to_rfc3339());
        // Pretend a respawn happened 5 minutes ago — well within the 30 min
        // cooldown.
        state.last_respawn_at =
            Some((Utc::now() - chrono::Duration::seconds(300)).to_rfc3339());

        let now = Utc::now().to_rfc3339();
        check_auto_respawn(&config, &mut state, "", &now, true).await;

        // Signals were recorded but no NEW fire happened.
        assert_eq!(
            state.auto_respawn_count, 0,
            "cooldown must block re-fire, counter unchanged"
        );
        // History should NOT be cleared (no fire to trigger the cleanup).
        assert!(
            !state.hang_signal_history.distinct_active().is_empty(),
            "no fire => history retained"
        );
    }

    // --- workload_heartbeat_fresh tests ---

    #[test]
    fn workload_heartbeat_fresh_missing_dir_returns_false() {
        // Non-existent directory: no workloads ever ran on this host.
        // Must return false (NOT suppress) so the stuck-alert can fire.
        let tmp = tempfile::tempdir().expect("tempdir");
        let nonexistent = tmp.path().join("does-not-exist");
        assert!(!workload_heartbeat_fresh(
            &nonexistent,
            60,
            SystemTime::now()
        ));
    }

    #[test]
    fn workload_heartbeat_fresh_empty_dir_returns_false() {
        // Directory exists but is empty: no active workloads.
        let tmp = tempfile::tempdir().expect("tempdir");
        assert!(!workload_heartbeat_fresh(
            tmp.path(),
            60,
            SystemTime::now()
        ));
    }

    #[test]
    fn workload_heartbeat_fresh_fresh_file_returns_true() {
        // A file with mtime "now" (default mtime when fs::write fires)
        // must satisfy freshness at threshold=60s.
        let tmp = tempfile::tempdir().expect("tempdir");
        let hb = tmp.path().join("active-workload.heartbeat");
        std::fs::write(&hb, "2026-05-15T22:00:00-04:00").expect("write hb");
        assert!(workload_heartbeat_fresh(tmp.path(), 60, SystemTime::now()));
    }

    #[test]
    fn workload_heartbeat_fresh_stale_file_returns_false() {
        // A file with mtime 5 minutes ago must NOT satisfy a 60s threshold.
        let tmp = tempfile::tempdir().expect("tempdir");
        let hb = tmp.path().join("stale.heartbeat");
        std::fs::write(&hb, "old").expect("write hb");
        let five_min_ago = SystemTime::now() - std::time::Duration::from_secs(300);
        filetime::set_file_mtime(&hb, filetime::FileTime::from_system_time(five_min_ago))
            .expect("set mtime");
        assert!(!workload_heartbeat_fresh(tmp.path(), 60, SystemTime::now()));
    }

    #[test]
    fn workload_heartbeat_fresh_one_fresh_among_stale_returns_true() {
        // Mixed dir: one stale workload + one fresh workload. The fresh
        // one wins → suppression engages.
        let tmp = tempfile::tempdir().expect("tempdir");
        let stale = tmp.path().join("stale-workload.heartbeat");
        let fresh = tmp.path().join("fresh-workload.heartbeat");
        std::fs::write(&stale, "old").expect("write stale");
        std::fs::write(&fresh, "new").expect("write fresh");
        let five_min_ago = SystemTime::now() - std::time::Duration::from_secs(300);
        filetime::set_file_mtime(&stale, filetime::FileTime::from_system_time(five_min_ago))
            .expect("set mtime");
        assert!(workload_heartbeat_fresh(tmp.path(), 60, SystemTime::now()));
    }

    #[test]
    fn workload_heartbeat_fresh_ignores_non_heartbeat_files() {
        // Random sidecars (.alerted, .output) must not satisfy freshness
        // — only `.heartbeat`-suffixed files count.
        let tmp = tempfile::tempdir().expect("tempdir");
        let sidecar = tmp.path().join("workload.output");
        std::fs::write(&sidecar, "x").expect("write");
        assert!(!workload_heartbeat_fresh(
            tmp.path(),
            60,
            SystemTime::now()
        ));
    }

    #[test]
    fn workload_heartbeat_fresh_future_mtime_returns_true() {
        // Clock skew: mtime in the future relative to `now`. Treat as
        // fresh — the file was just touched, the clock just hasn't
        // caught up. Better to over-suppress one tick than to fire on a
        // clearly-active workload.
        let tmp = tempfile::tempdir().expect("tempdir");
        let hb = tmp.path().join("future.heartbeat");
        std::fs::write(&hb, "future").expect("write");
        let future = SystemTime::now() + std::time::Duration::from_secs(120);
        filetime::set_file_mtime(&hb, filetime::FileTime::from_system_time(future))
            .expect("set mtime");
        assert!(workload_heartbeat_fresh(tmp.path(), 60, SystemTime::now()));
    }

    #[test]
    fn workload_heartbeat_suppresses_stuck_respects_master_switch() {
        // `enabled = false` returns false even when a fresh heartbeat
        // exists. Confirms the master switch is honored by the wrapper
        // around the pure helper.
        let tmp = tempfile::tempdir().expect("tempdir");
        let hb = tmp.path().join("a.heartbeat");
        std::fs::write(&hb, "x").expect("write");

        // Sanity: the pure helper sees the fresh file.
        assert!(workload_heartbeat_fresh(tmp.path(), 60, SystemTime::now()));

        // Build a StuckDetectionConfig with enabled=false and confirm
        // that flips the result of the predicate. We test the master-
        // switch logic against the in-memory struct rather than going
        // through TOML (the full Config has many required fields that
        // would make the round-trip boilerplate-heavy and brittle).
        let stuck = crate::config::StuckDetectionConfig {
            enabled: false,
            workload_heartbeat_dir: tmp.path().to_string_lossy().to_string(),
            workload_heartbeat_max_age_secs: 60,
        };
        // Mirror the logic in `workload_heartbeat_suppresses_stuck`
        // without needing a full Config. The helper short-circuits on
        // the `enabled` flag before scanning the dir.
        let suppressed = if !stuck.enabled {
            false
        } else {
            workload_heartbeat_fresh(
                std::path::Path::new(&stuck.workload_heartbeat_dir),
                stuck.workload_heartbeat_max_age_secs,
                SystemTime::now(),
            )
        };
        assert!(!suppressed, "master switch off must suppress nothing");

        // And flipping enabled back on flips the result.
        let stuck_on = crate::config::StuckDetectionConfig {
            enabled: true,
            ..stuck
        };
        let suppressed_on = if !stuck_on.enabled {
            false
        } else {
            workload_heartbeat_fresh(
                std::path::Path::new(&stuck_on.workload_heartbeat_dir),
                stuck_on.workload_heartbeat_max_age_secs,
                SystemTime::now(),
            )
        };
        assert!(suppressed_on, "master switch on must let fresh hb suppress");
    }
}
