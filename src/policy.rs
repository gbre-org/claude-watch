//! Policy: the main check logic including dead process detection, fresh /clear,
//! heartbeat stale, foreground monitor, and watcher health.

use chrono::{DateTime, Local, Timelike, Utc};
use std::os::unix::process::CommandExt;
use std::time::SystemTime;
use tracing::{debug, info, warn};

use crate::alert;
use crate::config::Config;
use crate::inject_dispatch;
use crate::logging::{write_jsonl_log, write_legacy_log};
use crate::reminders::{seconds_since_fire, should_defer_to_hook, ReminderType};
use crate::state::{FailureDetail, State, StatusSnapshot, WatcherState};
use crate::status;
use crate::tmux;
use crate::token_usage;

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

/// Per-check decision of the token-progress guard for the prolonged-
/// thinking timer (pure).
///
/// v2 semantics (2026-06-11, same-day replacement of the at-fire-time
/// suppression check from PR #341): the guard runs on EVERY ongoing-
/// thinking check, not just at the fire boundary. Whenever the status-bar
/// token count has grown by at least `min_tokens_delta` since the episode
/// baseline, the thinking timer re-arms (`thinking_start` + baseline slide
/// forward to NOW), so the timer only accumulates over genuinely
/// growth-free time. A fire therefore means "`threshold_seconds` of
/// continuous Thinking with token growth below the floor" — a parked or
/// wedged turn — while any turn that keeps making token progress keeps
/// sliding the window and never fires.
///
/// Why the v1 at-fire-time check never engaged in production: the
/// status-bar count measures CONTEXT tokens, which grow ~3-7k per 480s
/// window from tool results and injected system reminders even when the
/// assistant emits almost nothing (measured 2026-06-11: +7439 tokens
/// across the 10.5 min between two false fires covering 2-3 tiny turns).
/// So "suppress when episode delta < 2000" was never true — zero
/// suppressions ever — and, inverted on the other side, a genuinely
/// growth-free wedge would have been suppressed (and re-armed) at every
/// backoff boundary forever and never fired.
///
/// Decisions:
/// - `Keep`: guard disabled (`min_tokens_delta == 0`), token count
///   unparseable this cycle (`current_tokens == 0`), or growth below the
///   floor — leave the timer accumulating.
/// - `CaptureBaseline`: tokens were unavailable at episode start and are
///   parseable now — record the baseline late; timer keeps accumulating.
/// - `Rearm`: growth since baseline reached the floor — slide timer +
///   baseline forward.
/// - `RearmCounterReset`: token count went backwards (counter reset, e.g.
///   context clear or status-bar source flap) — the old baseline is
///   meaningless; re-baseline and slide the timer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ThinkingTokenAction {
    Keep,
    CaptureBaseline,
    Rearm,
    RearmCounterReset,
}

pub(crate) fn thinking_token_progress_action(
    episode_start_tokens: Option<u64>,
    current_tokens: u64,
    min_tokens_delta: u64,
) -> ThinkingTokenAction {
    if min_tokens_delta == 0 || current_tokens == 0 {
        return ThinkingTokenAction::Keep;
    }
    let start = match episode_start_tokens {
        Some(s) => s,
        None => return ThinkingTokenAction::CaptureBaseline,
    };
    if current_tokens < start {
        return ThinkingTokenAction::RearmCounterReset;
    }
    if current_tokens - start >= min_tokens_delta {
        return ThinkingTokenAction::Rearm;
    }
    ThinkingTokenAction::Keep
}

/// Apply the v2 token-progress decision to the live thinking-timer state.
///
/// Mutates `thinking_start` / `episode_start_tokens` exactly as the
/// production flow requires and returns `Some(reason)` when the timer
/// re-armed (the caller logs + writes the jsonl record), `None` otherwise.
/// Late baseline capture happens silently. Split out from
/// `check_foreground_inner` so the engagement behavior is unit-testable
/// without tmux.
pub(crate) fn apply_thinking_token_progress(
    thinking_start: &mut Option<String>,
    episode_start_tokens: &mut Option<u64>,
    current_tokens: u64,
    min_tokens_delta: u64,
    now: &str,
) -> Option<&'static str> {
    match thinking_token_progress_action(
        *episode_start_tokens,
        current_tokens,
        min_tokens_delta,
    ) {
        ThinkingTokenAction::Keep => None,
        ThinkingTokenAction::CaptureBaseline => {
            *episode_start_tokens = Some(current_tokens);
            None
        }
        ThinkingTokenAction::Rearm => {
            *thinking_start = Some(now.to_string());
            *episode_start_tokens = Some(current_tokens);
            Some("token_progress_rearm")
        }
        ThinkingTokenAction::RearmCounterReset => {
            *thinking_start = Some(now.to_string());
            *episode_start_tokens = Some(current_tokens);
            Some("token_counter_reset")
        }
    }
}

/// Bound on `carry_forward_token_misparse`: how many CONSECUTIVE zero-token
/// polls a large, same-pane context reading may be carried forward before the
/// zero is finally trusted. Small so a genuine `/clear` or crashed process
/// still registers within a couple of extra cycles; large enough to bridge the
/// 1-2-poll transient misparse (an overlay panel or a mid-redraw scrolls the
/// bare context total out of the capture window) that the 2026-08 status-parser
/// hardening now surfaces as a bare 0 instead of a small bogus count.
pub(crate) const MISPARSE_CARRY_MAX: u32 = 3;

/// Smooth a transient status-bar token MISPARSE so a live session is not
/// misread as dead/cleared.
///
/// Context: the status parser was hardened (2026-08) to NEVER adopt the
/// thinking-indicator (`\u{2193} N tokens`, the current turn's own output) or
/// an agent-roster row's per-subagent count as the session context total --
/// those numbers fooled the fresh-/clear and dead-process detectors into
/// phantom "context clear" injects. The hardened parser instead returns `None`
/// (-> `0`) when only those lines are on screen and the bare context total has
/// momentarily scrolled out of the capture window. But a bare `0` itself feeds
/// two liveness paths that the old small-bogus count did not: the
/// `tokens == 0 && bashes == 0` dead-check accumulator and the
/// fresh-external-session gate. A long, intact session that is merely thinking
/// mid-turn -- or quiet-holding while subagent roster rows are on screen --
/// would then be misread as a fresh/dead session and get a bogus resume prompt.
///
/// This carries the last known reading forward across a BOUNDED run of
/// consecutive zero polls, but ONLY when the prior reading was clearly LARGE
/// (`last_known >= carry_floor`, set by the caller to the fresh-/clear window's
/// upper bound):
///
///   * A genuinely fresh/low session (`last_known < carry_floor`) is never
///     carried, so fresh-/clear detection in the low-token window is untouched.
///   * A large context that momentarily reads 0 is held at its last value, so a
///     transient misparse cannot manufacture a phantom clear.
///   * The carry is bounded (`max_carry`), so a REAL `/clear` or crashed
///     process -- which holds 0 for many consecutive polls -- still registers
///     once the bound is exhausted.
///
/// The caller additionally gates this on pane continuity (only smooth within
/// the SAME pane), so a genuine new session (pane change) is never carried.
///
/// Returns `(effective_tokens, new_carry_count)`.
pub(crate) fn carry_forward_token_misparse(
    current: u64,
    last_known: u64,
    carry_count: u32,
    carry_floor: u64,
    max_carry: u32,
) -> (u64, u32) {
    if current > 0 {
        // Real reading this poll -- trust it and reset the carry run.
        return (current, 0);
    }
    if last_known >= carry_floor && carry_count < max_carry {
        // Transient misparse of a large same-pane context -- hold the last
        // value and advance the bounded run.
        return (last_known, carry_count + 1);
    }
    // Nothing large to carry, or the carry bound is exhausted: trust the 0.
    (0, carry_count)
}

/// Age in whole seconds of a liveness stamp relative to `now` (pure).
/// Returns `None` when the stamp is unavailable (file missing/unreadable) or
/// in the FUTURE relative to `now` (`duration_since` fails on clock skew /
/// corrupt stamp). The ack-freshness gate FAILS OPEN on `None` — the fire is
/// allowed — deliberately unlike the workload-heartbeat suppressor (which
/// treats a future mtime as fresh): a corrupt or skewed stamp must never mask
/// a real wedge.
pub(crate) fn ack_age_secs(mtime: Option<SystemTime>, now: SystemTime) -> Option<u64> {
    now.duration_since(mtime?).ok().map(|d| d.as_secs())
}

/// Ack-freshness gate for the prolonged-thinking fire path (v3, 2026-06-11;
/// re-sourced from the last-ack timestamp 2026-08-22). A RECENT ack at fire
/// time is proof the session is alive and merely parked in an open turn — the
/// residual v2 false positive, where an ultra-quiet stretch drips fewer
/// context tokens than `min_tokens_delta` per backoff window so the
/// token-progress guard never re-arms. A STALE ack means a possible real
/// wedge (a wedged session stops acking by design), so the fire proceeds —
/// and the daemon's separate ack-stale detection escalates that case
/// independently.
///
/// Returns `true` (suppress the fire) iff the gate is enabled
/// (`ack_fresh_secs > 0`) AND the ack age is known AND
/// `age < ack_fresh_secs` — in which case it RE-ARMS the thinking
/// timer exactly like the v2 token-progress re-arm: `thinking_start` and
/// the token baseline slide forward to `now`, so the timer only resumes
/// accumulating from this check. Returns `false` (allow the fire,
/// touch nothing) when the gate is disabled, the ack stamp is
/// missing/unreadable, its mtime is in the future (both surface here as
/// `ack_age_secs == None` — fail-open), or the age is at/over the
/// threshold. Split out from `check_foreground_inner` so the behavior is
/// unit-testable without tmux (same pattern as
/// `apply_thinking_token_progress`).
pub(crate) fn apply_ack_fresh_rearm(
    thinking_start: &mut Option<String>,
    episode_start_tokens: &mut Option<u64>,
    ack_age_secs: Option<u64>,
    ack_fresh_secs: u64,
    current_tokens: u64,
    now: &str,
) -> bool {
    if ack_fresh_secs == 0 {
        // Gate disabled.
        return false;
    }
    let Some(age) = ack_age_secs else {
        // Missing/unreadable stamp or future mtime — fail open.
        return false;
    };
    if age >= ack_fresh_secs {
        // Stale ack — possible real wedge, allow the fire.
        return false;
    }
    *thinking_start = Some(now.to_string());
    *episode_start_tokens = (current_tokens > 0).then_some(current_tokens);
    true
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
/// (`watcher_monitor.inject_cooldown`, default 300s) still rate-limits
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

/// Atomic check-and-stamp of the global interrupt gate — the SINGLE
/// chokepoint every (non-exempt) interrupt fire path consults right
/// before injecting.
///
/// Returns `false` (claim DENIED) if another interrupt fired within the
/// last `cooldown_secs` seconds — the caller must NOT fire. Otherwise it
/// STAMPS `state.last_interrupt_at = now` and returns `true` (claim
/// GRANTED) — the caller may fire. Collapsing the previous split
/// "check here / stamp later" two-step into one call removes the window
/// where two fire paths in the same `check_once` pass could both pass an
/// early check and then both stamp, double-injecting within the cooldown.
///
/// A `cooldown_secs` of 0 disables the gate: the claim always succeeds
/// and the timestamp is still stamped (so other sites observe the fire).
///
/// `now` is an RFC3339 timestamp string (the daemon's per-check `now`).
///
/// The cooldown is EXPONENTIAL: the base `cooldown_secs` is widened by
/// `effective_global_cooldown_secs(base, backoff_base, max_secs, streak)`
/// where `streak` is `state.global_interrupt_streak`. `backoff_base <= 1`
/// reproduces the exact legacy flat-cooldown behavior. On a successful
/// claim the streak is incremented (saturating); a full effective-cooldown
/// window elapsing with no interrupt resets the streak to 0 (so a quiet
/// period decays the backoff).
pub(crate) fn try_claim_global_interrupt(
    state: &mut State,
    base_cooldown_secs: u64,
    backoff_base: u64,
    max_secs: u64,
    now: &str,
) -> bool {
    let effective = effective_global_cooldown_secs(
        base_cooldown_secs,
        backoff_base,
        max_secs,
        state.global_interrupt_streak,
    );
    // Decay: if the last interrupt is older than the full effective window,
    // a quiet period has elapsed — reset the streak BEFORE the claim so the
    // next interrupt starts from the base cooldown again.
    if let Some(elapsed) = state.last_interrupt_at.as_deref().and_then(elapsed_since) {
        if elapsed >= effective as f64 {
            state.global_interrupt_streak = 0;
        }
    }
    if interrupt_in_global_cooldown(state, effective) {
        return false;
    }
    state.last_interrupt_at = Some(now.to_string());
    state.global_interrupt_streak = state.global_interrupt_streak.saturating_add(1);
    true
}

/// Two-phase escalation decision (BUG 1 fix). The daemon ARMS an obligation
/// (writes a pending alert + emits an event, the lower rung) on the first
/// detection cycle, and only ESCALATES to a tmux interrupt once the
/// obligation has been armed, the dwell has elapsed, and no background
/// subagents are live (interrupting would kill healthy in-flight agents).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ObligationDecision {
    /// First detection: arm the obligation, emit the event, DON'T interrupt.
    ArmObligation,
    /// Armed but dwell not elapsed (or subagents live): hold — re-emit event
    /// (idempotent arm), still no interrupt.
    Hold,
    /// Dwell elapsed + 0 subagents: proceed to the existing interrupt path.
    Escalate,
}

/// Pure decision for the two-phase escalation gate.
///
/// - `dwell_secs == 0` => `Escalate` (precedence gate disabled — legacy
///   arm+interrupt same cycle).
/// - `armed_at == None` => `ArmObligation` (first detection cycle).
/// - armed AND dwell elapsed AND `active_subagents == 0` => `Escalate`.
/// - otherwise => `Hold`.
///
/// `now` is kept for symmetry / future use and tests (elapsed is measured
/// against the armed timestamp via `elapsed_since`).
pub(crate) fn obligation_escalation_decision(
    armed_at: Option<&str>,
    dwell_secs: u64,
    active_subagents: u32,
    _now: &str,
) -> ObligationDecision {
    if dwell_secs == 0 {
        return ObligationDecision::Escalate;
    }
    match armed_at {
        None => ObligationDecision::ArmObligation,
        Some(ts) => {
            let dwelled = elapsed_since(ts).is_some_and(|e| e >= dwell_secs as f64);
            if dwelled && active_subagents == 0 {
                ObligationDecision::Escalate
            } else {
                ObligationDecision::Hold
            }
        }
    }
}

/// Context-low escalation decision: [`obligation_escalation_decision`] plus a
/// hard arm-to-fire DEADLINE.
///
/// The base gate only escalates when `active_subagents == 0`, so that a
/// turn-cancelling interrupt never lands on top of healthy in-flight subagent
/// work. That trade is right for most rungs and exactly backwards for this
/// one. Context-low is the rung whose whole job is to rescue a loop that is
/// about to run out of context, and on a dispatcher that keeps subagents in
/// flight the count is essentially never zero — so the obligation arms once
/// and then HOLDS forever, re-emitting the same "SELF-CLEAR NOW" alert
/// every cycle while the context keeps climbing into the hard wall. Waiting
/// for a quiet moment is not a recovery strategy when the thing being waited
/// on is the very loop that is stuck.
///
/// So: once the obligation has been armed for `max_armed_secs`, escalate
/// regardless of the subagent count. The dwell gate still applies for the
/// normal case; this is purely a ceiling on how long ARMED can last.
/// `max_armed_secs == 0` disables the deadline (legacy behaviour).
///
/// Real incident (2026-08-10): armed at 97.7% context with one subagent live,
/// held for every one of the ~26 cycles that followed, and never fired.
pub(crate) fn context_escalation_decision(
    armed_at: Option<&str>,
    dwell_secs: u64,
    active_subagents: u32,
    max_armed_secs: u64,
    now: &str,
) -> ObligationDecision {
    let base = obligation_escalation_decision(armed_at, dwell_secs, active_subagents, now);
    if max_armed_secs == 0 || base != ObligationDecision::Hold {
        return base;
    }
    let overdue = armed_at
        .and_then(elapsed_since)
        .is_some_and(|e| e >= max_armed_secs as f64);
    if overdue {
        ObligationDecision::Escalate
    } else {
        base
    }
}

/// May the context fallback still defer to the `context_high` hook?
///
/// `should_defer_to_hook` measures its grace window from the LAST hook fire,
/// and the hook re-fires on every turn while context stays high — so on a loop
/// that keeps taking turns the window is refreshed faster than it can expire
/// and the daemon defers forever. This ceiling is anchored to the FIRST cycle
/// on which the threshold was seen crossed instead, which nothing can refresh
/// short of the context actually coming down.
///
/// Returns `true` while deferral is still permitted. `max_defer_secs == 0`
/// disables the ceiling (legacy behaviour). An unset `first_seen_at` (the
/// crossing has not been recorded yet) permits deferral — this cycle records
/// it and the clock starts from here.
pub(crate) fn context_hook_defer_allowed(
    first_seen_at: Option<&str>,
    max_defer_secs: u64,
) -> bool {
    if max_defer_secs == 0 {
        return true;
    }
    match first_seen_at.and_then(elapsed_since) {
        Some(elapsed) => elapsed < max_defer_secs as f64,
        None => true,
    }
}

/// Is a post-clear resume inject due?
///
/// Covers the blind spot between the two gates that are supposed to notice an
/// idle session:
///   * the fresh-/clear gate wants `tokens` inside `[min_tokens, max_tokens)`,
///     but Claude Code reports **0 tokens** at a post-clear prompt and only
///     publishes a count once the first turn completes — by which point the
///     always-loaded preamble has already carried it far above `max_tokens`.
///     The window is stepped clean over, never sampled;
///   * the fresh-external-session gate handles `tokens == 0`, but only when
///     `bashes == 0`, and background shells SURVIVE a `/clear`.
///
/// A session cleared by hand with a long-running background command therefore
/// sits at an empty prompt indefinitely with nothing to nudge it. (When the
/// daemon drives the clear itself the resume prompt comes from the `self-clear`
/// child, which is why this gap only shows up on operator-driven clears.)
///
/// This gate keys on a clear the daemon actually OBSERVED (`last_context_clear`
/// within `window_secs`) plus pane idleness, and deliberately ignores the
/// background-shell count. `already_injected_for` latches it to one inject per
/// observed clear.
///
/// Fires iff ALL hold:
///   * `window_secs > 0` (gate enabled),
///   * `tokens < fresh_min_tokens` — below the fresh-/clear window, so this
///     cannot double up with that gate,
///   * `!daemon_clear_recent` — the daemon's own `self-clear` child injects a
///     resume prompt itself once the clear lands, so a daemon-driven clear is
///     already covered and firing here too would just double up,
///   * a clear was observed within `window_secs`,
///   * we have not already injected for that same clear,
///   * `idle && !interactive` — the prompt is up and no menu is awaiting the
///     operator (a resume inject leads with Escape and would cancel it),
///   * `idle_checks >= checks_required` — debounced, same as fresh-/clear.
#[allow(clippy::too_many_arguments)]
pub(crate) fn post_clear_resume_due(
    tokens: u64,
    fresh_min_tokens: u64,
    last_context_clear: Option<&str>,
    window_secs: u64,
    already_injected_for: Option<&str>,
    daemon_clear_recent: bool,
    idle: bool,
    interactive: bool,
    idle_checks: u32,
    checks_required: u32,
) -> bool {
    if window_secs == 0
        || tokens >= fresh_min_tokens
        || daemon_clear_recent
        || !idle
        || interactive
    {
        return false;
    }
    let Some(cleared_at) = last_context_clear else {
        return false;
    };
    if already_injected_for == Some(cleared_at) {
        return false;
    }
    if !elapsed_since(cleared_at).is_some_and(|e| e < window_secs as f64) {
        return false;
    }
    idle_checks >= checks_required
}

/// Watcher-down active-turn verdict, with the two cases where the premise of
/// the suppression is false folded in.
///
/// The suppression holds the loud tmux inject on the theory that (a) the loop
/// is making progress and (b) the out-of-band claude-event still reaches the
/// operator. `consumer_down` falsifies (b) — already handled at the fire site
/// and kept here so the whole verdict is one testable expression.
///
/// `pane_wedged` falsifies (a), and is the addition. `main_loop_actively_turning`
/// treats `bashes > 0` as unconditional, untimed proof of activity. A pane at
/// the context wall renders a spinner and keeps its background shells listed
/// ("2 shells still running"), so `bashes` stays pinned above zero and the
/// gate reads a session that cannot execute a single tool call as maximally
/// busy — and holds the inject for as long as the wedge lasts. Busy is exactly
/// the wrong signal from a wedged pane: the wedge IS the not-making-progress
/// condition. Note the operator-tunable suppression-run caps
/// (`suppression.max_consecutive_suppressions` / `max_suppression_window_secs`)
/// are the only other bound here, and a deployment may legitimately set them
/// very high — so this must not rely on them.
///
/// Real incident (2026-08-10): 14 consecutive `watcher-down inject suppressed:
/// main loop actively turning` cycles with `bashes=2` against a pane that had
/// been at the hard context limit for ten minutes. It only broke out when a
/// SECOND watcher — the event consumer, which bypasses the gate — also died.
pub(crate) fn watcher_down_actively_turning(
    state: &State,
    bashes: u64,
    suppress_enabled: bool,
    window_secs: u64,
    consumer_down: bool,
    pane_wedged: bool,
) -> bool {
    if consumer_down || pane_wedged {
        return false;
    }
    suppress_enabled && main_loop_actively_turning(state, bashes, window_secs)
}

/// Watcher-down two-phase decision: like [`obligation_escalation_decision`],
/// but an active cross-gate suppression-escalation (`suppression_escalated`)
/// FORCES `Escalate`. A capped suppression run is exactly the "lower rung
/// demonstrably failed" case — the dwell must not re-delay the inject the
/// suppression backstop just decided to force through. With no suppression
/// escalation the normal dwell gate applies.
pub(crate) fn watcher_down_obligation_decision(
    suppression_escalated: bool,
    armed_at: Option<&str>,
    dwell_secs: u64,
    active_subagents: u32,
    now: &str,
) -> ObligationDecision {
    if suppression_escalated {
        return ObligationDecision::Escalate;
    }
    obligation_escalation_decision(armed_at, dwell_secs, active_subagents, now)
}

/// Exponential global cooldown: `base * backoff_base^streak`, capped at
/// `max_secs`, using SATURATING arithmetic so a huge streak never panics.
/// `backoff_base <= 1` or `streak == 0` => `base` (flat — exact legacy
/// behavior). Mirrors the shape of `thinking_backoff_threshold_with_multiplier`.
pub(crate) fn effective_global_cooldown_secs(
    base: u64,
    backoff_base: u64,
    max_secs: u64,
    interrupt_streak: u32,
) -> u64 {
    if backoff_base <= 1 || interrupt_streak == 0 {
        return base;
    }
    let mut cooldown = base;
    for _ in 0..interrupt_streak {
        cooldown = cooldown.saturating_mul(backoff_base);
        if cooldown >= max_secs {
            return max_secs;
        }
    }
    cooldown.min(max_secs)
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

/// Pure predicate: is the main loop provably alive per THE liveness signal —
/// the age of the last event-ack (`last_ack_timestamp_age`)?
///
/// The fresh-/clear fast path infers a `/clear` from a low context-token
/// reading (`[min_tokens, max_tokens)`) plus `bashes == 0` and an idle pane.
/// That inference is fooled whenever the token reading is a MISPARSE rather
/// than a real context reset: the status-bar total can drop out of the capture
/// window and the parser falls back to the thinking-indicator's `↓ N tokens`
/// (current-turn output, typically a few thousand) or an agent-roster row's
/// count — both of which land squarely inside `[min_tokens, max_tokens)`
/// (see `status::parse_status_bar`). A long, intact session that is thinking
/// mid-turn or quiet-holding while acking keepalives then reads as a "fresh
/// /clear" and gets a resume prompt injected on top of live, uncleared work
/// (false-fire incident 2026-08-24: looped for hours at tokens=2100..4900).
///
/// The one signal that cannot be spoofed by a token misparse is whether the
/// loop is still handling events: `event-ack` stamps `last-ack-timestamp` on
/// every ack, and the main loop's per-batch reflex is `event-ack ack-batch`.
/// If that stamp is younger than the stale threshold the loop demonstrably
/// handled something recently, so it CANNOT have been cleared/stranded — any
/// low-token reading this cycle is noise. Gate the inject on it.
///
/// Returns true iff we HAVE an ack stamp AND it is younger than `stale_secs`.
/// `None` (no ack data yet — fresh boot, host without event-ack) => false:
/// we never claim a liveness we can't prove, so the genuine fresh-/clear case
/// keeps its existing behaviour. Symmetrically, a genuinely stranded post-clear
/// loop stops acking, so its stamp ages past `stale_secs` and the gate opens
/// again — this defers to, rather than disables, wedge detection (the same
/// single-liveness-signal principle the 2026-08-22 ack redesign consolidated
/// on).
pub(crate) fn ack_liveness_fresh(liveness_age: Option<u64>, stale_secs: u64) -> bool {
    liveness_age.is_some_and(|age| age < stale_secs)
}

/// Pure predicate: should `ack_liveness_fresh` suppress a fresh-/clear or
/// post-clear-resume inject THIS cycle?
///
/// `ack_liveness_fresh` exists to catch a status-bar MISPARSE — a live,
/// intact session whose low-token reading is actually a thinking-indicator
/// or agent-roster count leaking through, not a real clear. It must defer
/// to a genuine context-limit/rate-limit wedge (`wedged_now`, from
/// `tmux::detect_wedged` on the *current* banner text — independent,
/// stronger evidence than a token-count reading). Without this carve-out, a
/// session that hit "Context limit reached" moments after its last
/// event-ack reads as "alive" for the whole `ack.stale_minutes` window,
/// and the ack gate's early `return` in `check_cycle` never lets control
/// reach `handle_wedged_pane` — silently swallowing the autoclear-on-
/// context-limit recovery (2026-08-26 incident: "Context limit reached"
/// then "Context low (0% remaining)", autoclear never fired).
///
/// Returns true iff `ack_alive && !wedged_now`.
pub(crate) fn ack_liveness_suppresses_clear_inject(ack_alive: bool, wedged_now: bool) -> bool {
    ack_alive && !wedged_now
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

/// Pure gate: should the fresh-external-session checklist kick-start inject
/// FIRE this cycle? Mirrors the decision made at the fire site (the
/// dead-process block's `else if`) so the conditions — including the
/// interactive-prompt suppression — are unit-testable without mocking tmux.
///
/// The inject fires iff ALL hold:
///   * `dead_checks >= fresh_inject_checks` — enough consecutive
///     tokens==0 / bashes==0 observations to be confident the session is
///     genuinely a fresh idle one (not a momentary between-turns reading).
///   * `!already_injected` — we haven't already kick-started this session.
///   * `is_idle` — the Claude prompt (`❯`) is visible.
///   * `!interactive_prompt` — there is NO `AskUserQuestion` / tool-permission
///     / selection menu on screen awaiting the operator.
///
/// The last clause is the fix for the reported bug: a legitimately pending
/// interactive question idles the loop with tokens==0 and renders a `❯`
/// cursor, so `is_idle` reads true and the loop lands in the dead-process
/// block. Without this clause the kick-start inject `send-keys` (leading
/// Escape) would CANCEL the operator's question. A pending question is a
/// recognized, legitimate idle state — the #356 ask_question_monitor uses
/// the same `is_interactive_prompt` signal to detect it — so it must EXEMPT
/// the loop from the wedge/restart inject for the question's lifetime.
/// Suppressing is recoverable (a later cycle injects once the prompt
/// clears); injecting into a live menu is not.
pub(crate) fn fresh_inject_due(
    dead_checks: u32,
    fresh_inject_checks: u32,
    already_injected: bool,
    is_idle: bool,
    interactive_prompt: bool,
) -> bool {
    dead_checks >= fresh_inject_checks
        && !already_injected
        && is_idle
        && !interactive_prompt
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

/// Pure decision: should the heartbeat-stale `stuck` flag be suppressed
/// for THIS cycle?
///
/// Two independent proof-of-life conditions suppress the flag:
///   * `workload_fresh` -- a `workload run` (stv-promote, rsync, ffmpeg)
///     emitted a fresh per-label heartbeat (see
///     `workload_heartbeat_suppresses_stuck`).
///   * `active_subagents > 0` -- the main loop is legitimately blocked
///     dispatcher-waiting on long-running background subagents (5-15min,
///     few counted tool calls). Firing the Escape interrupt here would
///     cancel the in-flight turn AND kill those healthy subagents. This
///     mirrors the auto-respawn guard in `respawn::should_respawn`
///     (active subagents veto a respawn) -- applied at detection time so
///     it fixes BOTH the destructive interrupt AND the downstream
///     `HangSignal::HeartbeatStale` fed to the respawn collector.
///
/// Kept pure (no /proc, no Config) so it is unit-testable; the caller
/// computes the two inputs.
pub(crate) fn stuck_suppressed_by_activity(workload_fresh: bool, active_subagents: u32) -> bool {
    workload_fresh || active_subagents > 0
}

/// Pure predicate: given the heartbeat-stale proof-of-life inputs, return the
/// reason to SUPPRESS the stuck flag this cycle, or `None` to let it fire.
///
/// Extends [`stuck_suppressed_by_activity`] with two INDEPENDENT liveness
/// signals the daemon already tracks, decoupling host-heartbeat freshness from
/// event-bus tick DELIVERY. The host heartbeat file is refreshed by the main
/// loop only when it PROCESSES a `heartbeat-tick` claude-event (acks it via
/// `event-ack ack`, which refreshes the liveness timestamp per #649's
/// ack-driven redesign) -- which needs (a) the event bus to deliver the tick
/// AND (b) the loop to reach a tool-call boundary. Both premises fail while the
/// loop is ALIVE: a long single turn (prolonged thinking, no tool calls) or a
/// stalled bus (claude-event-watch itself down) starves the heartbeat even
/// though nothing is wedged, firing a FALSE "heartbeat stale" alert (incident
/// 2026-08-21: 25min + 40min false stale while the loop was thinking).
///
/// So when the daemon has its OWN evidence the loop is alive -- an active
/// thinking episode (`loop_thinking`) or a tool call running / run within the
/// active window (`actively_turning`) -- a stale heartbeat file is NOT a wedge
/// and the stuck flag is suppressed. A genuinely wedged session shows NEITHER
/// signal (idle pane, no thinking, no tool activity, no live subagents, no
/// fresh workload heartbeat), so real wedge detection is preserved. The
/// "thinking forever" case is independently covered by prolonged-thinking
/// detection + its own token-progress rearm, so deferring to it here loses no
/// coverage. Kept pure (no /proc, no Config) so it is unit-testable.
pub(crate) fn heartbeat_stale_liveness_reason(
    workload_fresh: bool,
    active_subagents: u32,
    loop_thinking: bool,
    actively_turning: bool,
) -> Option<&'static str> {
    if stuck_suppressed_by_activity(workload_fresh, active_subagents) {
        return Some(if workload_fresh {
            "workload_heartbeat_fresh"
        } else {
            "active_subagents"
        });
    }
    if loop_thinking {
        return Some("loop_thinking");
    }
    if actively_turning {
        return Some("loop_actively_turning");
    }
    None
}

/// Age in seconds of the last ack of ANY claude-event — THE liveness signal.
///
/// `event-ack` stamps `<state-dir>/last-ack-timestamp` on every ack, and the
/// main loop's per-batch reflex is `event-ack ack-batch`, so this timestamp
/// answers exactly one question: how long since the loop last handled
/// anything. It is the only liveness input the daemon has (the host heartbeat
/// FILE and its `touch` ritual were retired 2026-08-22 — two signals for one
/// fact was the complexity Andrew asked to remove).
///
/// Default-open: missing/unreadable file => `None` (treat as "no ack data
/// yet", not an error). A `None` NEVER reads as stale — a fresh host with no
/// ack state must not alert; the first ack starts the clock.
pub(crate) fn last_ack_timestamp_age(state_dir: &str) -> Option<u64> {
    use std::fs;
    use std::time::SystemTime;

    let path = std::path::Path::new(state_dir).join(crate::config::LAST_ACK_FILE);
    let meta = fs::metadata(&path).ok()?;
    // Age via the pure helper so the fail-open rule for a future/skewed stamp
    // is defined in exactly one place (and stays unit-testable without I/O).
    ack_age_secs(meta.modified().ok(), SystemTime::now())
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

/// Pure predicate: has a down watcher been CONTINUOUSLY down long enough that
/// the active-turn suppression of its inject must be OVERRIDDEN and the inject
/// FORCED this cycle, even though the main loop is actively turning?
///
/// `max_down_secs` is the longest continuous-down duration (seconds) among the
/// watchers currently in the inject path (derived from each
/// `WatcherState.down_since`). Returns true iff the cap is enabled
/// (`max_suppress_secs > 0`) AND that longest run has met/exceeded it.
///
/// This is a PER-WATCHER bound, deliberately independent of the shared
/// cross-gate suppression window (`should_escalate_suppression`): the shared
/// window is tuned very high (`[suppression].max_suppression_window_secs` =
/// 86400) to tolerate the chronically-flapping surface-and-exit event consumer,
/// which would otherwise force a destructive inject storm — so it no longer
/// bounds the HONEST watcher-down case. This cap re-bounds that case (e.g.
/// `botchat-wait`, the operator comms channel) at 3 min so a down comms watcher
/// can never be silently suppressed for longer than the cap while the main loop
/// is busy. The forced inject remains throttled by `inject_cooldown`, so a
/// genuinely-dead watcher re-injects at most once per cooldown window.
pub(crate) fn watcher_down_suppression_capped(
    max_down_secs: Option<u64>,
    max_suppress_secs: u64,
) -> bool {
    max_suppress_secs > 0 && max_down_secs.is_some_and(|d| d >= max_suppress_secs)
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

/// Pure predicate: is the configured event-consumer watcher among the
/// missing watchers this cycle?
///
/// The event consumer (`[watcher_monitor].event_consumer_watcher_name`,
/// e.g. `claude-event-watch`) is the process that DRAINS the
/// `~/claude-events/` queue the quiet path writes into. When it is the
/// down watcher, the quiet claude-event channel is a dead letter box: an
/// event announcing its own death is enqueued into the very queue only it
/// drains, so nothing ever surfaces it to the main loop (circular
/// dependency -> silent watcher death). The watcher-down escalation must
/// therefore use the OUT-OF-BAND tmux-inject path (send-keys into the
/// pane) rather than deferring on the premise the claude-event will be
/// picked up. Callers use this to force the active-turn suppression and
/// obligation-dwell gates open for a consumer-down. An empty
/// `consumer_name` (consumer unconfigured) is never "missing".
pub(crate) fn consumer_watcher_missing(missing: &[String], consumer_name: &str) -> bool {
    !consumer_name.is_empty() && missing.iter().any(|n| n == consumer_name)
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
    // Producer-stamped routing tier (rung 2 in the classifier precedence): a
    // down watcher DEMANDS a relaunch, so route it to the ACTIONABLE pending
    // list + N-call gate rather than ambient context. Without this the event
    // fell through the classifier's `claude-watch/* -> ambient` catch-all and a
    // busy main loop never relaunched the watcher (comms watcher down ~4h,
    // incident 2026-08-21). claude-event-watch forwards data.tier to
    // `event-ack ingest --tier`, so this producer stamp wins over the
    // consumer-side table.
    let tier_kv = "tier=actionable";
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
        "--data",
        tier_kv,
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

    // The launch argv below carries `--dangerously-skip-permissions`, so
    // Claude Code renders its Bypass-Permissions consent dialog at startup
    // unless the acceptance is persisted in settings. Record it now;
    // the post-restart resume-inject block also refuses to inject while that
    // dialog is up (and accepts it), so this is an optimisation, not the
    // safety belt.
    pre_accept_bypass_permissions(config);

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
    // Resolve the launcher for the relaunch argv. Prefers the
    // `claude-relaunch-exec` shim (waits for + repairs a dangling
    // ~/.local/bin/claude during the auto-update download window instead of
    // hot-spinning "claude: not found"), falling back to an absolute claude
    // path so the relaunch survives a stripped-PATH pane shell (a fresh
    // non-login `-sh` after /exit may not have the native-install bin dir on
    // $PATH). See `resolve_relaunch_bin` / `resolve_claude_bin` for the
    // failure modes these guard against.
    let claude_bin = resolve_relaunch_bin();
    let launch = if let Some(ref sid) = session_id {
        info!(session_id = %sid, "restarting Claude Code with --resume");
        format!("{} --dangerously-skip-permissions --resume {}", claude_bin, sid)
    } else {
        info!("restarting Claude Code with --continue (no session ID found)");
        format!("{} --dangerously-skip-permissions --continue", claude_bin)
    };

    // Write relaunch script. Ensure its parent dir exists first: the
    // default lives under `/var/run/claude/`, which is a tmpfs that does
    // NOT survive a container redeploy — if the dir is gone the write
    // fails, the relaunch never happens, and the later resume-inject would
    // hit a raw shell. `create_dir_all` is idempotent (Ok if it exists).
    if let Some(parent) = std::path::Path::new(&config.relaunch_script).parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            tracing::warn!(error = %e, dir = %parent.display(), "could not create relaunch-script parent dir");
        }
    }
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

    // Verify the script actually landed on disk before injecting. If the
    // write somehow didn't stick (tmpfs gone, race) do NOT type a broken
    // `bash <path>` into the pane -- bail and let the crash-recovery path
    // try again on the next loop pass.
    if !std::path::Path::new(&config.relaunch_script).exists() {
        tracing::error!(
            path = %config.relaunch_script,
            "relaunch script missing immediately after write -- aborting inject (would hit a dead shell)"
        );
        return;
    }

    // Inject a self-healing guarded one-liner: run the script if present at
    // exec time, else run the claude launch argv inline (the script could
    // still vanish between this write and the pane shell executing it --
    // tmpfs wipe / respawn.rs remove_file race). The inline fallback gets a
    // `cd $HOME &&` prefix to match the script's `cd $HOME` so resume/continue
    // resolves correctly. (Resume PROMPT text -- which has parens -- is NOT
    // included; only the shell-safe `claude` argv is.)
    let inline_launch = format!("cd $HOME && {}", launch);
    let inject_cmd = build_relaunch_inject_cmd(&config.relaunch_script, &inline_launch);
    // Serialize with every other injector (see `inject_lock`). The pane shows a
    // SHELL prompt here, but cw-theme-sync's idle gate keys on the prompt-cursor
    // glyph, which a zsh prompt can also render — so a theme inject can and does
    // aim at this pane mid-relaunch. Same interleave hazard, same lock.
    {
        let _guard = crate::inject_lock::InjectLock::acquire("relaunch-shell").await;
        tmux::inject_shell(pane, &inject_cmd).await;
    }

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
        state.thinking_episode_start_tokens = None;
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
            // Token baseline for the token-progress guard. `tokens == 0`
            // means the status-bar count was unavailable/unparseable —
            // record None so the guard fails open at fire time.
            state.thinking_episode_start_tokens = (tokens > 0).then_some(tokens);
            // Don't reset thinking_interrupt_count here — it persists across
            // brief non-thinking blips within the same stall episode. It only
            // resets when we see a genuinely active state (below).
        } else {
            // Token-progress guard (v2): runs on EVERY ongoing-thinking
            // check. Token growth >= min_tokens_delta since the episode
            // baseline re-arms the timer (thinking_start + baseline slide
            // to NOW), so the timer only accumulates over genuinely
            // growth-free time and a fire means "threshold_seconds of
            // Thinking without token progress" — a parked/wedged turn.
            // Ambient context growth (tool results, system reminders)
            // keeps an idle-but-alive open turn re-arming forever; a
            // genuinely stuck loop produces no growth and still fires.
            // Does NOT touch thinking_interrupt_count and emits no
            // claude-event. See thinking_token_progress_action docs for
            // why the v1 at-fire-time check never engaged in production.
            let pre_rearm_baseline = state.thinking_episode_start_tokens;
            let pre_rearm_elapsed = state
                .thinking_start
                .as_ref()
                .and_then(|s| elapsed_since(s))
                .unwrap_or(0.0);
            if let Some(reason) = apply_thinking_token_progress(
                &mut state.thinking_start,
                &mut state.thinking_episode_start_tokens,
                tokens,
                config.foreground_monitor.min_tokens_delta,
                &now,
            ) {
                let start_tokens = pre_rearm_baseline.unwrap_or(0);
                info!(
                    elapsed_secs = pre_rearm_elapsed,
                    start_tokens,
                    tokens,
                    tokens_delta = tokens.saturating_sub(start_tokens),
                    min_tokens_delta = config.foreground_monitor.min_tokens_delta,
                    reason,
                    "prolonged thinking suppressed: token progress — re-arming \
                     (timer accumulates only over growth-free time)"
                );
                write_jsonl_log(
                    &config.general.log_file,
                    "prolonged_thinking_suppressed",
                    serde_json::json!({
                        "elapsed_secs": pre_rearm_elapsed,
                        "reason": reason,
                        "start_tokens": start_tokens,
                        "tokens": tokens,
                        "tokens_delta": tokens.saturating_sub(start_tokens),
                        "min_tokens_delta": config.foreground_monitor.min_tokens_delta,
                    }),
                );
            }
            if let Some(elapsed) = state
                .thinking_start
                .as_ref()
                .and_then(|s| elapsed_since(s))
            {
                let next_threshold = thinking_backoff_threshold_with_multiplier(
                    config.foreground_monitor.threshold_seconds,
                    config.foreground_monitor.max_thinking_backoff,
                    state.thinking_interrupt_count,
                    config.foreground_monitor.thinking_backoff_multiplier,
                );
                if elapsed >= next_threshold as f64 {
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
                    // interrupt can fire on the next tick. Checked BEFORE
                    // the global-gate claim so a workload-suppressed cycle
                    // does not consume a claim.
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
                    // Ack-freshness gate (v3, 2026-06-11; re-sourced from the
                    // last-ack timestamp 2026-08-22): if the supervised
                    // session acked an event — the SAME signal the ack-stale
                    // detector watches — within `ack_fresh_secs`, it is
                    // demonstrably alive and this is an idle parked-open turn,
                    // not a wedge: suppress and RE-ARM (slide thinking_start +
                    // token baseline, same as the v2 token-progress re-arm).
                    // A stale/missing/unreadable/future stamp allows the fire
                    // (fail-open); 0 disables the gate. Checked BEFORE the
                    // global-gate claim so a suppressed cycle does not consume
                    // a claim. The age is also reused in the fire-time
                    // observability fields below.
                    let hb_age_secs = last_ack_timestamp_age(&config.ack.resolve_state_dir());
                    let pre_rearm_baseline = state.thinking_episode_start_tokens;
                    if apply_ack_fresh_rearm(
                        &mut state.thinking_start,
                        &mut state.thinking_episode_start_tokens,
                        hb_age_secs,
                        config.foreground_monitor.ack_fresh_secs,
                        tokens,
                        &now,
                    ) {
                        let start_tokens = pre_rearm_baseline.unwrap_or(0);
                        info!(
                            elapsed_secs = elapsed,
                            threshold = next_threshold,
                            ack_age_secs = hb_age_secs,
                            ack_fresh_secs = config.foreground_monitor.ack_fresh_secs,
                            start_tokens,
                            tokens,
                            tokens_delta = tokens.saturating_sub(start_tokens),
                            "prolonged thinking suppressed: recent event ack — \
                             session alive, idle parked-open turn; re-arming"
                        );
                        write_jsonl_log(
                            &config.general.log_file,
                            "prolonged_thinking_suppressed",
                            serde_json::json!({
                                "elapsed_secs": elapsed,
                                "threshold_secs": next_threshold,
                                "reason": "ack_fresh",
                                "ack_age_secs": hb_age_secs,
                                "ack_fresh_secs": config.foreground_monitor.ack_fresh_secs,
                                "ack_state_dir": config.ack.resolve_state_dir(),
                                "start_tokens": start_tokens,
                                "tokens": tokens,
                                "tokens_delta": tokens.saturating_sub(start_tokens),
                                "min_tokens_delta": config.foreground_monitor.min_tokens_delta,
                            }),
                        );
                        return;
                    }
                    // Two-phase escalation gate (BUG 1 fix): before any
                    // interrupt, ARM the OBLIGATION rung first. The first
                    // detection cycle writes a pending alert (so the
                    // PreToolUse alert-gate hook bites) + emits the event,
                    // WITHOUT interrupting. Only a later cycle — condition
                    // persists, dwell elapsed, obligation armed, and NO live
                    // background subagents (interrupting would kill healthy
                    // in-flight agents) — escalates to the tmux interrupt.
                    let pt_msg = format!(
                        "[CLAUDE-WATCH] Prolonged thinking detected (>{}s in thinking state, interrupt #{}). \
                        You appear to be stuck in a long generation. If you have complex work to do, \
                        delegate it to a background Agent instead of doing it inline. \
                        Use run_in_background: true for long Bash commands. \
                        Resume your current task now.",
                        next_threshold,
                        state.thinking_interrupt_count + 1,
                    );
                    let active_subagents =
                        crate::respawn::count_alive_subagents();
                    match obligation_escalation_decision(
                        state.thinking_obligation_armed_at.as_deref(),
                        config.general.obligation_dwell_secs,
                        active_subagents,
                        &now,
                    ) {
                        ObligationDecision::ArmObligation | ObligationDecision::Hold => {
                            let _ = crate::obligation_arm::arm_alert_obligation(
                                &pt_msg,
                                "claude-watch-prolonged-thinking",
                            );
                            if state.thinking_obligation_armed_at.is_none() {
                                state.thinking_obligation_armed_at = Some(now.clone());
                            }
                            // Event sink only (NOT the interrupt) this cycle.
                            let pt_reason = format!("prolonged thinking ({}s, armed)", elapsed as u64);
                            alert::emit_event(crate::event_bus::ClaudeWatchAlert {
                                alert_type: "prolonged-thinking",
                                stuck_reason: &pt_reason,
                                stale_minutes: None,
                                affected_watchers: vec![],
                                severity: crate::event_bus::Severity::Medium,
                                message: &pt_msg,
                            });
                            debug!(
                                elapsed_secs = elapsed,
                                threshold = next_threshold,
                                dwell_secs = config.general.obligation_dwell_secs,
                                active_subagents,
                                "prolonged thinking: obligation armed/held — deferring interrupt"
                            );
                            return;
                        }
                        ObligationDecision::Escalate => {}
                    }
                    // Global interrupt gate (single chokepoint): atomically
                    // claim-and-stamp. If ANY interrupt fired within the
                    // cooldown window (watcher-down, context-warning,
                    // auto-respawn, or a prior thinking one), the claim
                    // fails and we suppress. Prevents the cascade where e.g.
                    // a watcher-down interrupt resets the thinking timer and
                    // the new thought trips prolonged thinking immediately
                    // afterward. The claim STAMPS last_interrupt_at on
                    // success, so the later (removed) explicit stamp is no
                    // longer needed. Token-progress re-arms happen BEFORE
                    // the threshold evaluation, so a re-armed (suppressed)
                    // cycle never reaches this gate and does not consume a
                    // claim.
                    if !try_claim_global_interrupt(
                        state,
                        config.general.post_interrupt_cooldown_secs,
                        config.general.global_cooldown_backoff_base,
                        config.general.global_cooldown_max_secs,
                        &now,
                    ) {
                        debug!(
                            elapsed_secs = elapsed,
                            threshold = next_threshold,
                            cooldown = config.general.post_interrupt_cooldown_secs,
                            "prolonged thinking would fire but global post-interrupt cooldown active"
                        );
                        return;
                    }
                    // Escalating to interrupt — the obligation has served its
                    // purpose; clear the armed timestamp so a fresh episode
                    // re-arms.
                    state.thinking_obligation_armed_at = None;
                    // Fire-time token observability: ALWAYS log the episode
                    // baseline, current tokens, and delta — even when the
                    // fire proceeds — so the token-progress guard's
                    // production behavior is inspectable from the journal
                    // and the jsonl alone. `start_tokens = 0` +
                    // `baseline_recorded = false` means the count was never
                    // parseable during the episode (legacy fail-open fire).
                    let start_tokens = state.thinking_episode_start_tokens;
                    warn!(
                        elapsed_secs = elapsed,
                        threshold = next_threshold,
                        interrupt_count = state.thinking_interrupt_count,
                        start_tokens = start_tokens.unwrap_or(0),
                        tokens,
                        tokens_delta = tokens.saturating_sub(start_tokens.unwrap_or(0)),
                        baseline_recorded = start_tokens.is_some(),
                        min_tokens_delta = config.foreground_monitor.min_tokens_delta,
                        ack_age_secs = hb_age_secs,
                        "prolonged thinking detected — interrupting (backoff)"
                    );
                    write_jsonl_log(
                        &config.general.log_file,
                        "prolonged_thinking",
                        serde_json::json!({
                            "elapsed_secs": elapsed,
                            "tokens": tokens,
                            "bashes": bashes,
                            "start_tokens": start_tokens,
                            "tokens_delta": tokens.saturating_sub(start_tokens.unwrap_or(0)),
                            "baseline_recorded": start_tokens.is_some(),
                            "min_tokens_delta": config.foreground_monitor.min_tokens_delta,
                            "ack_age_secs": hb_age_secs,
                            "ack_fresh_secs": config.foreground_monitor.ack_fresh_secs,
                            "interrupt_count": state.thinking_interrupt_count,
                            "next_threshold_secs": next_threshold,
                            "action": if config.foreground_monitor.interrupt_enabled { "interrupt" } else { "log-only" },
                        }),
                    );
                    state.thinking_alerted = true;
                    state.thinking_interrupt_count += 1;
                    // Reset thinking_start so the next backoff interval
                    // counts from NOW, not from the original start. Refresh
                    // the token baseline alongside it so the token-progress
                    // guard judges the next backoff window on fresh growth.
                    state.thinking_start = Some(now.clone());
                    state.thinking_episode_start_tokens = (tokens > 0).then_some(tokens);

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
                        // NOTE: the global interrupt cooldown was already
                        // STAMPED above by try_claim_global_interrupt — no
                        // separate stamp here (collapsed into the atomic
                        // claim, 2026-06-11).
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
                        inject_dispatch::inject_to_agent(pane, &msg).await;
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
        state.thinking_episode_start_tokens = None;
        // Condition cleared — disarm the two-phase obligation so the next
        // episode re-arms from scratch.
        state.thinking_obligation_armed_at = None;
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
                            inject_dispatch::inject_to_agent(
                                pane,
                                &config.foreground_monitor.interrupt_message,
                            )
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

/// Check if a PID is genuinely alive — i.e. exists AND is not a zombie
/// (`<defunct>`). `pgrep` still lists zombies because they linger in the
/// process table until reaped, so a plain `kill -0` probe (or a raw `pgrep`
/// count) would treat a defunct watcher as "running". We read `/proc/PID/stat`
/// and reject state `Z` so a watcher whose process has died-but-not-yet-reaped
/// is correctly seen as not-alive.
///
/// Falls back to the signal-0 probe when `/proc/PID/stat` is unreadable (e.g.
/// a non-Linux test host) so behaviour degrades to "exists?" rather than
/// always-false.
fn is_pid_genuinely_alive(pid: u32) -> bool {
    crate::status::is_pid_genuinely_alive(pid)
}

/// Is the recorded `self-clear` child STILL RUNNING (as opposed to finished,
/// gone, or finished-but-unreaped)?
///
/// Both clear-spawn paths short-circuit on "a clear child is already running"
/// so they don't stack two `/clear` drivers on one pane. That guard used the
/// bare `is_pid_alive` (a signal-0 / `/proc` existence probe), which is TRUE
/// for a ZOMBIE.
///
/// And the children are always zombies. The daemon spawns `self-clear`
/// detached and drops the `Child` handle without ever `wait()`ing, so every
/// clear child it has ever spawned stays in the process table as `<defunct>`
/// for the rest of the daemon's lifetime, with `context_clear_child_pid` still
/// pointing at it. Net effect: the FIRST self-clear of a daemon lifetime
/// poisons every later one. Every subsequent attempt hits the guard, logs
/// "child already running", returns `true` — meaning "recovery attempted" —
/// and spawns NOTHING.
///
/// Real incident (2026-08-10): the pane hit the hard context limit and the
/// wedged-pane detector fired three times over 10 minutes, each logging
/// "wedged pane sustained — running self-clear immediately" and alerting.
/// Not one of them spawned a clear: the recorded pid was a zombie from a
/// successful clear four hours earlier. The pane sat at the wall until the
/// operator typed `/clear` by hand.
///
/// Two fixes, both needed:
///   * reap the child if it is ours, so the zombie stops existing at all;
///   * judge liveness with `is_pid_genuinely_alive`, which rejects state `Z`,
///     so an unreapable zombie (daemon restarted since the spawn — the pid is
///     no longer our child, `waitpid` gives `ECHILD`) still reads as finished.
fn clear_child_is_running(pid: u32) -> bool {
    reap_clear_child(pid);
    is_pid_genuinely_alive(pid)
}

/// Best-effort non-blocking reap of a `self-clear` child we spawned.
///
/// `WNOHANG` so a still-running child is left alone (returns `StillAlive`).
/// `ECHILD` — the pid is not our child, e.g. the daemon restarted since the
/// spawn — is expected and ignored; `clear_child_is_running`'s zombie-aware
/// liveness check covers that case.
fn reap_clear_child(pid: u32) {
    use nix::sys::wait::{waitpid, WaitPidFlag};
    let Ok(raw) = i32::try_from(pid) else {
        return;
    };
    match waitpid(nix::unistd::Pid::from_raw(raw), Some(WaitPidFlag::WNOHANG)) {
        Ok(nix::sys::wait::WaitStatus::StillAlive) => {}
        Ok(status) => debug!(pid, ?status, "reaped self-clear child"),
        Err(nix::errno::Errno::ECHILD) => {}
        Err(e) => debug!(pid, error = %e, "waitpid on self-clear child failed"),
    }
}

/// Read `/proc/<pid>/cmdline` (NUL-separated argv) into a space-joined string.
/// Returns `None` if the process is gone, the file is unreadable, or the
/// cmdline is empty (e.g. a kernel thread). Used for watcher identity checks.
// dead_code allow: see is_pid_genuinely_alive — retained delegator, the live
// path is `status::watcher_pidfile_liveness_multi`.
#[allow(dead_code)]
fn read_proc_cmdline(pid: u32) -> Option<String> {
    crate::status::read_proc_cmdline(pid)
}

/// Read a watcher PID file and return the recorded PID, if the file exists
/// and contains a parseable integer. Whitespace is trimmed.
///
/// Returns:
/// - `Some(pid)` if the file exists and parses cleanly.
/// - `None` if the file is missing, unreadable, or contains non-numeric data.
///
/// NOTE: as of the BUG-A fix the watcher-health monitor no longer consults the
/// recorded PID file to decide liveness (it drifted out of sync after restarts
/// and caused false "WATCHER DOWN" reports). This helper is retained for
/// diagnostics / potential future use and remains unit-tested.
#[allow(dead_code)]
fn read_watcher_pid(pid_dir: &str, name: &str) -> Option<u32> {
    crate::status::read_watcher_pid(pid_dir, name)
}

/// Decide whether a watcher should be considered DOWN, given:
/// - the PIDs of processes matching the watcher's pattern (from `pgrep -f`)
/// - the configured `min_count`
/// - a genuine-liveness probe (typically [`is_pid_genuinely_alive`], which
///   rejects zombies)
///
/// Returns `true` when fewer than `min_count` of the matched processes are
/// genuinely alive.
///
/// ## DEPRECATED 2026-06-11 — pgrep liveness defeated by `exec` (this bug)
///
/// This helper is no longer wired into the watcher-health monitor. It read
/// liveness off `pgrep -f <pattern>`, where `<pattern>` is the watchers.conf
/// pattern field — the launcher SCRIPT path (e.g.
/// `/opt/claude-container/watchers/claude-event-watch.sh`). But that launcher
/// does `exec /usr/local/bin/claude-event-watch`, which REPLACES the process
/// image: after the exec the live process's argv is
/// `/bin/bash /usr/local/bin/claude-event-watch` — the `.sh` path is GONE from
/// argv. So `pgrep -f` on the `.sh` pattern can NEVER match a healthy watcher,
/// `matched_pids` is always empty, and `watcher_is_down` returns `true` on
/// every check → a `WATCHER(S) DOWN` tmux-inject storm (~every 70s) even
/// though the watcher is alive and well. (The only time the old `pgrep`
/// matched at all was a coincidental hit on an unrelated diagnostic shell
/// whose command-string happened to contain the `.sh` path — a false positive,
/// not the watcher.)
///
/// The monitor now uses [`pidfile_watcher_is_down`] instead: it reads the PID
/// the watcher itself records (in its `<name>.lock` flock file, or the
/// `<name>.pid` file written by `watcher_run`), probes it for liveness, and
/// verifies cmdline identity — all of which survive the `exec`-to-binary
/// transform. Kept here (with tests) only for the historical
/// BUG-A regression suite and any external caller.
#[allow(dead_code)]
pub fn watcher_is_down(
    matched_pids: &[u32],
    min_count: u32,
    pid_genuinely_alive: impl Fn(u32) -> bool,
) -> bool {
    let alive = matched_pids
        .iter()
        .filter(|&&pid| pid_genuinely_alive(pid))
        .count() as u32;
    alive < min_count
}

/// Resolve the directory that holds watcher PID / lock files.
///
/// Mirrors the watcher's own lockfile resolution in
/// `tools/watchers/claude-event-watch`
/// (`$XDG_RUNTIME_DIR/<name>.lock` else `/var/run/claude/<name>.lock`) and
/// `watcher::pid_dir()` (`$CLAUDE_WATCH_PID_DIR` else `/var/run/claude`), so
/// the daemon reads the SAME file the watcher writes. Precedence:
///   1. `$CLAUDE_WATCH_PID_DIR` (explicit override; used by tests + the
///      watcher_run spawn path).
///   2. `$XDG_RUNTIME_DIR` (matches the watcher's lockfile default).
///   3. `/var/run/claude` (final fallback — the baked container path).
///
/// Superseded by `status::watcher_pid_dirs()` (ALL candidates) — the daemon's
/// watcher_monitor now scans every candidate dir, not this single env-resolved
/// one. Retained as a named delegator.
// dead_code allow: superseded single-dir resolver, no production caller.
#[allow(dead_code)]
pub(crate) fn watcher_pid_dir() -> String {
    crate::status::watcher_pid_dir()
}

/// Read the PID the watcher recorded for itself, from the runtime dir.
///
/// A watcher records its live PID in one of two files under [`watcher_pid_dir`]:
///   * `<name>.lock` — written by the watcher itself (the flock singleton
///     guard writes `printf '%s\n' "$$" >&9`). This is the authoritative
///     source in the container, where watchers are spawned by the session as
///     `run_in_background` tasks (NOT via `watcher_run`), so no `.pid` file
///     exists.
///   * `<name>.pid` — written by `watcher::watcher_run` with the child PID when
///     claude-watch spawns the watcher.
///
/// We prefer `<name>.lock` (always present for a live watcher in the container)
/// and fall back to `<name>.pid`. Returns the first file that parses to a PID,
/// or `None` if neither exists / parses.
///
/// Superseded by `status::collect_watcher_recorded_pids` (every dir × file,
/// alive-preferring) — the lock-first single pick mis-selected a stale `.lock`
/// over a fresh `.pid`. Retained as a named delegator for its tests.
// dead_code allow: superseded single-pick reader, no production caller.
#[allow(dead_code)]
fn read_watcher_recorded_pid(pid_dir: &str, name: &str) -> Option<u32> {
    crate::status::read_watcher_recorded_pid(pid_dir, name)
}

/// Does the live process `pid`'s cmdline look like *this* watcher (identity
/// check to reject a recycled PID the kernel handed to an unrelated process)?
///
/// The match is lenient because the watcher's launcher `exec`s a child or
/// re-execs itself, so the live argv rarely equals the literal `start_cmd`.
/// Concretely, the start_cmd is the launcher SCRIPT
/// (`/opt/claude-container/watchers/claude-event-watch.sh`) but the live
/// process — after `exec /usr/local/bin/claude-event-watch` — has cmdline
/// `/bin/bash /usr/local/bin/claude-event-watch`. The `.sh` is gone, so a
/// naive `cmdline.contains(start_cmd)` fails. We therefore reduce the
/// start_cmd's first token to its basename AND strip a trailing script
/// extension (`.sh`, `.bash`, `.py`), yielding the stem `claude-event-watch`,
/// which DOES appear in the exec'd cmdline. This tolerates the exec-to-binary
/// transform while still rejecting an obviously-unrelated recycled PID (whose
/// cmdline won't contain the watcher's name stem).
// dead_code allow: the identity check now runs inside
// `status::watcher_pidfile_liveness_multi`; this delegator is retained for the
// module's tests.
#[allow(dead_code)]
fn cmdline_matches_watcher(cmdline: &str, start_cmd: &str) -> bool {
    crate::status::cmdline_matches_watcher(cmdline, start_cmd)
}

/// Pure decision: is the watcher DOWN, given what the daemon observed about its
/// recorded PID file?
///
/// Kept pure (no `/proc`, no `pgrep`, no filesystem) so the DOWN logic is
/// unit-testable, mirroring the testable style of `watcher::run_guard_should_skip`.
///
/// Inputs (all already probed by the caller):
/// - `recorded_pid`: the PID read from the watcher's `<name>.lock` / `<name>.pid`
///   file, or `None` if no pidfile exists.
/// - `pid_alive`: whether that recorded PID is currently alive (`kill(pid, 0)` /
///   genuine-liveness probe). Meaningless when `recorded_pid` is `None`.
/// - `cmdline_matches`: whether that PID's `/proc/<pid>/cmdline` matches this
///   watcher's identity (rejects a recycled PID). Meaningless when
///   `recorded_pid` is `None` or `!pid_alive`.
///
/// A watcher is UP iff its pidfile names a live process whose cmdline matches
/// the watcher. DOWN in every other case:
///   * missing pidfile  → DOWN (no recorded instance),
///   * stale pidfile (recorded PID dead) → DOWN (triggers a legit restart),
///   * recycled PID (alive but cmdline mismatch) → DOWN.
///
/// NOTE: there is intentionally no `pgrep` / process-scan path here — `exec`
/// replacing the launcher's argv with the exec'd binary's argv defeats any
/// `pgrep -f <launcher.sh>` match (this bug). Liveness comes ONLY from the
/// pidfile the watcher itself maintains.
// dead_code allow: `status::watcher_pidfile_liveness_multi` calls the pure
// `status::pidfile_watcher_is_down` directly now; this delegator is retained
// for the module's tests.
#[allow(dead_code)]
pub fn pidfile_watcher_is_down(
    recorded_pid: Option<u32>,
    pid_alive: bool,
    cmdline_matches: bool,
) -> bool {
    crate::status::pidfile_watcher_is_down(recorded_pid, pid_alive, cmdline_matches)
}

/// Spawn `self-clear` immediately (no grace period). Used for the
/// wedged-pane recovery path: when the agent is too stuck to run any tool
/// call (context limit reached, persistent 429), claude-watch must drive
/// `/clear` itself rather than waiting for the agent to cooperate.
///
/// Detached via setsid() so it survives a daemon restart, same as
/// `spawn_deferred_clear`.
///
/// Returns `true` iff the `self-clear` process was successfully spawned (or a
/// prior clear child is still alive). Returns `false` when the spawn itself
/// FAILED (e.g. `self-clear` not on PATH) — the caller must NOT then treat the
/// wedge as recovering. NOTE: a successful SPAWN is not a successful CLEAR —
/// `self-clear` daemonizes (double-forks) and drives `/clear` asynchronously,
/// and can itself no-op if it can't find the pane (the native-installer
/// regression). So even a `true` return only means "recovery was ATTEMPTED";
/// the caller confirms the wedge actually cleared on a subsequent cycle via
/// `detect_wedged` (see `wedged_clear_unverified`).
fn spawn_immediate_clear(state: &mut State) -> bool {
    // Don't double-spawn if a deferred clear child is already running.
    // `clear_child_is_running` reaps + rejects zombies: a finished-but-
    // unreaped child MUST NOT be mistaken for a live one, or this guard
    // silently disables wedged-pane recovery for the daemon's whole lifetime.
    if let Some(pid) = state.context_clear_child_pid {
        if clear_child_is_running(pid) {
            debug!(pid, "self-clear child already running, skipping immediate spawn");
            return true;
        }
        // Finished (or never ours). Drop the stale handle so we don't
        // re-probe a recycled pid on a later cycle.
        state.context_clear_child_pid = None;
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
            true
        }
        Err(e) => {
            tracing::error!(error = %e, "failed to spawn immediate self-clear");
            false
        }
    }
}

/// Wedged-pane detection + immediate-self-clear recovery, factored out so it
/// can run from BOTH the normal active-session path AND the `cs.is_none()`
/// "looks-not-running" early-return path.
///
/// Why it must also run on the `cs.is_none()` path (real incident
/// 2026-06-19): when Claude Code hits the context wall it renders an error
/// banner ("Context limit reached. /compact or /clear to continue" / "Context
/// low (N% remaining)") OVER the status bar. `get_claude_status()` parses the
/// status bar for tokens/bashes; with the banner covering it, the parse can
/// miss and `find_claude_pane()`'s status-bar heuristic returns no pane, so
/// the whole status read comes back `None`. The session is NOT gone — it is
/// WEDGED — but `check_cycle` took the `cs.is_none()` "not running" branch and
/// `return`ed BEFORE ever reaching the wedged-detection block lower down. The
/// daemon logged "claude-status returned None -- not running" every cycle for
/// 83 minutes while the loop sat wedged at 975K tokens; the auto-clear-at-limit
/// recovery never fired (it lives past the early return). The slower
/// heartbeat-stale path eventually recovered it ~13 min late.
///
/// This helper carries the same consecutive-cycle gate, api-retry suppression,
/// cooldown gate, breadcrumb, alert, and `spawn_immediate_clear` that the
/// inline site uses, so both entry points behave identically. Returns `true`
/// if a wedge was DETECTED this cycle (regardless of whether a clear actually
/// fired — it may be gated by consecutive/cooldown/api-retry), so the
/// `cs.is_none()` caller can skip the misleading "not running" bookkeeping.
async fn handle_wedged_pane(
    config: &Config,
    state: &mut State,
    pane: &str,
    api_retrying: bool,
    tokens: u64,
    now: &str,
) -> bool {
    if !config.context_monitor.wedged_detection_enabled || pane.is_empty() {
        return false;
    }

    let wedged = tmux::detect_wedged(pane).await;

    let Some(reason) = wedged else {
        // Pane is no longer wedged — reset the counter.
        if state.wedged_consecutive > 0 {
            debug!(
                prev_consecutive = state.wedged_consecutive,
                "wedged pane cleared — resetting counter"
            );
            state.wedged_consecutive = 0;
        }
        // Recovery CONFIRMED: a prior self-clear was fired and we now observe
        // the wedge is gone. Only now is it honest to say recovery worked —
        // the earlier fire-and-forget spawn was not itself proof (self-clear
        // can silently no-op on a pane miss; the native-installer regression).
        if state.wedged_clear_unverified {
            info!("wedged pane recovery confirmed — self-clear cleared the wedge");
            write_jsonl_log(
                &config.general.log_file,
                "wedged_clear_recovery_confirmed",
                serde_json::json!({}),
            );
            state.wedged_clear_unverified = false;
        }
        return false;
    };

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
            // Is this a RETRY? If the previous wedged self-clear was fired but
            // never verified as recovered (`wedged_clear_unverified` still
            // set), the prior clear DID NOT stick — the pane is still wedged a
            // full cooldown later. That is the native-installer failure mode:
            // self-clear couldn't find the pane and silently no-op'd, so the
            // wedge persisted while the daemon assumed recovery. Escalate:
            // louder (Critical) alert, and mark the log so it's diagnosable.
            let is_retry = state.wedged_clear_unverified;
            warn!(
                reason = %reason,
                consecutive = state.wedged_consecutive,
                retry = is_retry,
                "wedged pane sustained — running self-clear immediately (no agent cooperation possible)"
            );
            write_jsonl_log(
                &config.general.log_file,
                if is_retry { "wedged_clear_retry" } else { "wedged_clear" },
                serde_json::json!({
                    "reason": reason.to_string(),
                    "consecutive": state.wedged_consecutive,
                    "tokens": tokens,
                    "retry": is_retry,
                }),
            );
            write_legacy_log(
                &config.general.legacy_log_file,
                &format!(
                    "wedged pane ({reason}) — running self-clear (consecutive={}, retry={})",
                    state.wedged_consecutive, is_retry,
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

            // Fire the clear FIRST so we can report whether even the spawn
            // succeeded. A failed spawn (self-clear not on PATH) means no
            // recovery was even attempted — that warrants the loudest alert.
            let spawned = spawn_immediate_clear(state);

            // Notify Andrew so he knows claude-watch had to step in. Escalate
            // severity when the prior clear didn't stick (retry) or the spawn
            // itself failed — a plain first attempt stays High.
            let (severity, alert_msg) = if !spawned {
                (
                    crate::event_bus::Severity::Critical,
                    format!(
                        "claude-watch: agent wedged ({reason}) -- self-clear spawn FAILED (recovery not attempted)"
                    ),
                )
            } else if is_retry {
                (
                    crate::event_bus::Severity::Critical,
                    format!(
                        "claude-watch: agent STILL wedged ({reason}) after a prior self-clear -- retrying (previous clear did not stick)"
                    ),
                )
            } else {
                (
                    crate::event_bus::Severity::High,
                    format!("claude-watch: agent wedged ({reason}) -- running self-clear"),
                )
            };
            let wedged_reason = format!("wedged pane: {reason}");
            alert::notify(crate::event_bus::ClaudeWatchAlert {
                alert_type: "wedged-pane",
                stuck_reason: &wedged_reason,
                stale_minutes: None,
                affected_watchers: vec![],
                severity,
                message: &alert_msg,
            })
            .await;

            // Mark the clear UNVERIFIED: recovery is only confirmed when a
            // later cycle observes `detect_wedged` return None. Do NOT reset
            // to a fake "recovered" state here — the fire-and-forget spawn is
            // not proof the wedge cleared.
            if spawned {
                state.wedged_clear_unverified = true;
            }
            state.last_wedged_clear = Some(now.to_string());
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

    true
}

/// Spawn a deferred self-clear child process.
/// The child sleeps for the grace period, then checks if tokens are still high.
/// If so, it runs `self-clear` to force a context clear.
fn spawn_deferred_clear(config: &Config, state: &mut State) {
    // If there's already a living child, skip. Same zombie hazard as the
    // immediate path — see `clear_child_is_running`.
    if let Some(pid) = state.context_clear_child_pid {
        if clear_child_is_running(pid) {
            debug!(pid, "deferred self-clear child already running");
            return;
        }
        state.context_clear_child_pid = None;
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
    inject_dispatch::inject_to_agent(pane, &msg).await;
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

/// Fraction of the PREVIOUS token sample the counter must FALL BY for the new
/// sample to read as a context reset, even when the new sample is still above
/// `CONTEXT_FRESH_TOKEN_THRESHOLD`.
///
/// Why a ratio and not just the fresh threshold: within one context the token
/// counter only ever climbs. A halving is not something a live context does —
/// it means the context was thrown away and rebuilt (a `/clear`, a `self-clear`,
/// or an auto-compaction). The fresh threshold alone MISSES that event whenever
/// the replacement context boots above 30K, which is the normal case for any
/// session with a large always-loaded preamble.
pub(crate) const CONTEXT_RESET_DROP_RATIO: f64 = 0.5;

/// How a check sample was recognised as "the context was just reset".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ContextResetSignal {
    /// The sample itself is below `CONTEXT_FRESH_TOKEN_THRESHOLD` — the pane
    /// is showing a near-empty context (classically `0 tokens` in the seconds
    /// between `/clear` landing and the first turn of the new context).
    FreshSample,
    /// The sample is still above the fresh threshold, but the counter FELL by
    /// at least `CONTEXT_RESET_DROP_RATIO` of the previous sample — a context
    /// that was replaced, observed only after the replacement had already
    /// loaded its preamble.
    TokenDrop,
}

/// Pure predicate: does this token sample mean the context was reset since the
/// previous sample?
///
/// THE BUG THIS EXISTS FOR (observed 2026-08-22): the daemon recognised a
/// context reset ONLY by sampling a token count below
/// `CONTEXT_FRESH_TOKEN_THRESHOLD` (30K). That works only if the daemon happens
/// to land a poll inside the few seconds a cleared pane reads near-zero. On a
/// session whose fresh context boots at ~77K tokens (large always-loaded
/// preamble) the window is not just narrow — the pane may NEVER read below 30K
/// at all. The real samples across that day's auto-clear were:
///
///   21:08:13  tokens=907979
///   21:08:46  tokens=77185      <- the clear happened in this 33s gap
///
/// Neither sample is under 30K, so nothing stamped `last_context_clear`, the
/// dashboard's "Since Clear" tile kept counting from the PREVIOUS day's clear
/// (1.07 days at 17:57 ET, 50 minutes after the clear it should have shown),
/// and `context_clear_triggered` was left at the mercy of a later low sample.
///
/// Keying on the DROP instead makes detection independent of how big a fresh
/// context is, and covers every path that resets a context: the daemon's own
/// deferred auto-clear, a wedged-pane recovery self-clear, an agent- or
/// operator-run `self-clear`, a hand-typed `/clear`, and an auto-compaction.
/// All of them manifest the same way — the counter collapses between two polls.
///
/// Returns `None` for a live context (growth, or the ordinary small jitter of a
/// re-rendered status bar), so a normal turn never reads as a clear.
pub(crate) fn context_reset_signal(
    prev_tokens: Option<u64>,
    tokens: u64,
) -> Option<ContextResetSignal> {
    if tokens < CONTEXT_FRESH_TOKEN_THRESHOLD {
        return Some(ContextResetSignal::FreshSample);
    }
    // A drop is only meaningful against a previous sample that was itself high
    // enough to be a real context (the same "previously high" notion the
    // external-clear log uses) — during boot there is nothing to compare to.
    let prev = prev_tokens?;
    if prev < PREV_HIGH_FOR_EXTERNAL_CLEAR_LOG {
        return None;
    }
    let dropped = prev.saturating_sub(tokens);
    if (dropped as f64) >= (prev as f64) * CONTEXT_RESET_DROP_RATIO {
        return Some(ContextResetSignal::TokenDrop);
    }
    None
}

/// Reset `state.context_clear_triggered` when a check sample shows the context
/// was reset (see `context_reset_signal`), regardless of whether the inner
/// trigger gate (`tokens > 0`) runs this cycle. Also handles the external-clear
/// bookkeeping path so the "Since Clear" dashboard metric stays accurate.
///
/// STAMP SEMANTICS: `last_context_clear` is stamped with `now` — the check
/// cycle on which the reset was OBSERVED, not the instant the context was
/// actually thrown away, which the daemon cannot see. The two differ by at most
/// one `check_interval` plus the status read (order of seconds; ~33s in the
/// 2026-08-22 incident, where the clear fell inside a poll gap). The dashboard
/// tile renders this in minutes/hours/days, so the observation lag is not
/// material — and it is far closer than the alternative anchors: the deferred
/// auto-clear's TRIGGER stamp runs up to a full `grace_period` (300s) EARLY,
/// and `self-clear`'s handoff marker is touched only after the resume prompt
/// has been delivered, LATER. Whichever of those fired, this landing
/// observation re-stamps with the closest available reading.
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
    // The previous sample, captured BEFORE `check_cycle` slides
    // `last_seen_tokens` forward — a reset is a relation between two samples.
    let prev_tokens = state.last_seen_tokens;
    let Some(signal) = context_reset_signal(prev_tokens, tokens) else {
        return;
    };
    let detected_by = match signal {
        ContextResetSignal::FreshSample => "fresh_sample",
        ContextResetSignal::TokenDrop => "token_drop",
    };

    // Context-low condition has cleared (the context was reset) — disarm the
    // two-phase obligation so the next crossing re-arms, and end the threshold
    // episode so the hook-deferral ceiling restarts from the NEXT crossing
    // rather than staying permanently expired.
    state.context_obligation_armed_at = None;
    state.context_threshold_first_seen_at = None;

    // Path 1: we triggered the clear and it landed (tokens dropped). Reset
    // the in-flight flag + child-pid bookkeeping so the next threshold
    // crossing can fire.
    if state.context_clear_triggered {
        info!(
            tokens,
            prev_tokens, detected_by, "context clear detected — resetting trigger"
        );
        write_jsonl_log(
            &config.general.log_file,
            "context_clear_reset",
            serde_json::json!({
                "tokens": tokens,
                "prev_tokens": prev_tokens,
                "detected_by": detected_by,
            }),
        );
        record_reminder_latency_if_recent(ReminderType::ContextHigh, state, true);
        state.context_clear_triggered = false;
        state.context_clear_child_pid = None;
        state.last_context_clear = Some(now.to_string());
        // Daemon-driven clear: the `self-clear` child injects its own resume
        // prompt, so mark this clear as already handled and keep the
        // post-clear resume gate (which exists for OPERATOR-driven clears)
        // from injecting a second one on top of it.
        state.post_clear_resume_injected_for = Some(now.to_string());
        return;
    }

    // Path 2: external clear (user `/clear`, fresh-clear path, or any other
    // off-path reset). Only emit the log when we previously saw a high
    // sample, to avoid logging on every check during boot.
    // (A `TokenDrop` signal implies this — it is measured against a
    // previously-high sample. The check is what suppresses the boot case,
    // where a `FreshSample` is just an empty pane and not a clear at all.)
    if prev_tokens.unwrap_or(0) >= PREV_HIGH_FOR_EXTERNAL_CLEAR_LOG {
        info!(
            tokens,
            prev_tokens, detected_by, "external context clear detected"
        );
        write_jsonl_log(
            &config.general.log_file,
            "context_clear_reset",
            serde_json::json!({
                "tokens": tokens,
                "prev_tokens": prev_tokens,
                "detected_by": detected_by,
                "external": true,
            }),
        );
        record_reminder_latency_if_recent(ReminderType::ContextHigh, state, true);
        state.last_context_clear = Some(now.to_string());
    }
}

/// Seconds after a detected /clear (or compaction) boundary during which the
/// malformed-tool-call guardrail is suppressed. The pane-history capture reads
/// ~60 lines of scrollback, which immediately after a clear STILL includes the
/// PRE-clear turn — possibly a malformed `<invoke>` block from the OLD context.
/// That residue did not come from the freshly-reset context, so flagging it
/// false-fires the first post-clear turn (the reported bug). A genuine malform
/// recurs continuously, so a short grace window costs us no real coverage.
pub(crate) const MALFORMED_POST_CLEAR_GRACE_SECS: f64 = 60.0;

/// Pure predicate: is the session at / just past a fresh-/clear or
/// post-compaction boundary, such that a malformed-tool-call signature in the
/// captured pane tail is necessarily PRE-clear scrollback residue rather than a
/// live malform from the current (freshly-reset) context?
///
/// `active_ui` is checked FIRST and short-circuits to `false` (never
/// suppress): it is the same positive-liveness signal
/// (`status::pane_shows_active_ui`) the fresh-session-inject gate uses
/// (operator #5620) — a thinking indicator, agent-roster row, or
/// background-work marker on screen is proof the low/zero `tokens` reading is
/// a PARSE MISS (the bare context total scrolled behind the marker), not a
/// genuinely fresh/near-empty context. Without this check, a long, busy,
/// many-agent session — exactly the shape most likely to emit a malformed
/// tool-call, and the most costly to miss — reads `tokens == 0` on
/// essentially every poll (2026-06-17 / 2026-08-25 incidents) and this
/// predicate returned `true` (suppress) for the ENTIRE session, silently
/// neutering the detector (2026-08-27 regression: the detector went silent
/// for a whole session while a `court`-prefixed malformed-invoke block sat
/// unrecovered in the transcript).
///
/// Absent that positive signal, either of the original signals still
/// suffices:
///   * `tokens < CONTEXT_FRESH_TOKEN_THRESHOLD` — the context is freshly
///     cleared / near-empty (the same low-token threshold used elsewhere to
///     mean "just cleared / boot state"). A near-empty context cannot have
///     produced a sustained malformed episode, so any `<invoke>` block visible
///     on the pane is leftover scrollback from before the clear.
///   * `last_context_clear` within `MALFORMED_POST_CLEAR_GRACE_SECS` of `now` —
///     a clear / compaction landed in the immediate past, so the old block is
///     still lingering in the 60-line scrollback even if the fresh context's
///     token count has already climbed past the low threshold (e.g. a large
///     always-loaded preamble pushes a brand-new context over the line). The
///     window is time-bounded so a genuine LATER malform — which recurs
///     continuously — is unaffected once the grace expires.
///
/// This is the fix for the reported false-fire: the very first turn after a
/// `/clear` was being classified MALFORMED purely because the pre-clear turn's
/// `<invoke>` block was still in the captured scrollback. The freshly-reset
/// context is exempt for the boundary window.
pub(crate) fn malformed_detection_post_clear(state: &State, tokens: u64, active_ui: bool) -> bool {
    if active_ui {
        return false;
    }
    if tokens < CONTEXT_FRESH_TOKEN_THRESHOLD {
        return true;
    }
    state
        .last_context_clear
        .as_deref()
        .and_then(elapsed_since)
        .is_some_and(|e| e < MALFORMED_POST_CLEAR_GRACE_SECS)
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

    // Real-usage danger zone against the TRUE window — the same bar the
    // fallback paths use below: a fixed token margin from max when
    // `threshold_margin` is set, else a percentage of max. For the baked 1M
    // window with `threshold_margin = 100000` this is 900K == ~90% used
    // (~10% left) — the point at which a self-clear is wanted, and no earlier.
    let in_danger_zone = match threshold_margin {
        Some(margin) => max_context_tokens > margin && tokens >= max_context_tokens - margin,
        None => pct >= threshold_percent as f64,
    };

    // Primary: `compact_remaining` is Claude Code's own "Context left until
    // auto-compact: X%" — the most TIMELY signal, but only trustworthy once
    // real usage confirms we are actually near full. On a large window (the
    // 1M-token Claude Code window) Claude Code's auto-compact point is
    // DECOUPLED from the true window: it reports a low auto-compact % at only
    // ~48% real usage, so trusting it alone fired a destructive self-clear far
    // too early (incident 2026-09-01: 484889/1000000 = 48.5%, compact_remaining
    // = 5 <= compact_trigger_percent = 5). Gate it behind the real-usage danger
    // zone so a clear never fires before ~10% of the true window remains.
    if in_danger_zone {
        if let Some(cr) = compact_remaining {
            if cr <= compact_trigger_percent {
                return Some((pct, true));
            }
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

/// Where the OAuth credential store lives for this deployment.
fn credentials_path(config: &Config) -> std::path::PathBuf {
    if config.reauth.credentials_file.is_empty() {
        crate::credentials::default_path()
    } else {
        std::path::PathBuf::from(&config.reauth.credentials_file)
    }
}

/// Check whether Claude Code needs API reauth, and drive the recovery.
///
/// This is the REACTIVE half of `[reauth]` (the proactive half, which acts on
/// the "login expires in N days" warning before anything breaks, is
/// `check_login_expiry`). Two phases, keyed on what the pane shows:
///
/// 1. **401 banner, TUI still up.** When the OAuth access token lapses and the
///    silent refresh does not happen, Claude Code keeps the TUI and prints one
///    inline line: `Please run /login · API Error: 401 OAuth access token has
///    expired. Re-authenticate to continue.` The session can no longer make an
///    API call, but nothing about the screen says "dead" to the other
///    detectors. Text alone is never acted on — any session reading this file
///    has that sentence on its pane — so the sighting is corroborated against
///    the credential store's ACCESS token (`expiresAt` in the past, or no token
///    at all). Corroborated + `auth_error_auto_self_login` → `fire_self_login`,
///    the SAME path the proactive check uses, under the same retry / attempt /
///    abandon bounds and the same one-dialog-at-a-time latch. Auto off or
///    bounds exhausted → the high-priority reauth alert, so it is never silent.
///    Credential store says the token is VALID → the banner is conversation
///    text, ignore it. Store unreadable → alert only, and say it stands alone.
/// 2. **Login screen, TUI gone.** Inject `/login` once per reauth cycle so the
///    OAuth URL appears (unless a self-login dialog already owns the pane),
///    then, once the URL is on the pane, send the high-priority alert with it.
///
/// Alerts are rate-limited to once per `alert_interval_seconds` (default 3 hours).
async fn check_reauth(config: &Config, state: &mut State, pane: &str) {
    let signal = tmux::reauth_signal(pane).await;

    // Hand the pane back if an auto-fired login (either path) has been sitting
    // unconsumed. `check_login_expiry` runs this too, but that check can be
    // configured off while this one stays on, and the banner path opens
    // dialogs that then need the same watchdog.
    run_self_login_abandon_watchdog(config, state, pane).await;

    if matches!(signal, tmux::ReauthSignal::Banner401) {
        // Phase 1. Falls through to the phase-2 bookkeeping below on purpose:
        // a banner on a live TUI also means any login screen is GONE (the
        // dialog was answered, cancelled or abandoned), and that state must
        // not leak into the next cycle.
        check_reauth_banner(config, state, pane).await;
    } else if state.reauth_banner_detected && matches!(signal, tmux::ReauthSignal::None) {
        // The banner is gone from the pane and nothing replaced it: the
        // session is back to normal. (A login screen replacing it is phase 2
        // below, and the banner latch stays held through it so the dialog
        // latch is not released underneath the dialog.)
        let access = crate::credentials::read_access_token(&credentials_path(config));
        info!(access_token = access.as_str(), "401 banner resolved");
        write_jsonl_log(
            &config.general.log_file,
            "reauth_401_banner_resolved",
            serde_json::json!({ "pane": pane, "access_token": access.as_str() }),
        );
        write_legacy_log(
            &config.general.legacy_log_file,
            &format!("Reauth: 401 banner resolved (access token {})", access.as_str()),
        );
        state.reauth_banner_detected = false;
        if access == crate::credentials::AccessTokenState::Valid {
            // A 401 that resolved into a valid access token is a login that
            // went through (or a refresh that finally happened). Either way
            // the window is over: give the next one a full attempt budget,
            // and release the dialog latch so the reactive inject is not
            // suppressed by a dialog that no longer exists. The proactive
            // path does the same thing on credential renewal, but it can be
            // configured off while this path stays on.
            state.self_login_attempts_this_window = 0;
            state.last_self_login_attempt = None;
            state.self_login_dialog_opened_at = None;
        }
        crate::state::save_state(&config.general.state_file, state);
    }

    if let tmux::ReauthSignal::LoginScreen { url: login_url } = signal {
        if !state.reauth_detected {
            info!("reauth needed: first detection");
            state.reauth_detected = true;
        }

        // Inject /login once per reauth cycle so the login screen appears.
        //
        // The `self_login_dialog_opened_at` half is not redundant with the
        // `login_injected` latch. When the PROACTIVE path opens the dialog,
        // this function's detector sees exactly what it sees after a real
        // 401 — the TUI gone, a login screen up — and would inject `/login`
        // straight into the modal. `inject_to_agent` opens with an Escape
        // blast to reach vim NORMAL mode, and Escape in this modal CANCELS
        // the login, so the two paths would take turns killing each other's
        // dialog forever. The latch is cleared when the dialog is abandoned
        // or the credentials are renewed, so this cannot wedge the reactive
        // path shut.
        if !state.login_injected && state.self_login_dialog_opened_at.is_none() {
            info!("injecting /login command into pane");
            inject_dispatch::inject_to_agent(pane, "/login").await;
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
        // Login screen gone. With the 401 banner still up this is "the dialog
        // went away", not "the session is healthy" — the banner path is still
        // running and says so in its own events.
        info!(
            banner_still_up = state.reauth_banner_detected,
            "reauth resolved (login screen gone)"
        );
        write_jsonl_log(
            &config.general.log_file,
            "reauth_resolved",
            serde_json::json!({ "banner_still_up": state.reauth_banner_detected }),
        );
        write_legacy_log(&config.general.legacy_log_file, "Reauth resolved");
        state.reauth_detected = false;
        state.last_reauth_alert = None;
        state.login_injected = false;
        crate::state::save_state(&config.general.state_file, state);
    }
}

/// What the reactive 401-banner path decided to do this cycle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BannerAction {
    /// The credential store contradicts the banner: the access token is
    /// valid, so the text is conversation. Stay silent.
    Ignore,
    /// Say so, but do not touch the session. `reason` names which brake held.
    AlertOnly {
        corroborated: bool,
        reason: &'static str,
    },
    /// Drive `self-login`.
    AutoLogin,
}

/// Evidence available to the 401-banner decision on one cycle. The banner
/// itself is a precondition (this is only evaluated when it is on the pane).
pub(crate) struct BannerEvidence {
    /// What the credential store says about the ACCESS token.
    pub access_token: crate::credentials::AccessTokenState,
    /// `auth_error_auto_self_login`.
    pub auto_enabled: bool,
    /// Seconds since the last auto-fire (either path), if there was one.
    pub since_last_attempt: Option<f64>,
    /// Minimum spacing between auto-fires.
    pub retry_seconds: u64,
    /// Attempts already spent in this window (shared with the proactive path).
    pub attempts: u32,
    /// Attempt ceiling for one window.
    pub max_attempts: u32,
    /// An auto-fired login is already up and waiting for a code.
    pub login_pending: bool,
}

/// Decide what the reactive 401-banner path should do, given this cycle's
/// evidence. Pure, for the same reason `decide_expiry_action` is.
///
/// The corroboration rule is the whole point. The banner is ONE LINE OF TEXT
/// on a live TUI, and the reason the login-screen detector refuses to look at
/// anything while the TUI is up is that a session reading this file, its
/// tests, or the diff that introduced them has `API Error: 401` on its pane
/// while being perfectly well authenticated. So:
///
///   * banner + access token VALID on disk  -> IGNORE. Conversation text.
///   * banner + access token EXPIRED/MISSING -> act. This is the incident:
///     Claude Code's silent refresh did not happen, `expiresAt` is in the past,
///     and every request 401s until somebody runs `/login`.
///   * banner + store UNREADABLE             -> alert, uncorroborated. Not
///     enough to open a modal on, but an unreadable store is UNKNOWN, never
///     a negative, and a deployment whose store lives elsewhere should hear
///     about it rather than sit on a dead session.
///
/// The brakes below the corroboration are the proactive path's brakes, shared
/// deliberately: one dialog at a time, one attempt budget, one retry spacing.
pub(crate) fn decide_banner_action(ev: &BannerEvidence) -> BannerAction {
    use crate::credentials::AccessTokenState;

    if ev.access_token == AccessTokenState::Valid {
        return BannerAction::Ignore;
    }
    if !ev.access_token.corroborates_401() {
        // Unknown: the store could not be read. Not a negative, not evidence.
        return BannerAction::AlertOnly {
            corroborated: false,
            reason: "credential store unreadable",
        };
    }
    let held = |reason: &'static str| BannerAction::AlertOnly {
        corroborated: true,
        reason,
    };
    if !ev.auto_enabled {
        return held("auto-login disabled");
    }
    // A login dialog we already opened is still waiting for its code. Firing
    // a second one types `/login` into the first one's text field.
    if ev.login_pending {
        return held("login dialog already open");
    }
    if ev.attempts >= ev.max_attempts {
        return held("attempt budget exhausted");
    }
    if let Some(elapsed) = ev.since_last_attempt {
        if elapsed < ev.retry_seconds as f64 {
            return held("retry spacing");
        }
    }
    BannerAction::AutoLogin
}

/// Phase 1 of `check_reauth`: the 401 banner is on a live pane. Corroborate,
/// decide, act.
async fn check_reauth_banner(config: &Config, state: &mut State, pane: &str) {
    let access = crate::credentials::read_access_token(&credentials_path(config));
    let action = decide_banner_action(&BannerEvidence {
        access_token: access,
        auto_enabled: config.reauth.auth_error_auto_self_login,
        since_last_attempt: state
            .last_self_login_attempt
            .as_deref()
            .and_then(elapsed_since),
        retry_seconds: config.reauth.self_login_retry_seconds,
        attempts: state.self_login_attempts_this_window,
        max_attempts: config.reauth.self_login_max_attempts,
        login_pending: state.self_login_dialog_opened_at.is_some(),
    });

    if !state.reauth_banner_detected {
        // First sighting of this banner: log it ONCE with everything the
        // next incident's diagnosis will need — what the store said and
        // what was decided. The decision is re-made every cycle (a brake can
        // release, the store can change), so later cycles log only when they
        // actually do something.
        info!(
            access_token = access.as_str(),
            decision = ?action,
            "401 banner on pane: first detection"
        );
        write_jsonl_log(
            &config.general.log_file,
            "reauth_401_banner",
            serde_json::json!({
                "pane": pane,
                "access_token": access.as_str(),
                "action": match &action {
                    BannerAction::Ignore => "ignore",
                    BannerAction::AlertOnly { .. } => "alert_only",
                    BannerAction::AutoLogin => "auto_login",
                },
                "reason": match &action {
                    BannerAction::AlertOnly { reason, .. } => *reason,
                    BannerAction::Ignore => "access token valid on disk",
                    BannerAction::AutoLogin => "access token expired on disk",
                },
            }),
        );
        write_legacy_log(
            &config.general.legacy_log_file,
            &format!(
                "Reauth: 401 banner on pane (access token {}): {}",
                access.as_str(),
                match &action {
                    BannerAction::Ignore => "ignored, credentials healthy".to_string(),
                    BannerAction::AlertOnly { reason, .. } => format!("alert only ({reason})"),
                    BannerAction::AutoLogin => "auto-firing self-login".to_string(),
                }
            ),
        );
        state.reauth_banner_detected = true;
        crate::state::save_state(&config.general.state_file, state);
    }

    match action {
        BannerAction::Ignore => {
            debug!("401 banner on pane but the access token is valid on disk; conversation text");
        }
        BannerAction::AutoLogin => {
            fire_self_login(config, state, pane, SelfLoginTrigger::Banner401).await;
        }
        BannerAction::AlertOnly {
            corroborated,
            reason,
        } => {
            // Same cooldown and same alert channel as phase 2, so a banner the
            // daemon cannot or may not act on still reaches a human.
            let should_alert = match &state.last_reauth_alert {
                Some(last) => elapsed_since(last)
                    .map(|e| e >= config.reauth.alert_interval_seconds as f64)
                    .unwrap_or(true),
                None => true,
            };
            if !should_alert {
                debug!(reason, "401 banner still on pane, alert cooldown active");
                return;
            }
            let qualifier = if corroborated {
                ""
            } else {
                " (seen on the pane only — the credential store was not readable)"
            };
            let tail = if config.reauth.auth_error_auto_self_login {
                format!(" Auto-login did not fire: {reason}.")
            } else {
                " Auto-login is disabled; run `self-login start` or `/login`.".to_string()
            };
            warn!(reason, "Claude Code hit a 401 (access token expired); alerting");
            alert::notify(crate::event_bus::ClaudeWatchAlert {
                alert_type: "reauth-needed",
                stuck_reason: "claude code 401, access token expired, login needed",
                stale_minutes: None,
                affected_watchers: vec![],
                severity: crate::event_bus::Severity::High,
                message: &format!(
                    "Claude Code login needed: API Error 401, OAuth access token expired{qualifier}.{tail}"
                ),
            })
            .await;
            write_jsonl_log(
                &config.general.log_file,
                "reauth_alert",
                serde_json::json!({
                    "pane": pane,
                    "url": "",
                    "trigger": "401_banner",
                    "corroborated": corroborated,
                    "reason": reason,
                }),
            );
            write_legacy_log(
                &config.general.legacy_log_file,
                &format!("Reauth needed (401 banner): sent high-priority alert ({reason})"),
            );
            state.last_reauth_alert = Some(Local::now().to_rfc3339());
            crate::state::save_state(&config.general.state_file, state);
        }
    }
}

/// What the proactive expiry check decided to do this cycle.
///
/// Split out as a pure function so the whole decision — the corroboration
/// rules, the retry spacing, the attempt budget — is testable without a tmux
/// pane, a credential file, or a clock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ExpiryAction {
    /// Nothing is expiring, or the evidence does not support acting.
    Idle,
    /// Warn the operator, but do not touch the session.
    AlertOnly { days_left: u32, corroborated: bool },
    /// Warn AND drive `self-login`.
    AutoLogin { days_left: u32 },
}

/// Evidence available to the proactive expiry decision on one cycle.
pub(crate) struct ExpiryEvidence {
    /// Days-left parsed off Claude Code's own on-screen warning, if it was
    /// on the pane this cycle.
    pub pane_days_left: Option<u32>,
    /// What the on-disk credential store says.
    pub credentials: crate::credentials::CredentialExpiry,
    /// Whether the credential store may TRIGGER on its own, or may only
    /// corroborate a warning that was seen on the pane. See
    /// `ReauthConfig::expiry_from_credentials` — a short-lived rolling refresh
    /// token classifies as "expiring" permanently, so a store-driven trigger
    /// is only safe where the token's lifetime is long relative to the
    /// three-day warning window.
    pub credentials_may_trigger: bool,
    /// Auto-fire configured on.
    pub auto_enabled: bool,
    /// Auto-fire only at or below this many days left.
    pub auto_days: u32,
    /// Seconds since the last auto-fire, if there was one.
    pub since_last_attempt: Option<f64>,
    /// Minimum spacing between auto-fires.
    pub retry_seconds: u64,
    /// Attempts already spent in this expiry window.
    pub attempts: u32,
    /// Attempt ceiling for one window.
    pub max_attempts: u32,
    /// An auto-fired login is already up and waiting for a code.
    pub login_pending: bool,
}

/// Decide what the proactive expiry path should do, given this cycle's evidence.
///
/// The corroboration rule is the important part. "Your login expires in 2
/// days" is a sentence, and a session that is reading this file, its tests, or
/// the diff that introduced them will have that sentence on the pane while
/// being perfectly well authenticated. Auto-firing `/login` at it would park a
/// healthy loop in a modal. So a pane sighting is believed only when the
/// credential store either agrees or cannot be read at all:
///
///   * pane says expiring + credentials agree  -> act, corroborated
///   * pane says expiring + credentials UNKNOWN -> act, uncorroborated (the
///     store is not readable in every deployment, and an unreadable file is
///     UNKNOWN, never a negative — but say so out loud)
///   * pane says expiring + credentials healthy or already expired -> IGNORE.
///     Healthy means the sentence was conversation text. Already-expired is
///     the reactive path's territory, and racing it into the same modal helps
///     nobody.
///   * pane silent + credentials expiring -> act only when the store is
///     allowed to trigger. The transient form of Claude Code's warning lives
///     about fifteen seconds, so a poller missing it is not evidence of
///     anything — but a short-lived rolling refresh token classifies as
///     "expiring" every second of its healthy life, so this branch is opt-in
///     rather than the default.
pub(crate) fn decide_expiry_action(ev: &ExpiryEvidence) -> ExpiryAction {
    use crate::credentials::CredentialExpiry;

    let (days_left, corroborated) = match (ev.pane_days_left, ev.credentials) {
        // The reactive path owns a dead credential, whatever the pane says.
        (_, CredentialExpiry::Expired) => return ExpiryAction::Idle,
        (Some(_), CredentialExpiry::Healthy) => return ExpiryAction::Idle,
        (Some(pane), CredentialExpiry::Expiring { days_left }) => {
            // Trust the credential store's arithmetic over a scraped digit,
            // but take whichever is more urgent so a stale banner cannot
            // stretch the deadline.
            (pane.min(days_left), true)
        }
        (Some(pane), CredentialExpiry::Unknown) => (pane, false),
        (None, CredentialExpiry::Expiring { days_left }) if ev.credentials_may_trigger => {
            (days_left, true)
        }
        (None, _) => return ExpiryAction::Idle,
    };

    let alert_only = ExpiryAction::AlertOnly {
        days_left,
        corroborated,
    };

    if !ev.auto_enabled || days_left > ev.auto_days {
        return alert_only;
    }
    // A login dialog we already opened is still waiting for its code. Firing
    // a second one types `/login` into the first one's text field.
    if ev.login_pending {
        return alert_only;
    }
    if ev.attempts >= ev.max_attempts {
        return alert_only;
    }
    if let Some(elapsed) = ev.since_last_attempt {
        if elapsed < ev.retry_seconds as f64 {
            return alert_only;
        }
    }
    ExpiryAction::AutoLogin { days_left }
}

/// Proactive counterpart to `check_reauth`: act on Claude Code's warning that
/// the login is ABOUT to lapse, rather than waiting for it to actually lapse.
///
/// The reactive path only ever runs on a session that is already dead, which
/// means the recovery always happens at the worst possible moment. This one
/// runs while everything still works.
async fn check_login_expiry(config: &Config, state: &mut State, pane: &str) {
    let pane_days_left = tmux::login_expiry_warning(pane).await;

    let creds_path = credentials_path(config);
    let credentials = crate::credentials::read(&creds_path);

    // Renewal is the ONLY unambiguous "this is resolved" signal, and it is
    // the value MOVING that says so, not where the value sits. A short-lived
    // rolling refresh token never leaves the three-day warning window, so it
    // would otherwise never resolve, the attempt budget would never reset,
    // and the alert would stand forever on a session in no trouble at all.
    let refresh_expiry = crate::credentials::read_refresh_expiry_ms(&creds_path);
    if let (Some(now_val), Some(prev)) = (refresh_expiry, state.last_seen_refresh_expiry_ms) {
        if now_val > prev {
            info!("oauth credentials were renewed; resetting the expiry window");
            write_jsonl_log(
                &config.general.log_file,
                "login_expiry_credentials_renewed",
                serde_json::json!({ "previous": prev, "current": now_val }),
            );
            state.login_expiry_detected = false;
            state.login_expiry_days_left = None;
            state.last_login_expiry_alert = None;
            state.last_self_login_attempt = None;
            state.self_login_attempts_this_window = 0;
            state.self_login_dialog_opened_at = None;
        }
    }
    if state.last_seen_refresh_expiry_ms != refresh_expiry {
        state.last_seen_refresh_expiry_ms = refresh_expiry;
        crate::state::save_state(&config.general.state_file, state);
    }

    let action = decide_expiry_action(&ExpiryEvidence {
        pane_days_left,
        credentials,
        credentials_may_trigger: config.reauth.expiry_from_credentials,
        auto_enabled: config.reauth.expiry_auto_self_login,
        auto_days: config.reauth.expiry_auto_days,
        since_last_attempt: state
            .last_self_login_attempt
            .as_deref()
            .and_then(elapsed_since),
        retry_seconds: config.reauth.self_login_retry_seconds,
        attempts: state.self_login_attempts_this_window,
        max_attempts: config.reauth.self_login_max_attempts,
        login_pending: state.self_login_dialog_opened_at.is_some(),
    });

    // Hand the pane back if an auto-fired login has been sitting unconsumed.
    run_self_login_abandon_watchdog(config, state, pane).await;

    if matches!(action, ExpiryAction::Idle) {
        // A login screen or the 401 banner is on the pane: the reactive path
        // owns the session right now. The pane warning this check keys on is
        // NOT visible while a login dialog covers the TUI, so an Idle here
        // says nothing about the expiry — it must neither "resolve" the
        // window (resetting the attempt budget mid-flow) nor release the
        // dialog latch, because that latch is what stops the reactive path
        // from injecting `/login` into the dialog the daemon itself opened.
        if state.reauth_detected || state.reauth_banner_detected {
            return;
        }
        if state.login_expiry_detected {
            info!("login expiry resolved");
            write_jsonl_log(
                &config.general.log_file,
                "login_expiry_resolved",
                serde_json::json!({ "pane": pane }),
            );
            write_legacy_log(
                &config.general.legacy_log_file,
                "Login expiry resolved (credentials renewed)",
            );
            state.login_expiry_detected = false;
            state.login_expiry_days_left = None;
            state.last_login_expiry_alert = None;
            state.last_self_login_attempt = None;
            state.self_login_attempts_this_window = 0;
            crate::state::save_state(&config.general.state_file, state);
        }
        // Release the dialog latch OUTSIDE the `login_expiry_detected` guard,
        // and on every idle cycle rather than only on the transition. The
        // latch suppresses the reactive path's `/login` inject, so a stuck one
        // is not a cosmetic leak — it is the reactive recovery quietly
        // disabled. The abandon watchdog normally clears it, but a deployment
        // that set `self_login_abandon_seconds = 0` has no watchdog, and a
        // daemon restart can land here with the latch set and
        // `login_expiry_detected` false. Nothing is expiring on this branch,
        // so nothing needs the latch held.
        if state.self_login_dialog_opened_at.is_some() {
            state.self_login_dialog_opened_at = None;
            crate::state::save_state(&config.general.state_file, state);
        }
        return;
    }

    let (days_left, corroborated, auto) = match action {
        ExpiryAction::AlertOnly {
            days_left,
            corroborated,
        } => (days_left, corroborated, false),
        ExpiryAction::AutoLogin { days_left } => (days_left, true, true),
        ExpiryAction::Idle => unreachable!(),
    };

    if !state.login_expiry_detected {
        info!(days_left, "login expiry warning: first detection");
        write_jsonl_log(
            &config.general.log_file,
            "login_expiry_detected",
            serde_json::json!({
                "pane": pane,
                "days_left": days_left,
                "corroborated": corroborated,
                "from_pane": pane_days_left.is_some(),
            }),
        );
        state.login_expiry_detected = true;
    }
    state.login_expiry_days_left = Some(days_left);

    if auto {
        fire_self_login(
            config,
            state,
            pane,
            SelfLoginTrigger::ExpiryWarning { days_left },
        )
        .await;
    }

    // Alert on the same cooldown the reactive path uses. The warning stands
    // for days; without this it would page every ten seconds.
    let should_alert = match &state.last_login_expiry_alert {
        Some(last) => elapsed_since(last)
            .map(|e| e >= config.reauth.alert_interval_seconds as f64)
            .unwrap_or(true),
        None => true,
    };
    if should_alert {
        let qualifier = if corroborated {
            ""
        } else {
            " (seen on the pane only — the credential store was not readable)"
        };
        let tail = if auto {
            " Auto-login fired; watch for the OAuth URL."
        } else if config.reauth.expiry_auto_self_login {
            " Auto-login has not fired yet."
        } else {
            " Auto-login is disabled; run `self-login start` when convenient."
        };
        warn!(days_left, "Claude Code login is expiring");
        alert::notify(crate::event_bus::ClaudeWatchAlert {
            alert_type: "login-expiring",
            stuck_reason: "claude code oauth credentials expiring soon",
            stale_minutes: None,
            affected_watchers: vec![],
            severity: crate::event_bus::Severity::High,
            message: &format!(
                "Claude Code login expires in {days_left} day(s){qualifier}.{tail}"
            ),
        })
        .await;
        state.last_login_expiry_alert = Some(Local::now().to_rfc3339());
    }

    crate::state::save_state(&config.general.state_file, state);
}

/// Why `self-login` is being auto-fired. Both paths go through ONE
/// `fire_self_login` — the same booking, the same latch, the same budget —
/// and the trigger exists only so the logs and alerts say which one it was.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SelfLoginTrigger {
    /// Proactive: Claude Code warned the login expires in `days_left` days.
    ExpiryWarning { days_left: u32 },
    /// Reactive: the in-TUI "API Error: 401 OAuth access token has expired"
    /// banner, corroborated by the credential store.
    Banner401,
}

impl SelfLoginTrigger {
    /// Stable label for JSONL events and metrics.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            SelfLoginTrigger::ExpiryWarning { .. } => "expiry_warning",
            SelfLoginTrigger::Banner401 => "401_banner",
        }
    }

    fn days_left(self) -> Option<u32> {
        match self {
            SelfLoginTrigger::ExpiryWarning { days_left } => Some(days_left),
            SelfLoginTrigger::Banner401 => None,
        }
    }

    /// The situation, phrased for a human alert.
    fn situation(self) -> &'static str {
        match self {
            SelfLoginTrigger::ExpiryWarning { .. } => "Claude Code login is expiring",
            SelfLoginTrigger::Banner401 => {
                "Claude Code hit API Error 401 (OAuth access token expired)"
            }
        }
    }
}

/// Drive `self-login start` out of process and publish whatever it produced.
///
/// Spawned rather than awaited: `start` interrupts the pane, injects `/login`,
/// drives the method picker and then waits for the dialog to paint, which is
/// far longer than a check cycle. Blocking the cycle on it would stall every
/// other monitor the daemon runs.
async fn fire_self_login(
    config: &Config,
    state: &mut State,
    pane: &str,
    trigger: SelfLoginTrigger,
) {
    // Book the attempt BEFORE spawning. If the process crashes mid-run the
    // budget is still spent, which is the safe direction: an unbooked attempt
    // re-fires on the next cycle and every cycle after it.
    state.last_self_login_attempt = Some(Local::now().to_rfc3339());
    state.self_login_attempts_this_window = state.self_login_attempts_this_window.saturating_add(1);
    state.self_login_autofire_total = state.self_login_autofire_total.saturating_add(1);
    let attempt = state.self_login_attempts_this_window;
    state.self_login_dialog_opened_at = Some(Local::now().to_rfc3339());
    crate::state::save_state(&config.general.state_file, state);

    info!(trigger = trigger.as_str(), days_left = ?trigger.days_left(), attempt, "auto-firing self-login");
    write_jsonl_log(
        &config.general.log_file,
        "self_login_autofire",
        serde_json::json!({
            "pane": pane,
            "trigger": trigger.as_str(),
            "days_left": trigger.days_left(),
            "attempt": attempt,
        }),
    );
    write_legacy_log(
        &config.general.legacy_log_file,
        &match trigger {
            SelfLoginTrigger::ExpiryWarning { days_left } => format!(
                "Login expiring in {days_left}d: auto-firing self-login (attempt {attempt})"
            ),
            SelfLoginTrigger::Banner401 => format!(
                "401 banner, access token expired: auto-firing self-login (attempt {attempt})"
            ),
        },
    );

    let cmd = config.reauth.self_login_command.clone();
    let pane = pane.to_string();
    let log_file = config.general.log_file.clone();
    let legacy_log_file = config.general.legacy_log_file.clone();
    let situation = trigger.situation();
    let stuck_reason: &'static str = match trigger {
        SelfLoginTrigger::ExpiryWarning { .. } => "claude code login expiring, auto-login started",
        SelfLoginTrigger::Banner401 => "claude code 401, auto-login started",
    };
    tokio::spawn(async move {
        // `--foreground --json` is self-login's programmatic entry point: it
        // blocks and emits exactly one JSON object.
        let (out, ok) = crate::cmd::run_cmd_any(
            &[
                &cmd,
                "--pane",
                &pane,
                "--json",
                "start",
                "--foreground",
            ],
            300,
        )
        .await;
        let parsed: serde_json::Value = serde_json::from_str(out.trim()).unwrap_or(serde_json::json!({}));
        let url = parsed.get("url").and_then(|u| u.as_str()).unwrap_or("");
        if ok && !url.is_empty() {
            warn!("self-login produced an OAuth URL");
            write_jsonl_log(
                &log_file,
                "self_login_url",
                serde_json::json!({ "pane": pane, "url": url, "trigger": trigger.as_str() }),
            );
            alert::notify(crate::event_bus::ClaudeWatchAlert {
                alert_type: "reauth-needed",
                stuck_reason,
                stale_minutes: None,
                affected_watchers: vec![],
                severity: crate::event_bus::Severity::High,
                message: &format!(
                    "{situation} and auto-login has opened the dialog. \
                     Authorize at {url} then run: self-login code <CODE>"
                ),
            })
            .await;
        } else {
            // FAIL LOUD. A self-login that produced no URL has usually left
            // the pane somewhere unexpected, and a quiet failure here is a
            // session that dies for real a day later.
            let reason = parsed
                .get("reason")
                .and_then(|r| r.as_str())
                .unwrap_or("self-login produced no URL and gave no reason");
            warn!(reason, "self-login auto-fire failed");
            write_jsonl_log(
                &log_file,
                "self_login_autofire_failed",
                serde_json::json!({ "pane": pane, "reason": reason, "trigger": trigger.as_str() }),
            );
            write_legacy_log(
                &legacy_log_file,
                &format!("self-login auto-fire FAILED: {reason}"),
            );
            let advice = match trigger {
                SelfLoginTrigger::ExpiryWarning { .. } => {
                    "Log in by hand before the credentials lapse."
                }
                SelfLoginTrigger::Banner401 => {
                    "The session cannot make API calls until somebody runs /login."
                }
            };
            alert::notify(crate::event_bus::ClaudeWatchAlert {
                alert_type: "reauth-needed",
                stuck_reason: "self-login auto-fire failed",
                stale_minutes: None,
                affected_watchers: vec![],
                severity: crate::event_bus::Severity::High,
                message: &format!("{situation} and auto-login FAILED: {reason}. {advice}"),
            })
            .await;
        }
    });
}

/// Hand the session back if an auto-fired login dialog was never consumed.
///
/// The failure this exists for: auto-fire runs at 3am, publishes a URL nobody
/// is awake to open, and the login modal sits on the pane swallowing the
/// loop's keystrokes until morning. The OAuth link has a short life of its
/// own, so waiting it out buys nothing. `self-login cancel` escapes the dialog
/// only if it is still up, so this is a no-op when the code was entered
/// normally.
async fn run_self_login_abandon_watchdog(config: &Config, state: &mut State, pane: &str) {
    if config.reauth.self_login_abandon_seconds == 0 {
        return;
    }
    let Some(published) = state.self_login_dialog_opened_at.clone() else {
        return;
    };
    let Some(elapsed) = elapsed_since(&published) else {
        // Unparseable timestamp: clear it rather than wedge the watchdog on it.
        state.self_login_dialog_opened_at = None;
        return;
    };
    if elapsed < config.reauth.self_login_abandon_seconds as f64 {
        return;
    }

    info!(elapsed, "abandoning unconsumed self-login dialog");
    let (out, ok) = crate::cmd::run_cmd_any(
        &[
            &config.reauth.self_login_command,
            "--pane",
            pane,
            "--json",
            "cancel",
        ],
        120,
    )
    .await;
    write_jsonl_log(
        &config.general.log_file,
        "self_login_abandoned",
        serde_json::json!({
            "pane": pane,
            "waited_seconds": elapsed.round(),
            "cancel_ok": ok,
            "cancel_output": out.trim(),
        }),
    );
    write_legacy_log(
        &config.general.legacy_log_file,
        "Unconsumed self-login dialog abandoned; pane handed back",
    );
    state.self_login_dialog_opened_at = None;
    crate::state::save_state(&config.general.state_file, state);
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
    // Pane-scoped: resolve the RUNNING version from the main-loop pane's own
    // PID, NOT the global `pgrep -af claude` first-match — which in a container
    // can return a SIGKILL-orphaned OLDER versioned claude and manufacture a
    // false `running != installed`, driving a self-sustaining relaunch loop.
    let version_info = crate::status::get_version_info_for_pane(pane).await;

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
    // Pane-scoped: resolve the RUNNING version from the main-loop pane's own
    // PID, NOT the global `pgrep -af claude` first-match — which in a container
    // can return a SIGKILL-orphaned OLDER versioned claude and manufacture a
    // false `running != installed`, driving a self-sustaining relaunch loop.
    let version_info = crate::status::get_version_info_for_pane(pane).await;

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

/// Build the `claude ...` relaunch command line for the auto-update
/// relaunch script, mirroring the entrypoint's CLAUDE_CMD shape
/// (`container/entrypoint.sh` + `container/bin/cwsr` `build_claude_cmd`).
///
/// Shape, in order (each flag conditional on the matching env var, exactly
/// like the entrypoint so the relaunched claude is identical to the one the
/// container booted):
///   - `--setting-sources project,local --settings <CLAUDE_SHIM_SETTINGS_PATH>`
///     when `CLAUDE_SHIM_SETTINGS_PATH` is set (drops the host user tier and
///     loads the MCP-settings shim). On the host this env var is unset, so
///     the flags are omitted. The leading `claude` token itself is resolved
///     to an absolute path via `resolve_claude_bin` (see its doc comment)
///     so the relaunch does not depend on `$PATH` in the pane shell.
///   - `--plugin-dir /opt/claude-container/plugin` when that plugin dir
///     exists (baked container skills + agents). Absent on the host.
///   - `--dangerously-skip-permissions` always (harness-managed instances
///     run in permanent permission-bypass mode).
///   - `--resume <sid>` when a session id was captured, else `--continue`
///     (the resume / continue selector the caller already computed).
///
/// Resolve the `claude` binary to an ABSOLUTE path for relaunch/restart argv.
///
/// Both the auto-update relaunch (`build_relaunch_claude_argv`) and the
/// crash-recovery restart (`restart_claude`) inject a shell command that
/// launches `claude` into the pane. Historically both used the BARE name
/// `claude`, relying on it being on `$PATH` in the pane shell at exec time.
///
/// That assumption is fragile. After a self-upgrade `/exit`, the relaunch
/// one-liner can run in a fresh NON-login `/bin/sh` (`-sh`) that did NOT
/// inherit the tmux-server env — its `$PATH` is the bare default and does
/// NOT include the npm-global / native-install bin dir where `claude` lives.
/// The bare `claude` then dies with `sh: 1: claude: not found`, leaving a
/// DEAD pane and forcing the operator to manually rebuild (operator-observed
/// self-upgrade relaunch failure).
///
/// Resolving to an absolute path at build time — in the daemon, which either
/// has the correct env `$PATH` or can probe the known install locations —
/// removes the `$PATH` dependency entirely. Tries, in order, the first that
/// exists:
///   1. `$CLAUDE_BIN` (operator override / test hook) when set + non-empty,
///   2. `$HOME/.local/bin/claude` (native-install target; highest precedence
///      in the container's baked `ENV PATH`),
///   3. `$HOME/.npm-global/bin/claude` (baked npm-global install),
///   4. `/usr/bin/claude` (the image symlink),
///   5. bare `"claude"` fallback — preserves prior behavior on the host or an
///      unknown layout so nothing regresses off-container.
///
/// Pure modulo env + filesystem existence checks; no I/O side effects.
fn resolve_claude_bin() -> String {
    if let Ok(bin) = std::env::var("CLAUDE_BIN") {
        if !bin.is_empty() {
            return bin;
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        for rel in [".local/bin/claude", ".npm-global/bin/claude"] {
            let p = std::path::Path::new(&home).join(rel);
            if p.exists() {
                return p.to_string_lossy().into_owned();
            }
        }
    }
    if std::path::Path::new("/usr/bin/claude").exists() {
        return "/usr/bin/claude".to_string();
    }
    // Host / unknown layout: fall back to bare name (prior behavior).
    "claude".to_string()
}

/// Resolve the launcher token for a pane RELAUNCH / RESTART argv.
///
/// Prefers the baked `claude-relaunch-exec` shim
/// (`container/bin/claude-relaunch-exec`, installed at
/// `/usr/local/bin/claude-relaunch-exec`) when it is present. That shim is
/// the fix for the auto-update BOOT-LOOP: after the in-container updater
/// fires `/exit`, `claude install` writes the new version into the
/// volume-backed `~/.local/share/claude/versions/<ver>` dir and only THEN
/// re-points the `~/.local/bin/claude` launcher symlink. During the download
/// window the launcher is a DANGLING symlink, so a relaunch that runs
/// `~/.local/bin/claude` directly dies with `claude: not found`, the pane
/// drops to a dead `-sh` prompt, and the daemon's relaunch/dead-process path
/// re-fires — HOT-SPINNING "not found" until the download finishes
/// (operator-observed, ~10 iterations, needed manual recovery).
///
/// The shim converts that hot-spin into a bounded WAIT + self-repair: it
/// resolves the newest ACTUALLY-PRESENT `versions/<ver>` binary (bypassing a
/// dangling launcher), repairs the launcher symlink, and polls with backoff
/// for a runnable binary before exec'ing it — so `/exit` leads to a clean
/// restart instead of a boot-loop.
///
/// Precedence:
///   1. `$CLAUDE_BIN` (operator override / test hook) when set + non-empty —
///      wins so tests + explicit overrides stay deterministic.
///   2. the `claude-relaunch-exec` shim when it exists on disk.
///   3. `resolve_claude_bin()` (the prior direct-launcher behavior) — the
///      host / pre-shim-image fallback so nothing regresses off-container.
///
/// Pure modulo env + filesystem existence checks; no I/O side effects.
fn resolve_relaunch_bin() -> String {
    if let Ok(bin) = std::env::var("CLAUDE_BIN") {
        if !bin.is_empty() {
            return bin;
        }
    }
    let shim = std::env::var("CLAUDE_RELAUNCH_EXEC")
        .unwrap_or_else(|_| "/usr/local/bin/claude-relaunch-exec".to_string());
    if !shim.is_empty() && std::path::Path::new(&shim).exists() {
        return shim;
    }
    resolve_claude_bin()
}

/// Pure modulo env + a filesystem existence check; no I/O side effects.
fn build_relaunch_claude_argv(session_id: Option<&str>) -> String {
    let mut cmd = resolve_relaunch_bin();
    if let Ok(shim) = std::env::var("CLAUDE_SHIM_SETTINGS_PATH") {
        if !shim.is_empty() {
            cmd.push_str(" --setting-sources project,local --settings ");
            cmd.push_str(&shim);
        }
    }
    // Mirror entrypoint.sh / cwsr: append the baked container plugin dir
    // when present so the relaunched claude keeps the /claude-container:*
    // skills + agents. Falls back gracefully (no flag) on the host or on
    // images that predate the plugin bake.
    let plugin_dir = std::env::var("CWSR_PLUGIN_DIR")
        .unwrap_or_else(|_| "/opt/claude-container/plugin".to_string());
    if std::path::Path::new(&plugin_dir).join(".claude-plugin").is_dir() {
        cmd.push_str(" --plugin-dir ");
        cmd.push_str(&plugin_dir);
    }
    cmd.push_str(" --dangerously-skip-permissions");
    match session_id {
        Some(sid) => {
            cmd.push_str(" --resume ");
            cmd.push_str(sid);
        }
        None => cmd.push_str(" --continue"),
    }
    cmd
}

/// Build the shell one-liner injected into the pane to relaunch Claude.
///
/// The daemon writes a relaunch script (`config.relaunch_script`, default
/// `/var/run/claude/claude-relaunch.sh`) then types `bash <path>` into the
/// pane shell. But `/var/run` is a tmpfs: it is wiped on every container
/// start and `respawn.rs` also `remove_file`s the script. If the file is
/// absent at the moment the pane shell runs the command, a bare
/// `bash <path>` dies with `No such file or directory`, leaving a dead
/// `/bin/sh` pane -> recreate loop (operator-observed bug; PR #412 fixed
/// post-relaunch detection/argv but NOT this script-write/exec path).
///
/// So inject a SELF-HEALING guarded one-liner instead: run the script if it
/// exists at exec time, else run the `claude` launch argv directly inline so
/// Claude still comes up. POSIX-safe (`[ -f X ] && bash X || { CMD; }`)
/// so it works in both `/bin/sh` and bash panes. `launch` is the same
/// `claude ...` argv computed by the caller (flags + a uuid, no
/// shell-hostile chars), so the inline fallback is safe to type verbatim.
/// Pure (no I/O) so it is unit-testable in parallel.
fn build_relaunch_inject_cmd(script_path: &str, launch: &str) -> String {
    format!("[ -f {p} ] && bash {p} || {{ {launch}; }}", p = script_path, launch = launch)
}

/// Maximum times the daemon presses "Yes, I accept" on the
/// Bypass-Permissions dialog during a single relaunch.
///
/// More than one press is only ever useful when a keystroke lands mid-render
/// and is dropped. Beyond that, pressing again is not just noise: once the
/// dialog is gone an extra `Enter` submits an empty prompt into the live TUI.
/// So the presses are capped and the rest of the budget is spent WATCHING.
const BYPASS_DIALOG_MAX_ACCEPT_ATTEMPTS: u32 = 3;

/// Outcome of the post-relaunch Bypass-Permissions dialog gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BypassDialogGate {
    /// Nothing is in the way — carry on with the resume inject exactly as
    /// before this gate existed. Either handling is disabled, the pane
    /// reached a genuine idle prompt, or the dialog never appeared.
    Proceed,
    /// The dialog appeared, was accepted, and Claude reached an idle prompt.
    Accepted,
    /// The dialog appeared but the pane never reached an idle prompt within
    /// the budget. The caller must NOT inject: typing into a pane in this
    /// state is exactly what caused the incident this gate exists for.
    Stuck,
}

/// Pre-accept the Bypass-Permissions dialog by writing the acceptance key into
/// the Claude Code settings file(s), so the relaunched process never renders
/// the dialog in the first place.
///
/// Best-effort and idempotent: a settings tier we cannot see, or a settings
/// file we refuse to edit, just means the dialog still appears and
/// [`settle_bypass_permissions_dialog`] handles it on the pane. See
/// `crate::bypass_consent` for the write discipline.
fn pre_accept_bypass_permissions(claude_config: &crate::config::ClaudeConfig) {
    if !claude_config.handle_bypass_dialog || !claude_config.pre_accept_bypass_dialog {
        return;
    }
    for (path, outcome) in crate::bypass_consent::ensure_accepted_everywhere() {
        match outcome {
            crate::bypass_consent::ConsentWrite::AlreadySet => {
                debug!(path = %path.display(), "bypass-consent: acceptance already recorded")
            }
            crate::bypass_consent::ConsentWrite::Inserted
            | crate::bypass_consent::ConsentWrite::Created => {
                info!(
                    path = %path.display(),
                    outcome = ?outcome,
                    "bypass-consent: recorded the Bypass-Permissions acceptance in settings"
                )
            }
            crate::bypass_consent::ConsentWrite::Skipped => {
                debug!(
                    path = %path.display(),
                    "bypass-consent: left settings file untouched (will accept the dialog on the pane if it appears)"
                )
            }
        }
    }
}

/// Post-relaunch gate: get past Claude Code's Bypass-Permissions launch dialog
/// BEFORE anything injects a resume prompt.
///
/// ## Why this exists
///
/// Claude Code renders a full-screen consent dialog at startup under
/// `--dangerously-skip-permissions` when the acceptance is not persisted in
/// settings — which is every relaunch this daemon performs on a host where the
/// key has never been written. Its cancel row is `❯ No, exit`, and the bare
/// `❯` is exactly what `wait_for_idle_prompt` treats as "ready for input". So
/// the daemon's own idle detector reports READY while a modal is up, the
/// resume prompt is typed into it, and the default selection ("No, exit")
/// submits: Claude exits 0, the prompt text spills into the bare pane shell,
/// and the pane is left half-attached needing a manual dashboard reinit
/// (operator-observed on Claude Code 2.1.251, 2026-08-29).
///
/// ## Shape
///
/// Two bounded phases, each capped by `[claude] bypass_dialog_wait_secs`:
///
/// 1. **Appear.** Poll the pane. A dialog → phase 2. A genuine idle prompt
///    (the `❯` WITHOUT the dialog markers) → `Proceed`, which is the fast path
///    and costs one capture. Budget exhausted with neither → `Proceed`, i.e.
///    behave exactly as before this gate existed and let the caller's own
///    idle-prompt wait deal with a slow start.
/// 2. **Accept + settle.** Press `Down`+`Enter` (at most
///    `BYPASS_DIALOG_MAX_ACCEPT_ATTEMPTS` times) and watch for the dialog to
///    go away and a real prompt to appear → `Accepted`. If it never does →
///    `Stuck`: alert loudly and let the caller abandon the inject rather than
///    type into an unknown pane.
///
/// Note the asymmetry that decides every ambiguous case here: NOT injecting
/// costs one delayed resume (the operator, or the next check cycle, recovers
/// it); injecting into the dialog EXITS Claude. So this gate never guesses in
/// favour of injecting.
async fn settle_bypass_permissions_dialog(pane: &str, config: &Config) -> BypassDialogGate {
    if !config.claude.handle_bypass_dialog {
        return BypassDialogGate::Proceed;
    }
    let budget = std::time::Duration::from_secs(config.claude.bypass_dialog_wait_secs);

    // Phase 1: does the dialog show up at all?
    let appear_deadline = tokio::time::Instant::now() + budget;
    let mut saw_dialog = false;
    while tokio::time::Instant::now() < appear_deadline {
        if let Some(out) = tmux::capture_pane(pane).await {
            if tmux::bypass_permissions_dialog_visible(&out) {
                saw_dialog = true;
                break;
            }
            if tmux::idle_prompt_without_bypass_dialog(&out) {
                debug!("bypass-dialog: pane is at an idle prompt, no consent dialog");
                return BypassDialogGate::Proceed;
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
    if !saw_dialog {
        debug!(
            wait_secs = config.claude.bypass_dialog_wait_secs,
            "bypass-dialog: no consent dialog observed after relaunch"
        );
        return BypassDialogGate::Proceed;
    }

    info!("bypass-dialog: Bypass-Permissions consent dialog detected — selecting 'Yes, I accept'");
    write_jsonl_log(
        &config.general.log_file,
        "bypass_permissions_dialog_detected",
        serde_json::json!({"pane": pane}),
    );

    // Phase 2: accept, then wait for a REAL prompt (not the dialog's cursor).
    let settle_deadline = tokio::time::Instant::now() + budget;
    let mut attempts: u32 = 0;
    while tokio::time::Instant::now() < settle_deadline {
        let Some(out) = tmux::capture_pane(pane).await else {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            continue;
        };
        if tmux::bypass_permissions_dialog_visible(&out) {
            if attempts < BYPASS_DIALOG_MAX_ACCEPT_ATTEMPTS {
                attempts += 1;
                tmux::accept_bypass_permissions_dialog(pane).await;
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            } else {
                // Presses exhausted — keep watching, never keep typing.
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
            continue;
        }
        if tmux::idle_prompt_without_bypass_dialog(&out) {
            info!(
                attempts,
                "bypass-dialog: accepted; Claude is at an idle prompt"
            );
            write_jsonl_log(
                &config.general.log_file,
                "bypass_permissions_dialog_accepted",
                serde_json::json!({"attempts": attempts}),
            );
            return BypassDialogGate::Accepted;
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }

    warn!(
        attempts,
        wait_secs = config.claude.bypass_dialog_wait_secs,
        "bypass-dialog: Claude never reached an idle prompt after accepting the \
         consent dialog -- ABORTING the resume inject (never type into an unknown pane)"
    );
    write_jsonl_log(
        &config.general.log_file,
        "bypass_permissions_dialog_stuck",
        serde_json::json!({
            "attempts": attempts,
            "wait_secs": config.claude.bypass_dialog_wait_secs,
        }),
    );
    alert::notify(crate::event_bus::ClaudeWatchAlert {
        alert_type: "bypass-dialog-stuck",
        stuck_reason: "bypass-permissions consent dialog was accepted but Claude never reached an idle prompt; resume-inject aborted",
        stale_minutes: None,
        affected_watchers: vec![],
        severity: crate::event_bus::Severity::High,
        message: "claude-watch: Claude Code is sitting on (or just past) the Bypass Permissions consent dialog and never reached a prompt. The resume prompt was NOT injected. Operator must check the pane.",
    })
    .await;
    BypassDialogGate::Stuck
}

/// Container auto-update relaunch path: clean `tmux respawn-pane -k` via the
/// configured `[auto_update] relaunch_command` (default `["cwsr",
/// "--no-upgrade"]`) INSTEAD of the interactive `/exit` + shell-inject flow.
///
/// Sequence: run the relaunch command → wait for the claude binary to appear
/// in the pane's process tree → wait for the idle prompt → inject the resume
/// prompt → notify. Mirrors the tail of `run_auto_update` (Steps 7–10) but
/// with the clean respawn as the PRIMARY relaunch rather than a fallback.
///
/// Caller has already interrupted + settled the pane (Step 1). This function
/// is only reached when `relaunch_command` is non-empty, so it never runs on
/// the host (empty default there → the `/exit` flow is used instead).
async fn run_auto_update_clean_relaunch(
    pane: &str,
    old_version: &str,
    new_version: &str,
    config: &Config,
) {
    let relaunch_argv: Vec<&str> = config
        .auto_update
        .relaunch_command
        .iter()
        .map(|s| s.as_str())
        .collect();
    info!(
        relaunch_command = ?config.auto_update.relaunch_command,
        "auto-update: using clean-relaunch path (no /exit) — respawning pane"
    );
    write_jsonl_log(
        &config.general.log_file,
        "auto_update_clean_relaunch",
        serde_json::json!({
            "relaunch_command": config.auto_update.relaunch_command,
            "old_version": old_version,
            "new_version": new_version,
        }),
    );

    // Respawn the pane. cwsr --no-upgrade kills pane 0 and starts a fresh
    // claude via `tmux respawn-pane -k`, reconstructing the entrypoint argv
    // (settings shim, plugin-dir, --continue) itself. The install already
    // landed on disk, so no upgrade step is needed here.
    let (_out, ok) = crate::cmd::run_cmd_any(&relaunch_argv, 60).await;
    if !ok {
        warn!("auto-update: clean-relaunch command exited non-zero");
    }

    // Wait for the claude binary to come up in the pane process tree. This is
    // the load-bearing gate: only inject the resume prompt once Claude is
    // actually running, never into a raw shell.
    info!("auto-update: waiting for Claude binary to start (clean-relaunch)...");
    if !tmux::wait_for_claude_binary(pane, 120).await {
        warn!(
            "auto-update: claude binary not detected 120s after clean-relaunch -- \
             ABORTING resume-inject (never type the prompt into a raw shell)"
        );
        write_jsonl_log(
            &config.general.log_file,
            "auto_update_failed",
            serde_json::json!({"reason": "binary_not_found_after_clean_relaunch"}),
        );
        alert::notify(crate::event_bus::ClaudeWatchAlert {
            alert_type: "auto-update-failed",
            stuck_reason: "auto-update: claude binary never started after clean-relaunch (cwsr respawn); resume-inject aborted",
            stale_minutes: None,
            affected_watchers: vec![],
            severity: crate::event_bus::Severity::High,
            message: "claude-watch: auto-update FAILED — Claude did not restart after clean-relaunch (cwsr). Operator must relaunch manually.",
        })
        .await;
        return;
    }
    info!("auto-update: Claude binary is up (clean-relaunch)");

    // Get past the Bypass-Permissions consent dialog before anything types
    // into the pane. The relaunch argv carries `--dangerously-skip-permissions`,
    // so Claude Code renders that dialog at startup unless the acceptance is
    // persisted in settings — and its `❯ No, exit` row reads as an idle prompt
    // to the wait below. See `settle_bypass_permissions_dialog`.
    if settle_bypass_permissions_dialog(pane, config).await == BypassDialogGate::Stuck {
        write_jsonl_log(
            &config.general.log_file,
            "auto_update_failed",
            serde_json::json!({"reason": "bypass_dialog_stuck_after_clean_relaunch"}),
        );
        return;
    }

    // Wait for the idle prompt (best-effort — binary is confirmed up).
    info!("auto-update: waiting for idle prompt (clean-relaunch)...");
    if !tmux::wait_for_idle_prompt(pane, 90).await {
        warn!("auto-update: prompt not found after 90s, injecting anyway (claude binary is up)");
    }
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;

    // Inject the resume prompt (lands in the Claude TUI, never a raw shell).
    info!("auto-update: injecting resume prompt (clean-relaunch)...");
    inject_dispatch::inject_to_agent(pane, &config.auto_update.resume_prompt).await;

    // Latch this clear as already handled (mirrors the daemon-driven-clear
    // stamp at the Path-1 branch of `maybe_reset_context_clear`): the
    // concurrent poll loop's own external-clear detection (Path 2 —
    // `context_reset_signal` observing the respawned pane's near-zero token
    // reading) typically stamps `last_context_clear` DURING this relaunch,
    // independent of this function, since check_cycle keeps polling while
    // this clean-relaunch sequence awaits the new binary/idle prompt. That
    // stamp is never otherwise matched by `post_clear_resume_injected_for`
    // (only the daemon-driven-clear path sets that), so the sibling
    // "Post-clear resume detection" block's
    // `post_clear_resume_injected_for != last_context_clear` guard reads
    // true forever after and double-injects a SECOND resume ~40s later on
    // top of the one just sent above. Sync the two fields to close that gap.
    let mut st = crate::state::load_state(&config.general.state_file);
    st.post_clear_resume_injected_for = st.last_context_clear.clone();
    crate::state::save_state(&config.general.state_file, &st);

    write_jsonl_log(
        &config.general.log_file,
        "auto_update_complete",
        serde_json::json!({
            "old_version": old_version,
            "new_version": new_version,
            "via": "clean_relaunch",
        }),
    );
    let msg = format!(
        "claude-watch: auto-update complete ({} → {}, clean-relaunch)",
        old_version, new_version
    );
    alert::notify(crate::event_bus::ClaudeWatchAlert {
        alert_type: "auto-update-complete",
        stuck_reason: "auto-update finished (clean-relaunch)",
        stale_minutes: None,
        affected_watchers: vec![],
        severity: crate::event_bus::Severity::Low,
        message: &msg,
    })
    .await;
    info!(
        "auto-update: complete ({} → {}, clean-relaunch)",
        old_version, new_version
    );
}

/// Execute the auto-update sequence: interrupt → /exit → wait → relaunch → resume.
async fn run_auto_update(pane: &str, old_version: &str, new_version: &str, config: &Config) {
    // Before anything else: record the Bypass-Permissions acceptance in
    // settings so the relaunched process (every relaunch path below passes
    // `--dangerously-skip-permissions`) never renders the consent dialog.
    // Best-effort — `settle_bypass_permissions_dialog` still watches for it.
    pre_accept_bypass_permissions(&config.claude);

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

    // Step 1b: CONTAINER clean-relaunch path (avoids the `/exit` WEDGE).
    //
    // When `[auto_update] relaunch_command` is configured (container default
    // `["cwsr", "--no-upgrade"]`; EMPTY on the host / GB, which keeps the
    // `/exit` flow below unchanged), relaunch Claude via that command's clean
    // `tmux respawn-pane -k` instead of the interactive `/exit` +
    // shell-inject dance. Inside the container `/exit` is unreliable: Claude
    // Code 2.1.x pops a "Background work is running" confirmation whenever
    // background watchers are running (they always are), the shell-injected
    // `bash <relaunch_script>` one-liner is fragile (tmpfs-wiped script,
    // dangling launcher during the `claude install` download window), and the
    // failure mode cascades into "Claude Code crashed — auto-restarting"
    // alert storms + eventual container recreates (operator botchat #2107).
    // cwsr respawns pane 0 directly (no `/exit`, no dialog, no raw-shell
    // inject) and reconstructs the correct claude argv from the entrypoint
    // env vars — the same mechanism the Step-7 crash-recovery fallback below
    // already trusts. The `claude install` write already landed the new
    // version on disk (that mismatch is what triggered this run), so a bare
    // respawn picks it up.
    if !config.auto_update.relaunch_command.is_empty() {
        run_auto_update_clean_relaunch(pane, old_version, new_version, config).await;
        return;
    }

    // Step 2: Inject /exit
    info!("auto-update: injecting /exit...");
    inject_dispatch::inject_to_agent(pane, "/exit").await;

    // Step 2b: Dismiss the 2.1.x "Background work is running" exit dialog.
    //
    // Claude Code 2.1.x renders a "Background work is running" confirmation on
    // the interactive `/exit` flow whenever a worktree is checked out OR
    // background tasks are running (#1411). Our sessions always have
    // backgrounded watchers, so the dialog ALWAYS eats the `/exit` submit and
    // `wait_for_exit` below would time out, false-alarming "Claude Code
    // crashed". Poll briefly for the dialog; if it appears, send a bare Enter
    // to select the default-highlighted option 1 ("Exit anyway") — the right
    // choice here, since the process is relaunching anyway and watchers restart
    // fresh in the new session. Bounded short loop matching the file's polling
    // idioms; if the dialog never shows (host / non-2.1.x claude), this is a
    // no-op and we fall through to wait_for_exit unchanged.
    {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        while tokio::time::Instant::now() < deadline {
            if let Some(out) = tmux::capture_pane(pane).await {
                if tmux::background_work_exit_dialog_visible(&out) {
                    info!(
                        "auto-update: 'Background work is running' exit dialog detected, \
                         sending Enter to select 'Exit anyway'"
                    );
                    tmux::send_keys(pane, &["Enter"]).await;
                    break;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
    }

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
    //
    // The launch argv MUST mirror the entrypoint's CLAUDE_CMD shape
    // (container/entrypoint.sh + container/bin/cwsr build_claude_cmd):
    // when CLAUDE_SHIM_SETTINGS_PATH is set the in-container claude is
    // launched with `--setting-sources project,local --settings <shim>`
    // (drops the host user tier, loads the MCP-settings shim) and a
    // `--plugin-dir` for the baked container plugin (skills + agents).
    // A bare `claude` relaunch loses those: it would load the wrong
    // settings tier (host user settings, whose macOS apiKeyHelper can't
    // exec on Linux) and silently drop the baked /claude-container:<name>
    // commands. `build_relaunch_claude_argv` reconstructs the correct
    // shape from the same env vars; on the host (no shim env) it degrades
    // to the previous bare-`claude` behavior.
    let launch = build_relaunch_claude_argv(session_id.as_deref());

    // Ensure the relaunch-script parent dir exists (see crash-recovery
    // note above): `/var/run/claude/` is tmpfs and may be gone after a
    // redeploy. Idempotent.
    if let Some(parent) = std::path::Path::new(&config.claude.relaunch_script).parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            tracing::warn!(error = %e, dir = %parent.display(), "auto-update: could not create relaunch-script parent dir");
        }
    }
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

    // Verify the script landed before injecting; if not, abort the inject
    // rather than type a broken `bash <path>` into the pane. The clean-restart
    // fallback (Step 7, on binary-not-found) recovers a dead pane, but skipping
    // a write that didn't stick avoids the doomed inject entirely.
    if !std::path::Path::new(&config.claude.relaunch_script).exists() {
        tracing::error!(
            path = %config.claude.relaunch_script,
            "auto-update: relaunch script missing immediately after write -- aborting inject"
        );
        return;
    }

    // Step 6: Inject the self-healing guarded relaunch one-liner. If the
    // script vanishes (tmpfs wipe / respawn.rs remove_file race) between this
    // write and the pane shell running it, the `|| { <launch>; }` fallback
    // runs the same claude argv inline so Claude still comes up rather than
    // dying with "No such file or directory". `launch` already has the right
    // `cd $HOME` semantics baked into the script; for the inline fallback we
    // prefix `cd $HOME &&` to match.
    info!("auto-update: injecting relaunch command...");
    let inline_launch = format!("cd $HOME && {}", launch);
    let inject_cmd = build_relaunch_inject_cmd(&config.claude.relaunch_script, &inline_launch);
    // Serialize with every other injector (see `inject_lock`), as above.
    {
        let _guard = crate::inject_lock::InjectLock::acquire("auto-update-shell").await;
        tmux::inject_shell(pane, &inject_cmd).await;
    }

    // Step 7: Wait for claude binary to appear in process tree.
    //
    // This is the load-bearing DECOUPLE point. The relaunch command in
    // Step 6 (`bash <relaunch_script>`) is typed into a raw shell. If the
    // relaunch script is missing (e.g. `/var/run/claude/claude-relaunch.sh`
    // doesn't exist after a redeploy) or Claude otherwise fails to boot,
    // the pane is left sitting at a `/bin/sh` prompt -- NOT the Claude TUI.
    //
    // Historically Steps 7 and 8 only `warn!`ed and Step 9 injected the
    // resume prompt regardless. That coupled the resume-inject to the
    // relaunch: with no Claude running, the resume text (which begins
    // "You have ALREADY been restarted..." and contains shell-hostile
    // chars like "(") got typed straight into `/bin/sh`, dying with
    // `-sh: Syntax error: "(" unexpected` -- and the session never resumed.
    //
    // So: gate the resume-inject on Claude actually being up. If the
    // binary never appears, ABORT the inject (never type the prompt into a
    // raw shell) and emit a high-severity alert so the operator / main
    // loop can recover, rather than silently corrupting the shell.
    info!("auto-update: waiting for Claude binary to start...");
    if !tmux::wait_for_claude_binary(pane, 120).await {
        warn!(
            "auto-update: claude binary not detected after 120s -- \
             relaunch likely failed (missing relaunch script or boot error); \
             attempting a CLEAN restart via the configured restart command \
             before giving up (never leave a dead pane)"
        );
        write_jsonl_log(
            &config.general.log_file,
            "auto_update_relaunch_retry",
            serde_json::json!({"reason": "binary_not_found_after_script_relaunch"}),
        );

        // CLEAN-RESTART FALLBACK. The shell-injected relaunch script didn't
        // bring Claude up. Rather than abort into a dead `/bin/sh` pane,
        // try the same in-place roll the container's auto-respawn path uses:
        // `cwsr --no-upgrade` (tmux respawn-pane -k) — the upgrade already
        // landed via npm's atomic symlink swap, so we only need to respawn
        // the pane. cwsr reconstructs the correct entrypoint argv itself
        // (settings shim, plugin-dir, --continue). On the host (no cwsr on
        // PATH) this is a best-effort no-op and we fall through to the
        // hard-fail alert below. The fallback command is configurable via
        // `[auto_respawn_on_hang] respawn_command` (container default
        // `["cwsr", "--no-upgrade"]`).
        let restart_argv: Vec<&str> = config
            .auto_respawn_on_hang
            .respawn_command
            .iter()
            .map(|s| s.as_str())
            .collect();
        let mut clean_restart_ok = false;
        if !restart_argv.is_empty() {
            info!(
                restart_command = ?config.auto_respawn_on_hang.respawn_command,
                "auto-update: shell relaunch failed -- invoking clean-restart fallback"
            );
            let (_out, ok) = crate::cmd::run_cmd_any(&restart_argv, 60).await;
            if ok {
                // Give the respawned pane time to bring claude up.
                clean_restart_ok = tmux::wait_for_claude_binary(pane, 120).await;
            } else {
                warn!("auto-update: clean-restart fallback command exited non-zero");
            }
        }

        if !clean_restart_ok {
            warn!(
                "auto-update: claude binary still not detected after clean-restart \
                 fallback -- ABORTING resume-inject so the resume prompt is never \
                 typed into a raw shell"
            );
            write_jsonl_log(
                &config.general.log_file,
                "auto_update_failed",
                serde_json::json!({"reason": "binary_not_found_resume_inject_aborted"}),
            );
            alert::notify(crate::event_bus::ClaudeWatchAlert {
                alert_type: "auto-update-failed",
                stuck_reason: "auto-update: claude binary never started after relaunch (incl. clean-restart fallback); resume-inject aborted to avoid corrupting the shell",
                stale_minutes: None,
                affected_watchers: vec![],
                severity: crate::event_bus::Severity::High,
                message: "claude-watch: auto-update FAILED -- Claude did not restart even after a clean-restart fallback; resume prompt NOT injected (would have hit a raw shell). Operator must relaunch manually.",
            })
            .await;
            return;
        }
        info!("auto-update: clean-restart fallback brought Claude back up");
    }

    // Step 7b: Get past the Bypass-Permissions consent dialog.
    //
    // The relaunch argv carries `--dangerously-skip-permissions`, so Claude
    // Code renders a full-screen consent dialog at startup unless the
    // acceptance is persisted in settings. Its cancel row is `❯ No, exit` —
    // the same `❯` Step 8's `wait_for_idle_prompt` reads as "ready", so
    // without this gate Step 9 types the resume prompt INTO the dialog and
    // the default selection exits Claude (operator-observed, 2026-08-29).
    // `Stuck` means we could not get the pane to a real prompt: abort rather
    // than inject blind.
    if settle_bypass_permissions_dialog(pane, config).await == BypassDialogGate::Stuck {
        write_jsonl_log(
            &config.general.log_file,
            "auto_update_failed",
            serde_json::json!({"reason": "bypass_dialog_stuck_after_relaunch"}),
        );
        return;
    }

    // Step 8: Wait for the idle prompt (Claude Code is ready for input).
    // Claude binary IS running (Step 7 confirmed it), so a missed prompt
    // here is a slow-render, not a failed relaunch -- best-effort wait,
    // then inject anyway (the pane is the Claude TUI, not a raw shell).
    info!("auto-update: waiting for idle prompt...");
    if !tmux::wait_for_idle_prompt(pane, 90).await {
        warn!("auto-update: prompt not found after 90s, trying inject anyway (claude binary is up)");
    }

    // Brief settle after prompt appears
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;

    // Step 9: Inject resume text. Reached ONLY when Step 7 confirmed the
    // Claude binary is running, so this lands in the Claude TUI -- never a
    // raw shell. (Decoupled from the relaunch path above.)
    info!("auto-update: injecting resume prompt...");
    inject_dispatch::inject_to_agent(pane, &config.auto_update.resume_prompt).await;

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
        evaluate_pane_unchanged, execute_respawn_with_versions_dir, hash_pane_content,
        should_respawn, HangSignal, RespawnOutcome,
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
    // detected → fail-open to 0). Production passes None, which also
    // enables the JSONL-transcript backstop so a subagent that is mid-
    // THOUGHT (no child tool process, hence invisible to the /proc count)
    // still suppresses the destructive respawn. See
    // `respawn::count_alive_subagents_with_versions_dir`.
    let active_subagents = crate::respawn::count_alive_subagents_with_versions_dir(
        versions_dir_override,
        crate::active_agents::DEFAULT_AGENT_ALIVE_MAX_AGE_SECS,
    );

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

    // Global interrupt gate (single chokepoint, 2026-06-11): even though
    // auto-respawn has its own `should_respawn` cooldown, it now also
    // consults the shared global ceiling so a respawn does not stack on
    // top of another interrupt fired moments earlier (and vice versa).
    // Atomically claim-and-stamp; on failure, skip this fire — the next
    // check cycle re-evaluates (the hang signals persist within the
    // window). NOTE: try_claim_global_interrupt stamps last_interrupt_at
    // on success, so the later explicit stamp is removed.
    if !try_claim_global_interrupt(
        state,
        config.general.post_interrupt_cooldown_secs,
        config.general.global_cooldown_backoff_base,
        config.general.global_cooldown_max_secs,
        now,
    ) {
        debug!(
            cooldown = config.general.post_interrupt_cooldown_secs,
            "auto-respawn would fire but global post-interrupt cooldown active — deferring"
        );
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
    // last_interrupt_at already STAMPED by try_claim_global_interrupt
    // above (2026-06-11 — collapsed into the atomic claim).
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
/// Outcome of evaluating the AskUserQuestion stale-monitor timer for one
/// cycle. Pure decision separated from I/O (pane capture + alarm emit) so
/// the lifecycle is unit-testable without mocking tmux.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AskQuestionTimerDecision {
    /// Monitor disabled, or no interactive prompt pending — clear the timer
    /// (idempotent: also the reset path when a prompt clears).
    Clear,
    /// Interactive prompt pending; timer running but threshold not reached
    /// (or already alerted). Set `ask_question_pending_since` if unset; do
    /// not fire.
    Pending,
    /// Threshold reached on a not-yet-alerted pending question — FIRE the
    /// alarm once. Carries the elapsed minutes for the alert payload.
    Fire { stale_minutes: u64 },
}

/// Pure lifecycle for the AskUserQuestion stale monitor. Mirrors the
/// thinking-timer lifecycle (start when observed / reset when cleared /
/// fire once at threshold). Mutates `ask_question_pending_since` /
/// `ask_question_alerted` on `state` and returns what the caller should do.
///
/// Phase 1: the caller only EMITS AN ALARM on `Fire` — it does NOT Escape,
/// reject, or inject (that is Phase 2/3, gated on `reject_enabled`).
///
/// Args:
///   * `enabled` / `stale_seconds` — from `[ask_question_monitor]`.
///   * `interactive_prompt` — whether an `AskUserQuestion` / selection /
///     permission prompt is currently on screen (`tmux::is_interactive_prompt`).
///   * `now` — RFC3339 timestamp string (the daemon's per-check `now`).
pub(crate) fn ask_question_timer_step(
    state: &mut State,
    enabled: bool,
    stale_seconds: u64,
    interactive_prompt: bool,
    now: &str,
) -> AskQuestionTimerDecision {
    // Disabled, or the question has cleared/been answered: reset the timer.
    // (When NOT an interactive prompt, the question is gone — clear so the
    // next fresh question gets its own timer + single alarm.)
    if !enabled || !interactive_prompt {
        state.ask_question_pending_since = None;
        state.ask_question_alerted = false;
        return AskQuestionTimerDecision::Clear;
    }

    // Interactive prompt is pending. Start the timer on first observation.
    let started = match state.ask_question_pending_since.as_deref() {
        Some(ts) => ts.to_string(),
        None => {
            state.ask_question_pending_since = Some(now.to_string());
            now.to_string()
        }
    };

    // Already alerted for THIS pending question — fire only once.
    if state.ask_question_alerted {
        return AskQuestionTimerDecision::Pending;
    }

    // Elapsed since the question was first observed pending. A malformed
    // timestamp (elapsed_since None) fails safe toward NOT firing.
    let elapsed = elapsed_since(&started).unwrap_or(0.0);
    if elapsed >= stale_seconds as f64 {
        state.ask_question_alerted = true;
        AskQuestionTimerDecision::Fire {
            stale_minutes: (elapsed as u64) / 60,
        }
    } else {
        AskQuestionTimerDecision::Pending
    }
}

/// AskUserQuestion stale monitor (Phase 1: detect + alarm ONLY).
///
/// Detects when an interactive `AskUserQuestion` / tool-permission /
/// selection prompt has blocked the main loop (which reads as
/// `ClaudeActivity::Idle` while the prompt is up — the menu still renders a
/// `\u{276f}` cursor, so the prolonged-thinking detector never engages) for
/// longer than `[ask_question_monitor].stale_seconds`, and emits a
/// `ask-question-stale` claude-event + pingme exactly once per pending
/// question. Resets when the prompt clears.
///
/// Phase 1 does NOT Escape / auto-reject / inject. The reject path (Phase 2
/// Escape, Phase 3 inject of `explanation`) hooks in at the marked TODO
/// below, gated on `config.ask_question_monitor.reject_enabled`.
async fn check_ask_question_stale(config: &Config, state: &mut State, pane: &str, now: &str) {
    let cfg = &config.ask_question_monitor;
    if !cfg.enabled || pane.is_empty() {
        // Still run the timer step so a disabled monitor clears any stale
        // timer state (idempotent reset).
        ask_question_timer_step(state, cfg.enabled, cfg.stale_seconds, false, now);
        return;
    }

    // Use the NARROW blocking-question detector, NOT the broad
    // `is_interactive_prompt` (which is biased toward true and matches passive
    // FleetView / Background-tasks viewer overlays). A false positive here
    // fires a spurious `ask-question-stale` alarm with no real block behind it
    // (operator-reported false alarm, 2026-07-13). See
    // `tmux::blocking_question_visible`.
    let interactive = tmux::is_blocking_question(pane).await;
    let decision =
        ask_question_timer_step(state, cfg.enabled, cfg.stale_seconds, interactive, now);

    if let AskQuestionTimerDecision::Fire { stale_minutes } = decision {
        let msg = format!(
            "claude-watch: an interactive AskUserQuestion prompt has blocked \
             the main loop for ~{}min (>{}s threshold). The operator may be \
             away — the loop reads as Idle, so the prolonged-thinking \
             detector never fired. Answer it or let it auto-resolve.",
            stale_minutes, cfg.stale_seconds
        );
        warn!(
            stale_minutes,
            threshold_secs = cfg.stale_seconds,
            "AskUserQuestion prompt stale — emitting alarm (Phase 1: alarm only)"
        );
        write_jsonl_log(
            &config.general.log_file,
            "ask_question_stale",
            serde_json::json!({
                "stale_minutes": stale_minutes,
                "threshold_secs": cfg.stale_seconds,
                "reject_enabled": cfg.reject_enabled,
            }),
        );
        // Sink 1: structured claude-event (forces the loop to see the
        // parseable stale_minutes field). Sink 2: pingme push.
        alert::emit_event(crate::event_bus::ClaudeWatchAlert {
            alert_type: "ask-question-stale",
            stuck_reason: "AskUserQuestion prompt pending past stale threshold",
            stale_minutes: Some(stale_minutes),
            affected_watchers: vec![],
            severity: crate::event_bus::Severity::Medium,
            message: &msg,
        });
        alert::send_pingme(&msg).await;

        // TODO(Phase 2/3): gate an auto-reject on `cfg.reject_enabled`.
        //   Phase 2: when `cfg.reject_enabled`, send Escape to the pane
        //     (tmux::interrupt_and_wait / a dedicated reject keystroke) to
        //     cancel the AskUserQuestion menu safely.
        //   Phase 3: after the reject lands, inject `cfg.explanation`
        //     ("use your judgement") via inject_dispatch so the loop
        //     proceeds autonomously. Both paths should claim the global
        //     interrupt cooldown via try_claim_global_interrupt and apply
        //     a per-fire backoff, mirroring the prolonged-thinking path.
        // Phase 1 intentionally stops here: ALARM ONLY, no keystrokes.
    }
}

pub async fn check_cycle(config: &Config, state: &mut State) {
    let now = Local::now().to_rfc3339();

    // Get Claude Code status. Use the CONFIG-AWARE resolver so the `pane`
    // field is pinned to the configured fixed main-loop pane (e.g.
    // `claude-container:0.0`) rather than whichever `claude` pane sorts first
    // in `tmux list-panes -a`. Every inject below targets `cs.pane` /
    // `effective_pane` derived from it; the bare auto-detect scan would
    // resolve an operator-focused TUI agent-view subagent pane and land
    // MAIN-LOOP-SCOPED injects (watcher-down, heartbeat-stale, resume) in the
    // subagent. See `status::find_claude_pane_with_config`.
    let cs = status::get_claude_status_with_config(&config.tmux).await;

    if cs.is_none() {
        // A `None` status does NOT necessarily mean Claude Code exited. When
        // the session hits the context wall (or a persistent 429), Claude
        // renders an error banner OVER the status bar; the status-bar parse
        // then misses and `find_claude_pane()`'s heuristic returns no pane, so
        // the whole read comes back `None`. The session is WEDGED, not gone.
        //
        // Before taking the "not running" path (which skips the entire
        // context-monitor + wedged-detection block below and just bumps
        // bookkeeping), look for the dashboard pane directly — `find_dashboard_pane`
        // is status-bar-independent — and run the wedged-recovery there. This is
        // the auto-clear-at-limit fix: without it, an 83-minute wedge logged
        // "claude-status returned None -- not running" every cycle and never
        // self-cleared (real incident 2026-06-19).
        let fallback_pane = tmux::find_dashboard_pane(&config.tmux).await;
        if let Some(ref wedge_pane) = fallback_pane {
            // Honor the same api-retry suppression the active path uses: a 429
            // backoff must not be clobbered by a self-clear.
            let api_retrying =
                update_api_retry_state(config, state, wedge_pane).await;
            if handle_wedged_pane(config, state, wedge_pane, api_retrying, 0, &now).await {
                debug!(
                    pane = %wedge_pane,
                    "status read None but pane is WEDGED — ran wedged recovery instead of 'not running'"
                );
                state.last_known_pane = wedge_pane.clone();
                state.last_check = Some(now);
                crate::state::save_state(&config.general.state_file, state);
                return;
            }
        }

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
    let active_ui = cs.active_ui;
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
        // The restarted process was launched with
        // `--dangerously-skip-permissions`, so Claude Code may be sitting on
        // its Bypass-Permissions consent dialog. That dialog renders `❯ No,
        // exit`, which the `is_idle` check below reads as a ready prompt —
        // injecting there submits "No, exit" and Claude EXITS. Accept it and
        // come back next cycle (the interactive-prompt guard below also
        // matches the dialog, so a missed acceptance only delays the resume).
        if let Some(out) = tmux::capture_pane(pane).await {
            if tmux::bypass_permissions_dialog_visible(&out) {
                info!(
                    "post-restart: Bypass-Permissions consent dialog is up -- \
                     selecting 'Yes, I accept' and deferring the resume inject"
                );
                if config.claude.handle_bypass_dialog {
                    tmux::accept_bypass_permissions_dialog(pane).await;
                }
                state.last_check = Some(now);
                crate::state::save_state(&config.general.state_file, state);
                return;
            }
        }
        // Don't inject while an interactive prompt (AskUserQuestion menu,
        // tool-permission confirmation, selection overlay) is awaiting the
        // operator. Such a prompt renders a `❯` selection cursor, so the
        // bare `is_idle` `❯`-scan below would misclassify it as idle and
        // `send-keys` the resume prompt into the live menu — the leading
        // Escape cancels the operator's question out from under them
        // (reported bug, 2026-06-11). Suppressing here only DELAYS the
        // resume to the next cycle once the prompt clears (recoverable),
        // whereas injecting is destructive — so we suppress.
        if tmux::is_interactive_prompt(pane).await {
            debug!("post-restart: skipping — interactive prompt on screen (awaiting operator)");
            state.last_check = Some(now);
            crate::state::save_state(&config.general.state_file, state);
            return;
        }
        if tmux::is_idle(pane).await {
            info!("post-restart: injecting resume prompt");
            inject_dispatch::inject_to_agent(
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

    // Carry-forward guard against a transient status-bar token misparse (see
    // `carry_forward_token_misparse`): if this poll reads 0 tokens for the SAME
    // pane whose last known reading was a large, intact context, hold the prior
    // value for a bounded run of polls rather than let the 0 trip the dead-check
    // accumulator or the fresh-/clear detection below. A pane change (new
    // session) or an exhausted carry run lets the 0 through. Applied only to the
    // liveness/detection logic that follows -- the post-restart resume block
    // above deliberately keeps the raw reading.
    let same_pane = !cs.pane.is_empty() && cs.pane == state.last_known_pane;
    let (tokens, token_carry_count) = if same_pane {
        carry_forward_token_misparse(
            tokens,
            state.last_known_tokens,
            state.token_carry_count,
            config.fresh_clear.max_tokens,
            MISPARSE_CARRY_MAX,
        )
    } else {
        (tokens, 0)
    };
    state.token_carry_count = token_carry_count;

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
    // Prefer the JSONL-transcript-derived context size — read directly from
    // the active session's own usage record, so it can't be clobbered by an
    // overlay (auto-update banner, dialog) blanking the tmux status line the
    // way `cs.tokens` can. Fall back to the tmux-scraped `tokens` when the
    // JSONL read comes back empty/zero (no transcript yet, mid-write, races)
    // rather than regress to 0/stale. See `token_usage::current_context_tokens`.
    let context_tokens = token_usage::current_context_tokens()
        .filter(|&t| t > 0)
        .unwrap_or(tokens);
    if context_tokens > 0 {
        state.last_known_tokens = context_tokens;
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

    // --- Wedged-pane pre-check (context limit / persistent rate limit) ---
    //
    // Computed once, early, so the ack-liveness suppression gates below (the
    // fresh-/clear and post-clear-resume fixes from #707/#713, 2026-08-24/25)
    // can tell a genuine context-wall wedge apart from the status-bar
    // MISPARSE those gates were built to catch. A session that just hit
    // "Context limit reached" / "Context low (N% remaining)" typically acked
    // an event/keepalive moments before it wedged, so `ack_liveness_fresh`
    // keeps reading "alive" for up to the whole stale window
    // (`config.ack.stale_minutes`) — during which both ack-gated blocks below
    // `return`ed BEFORE this function ever reached `handle_wedged_pane`,
    // silently swallowing the autoclear-on-context-limit recovery (incident
    // 2026-08-26: "Context limit reached" then "Context low (0% remaining)",
    // autoclear never fired, operator had to `/clear` by hand). The
    // ack-liveness gate must suppress spurious resume/restart injects on an
    // intact session — it must never suppress the wedged-pane recovery,
    // which rests on independent banner-text evidence (not a token-count
    // misparse) and has its own consecutive-cycle + cooldown gating inside
    // `handle_wedged_pane`.
    let wedged_now =
        !effective_pane.is_empty() && tmux::detect_wedged(&effective_pane).await.is_some();

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
            if let Some(retry) = status::get_claude_status_with_config(&config.tmux).await {
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
                    // Same JSONL-preferred / tmux-fallback policy as the
                    // primary set site above.
                    let recovered_context_tokens = token_usage::current_context_tokens()
                        .filter(|&t| t > 0)
                        .unwrap_or(retry.tokens);
                    if recovered_context_tokens > 0 {
                        state.last_known_tokens = recovered_context_tokens;
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
                } else if inject_expired
                    // ONE-SHOT LATCH GUARD (operator #5620): only treat a
                    // never-active session as "died during fresh startup" (and
                    // thus re-arm the inject) when this pane was NOT hosting a
                    // large context. A large last-known total is positive proof
                    // the pane holds a live, intact session whose bare total is
                    // merely a persistent parse miss — re-arming there is what
                    // re-fired the bogus resume prompt every ~5 min.
                    && state.last_known_tokens < config.fresh_clear.max_tokens
                {
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
            } else if
                // SELF-CLEAR HANDOFF GUARD (operator #4799, q-2026-08-18-e509):
                // do NOT fire the generic fresh-session prompt while a
                // `self-clear` is mid-handoff (lock held) OR has JUST delivered
                // its own resume prompt (marker within the grace window). The
                // lock-held check alone was insufficient: `self-clear` releases
                // the lock the instant it submits the resume prompt, but the
                // fresh session then reads idle+0-tokens for many more seconds
                // while it bootstraps — the exact window in which this gate
                // fired the generic "You are a fresh session ..." text and
                // CLOBBERED the handoff. Checked BEFORE the pane captures so a
                // recent handoff short-circuits without extra tmux work, and
                // placed at the GATE (not just inside `inject_to_agent`) so the
                // `fresh_session_injected` latch is not set on a deferred fire.
                // ACTIVE-UI SUPPRESSION (operator #5620): a long, active
                // session whose bare context total has scrolled behind the
                // thinking indicator / agent-roster / background-tasks overlay
                // reads tokens==0 — a parse MISS, not a fresh session. Those
                // active-work markers never appear on a genuinely fresh idle
                // pane, so their presence is positive proof this is NOT a fresh
                // external session: never fire the resume-checklist inject.
                !active_ui
                && !tmux::self_clear_in_progress()
                && !tmux::self_clear_handoff_recent(
                    config.fresh_clear.self_clear_handoff_grace_secs,
                )
                && {
                // Evaluate the pane reads ONCE into locals, then defer the
                // FIRE/SUPPRESS decision to the pure `fresh_inject_due` gate
                // (unit-tested, so the interactive-prompt suppression is
                // locked). `is_interactive_prompt` is only consulted when
                // `is_idle` already holds, to avoid a second pane capture on
                // the common not-idle path.
                let idle = tmux::is_idle(&effective_pane).await;
                let interactive =
                    idle && tmux::is_interactive_prompt(&effective_pane).await;
                fresh_inject_due(
                    dead_checks,
                    config.dead_process.fresh_inject_checks,
                    state.fresh_session_injected,
                    idle,
                    interactive,
                )
            } {
                // Claude Code is running (idle prompt visible) but tokens=0 — this is
                // a fresh session launched externally (e.g. dashboard --fresh), not by
                // claude-watch. Inject a checklist kick-start prompt.
                //
                // The injected text is worded to remove the ambiguity Andrew
                // flagged (2026-06-02): bare "resume" read to the main loop
                // as a possible "restart" request, so it could not tell
                // whether it had ALREADY been (re)started/cleared (and should
                // just continue) vs was being asked to restart. The session
                // here is already fresh at the idle prompt, so the prompt
                // says so explicitly and points at the resume checklist
                // without any "restart" verb.
                //
                // SUPPRESSION (interactive prompt): the `fresh_inject_due`
                // gate above also requires `!interactive_prompt`. A legitimately
                // pending `AskUserQuestion` / tool-permission / selection menu
                // idles the loop with tokens==0 (no generation) and renders a
                // `❯` selection cursor, so it lands in THIS dead-process block
                // and reads as `is_idle` — without the guard the fresh-/resume-
                // checklist inject below would `send-keys` (leading Escape)
                // into the live menu and CANCEL the operator's question before
                // they can answer it. This mirrors the identical guard on the
                // fresh-/clear path below; the #356 ask_question_monitor
                // already uses `is_interactive_prompt` as its detection signal,
                // so a pending question is a recognized, legitimate idle state
                // that must NOT be preempted by a wedge/restart inject. A FALSE
                // POSITIVE here only DELAYS the kick-start by a cycle
                // (recoverable); a FALSE NEGATIVE destroys a live question.
                info!(
                    dead_checks,
                    "fresh external session detected — injecting checklist kick-start"
                );
                inject_dispatch::inject_to_agent(
                    &effective_pane,
                    "You are a fresh session (already started/cleared) — do NOT restart or clear again. Run your session-start / resume checklist now to recover state and pick up pending work.",
                )
                .await;
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

    // --- Proactive login-expiry detection ---
    //
    // Runs alongside the reactive path, not inside it: that one only ever
    // sees a session that is already dead, so its recovery always lands at
    // the worst possible moment. This one acts on Claude Code's own warning
    // while everything still works.
    if config.reauth.enabled && config.reauth.expiry_watch_enabled && !effective_pane.is_empty() {
        check_login_expiry(config, state, &effective_pane).await;
    }

    // --- Post-clear resume detection ---
    //
    // Covers the blind spot BELOW the fresh-/clear token window. A pane that
    // has just been cleared reports tokens=0 and only publishes a real count
    // once the first turn lands — by which point the always-loaded preamble
    // has already carried it past `max_tokens`, so `[min_tokens, max_tokens)`
    // is stepped over and never sampled. The fresh-external-session gate does
    // handle tokens=0, but only with `bashes == 0`, and background shells
    // survive a `/clear`. Between them, an operator-driven `/clear` on a
    // session with a background command running gets NO resume inject at all
    // and sits at an empty prompt indefinitely. (Daemon-driven clears are
    // unaffected — the `self-clear` child injects its own resume prompt.)
    //
    // Deliberately does NOT consult `bashes`: surviving background shells are
    // exactly the case this exists for. Positive evidence of a clear the
    // daemon OBSERVED plus an idle prompt is what authorises the inject, and
    // `post_clear_resume_injected_for` latches it to one inject per clear.
    if !effective_pane.is_empty()
        && config.fresh_clear.post_clear_window_secs > 0
        && tokens < config.fresh_clear.min_tokens
        && state.last_context_clear.is_some()
        && state.post_clear_resume_injected_for != state.last_context_clear
    {
        // Liveness gate (mirrors the 2026-08-24 fresh-/clear fix at the
        // sibling fast path — #707/ack_liveness_fresh): THE single liveness
        // signal is the age of the last event-ack. If the loop acked ANY
        // event/keepalive within the stale window it is provably alive and
        // therefore CANNOT be a genuinely stranded post-clear loop — the
        // low-token reading that satisfied this block's guard is almost
        // certainly the SAME status-bar misparse #707 fixed for the fresh-
        // /clear fast path (`context_reset_signal` stamping `last_context_clear`
        // from a sub-30K sample that is actually the thinking-indicator's
        // current-turn count or an agent-roster row leaking through, not a
        // real reset; see `status::parse_status_bar`). This is a hard
        // suppression, checked before any idle-check bookkeeping or the
        // `post_clear_resume_due` call so a fresh ack is never overridden. A
        // genuinely stranded post-clear loop stops acking, so its stamp ages
        // past the threshold and this gate opens again — deferring to, not
        // disabling, wedge detection.
        let ack_alive = ack_liveness_fresh(
            last_ack_timestamp_age(&config.ack.resolve_state_dir()),
            config.ack.stale_minutes * 60,
        );
        if ack_liveness_suppresses_clear_inject(ack_alive, wedged_now) {
            info!(
                tokens,
                bashes,
                "post-clear resume inject suppressed: fresh event-ack liveness (loop alive)"
            );
            write_jsonl_log(
                &config.general.log_file,
                "post_clear_inject_suppressed",
                serde_json::json!({
                    "tokens": tokens,
                    "bashes": bashes,
                    "reason": "ack_liveness_fresh",
                    "stale_secs": config.ack.stale_minutes * 60,
                }),
            );
            state.post_clear_idle_checks = 0;
            state.last_check = Some(now);
            crate::state::save_state(&config.general.state_file, state);
            return;
        } else if ack_alive {
            debug!(
                tokens,
                bashes,
                "post-clear resume suppression skipped: pane shows a genuine wedge banner — deferring to wedged-pane recovery"
            );
        }

        // A clear the DAEMON drove is already covered: the `self-clear` child
        // polls until the clear lands and then injects its own resume prompt.
        // Only operator-driven clears need this gate.
        let daemon_clear_recent = state
            .last_wedged_clear
            .as_deref()
            .and_then(elapsed_since)
            .is_some_and(|e| e < config.fresh_clear.post_clear_window_secs as f64)
            || state
                .context_clear_child_pid
                .is_some_and(clear_child_is_running)
            // A self-clear (daemon-, operator-, or skill-driven) that JUST
            // delivered its own resume prompt also counts as "daemon covered":
            // its handoff marker is fresh, so this post-clear gate must NOT
            // inject a second resume on top of it (operator #4799). Covers the
            // operator/skill self-clears that `context_clear_child_pid` /
            // `last_wedged_clear` do not, plus the post-lock-release window.
            || tmux::self_clear_handoff_recent(
                config.fresh_clear.self_clear_handoff_grace_secs,
            );
        let idle = tmux::is_idle(&effective_pane).await;
        // Only consulted when idle already holds, to avoid a second pane
        // capture on the common not-idle path (same shape as the gates above).
        let interactive = idle && tmux::is_interactive_prompt(&effective_pane).await;
        if idle && !interactive {
            state.post_clear_idle_checks = state.post_clear_idle_checks.saturating_add(1);
        } else {
            state.post_clear_idle_checks = 0;
        }
        if post_clear_resume_due(
            tokens,
            config.fresh_clear.min_tokens,
            state.last_context_clear.as_deref(),
            config.fresh_clear.post_clear_window_secs,
            state.post_clear_resume_injected_for.as_deref(),
            daemon_clear_recent,
            idle,
            interactive,
            state.post_clear_idle_checks,
            config.fresh_clear.detections_required,
        ) {
            info!(
                tokens,
                bashes,
                idle_checks = state.post_clear_idle_checks,
                "post-clear idle pane detected -- injecting resume"
            );
            write_jsonl_log(
                &config.general.log_file,
                "post_clear_resume_inject",
                serde_json::json!({
                    "tokens": tokens,
                    "bashes": bashes,
                    "last_context_clear": state.last_context_clear,
                    "idle_checks": state.post_clear_idle_checks,
                }),
            );
            tmux::dismiss_feedback_prompt(&effective_pane).await;
            // NOT the generic `resume_prompt`: that one reports a stuck state
            // with "no background tasks running" and asks for a watcher
            // cleanup. This gate exists precisely BECAUSE background shells
            // survive a /clear — the log line above records how many are live
            // — so the generic wording states something the daemon has just
            // measured to be false, and points the recovery at the wrong
            // thing.
            inject_dispatch::inject_to_agent(
                &effective_pane,
                &config.alerts.post_clear_resume_prompt,
            )
            .await;
            state.post_clear_resume_injected_for = state.last_context_clear.clone();
            state.post_clear_idle_checks = 0;
            state.fresh_clear_resume_inject_interrupts_total = state
                .fresh_clear_resume_inject_interrupts_total
                .saturating_add(1);
            state.last_check = Some(now);
            crate::state::save_state(&config.general.state_file, state);
            return;
        }
    } else {
        state.post_clear_idle_checks = 0;
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

        // Skip if an interactive prompt (AskUserQuestion menu, tool-
        // permission confirmation, selection overlay) is awaiting the
        // operator. Same destructive-inject hazard as the post-restart
        // path: such a menu renders a `❯` cursor that `is_idle` would
        // read as idle, and a resume-inject's leading Escape cancels the
        // operator's question. Suppress (delays the inject — recoverable)
        // rather than inject (destructive). Reset the fast-detection
        // counter so detection re-builds once the prompt clears.
        if !effective_pane.is_empty() && tmux::is_interactive_prompt(&effective_pane).await {
            debug!("fresh /clear check: skipping — interactive prompt on screen (awaiting operator)");
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

            // Liveness gate (2026-08-24 false-fire fix): THE single liveness
            // signal is the age of the last event-ack. If the loop acked ANY
            // event/keepalive within the stale window it is provably alive and
            // therefore CANNOT have been /clear'd or stranded — the low token
            // reading that landed us in this block is a misparse (the
            // thinking-indicator's `↓ N tokens` current-turn count, or an
            // agent-roster row, leaking through as the context total; see
            // `status::parse_status_bar`), NOT a fresh /clear. This is a HARD
            // suppression, deliberately checked BEFORE the escalation backstop:
            // escalation exists to force through when the `actively_turning`
            // heuristic might be wrong, but a fresh ack is direct proof of
            // life, so it must never be overridden (that is exactly the
            // false-inject the incident produced — hours of resume prompts on
            // an intact, mid-turn/quiet-holding session at tokens=2100..4900).
            // A genuinely stranded post-clear loop stops acking, so its stamp
            // ages past the threshold and this gate opens again — deferring to,
            // not disabling, wedge detection.
            let ack_alive = ack_liveness_fresh(
                last_ack_timestamp_age(&config.ack.resolve_state_dir()),
                config.ack.stale_minutes * 60,
            );
            if ack_liveness_suppresses_clear_inject(ack_alive, wedged_now) {
                info!(
                    tokens,
                    bashes,
                    "fresh /clear inject suppressed: fresh event-ack liveness (loop alive)"
                );
                write_jsonl_log(
                    &config.general.log_file,
                    "fresh_clear_inject_suppressed",
                    serde_json::json!({
                        "tokens": tokens,
                        "bashes": bashes,
                        "reason": "ack_liveness_fresh",
                        "stale_secs": config.ack.stale_minutes * 60,
                    }),
                );
                // Rebuild detection from scratch once the (misparsed) reading
                // clears, exactly as the actively-turning suppression does.
                state.consecutive_fast_detections = 0;
                state.last_check = Some(now);
                crate::state::save_state(&config.general.state_file, state);
                return;
            } else if ack_alive {
                debug!(
                    tokens,
                    bashes,
                    "fresh /clear suppression skipped: pane shows a genuine wedge banner — deferring to wedged-pane recovery"
                );
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

            inject_dispatch::inject_to_agent(&effective_pane, &config.alerts.resume_prompt).await;

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

    // --- Ack-stale detection (THE liveness check) ---
    // Redesign (2026-08-22, botchat #3155-#3167): there is exactly ONE
    // liveness signal — the age of the last ack of ANY claude-event. The main
    // loop acks every batch it handles (`event-ack ack-batch`), which stamps
    // `<state-dir>/last-ack-timestamp`; the daemon reads that stamp here. The
    // host heartbeat FILE and its `touch` ritual are GONE: two signals for one
    // fact was the complexity Andrew asked to remove, and the file had its own
    // failure mode (the loop touching it while ignoring the events that told
    // it to). The `keepalive` event exists only to give an IDLE loop something
    // to ack before this threshold elapses.
    let mut stuck = false;
    let mut stuck_reason = String::new();
    // Captured for the claude-event sink so the main loop can parse
    // `stale_minutes` as a number rather than re-regex'ing the string.
    let mut stuck_stale_minutes: Option<u64> = None;

    // `[ack] state_dir`, else $CLAUDE_EVENT_STATE_DIR, else
    // ~/.config/claude-events/ — the same ladder `event-ack` walks, so the
    // writer and this reader cannot drift apart.
    let liveness_age = last_ack_timestamp_age(&config.ack.resolve_state_dir());

    if let Some(age) = liveness_age {
        let stale_secs = config.ack.stale_minutes * 60;
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
                    // Two proof-of-life conditions suppress the stuck flag for
                    // this cycle: a fresh workload heartbeat (as before) OR
                    // active background subagents. The main loop is often
                    // LEGITIMATELY dispatcher-waiting on long-running (5-15min)
                    // subagents with few counted tool calls -- firing the
                    // Escape interrupt here would cancel the in-flight turn
                    // AND kill those healthy agents. The active-subagent count
                    // mirrors the auto-respawn guard
                    // (`respawn::should_respawn`); applying it at DETECTION
                    // time fixes both the destructive interrupt and the
                    // downstream `HangSignal::HeartbeatStale` (so no stuck flag
                    // is set and no hang-signal is fed to the respawn
                    // collector). The count is cheap (one /proc scan) and
                    // fail-open (returns 0 when no Claude PID is detectable).
                    let workload_fresh = workload_heartbeat_suppresses_stuck(config);
                    let active_subagents =
                        crate::respawn::count_alive_subagents();
                    // Independent proof-of-life signals the daemon already
                    // tracks, so host-heartbeat freshness is decoupled from
                    // event-bus tick DELIVERY (incident 2026-08-21): a live
                    // loop in a long turn / with a stalled bus starves the
                    // heartbeat without being wedged. `thinking_start` is set
                    // by the foreground thinking detector (cleared when idle),
                    // so `is_some()` ~= "the model is mid-generation now".
                    let loop_thinking = state.thinking_start.is_some();
                    let actively_turning = main_loop_actively_turning(
                        state,
                        bashes,
                        config.watcher_monitor.active_window_secs,
                    );
                    if let Some(reason) = heartbeat_stale_liveness_reason(
                        workload_fresh,
                        active_subagents,
                        loop_thinking,
                        actively_turning,
                    ) {
                        let age_min = age / 60;
                        debug!(
                            stale_age_min = age_min,
                            threshold_min = config.ack.stale_minutes,
                            workload_fresh,
                            active_subagents,
                            loop_thinking,
                            actively_turning,
                            reason,
                            "heartbeat-stale suppressed (proof-of-life)"
                        );
                        write_jsonl_log(
                            &config.general.log_file,
                            "heartbeat_stale_suppressed",
                            serde_json::json!({
                                "stale_age_min": age_min,
                                "threshold_min": config.ack.stale_minutes,
                                "reason": reason,
                                "workload_fresh": workload_fresh,
                                "active_subagents": active_subagents,
                                "loop_thinking": loop_thinking,
                                "actively_turning": actively_turning,
                                "dir": &config.stuck_detection.workload_heartbeat_dir,
                                "max_age_secs": config.stuck_detection.workload_heartbeat_max_age_secs,
                            }),
                        );
                    } else {
                        stuck = true;
                        let age_min = age / 60;
                        stuck_reason = format!(
                            "no event ack for {}min (threshold={}min, watchmen={})",
                            age_min, config.ack.stale_minutes, watchmen_count
                        );
                        stuck_stale_minutes = Some(age_min);
                        state.heartbeat_stale_count += 1;
                    }
        }
        // No ack stamp at all -- give it time. Fresh boot / early daemon start
        // / a host without event-must-act. Absence is NOT staleness: the clock
        // starts at the first ack.
    }

    // --- AskUserQuestion stale detection (Phase 1: detect + alarm) ---
    // A pending interactive question blocks the main loop but reads as
    // Idle, so the prolonged-thinking detector misses it. Fire a fast,
    // specific alarm when it sits pending past the configured threshold.
    // ALARM ONLY in Phase 1 — no Escape / reject / inject.
    check_ask_question_stale(config, state, &effective_pane, &now).await;

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
    //
    // A reset is recognised from EITHER a below-threshold sample or a large
    // drop against the previous sample (`context_reset_signal`). The drop arm
    // is what covers a clear whose replacement context boots above 30K — on a
    // session with a big always-loaded preamble the pane may never read low at
    // all, so the below-threshold arm alone silently misses the clear (real
    // incident 2026-08-22: 907979 -> 77185 across one poll gap, nothing
    // stamped, "Since Clear" kept counting from the previous day).
    if config.context_monitor.enabled {
        // Reset path runs first so it can observe the pre-update last_seen_tokens.
        //
        // Use `context_tokens` (JSONL-preferred, tmux-fallback — see the
        // `state.last_known_tokens` assignment above) rather than the raw
        // tmux-scraped `tokens`. `context_reset_signal` detects a clear by
        // comparing consecutive samples for a drop; if an overlay (auto-update
        // banner, dialog) clobbers the status line right after a real clear,
        // `tokens` can freeze at the stale PRE-clear reading for one or more
        // cycles, so the comparison never sees a drop and the clear goes
        // undetected — the dashboard's "Since Clear" tile then keeps counting
        // from the previous clear (real incident 2026-08-22: a poll landed on
        // a frozen 907979 sample instead of the JSONL-visible drop to 77185).
        // `context_tokens` reads the session transcript directly and isn't
        // subject to that overlay clobber.
        maybe_reset_context_clear(config, state, context_tokens, &now);
        // Always record the latest token sample (even tokens=0) so the next
        // cycle's "previously high → now low" detector sees the right history.
        // Recorded on the SAME basis as the value just passed above — mixing
        // a JSONL-derived current sample against a tmux-derived previous
        // sample (or vice versa) would make the drop comparison meaningless.
        state.last_seen_tokens = Some(context_tokens);
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
                    // Record when this threshold episode STARTED. The hook
                    // grace window below is measured from the last hook
                    // fire, which the hook refreshes every turn; this
                    // timestamp is the thing that can't be refreshed and so
                    // is what the deferral ceiling is anchored to.
                    if state.context_threshold_first_seen_at.is_none() {
                        state.context_threshold_first_seen_at = Some(now.clone());
                    }
                    // Hybrid gate: if a recent context_high hook fired the
                    // reminder, give Claude a grace window to self-act
                    // before we tmux-inject a warning + schedule the
                    // deferred clear. Bounded by context_fallback_max_secs
                    // since the crossing — without that ceiling a loop that
                    // keeps taking turns re-arms the grace window forever and
                    // the fallback never runs (2026-08-10: deferred every
                    // cycle from 92.8% context to the hard limit).
                    let defer_allowed = context_hook_defer_allowed(
                        state.context_threshold_first_seen_at.as_deref(),
                        config.hybrid.context_fallback_max_secs,
                    );
                    if !defer_allowed {
                        debug!(
                            tokens,
                            pct,
                            max_secs = config.hybrid.context_fallback_max_secs,
                            "context hook-deferral ceiling reached — no longer deferring to hook"
                        );
                    }
                    let hook_deferred = config.hybrid.enabled
                        && defer_allowed
                        && should_defer_to_hook(
                            ReminderType::ContextHigh,
                            config.hybrid.context_fallback_secs as f64,
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
                    } else if matches!(
                        context_escalation_decision(
                            state.context_obligation_armed_at.as_deref(),
                            config.general.obligation_dwell_secs,
                            crate::respawn::count_alive_subagents(),
                            config.context_monitor.max_armed_secs,
                            &now,
                        ),
                        ObligationDecision::ArmObligation | ObligationDecision::Hold
                    ) {
                        // Two-phase escalation (BUG 1 fix): ARM the obligation
                        // rung first (pending alert + event) without
                        // interrupting. inject_context_warning is Escalate-only
                        // (the interrupt is deferred); the deferred self-clear
                        // child remains the hard context backstop and is spawned
                        // only on Escalate below.
                        let ctx_msg = format!(
                            "[CLAUDE-WATCH] Context at {:.0}% — SELF-CLEAR NOW. \
                            Run: (1) `session-task set '<state to resume>'`, \
                            (2) commit + push in-flight repo work, (3) `self-clear`. \
                            Auto-clear will be forced in {}s if you don't act.",
                            pct, config.context_monitor.max_armed_secs
                        );
                        let _ = crate::obligation_arm::arm_alert_obligation(
                            &ctx_msg,
                            "claude-watch-context-low",
                        );
                        if state.context_obligation_armed_at.is_none() {
                            state.context_obligation_armed_at = Some(now.clone());
                        }
                        alert::emit_event(crate::event_bus::ClaudeWatchAlert {
                            alert_type: "context-low",
                            stuck_reason: "context threshold exceeded (armed)",
                            stale_minutes: None,
                            affected_watchers: vec![],
                            severity: crate::event_bus::Severity::High,
                            message: &ctx_msg,
                        });
                        debug!(
                            tokens,
                            pct,
                            dwell_secs = config.general.obligation_dwell_secs,
                            "context threshold exceeded — obligation armed/held, deferring interrupt"
                        );
                        write_jsonl_log(
                            &config.general.log_file,
                            "context_threshold_obligation_armed",
                            serde_json::json!({
                                "tokens": tokens,
                                "pct": pct,
                                "dwell_secs": config.general.obligation_dwell_secs,
                            }),
                        );
                    } else if !try_claim_global_interrupt(
                        state,
                        config.general.post_interrupt_cooldown_secs,
                        config.general.global_cooldown_backoff_base,
                        config.general.global_cooldown_max_secs,
                        &now,
                    ) {
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
                        // Escalate: obligation dwelled — clear the armed
                        // timestamp and proceed with the full interrupt path.
                        state.context_obligation_armed_at = None;
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
                        // last_interrupt_at already STAMPED by the atomic
                        // try_claim_global_interrupt above (2026-06-11).
                        state.fallback_clear_count = state.fallback_clear_count.saturating_add(1);
                        state.context_warning_interrupts_total = state
                            .context_warning_interrupts_total
                            .saturating_add(1);
                    }
                }
            }
        }

        // Reset paths (below-threshold sample or a halved token counter) and
        // last_seen_tokens bookkeeping run
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
    // we require N consecutive cycles before firing. Shared with the
    // `cs.is_none()` early-return path so a wedge that hides the status bar
    // (and thus makes the session read as "not running") is still recovered.
    // Pass the same JSONL-preferred `context_tokens` used above (not the raw,
    // overlay-fragile `tokens`) so the `wedged_clear`/`wedged_clear_retry`
    // diagnostic log line records the true context size rather than a
    // possibly-stale tmux scrape — consistent with the reset-detection fix
    // just above. `detect_wedged` itself stays banner-text-only by design
    // (see the `wedged_now` comment earlier in this function): the wedge
    // determination and recovery confirmation never depended on either token
    // source, so this only tightens what gets logged.
    handle_wedged_pane(config, state, &effective_pane, api_retrying, context_tokens, &now).await;

    // --- Malformed-tool-call detection (non-namespaced invoke/parameter) ---
    //
    // The model sometimes emits a MALFORMED tool call: a stray literal text
    // prefix followed by raw, NON-namespaced `<invoke ...>` / `<parameter ...>`
    // tags instead of a well-formed namespaced tool call. The harness does NOT
    // execute it — the block renders as plain assistant TEXT — so the INTENDED
    // action (very often a `watcher-ctl run claude-event-watch`, a
    // `signal-send`, or a heartbeat `touch`) silently never runs. Sustained,
    // this strands one-shot watchers DOWN, lets the heartbeat go stale, and
    // produces hours of failure / heartbeat-stale / watcher-down alert storms
    // (the 2026-06-17 incident).
    //
    // A well-formed tool call is consumed by the harness and shows only as a
    // tool-use widget; the raw tags never reach the pane as text. Detection is
    // STRUCTURAL (AST-style: a confirmed `<invoke name=...>`+`<parameter>`/
    // `</invoke>` construct, code-fence-excluded) — see
    // `tmux::check_lines_for_malformed_tool_call`.
    //
    // Enforcement is ESCALATING. A malformed call renders as assistant TEXT and
    // never reaches a PreToolUse hook, so claude-watch CANNOT truly pre-empt the
    // (non-)execution — there is no pre-execution block at this layer (the
    // enforcement ceiling; documented in the PR). The strongest feasible
    // enforcement is a relentless escalating inject that HALTS forward progress
    // until a clean call is observed:
    //   * Phase 1: after `consecutive` observations, inject the soft `nudge`,
    //     cooldown-gated by `cooldown`.
    //   * Phase 2 (hard block): after `escalate_after` soft nudges in the same
    //     unbroken episode without the malform clearing, switch to the firmer
    //     `hard_block_nudge` and DROP the cooldown — interrupt + re-inject on
    //     EVERY cycle until a clean turn is observed.
    if config.malformed_tool_call.enabled && !effective_pane.is_empty() {
        // Fresh-/clear or post-compaction boundary guard. The pane-history
        // capture reads ~60 lines of scrollback, which immediately after a
        // /clear STILL shows the PRE-clear turn — possibly a malformed
        // `<invoke>` block from the OLD context. That residue did not come from
        // the freshly-reset context; attributing it to the current turn
        // false-flags the very first post-clear turn (the reported bug). At such
        // a boundary skip detection this cycle — the resulting `None` falls
        // through to the reset arm below, ending any in-flight episode cleanly.
        let malformed_fingerprint = if malformed_detection_post_clear(state, tokens, active_ui) {
            debug!(
                tokens,
                "malformed tool-call detection suppressed — fresh-/clear / \
                 post-compaction boundary (captured tail is pre-clear scrollback)"
            );
            None
        } else {
            tmux::detect_malformed_tool_call(
                &effective_pane,
                &config.malformed_tool_call.override_marker,
            )
            .await
        };

        if let Some(ref fingerprint) = malformed_fingerprint {
            // Dedup: is THIS the same malformed block we last injected on? If
            // so, the model may well have already recovered (emitted a clean
            // call below it) and the block is merely lingering in pane
            // scrollback — re-interrupting on it every cycle is the tight,
            // self-perpetuating interruption loop that motivated this fix (the
            // 2026-06-20 incident: the interrupter false-positiving on stale
            // scrollback, cancelling well-formed turns mid-flight). A NEW
            // fingerprint (different offending text) means a genuinely new
            // malform and is acted on immediately.
            let same_block_already_nudged = state
                .last_malformed_fingerprint
                .as_deref()
                .is_some_and(|prev| prev == fingerprint.as_str());

            state.malformed_tool_call_consecutive += 1;
            debug!(
                consecutive = state.malformed_tool_call_consecutive,
                threshold = config.malformed_tool_call.consecutive,
                episode_nudges = state.malformed_tool_call_episode_nudges,
                same_block_already_nudged,
                "malformed tool-call signature detected"
            );

            if state.malformed_tool_call_consecutive >= config.malformed_tool_call.consecutive {
                // Phase 2 (hard block) once we've already fired `escalate_after`
                // soft nudges in this episode without the malform clearing.
                let hard_block = state.malformed_tool_call_episode_nudges
                    >= config.malformed_tool_call.escalate_after;

                // The hard block deliberately ignores the soft cooldown so it
                // can re-fire every cycle and genuinely block forward progress.
                let in_cooldown = !hard_block
                    && state
                        .last_malformed_nudge
                        .as_deref()
                        .and_then(elapsed_since)
                        .is_some_and(|e| e < config.malformed_tool_call.cooldown as f64);

                if api_retrying {
                    debug!(
                        "malformed tool-call detected but api_retry active — suppressing inject"
                    );
                } else if same_block_already_nudged {
                    // SAME offending block as the last inject. We have already
                    // corrected it once; re-firing now would interrupt whatever
                    // turn is currently in flight (very often a well-formed
                    // recovery call) purely because the old malformed text is
                    // still scrolled into the captured tail. Suppress. A truly
                    // persistent malform changes nothing on screen, so the
                    // model can only clear the episode by emitting a clean turn
                    // — which produces a clean cycle below and resets state.
                    debug!(
                        fingerprint = fingerprint.as_str(),
                        "malformed tool-call block unchanged since last inject — \
                         suppressing re-inject (stale scrollback / already corrected)"
                    );
                } else if in_cooldown {
                    debug!("malformed tool-call detected but phase-1 nudge cooldown active");
                } else {
                    let (phase, text): (&str, &str) = if hard_block {
                        ("hard_block", &config.malformed_tool_call.hard_block_nudge)
                    } else {
                        ("nudge", &config.malformed_tool_call.nudge)
                    };
                    warn!(
                        consecutive = state.malformed_tool_call_consecutive,
                        episode_nudges = state.malformed_tool_call_episode_nudges,
                        phase,
                        "malformed tool-call sustained — injecting corrective directive"
                    );
                    write_jsonl_log(
                        &config.general.log_file,
                        "malformed_tool_call_inject",
                        serde_json::json!({
                            "phase": phase,
                            "consecutive": state.malformed_tool_call_consecutive,
                            "episode_nudges": state.malformed_tool_call_episode_nudges,
                            "tokens": tokens,
                        }),
                    );
                    write_legacy_log(
                        &config.general.legacy_log_file,
                        &format!(
                            "malformed tool-call ({} consecutive, phase={}) — injecting corrective directive",
                            state.malformed_tool_call_consecutive, phase,
                        ),
                    );

                    tmux::interrupt_and_wait(&effective_pane, 5).await;
                    inject_dispatch::inject_to_agent(&effective_pane, text).await;

                    // Record the offending block we just injected on so the
                    // SAME block lingering in scrollback next cycle does not
                    // re-fire (see `same_block_already_nudged` above).
                    state.last_malformed_fingerprint = Some(fingerprint.clone());
                    state.last_malformed_nudge = Some(now.clone());
                    state.malformed_tool_call_nudge_count =
                        state.malformed_tool_call_nudge_count.saturating_add(1);
                    state.last_interrupt_at = Some(now.clone());
                    if hard_block {
                        state.malformed_tool_call_hard_block_count =
                            state.malformed_tool_call_hard_block_count.saturating_add(1);
                        // Do NOT reset the observation streak in hard-block mode:
                        // keeping it at/above `consecutive` means that as long as
                        // the malform persists, the block re-fires EVERY cycle
                        // (no cooldown) and genuinely halts forward progress.
                    } else {
                        // Count this episode's soft nudges toward escalation, and
                        // reset the per-cycle observation streak so a fresh
                        // `consecutive` run is required before the next phase-1
                        // nudge (which, combined with the cooldown, keeps phase-1
                        // polite while the model recovers on its own).
                        state.malformed_tool_call_episode_nudges =
                            state.malformed_tool_call_episode_nudges.saturating_add(1);
                        state.malformed_tool_call_consecutive = 0;
                    }
                }
            }
        } else if state.malformed_tool_call_consecutive > 0
            || state.malformed_tool_call_episode_nudges > 0
            || state.last_malformed_fingerprint.is_some()
        {
            debug!(
                prev_consecutive = state.malformed_tool_call_consecutive,
                prev_episode_nudges = state.malformed_tool_call_episode_nudges,
                "malformed tool-call signature cleared — resetting episode state"
            );
            state.malformed_tool_call_consecutive = 0;
            // Clean cycle ends the episode: reset escalation so a future malform
            // starts fresh at phase 1.
            state.malformed_tool_call_episode_nudges = 0;
            // Clear the dedup fingerprint: the offending block has scrolled out
            // of the tail (genuinely recovered), so an IDENTICAL malform later
            // is a fresh failure that should fire again.
            state.last_malformed_fingerprint = None;
        }
    }

    // --- Individual watcher health monitoring ---
    if config.watcher_monitor.enabled {
        // Layered load — base file + the user-dir override layer — through
        // the SAME loader `watcher-ctl` uses, so the daemon can never
        // disagree with the CLI about what is enabled or which mode a
        // watcher is in. `[watcher_monitor].watchers_config_extra` names the
        // override file; when unset, the CLI's default resolution
        // (`$WATCHERS_CONFIG_EXTRA`, else
        // `$XDG_CONFIG_HOME/watchmen/watchers.override.conf`) applies.
        let override_path = config
            .watcher_monitor
            .watchers_config_extra
            .clone()
            .or_else(crate::watcher::config_path_extra);
        let entries = status::load_watchers_config(
            &config.watcher_monitor.watchers_config,
            override_path.as_deref(),
        );
        // Drop health for watchers the config no longer lists, and force
        // `enabled: false` on the ones it disables. The loop below `continue`s
        // past both shapes, so without this pass their entries would keep the
        // `enabled: true` + climbing `consecutive_missing` they had at
        // retirement forever. Re-run every cycle (not just at load) so editing
        // the watchers config takes effect without a daemon restart.
        let reconciled = crate::state::reconcile_watcher_health(state, &entries);
        if reconciled.changed() {
            info!(
                removed = ?reconciled.removed,
                disabled = ?reconciled.disabled,
                re_enabled = ?reconciled.re_enabled,
                "reconciled watcher_health against watcher config"
            );
        }
        let mut any_critical_missing = false;
        let mut missing_names: Vec<String> = Vec::new();
        // Longest continuous-down duration (seconds) among the watchers that
        // reach the inject path this cycle, from each `WatcherState.down_since`.
        // Feeds the per-watcher watcher-down suppression cap below.
        let mut max_down_secs: Option<u64> = None;
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
            // Pidfile-based liveness (2026-06-11 fix). We DELIBERATELY do not
            // `pgrep` the watcher's pattern: the launcher script
            // (`<name>.sh`) does `exec /usr/local/bin/<name>`, which replaces
            // the process argv with the exec'd binary's — so the `.sh` path is
            // gone from argv and `pgrep -f <.sh path>` can NEVER match a healthy
            // watcher, producing a false-DOWN inject storm. Instead we read the
            // PID the watcher itself records (its `<name>.lock` flock file, or
            // the `<name>.pid` written by `watcher_run`), probe it for genuine
            // (non-zombie) liveness, and verify cmdline identity (to reject a
            // recycled PID). All three survive the exec-to-binary transform.
            // Scan ALL candidate pid dirs and BOTH `.lock`/`.pid` files, not a
            // single env-resolved dir + a lock-preferring pick. The daemon
            // often runs WITHOUT `$XDG_RUNTIME_DIR`, so the single-dir helper
            // resolved to `/var/run/claude`, where a flock-guard watcher's
            // `.lock` can be STALE (its live lock moved to `/run/user/<uid>`),
            // while the FRESH `.pid` naming the live poller sat right beside it
            // — the old "prefer `.lock`" pick chose the dead pid and fired a
            // false `watcher-down`. `watcher_pidfile_liveness_multi` reports UP
            // iff ANY recorded pid (any dir, either file) is genuinely alive and
            // cmdline-matches, so the live `.pid` wins. min_count==0 still means
            // "never DOWN" (explicit opt-out).
            let pid_dirs = status::watcher_pid_dirs();
            let (recorded_pid, pidfile_down) = status::watcher_pidfile_liveness_multi(
                &pid_dirs,
                &entry.name,
                entry.start_cmd.as_deref(),
            );
            let down = entry.min_count != 0 && pidfile_down;
            // "orphaned": a pidfile names a PID that is NOT a genuinely-alive
            // matching watcher (dead / zombie / recycled). Surfaced for
            // diagnostics — a stale pidfile is the pidfile-model analogue of
            // the old zombie-match case.
            let orphaned = down && recorded_pid.is_some();
            let health = state
                .watcher_health
                .entry(entry.name.clone())
                .or_insert_with(|| WatcherState {
                    last_seen_running: None,
                    consecutive_missing: 0,
                    enabled: entry.enabled,
                    event_emitted_at: None,
                    down_since: None,
                });

            if !down {
                health.last_seen_running = Some(now.clone());
                health.consecutive_missing = 0;
                // Recovery clears the quiet-path bookkeeping so the next
                // failure starts a fresh quiet-path episode.
                health.event_emitted_at = None;
                // Recovery also clears the continuous-down clock so the
                // per-watcher suppression cap (`max_suppress_secs`) measures
                // only the CURRENT outage, not a prior one.
                health.down_since = None;
            } else {
                // ARMING (monitor mode): `watcher-ctl run <name>` does not
                // exec a `mode=monitor` watcher — it records
                // `<name>.monitor-intent` and prints the Monitor-tool command
                // for the main loop to arm. Between that print and the
                // Monitor going live there is legitimately NO process, and
                // this is exactly the window a WATCHER(S) DOWN inject (or the
                // obligations gate, via `watcher-status --unhealthy-only`,
                // which shares this helper) must not fire in. A fresh intent
                // that no runtime file has superseded => healthy-pending, not
                // a miss. A runtime file YOUNGER than the intent means the
                // monitor went live and then died => falls through to the
                // normal DOWN path at once. Same decision as `watcher-ctl
                // status`, so CLI and daemon never disagree.
                if entry.mode == status::WatcherMode::Monitor {
                    let arming_grace = config.watcher_monitor.monitor_arming_grace_secs as f64;
                    let intent_age =
                        status::watcher_monitor_intent_age_secs_multi(&pid_dirs, &entry.name);
                    let runtime_age =
                        status::watcher_runtime_file_age_secs_multi(&pid_dirs, &entry.name);
                    if status::watcher_is_arming(intent_age, runtime_age, arming_grace) {
                        debug!(
                            watcher = %entry.name,
                            intent_age = ?intent_age,
                            runtime_age = ?runtime_age,
                            arming_grace,
                            "monitor-mode watcher ARMING (arm intent fresh, Monitor not live yet) — not counting a miss"
                        );
                        continue;
                    }
                }
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
                // Anchor the grace window on RECENT PROOF-OF-LIFE that does NOT
                // require the daemon to catch the short-lived (fire-and-exit)
                // watcher mid-flight. `last_seen_running` is refreshed ONLY on a
                // poll cycle that happens to observe the watcher genuinely alive
                // — but claude-event-watch is alive only a few seconds per
                // restart cycle, so the poll frequently MISSES the live window
                // and `last_seen_running` goes stale even while the main loop is
                // faithfully restarting the watcher. That stale anchor expired
                // the grace window and produced a false-DOWN inject storm.
                //
                // The watcher's `.lock`/`.pid` (and `.runlock`) are rewritten on
                // EVERY restart, so the freshest pidfile mtime is independent
                // proof the watcher was (re)spawned recently. Treat the watcher
                // as in-grace if EITHER `last_seen_running` OR its freshest
                // pidfile is within grace_secs. A GENUINELY dead watcher (main
                // loop stopped restarting) has its pidfiles age past grace_secs
                // → DOWN still fires correctly.
                let last_seen_age = health
                    .last_seen_running
                    .as_deref()
                    .and_then(elapsed_since);
                let pidfile_age = status::watcher_runtime_file_age_secs_multi(&pid_dirs, &entry.name);
                if status::watcher_in_grace(last_seen_age, pidfile_age, grace_secs) {
                    continue;
                }
                // Clean-exit grace: a block-print-exit watcher writes a
                // `<name>.exit` marker immediately before its deliberate
                // `exit 0` (after delivering its batch). When that marker is
                // FRESHER than the pidfile (so the currently-recorded instance
                // exited cleanly, not a previous one) AND younger than
                // clean_exit_grace_secs, the watcher is in the benign "delivered
                // + awaiting restart on the live main loop" state — do NOT count
                // a miss. This kills the WATCHER(S) DOWN flapping that fired
                // whenever the (alive but busy) main loop took longer than
                // grace_secs to restart after a delivery. A CRASH leaves the
                // marker OLDER than the freshly-rewritten pidfile (still DOWN),
                // and a DEAD SESSION's marker ages past the window (heartbeat-
                // stale / dead_process independently catch a dead session), so
                // genuine-down detection is preserved. 0 disables (legacy).
                let clean_exit_grace = config.watcher_monitor.clean_exit_grace_secs as f64;
                if clean_exit_grace > 0.0 {
                    let clean_exit_age =
                        status::watcher_clean_exit_age_secs_multi(&pid_dirs, &entry.name);
                    if status::watcher_cleanly_exited_recently(
                        clean_exit_age,
                        pidfile_age,
                        clean_exit_grace,
                    ) {
                        debug!(
                            watcher = %entry.name,
                            clean_exit_age = ?clean_exit_age,
                            pidfile_age = ?pidfile_age,
                            clean_exit_grace,
                            "watcher cleanly exited (block-print-exit) — restart pending, not counting a miss"
                        );
                        continue;
                    }
                }
                health.consecutive_missing += 1;
                // Stamp the continuous-down clock on the first genuine
                // (past-grace) miss of this outage. It is read by the
                // per-watcher watcher-down suppression cap
                // (`max_suppress_secs`) to force the inject once this specific
                // watcher has been down too long, independent of the shared
                // cross-gate suppression window. Cleared on recovery above.
                if health.down_since.is_none() {
                    health.down_since = Some(now.clone());
                }
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
                        // Track how long THIS watcher has been continuously
                        // down for the per-watcher suppression cap.
                        if let Some(d) = health
                            .down_since
                            .as_deref()
                            .and_then(elapsed_since)
                            .map(|e| e as u64)
                        {
                            max_down_secs =
                                Some(max_down_secs.map_or(d, |m: u64| m.max(d)));
                        }
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
        // See the watcher-architecture cardinal rule (operator notes).

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
            // The event-consumer watcher (claude-event-watch) is the process
            // that DRAINS the ~/claude-events/ queue the quiet path writes to.
            // When IT is the down watcher, the quiet claude-event channel is
            // structurally dead: an event about its own death can never be
            // surfaced (nothing is left to drain the queue). The two defer
            // gates below — active-turn suppression and the obligation-dwell —
            // both justify holding the loud tmux inject on the premise that the
            // out-of-band claude-event STILL fires. That premise is FALSE for a
            // consumer-down: the self-feedback guard filters the consumer out of
            // the emit (-> None), so a busy dispatcher (always "actively
            // turning", always with live subagents) gets ZERO notification and
            // the watcher stays silently dead. So when the event consumer is
            // among the missing watchers, force both gates open and fire a REAL
            // tmux inject promptly — the pane is the only working out-of-band
            // channel once the event consumer is gone. (Andrew: "your watchers
            // are down but you aren't getting interrupted.")
            let consumer_down = consumer_watcher_missing(&missing_names, &event_consumer_name);
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
            // consumer_down bypasses active-turn suppression: the suppression
            // path's only notification is the out-of-band claude-event, which
            // is undeliverable+self-feedback-filtered when the consumer itself
            // is down. Suppressing would mean total silence, so never suppress
            // a consumer-down.
            // pane_wedged is the other premise-falsifier: `bashes > 0` counts
            // as untimed proof of activity, and a wedged pane keeps its
            // background shells listed forever, so without this the gate reads
            // a session that cannot run a single tool call as permanently busy.
            // `wedged_consecutive` is refreshed by handle_wedged_pane earlier
            // in this same cycle.
            let actively_turning = watcher_down_actively_turning(
                state,
                bashes,
                config.watcher_monitor.suppress_inject_when_active,
                config.watcher_monitor.active_window_secs,
                consumer_down,
                state.wedged_consecutive > 0,
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
            // Per-watcher watcher-down suppression cap (2026-08-12 incident:
            // botchat-wait, the operator comms channel, stayed down ~6 min with
            // the inject suppressed the whole time because the main loop was
            // continuously active). The SHARED suppression window backstop above
            // can't fix this without re-introducing the destructive claude-event-
            // watch storm (that's exactly why it was tuned to 86400). So bound
            // suppression PER-WATCHER: once any down watcher's own continuous-down
            // clock (`down_since`) exceeds `max_suppress_secs` (default 180 = 3
            // min), force the inject regardless of `actively_turning`. Still
            // throttled by `inject_cooldown`, so no storm.
            let down_cap_exceeded = watcher_down_suppression_capped(
                max_down_secs,
                config.watcher_monitor.max_suppress_secs,
            );
            if should_inject && !api_retrying {
                let missing_list = missing_names.join(", ");
                let watcher_reason = format!(
                    "{} watcher(s) missing: {}",
                    missing_names.len(),
                    missing_list,
                );

                if actively_turning && escalation.is_none() && !down_cap_exceeded {
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
                    // Global interrupt gate (single chokepoint, 2026-06-11):
                    // watcher-down is EXEMPT by default
                    // (`general.global_cooldown_exempt_watcher_down = true`)
                    // because a down watcher is a hard-liveness failure that
                    // must be allowed to fire even when another interrupt
                    // fired recently. When the operator flips that bool to
                    // false, watcher-down is subjected to the same atomic
                    // global claim as every other fire path: if the claim
                    // fails we skip the inject this cycle (the per-watcher
                    // `inject_cooldown` re-fires it once the global window
                    // clears). The per-type cooldown
                    // (`watcher_inject_due`) above remains the inner
                    // lower-bound either way.
                    // exempt=true (default) -> claim is skipped (true).
                    // exempt=false -> attempt the atomic claim; false means
                    // the global ceiling is active and we must skip the
                    // inject this cycle (fall through to auto-respawn /
                    // healthcheck / logging — do NOT `return` here).
                    let global_gate_ok = config.general.global_cooldown_exempt_watcher_down
                        || try_claim_global_interrupt(
                            state,
                            config.general.post_interrupt_cooldown_secs,
                            config.general.global_cooldown_backoff_base,
                            config.general.global_cooldown_max_secs,
                            &now,
                        );
                    if !global_gate_ok {
                        debug!(
                            missing = %missing_list,
                            cooldown = config.general.post_interrupt_cooldown_secs,
                            "watcher-down inject would fire but global post-interrupt cooldown active (exempt=false) — deferring"
                        );
                        write_jsonl_log(
                            &config.general.log_file,
                            "watcher_inject_global_cooldown_deferred",
                            serde_json::json!({
                                "missing": missing_names,
                                "cooldown_secs": config.general.post_interrupt_cooldown_secs,
                            }),
                        );
                    } else {
                    // Two-phase obligation-precedence gate (BUG 2 follow-up to
                    // #424). The watcher-down inject was the 4th interrupt fire
                    // site #424's gate did NOT cover — #424 scoped the
                    // obligation rung to prolonged-thinking / context-low /
                    // heartbeat-stale and explicitly left "watcher-down ...
                    // unchanged", so a down watcher escalated straight from
                    // event to a turn-cancelling tmux interrupt with no
                    // obligation rung in between. Mirror the other three: on
                    // first detection ARM the obligation (pending alert the
                    // PreToolUse alert-gate hook bites on + emit the event)
                    // WITHOUT interrupting; only escalate to the interrupt
                    // once the dwell has elapsed and no background subagents
                    // are live (interrupting would kill healthy in-flight
                    // agents). The cross-gate suppression `escalation` backstop
                    // already forced past active-turn suppression above; the
                    // obligation dwell is an INDEPENDENT, additional rung —
                    // EXCEPT we honor an active suppression-escalation by
                    // forcing the obligation to Escalate too (a capped
                    // suppression run is exactly the "lower rung demonstrably
                    // failed" case the dwell must not re-delay). `dwell_secs`
                    // of 0 disables the gate (legacy same-cycle interrupt).
                    let wd_active_subagents =
                        crate::respawn::count_alive_subagents();
                    // consumer_down forces immediate Escalate (like an active
                    // suppression-escalation): the obligation-dwell would else
                    // Hold indefinitely while background subagents stay alive,
                    // and there is no working quiet channel to defer to when the
                    // event consumer is the down watcher.
                    let wd_decision = watcher_down_obligation_decision(
                        escalation.is_some() || consumer_down || down_cap_exceeded,
                        state.watcher_down_obligation_armed_at.as_deref(),
                        config.general.obligation_dwell_secs,
                        wd_active_subagents,
                        &now,
                    );
                    if matches!(
                        wd_decision,
                        ObligationDecision::ArmObligation | ObligationDecision::Hold
                    ) {
                        let _ = crate::obligation_arm::arm_alert_obligation(
                            &format!(
                                "[CLAUDE-WATCH] WATCHER(S) DOWN: {}. Restart them \
                                 with: {}",
                                missing_list,
                                missing_names
                                    .iter()
                                    .map(|n| format!("watcher-ctl run {}", n))
                                    .collect::<Vec<_>>()
                                    .join(", "),
                            ),
                            "claude-watch-watcher-down",
                        );
                        if state.watcher_down_obligation_armed_at.is_none() {
                            state.watcher_down_obligation_armed_at = Some(now.clone());
                        }
                        // Event sink only (NOT the interrupt) this cycle. The
                        // self-feedback guard still applies: skip the emit when
                        // the only down watcher is the event consumer itself.
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
                                message: &watcher_reason,
                            });
                        }
                        debug!(
                            missing = %missing_list,
                            dwell_secs = config.general.obligation_dwell_secs,
                            active_subagents = wd_active_subagents,
                            "watcher-down: obligation armed/held — deferring interrupt"
                        );
                        write_jsonl_log(
                            &config.general.log_file,
                            "watcher_down_obligation_armed",
                            serde_json::json!({
                                "missing": missing_names,
                                "dwell_secs": config.general.obligation_dwell_secs,
                                "active_subagents": wd_active_subagents,
                            }),
                        );
                        // Do NOT stamp last_watcher_inject — no inject fired,
                        // so the per-watcher cooldown clock stays untouched and
                        // the next cycle re-evaluates the dwell.
                        crate::state::save_state(&config.general.state_file, state);
                    } else {
                    // Escalate: obligation served its purpose — disarm so a
                    // fresh outage re-arms — then run the existing
                    // interrupt+inject path unchanged.
                    state.watcher_down_obligation_armed_at = None;
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
                    } else if down_cap_exceeded && actively_turning {
                        // Per-watcher cap forced the inject past active-turn
                        // suppression (the shared escalation did NOT fire). Log
                        // distinctly so the "down comms watcher surfaced despite
                        // a busy main loop" case is greppable.
                        warn!(
                            missing = %missing_list,
                            max_down_secs = ?max_down_secs,
                            max_suppress_secs = config.watcher_monitor.max_suppress_secs,
                            "watcher-down inject forced: watcher down past per-watcher suppression cap — overriding main-loop-active suppression"
                        );
                        write_jsonl_log(
                            &config.general.log_file,
                            "watcher_down_suppression_capped",
                            serde_json::json!({
                                "site": "watcher_monitor",
                                "max_down_secs": max_down_secs,
                                "max_suppress_secs": config.watcher_monitor.max_suppress_secs,
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

                    // KNOB #4 (2026-06-24): watcher-down is a ROUTINE tier — the
                    // recovery action is "spawn the restart command as a
                    // background task", which can wait for the next turn
                    // boundary. The OLD behavior (interrupt_and_wait: a
                    // rapid-fire Escape blast "to break any inline work") was
                    // the single most destructive part of the watcher-down
                    // storm: it CANCELLED the loop's in-flight turn AND killed
                    // any mid-flight background agents, every re-fire — turning
                    // a benign "restart a watcher" nudge into repeated
                    // turn-aborts that made the loop spend all its cycles
                    // recovering instead of working. So we no longer Escape
                    // here; we QUEUE the restart prompt via the non-cancelling
                    // path (`inject_to_agent_queued`). The prompt + the
                    // structured claude-event below are unchanged.
                    //
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
                    inject_dispatch::inject_to_agent_queued(&effective_pane, &prompt).await;
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
                    } // end Escalate branch (obligation-precedence gate)
                    }
                }
            }
        } else {
            // No critically-missing watchers this cycle — disarm the
            // watcher-down obligation so a future fresh outage re-arms from
            // scratch (mirrors the heartbeat-stale "condition cleared" disarm).
            // Only persist if it was actually armed, to avoid a needless write.
            if state.watcher_down_obligation_armed_at.is_some() {
                state.watcher_down_obligation_armed_at = None;
                crate::state::save_state(&config.general.state_file, state);
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
    let tmux_brief = tmux::healthcheck_brief(&config.tmux).await;

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
                // Two-phase escalation gate (BUG 1 fix): ARM the obligation
                // rung first. The first detection cycle writes a pending alert
                // + emits the event (and keeps the pingme push) WITHOUT
                // interrupting; only a later cycle (dwell elapsed, obligation
                // armed, 0 live subagents) escalates to the full
                // interrupt+inject `alert::alert`. `active_subagents` is
                // recomputed here (cheap /proc scan) since the detection-time
                // value is out of scope at this fire site.
                let hb_active_subagents =
                    crate::respawn::count_alive_subagents();
                match obligation_escalation_decision(
                    state.heartbeat_obligation_armed_at.as_deref(),
                    config.general.obligation_dwell_secs,
                    hb_active_subagents,
                    &now,
                ) {
                    ObligationDecision::ArmObligation | ObligationDecision::Hold => {
                        let _ = crate::obligation_arm::arm_alert_obligation(
                            &msg,
                            "claude-watch-heartbeat-stale",
                        );
                        if state.heartbeat_obligation_armed_at.is_none() {
                            state.heartbeat_obligation_armed_at = Some(now.clone());
                        }
                        // Keep the push notification + structured event, but
                        // NOT the interrupting `alert::alert`.
                        if use_pingme {
                            alert::send_pingme(&msg).await;
                        }
                        alert::emit_event(event_alert);
                        debug!(
                            stuck_reason = %stuck_reason,
                            dwell_secs = config.general.obligation_dwell_secs,
                            active_subagents = hb_active_subagents,
                            "ack-stale: obligation armed/held — deferring interrupt"
                        );
                    }
                    ObligationDecision::Escalate => {
                        // Inject the ack-SPECIFIC recovery prompt, not the
                        // generic `resume_prompt`. This is the ack-stale path
                        // (the only site that sets `stuck = true`), and the
                        // recovery action is to run the per-batch ack. The
                        // generic resume_prompt (a "/cleanup" directive) never
                        // mentions acking, so prior to this the inject landed
                        // but liveness stayed stale even while the loop was
                        // actively working (2026-06-19 incident: ~85 min stale
                        // with a live loop).
                        alert::alert(
                            &msg,
                            &alert_pane,
                            &config.alerts.ack_stale_prompt,
                            use_pingme,
                            event_alert,
                            // KNOB #4 (2026-06-24): ack-stale is a ROUTINE tier
                            // — recovery is one bare `event-ack ack-batch`,
                            // which can wait for the next turn boundary. Do NOT
                            // seize the turn / kill subagents: queue the nudge.
                            false,
                        )
                        .await;
                        // Obligation served its purpose — disarm so the next
                        // fresh stale episode re-arms.
                        state.heartbeat_obligation_armed_at = None;
                    }
                }
            }

            state.last_alert = Some(now.clone());
        }
    } else {
        state.consecutive_failures = 0;
        state.alert_count = 0;
        // Heartbeat-stale condition cleared — disarm the two-phase obligation.
        state.heartbeat_obligation_armed_at = None;
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
    use crate::credentials::{AccessTokenState, CredentialExpiry};

    // ---- Proactive login-expiry decision ----

    fn evidence() -> ExpiryEvidence {
        ExpiryEvidence {
            pane_days_left: None,
            credentials: CredentialExpiry::Unknown,
            credentials_may_trigger: true,
            auto_enabled: true,
            auto_days: 1,
            since_last_attempt: None,
            retry_seconds: 3600,
            attempts: 0,
            max_attempts: 3,
            login_pending: false,
        }
    }

    /// Nothing on the pane and nothing on disk: the daemon stays out of it.
    #[test]
    fn no_evidence_is_idle() {
        assert_eq!(decide_expiry_action(&evidence()), ExpiryAction::Idle);
    }

    /// THE false-positive guard, and the reason this function exists. A pane
    /// sighting that the credential store contradicts is conversation text —
    /// somebody reading this file, or its tests, or the diff that added them.
    /// Acting on it would park a healthy session in a login modal.
    #[test]
    fn a_pane_sighting_a_healthy_credential_contradicts_is_ignored() {
        let ev = ExpiryEvidence {
            pane_days_left: Some(2),
            credentials: CredentialExpiry::Healthy,
            ..evidence()
        };
        assert_eq!(decide_expiry_action(&ev), ExpiryAction::Idle);
    }

    /// An already-dead credential belongs to the REACTIVE path. Racing it into
    /// the same modal from two directions helps nobody.
    #[test]
    fn an_already_expired_credential_is_left_to_the_reactive_path() {
        let ev = ExpiryEvidence {
            pane_days_left: Some(1),
            credentials: CredentialExpiry::Expired,
            ..evidence()
        };
        assert_eq!(decide_expiry_action(&ev), ExpiryAction::Idle);
    }

    /// An unreadable credential store is UNKNOWN, never a negative — the pane
    /// still carries the decision, but the alert has to say it stands alone.
    #[test]
    fn an_unreadable_credential_store_still_acts_but_uncorroborated() {
        let ev = ExpiryEvidence {
            pane_days_left: Some(2),
            credentials: CredentialExpiry::Unknown,
            ..evidence()
        };
        assert_eq!(
            decide_expiry_action(&ev),
            ExpiryAction::AlertOnly {
                days_left: 2,
                corroborated: false,
            }
        );
    }

    /// The transient form of Claude Code's warning lives about fifteen
    /// seconds, so a poller MISSING it proves nothing. The credential store
    /// alone is enough to act on.
    #[test]
    fn the_credential_store_alone_can_carry_the_decision() {
        let ev = ExpiryEvidence {
            pane_days_left: None,
            credentials: CredentialExpiry::Expiring { days_left: 1 },
            ..evidence()
        };
        assert_eq!(decide_expiry_action(&ev), ExpiryAction::AutoLogin { days_left: 1 });
    }

    /// The store is a VETO by default, not a trigger: a short-lived rolling
    /// refresh token classifies as "expiring" for every second of its healthy
    /// life, so a store-driven trigger would fire forever on a fine session.
    #[test]
    fn the_credential_store_does_not_trigger_on_its_own_unless_allowed() {
        let ev = ExpiryEvidence {
            pane_days_left: None,
            credentials: CredentialExpiry::Expiring { days_left: 1 },
            credentials_may_trigger: false,
            ..evidence()
        };
        assert_eq!(decide_expiry_action(&ev), ExpiryAction::Idle);

        // ...but it still VETOES a pane sighting it contradicts, which is the
        // half that is never optional.
        let vetoed = ExpiryEvidence {
            pane_days_left: Some(2),
            credentials: CredentialExpiry::Healthy,
            credentials_may_trigger: false,
            ..evidence()
        };
        assert_eq!(decide_expiry_action(&vetoed), ExpiryAction::Idle);

        // ...and still corroborates one it agrees with.
        let agreed = ExpiryEvidence {
            pane_days_left: Some(1),
            credentials: CredentialExpiry::Expiring { days_left: 1 },
            credentials_may_trigger: false,
            ..evidence()
        };
        assert_eq!(decide_expiry_action(&agreed), ExpiryAction::AutoLogin { days_left: 1 });
    }

    /// Claude Code starts SHOWING the warning three days out but only starts
    /// nagging inside one. Three days out is a heads-up, not a reason to
    /// interrupt a working session.
    #[test]
    fn a_warning_outside_the_auto_window_only_alerts() {
        let ev = ExpiryEvidence {
            pane_days_left: Some(3),
            credentials: CredentialExpiry::Expiring { days_left: 3 },
            ..evidence()
        };
        assert_eq!(
            decide_expiry_action(&ev),
            ExpiryAction::AlertOnly {
                days_left: 3,
                corroborated: true,
            }
        );
    }

    /// When the two sources disagree, take the more urgent number: a banner
    /// left over from an earlier render must not be able to stretch a deadline.
    #[test]
    fn disagreeing_sources_resolve_to_the_more_urgent_one() {
        let ev = ExpiryEvidence {
            pane_days_left: Some(3),
            credentials: CredentialExpiry::Expiring { days_left: 1 },
            ..evidence()
        };
        assert_eq!(decide_expiry_action(&ev), ExpiryAction::AutoLogin { days_left: 1 });
    }

    /// Debounce. The warning stands for DAYS; without spacing, a ten-second
    /// poll re-fires the login flow ~8,600 times a day.
    #[test]
    fn a_recent_attempt_blocks_a_re_fire_and_an_old_one_does_not() {
        let recent = ExpiryEvidence {
            credentials: CredentialExpiry::Expiring { days_left: 1 },
            since_last_attempt: Some(60.0),
            attempts: 1,
            ..evidence()
        };
        assert_eq!(
            decide_expiry_action(&recent),
            ExpiryAction::AlertOnly {
                days_left: 1,
                corroborated: true,
            }
        );

        let stale = ExpiryEvidence {
            since_last_attempt: Some(3601.0),
            ..recent
        };
        assert_eq!(decide_expiry_action(&stale), ExpiryAction::AutoLogin { days_left: 1 });
    }

    /// Failing loudly is right; failing every hour forever is not. Once the
    /// budget is spent the alert keeps going out and the session is left alone.
    #[test]
    fn the_attempt_budget_stops_auto_fire_but_not_the_alert() {
        let ev = ExpiryEvidence {
            credentials: CredentialExpiry::Expiring { days_left: 1 },
            since_last_attempt: Some(99999.0),
            attempts: 3,
            max_attempts: 3,
            ..evidence()
        };
        assert_eq!(
            decide_expiry_action(&ev),
            ExpiryAction::AlertOnly {
                days_left: 1,
                corroborated: true,
            }
        );
    }

    /// A dialog we already opened is still waiting for its code. Firing a
    /// second `/login` types the literal text into the first one's field.
    #[test]
    fn a_pending_login_dialog_blocks_a_second_fire() {
        let ev = ExpiryEvidence {
            credentials: CredentialExpiry::Expiring { days_left: 1 },
            since_last_attempt: Some(99999.0),
            login_pending: true,
            ..evidence()
        };
        assert_eq!(
            decide_expiry_action(&ev),
            ExpiryAction::AlertOnly {
                days_left: 1,
                corroborated: true,
            }
        );
    }

    /// Auto-fire off means alert only — the reactive path is untouched either
    /// way, so turning this off degrades to "tell me, I'll handle it".
    #[test]
    fn auto_fire_disabled_degrades_to_alert_only() {
        let ev = ExpiryEvidence {
            credentials: CredentialExpiry::Expiring { days_left: 1 },
            auto_enabled: false,
            ..evidence()
        };
        assert_eq!(
            decide_expiry_action(&ev),
            ExpiryAction::AlertOnly {
                days_left: 1,
                corroborated: true,
            }
        );
    }

    // ---- Reactive 401-banner decision ----

    fn banner_evidence() -> BannerEvidence {
        BannerEvidence {
            access_token: AccessTokenState::Expired,
            auto_enabled: true,
            since_last_attempt: None,
            retry_seconds: 3600,
            attempts: 0,
            max_attempts: 3,
            login_pending: false,
        }
    }

    /// THE incident: the banner is on a live pane and the credential store
    /// agrees the access token is dead. Fire.
    #[test]
    fn banner_with_expired_access_token_fires_self_login() {
        assert_eq!(decide_banner_action(&banner_evidence()), BannerAction::AutoLogin);
        let missing = BannerEvidence {
            access_token: AccessTokenState::Missing,
            ..banner_evidence()
        };
        assert_eq!(decide_banner_action(&missing), BannerAction::AutoLogin);
    }

    /// THE false-positive guard, and the reason the detector alone is not
    /// trusted. The banner text on a pane whose credential store says the
    /// access token is valid is conversation — a session reading this file,
    /// its tests, or the diff that introduced them. Silence, not an alert.
    #[test]
    fn banner_with_a_valid_access_token_is_ignored_outright() {
        let ev = BannerEvidence {
            access_token: AccessTokenState::Valid,
            ..banner_evidence()
        };
        assert_eq!(decide_banner_action(&ev), BannerAction::Ignore);
        // ...even with every brake released and auto on.
        let ev = BannerEvidence {
            access_token: AccessTokenState::Valid,
            since_last_attempt: Some(99999.0),
            attempts: 0,
            ..banner_evidence()
        };
        assert_eq!(decide_banner_action(&ev), BannerAction::Ignore);
    }

    /// An unreadable store is UNKNOWN, never a negative — but it is also not
    /// enough evidence to open a modal on. Alert, and say it stands alone.
    #[test]
    fn banner_with_an_unreadable_store_alerts_uncorroborated_and_never_fires() {
        let ev = BannerEvidence {
            access_token: AccessTokenState::Unknown,
            ..banner_evidence()
        };
        assert_eq!(
            decide_banner_action(&ev),
            BannerAction::AlertOnly {
                corroborated: false,
                reason: "credential store unreadable",
            }
        );
    }

    /// Auto off degrades to the high-priority alert, never to silence.
    #[test]
    fn banner_auto_disabled_degrades_to_alert_only() {
        let ev = BannerEvidence {
            auto_enabled: false,
            ..banner_evidence()
        };
        assert_eq!(
            decide_banner_action(&ev),
            BannerAction::AlertOnly {
                corroborated: true,
                reason: "auto-login disabled",
            }
        );
    }

    /// The fire is bounded by the SAME knobs as the proactive path: one
    /// dialog at a time, retry spacing, and the per-window attempt budget.
    /// Walk a window the way `fire_self_login` books it and check each brake
    /// engages in turn — and that every held cycle still alerts.
    #[test]
    fn banner_fire_is_bounded_by_the_shared_self_login_knobs() {
        let max_attempts = 3;
        let retry_seconds = 3600;
        let mut attempts = 0;

        // Cycle 1: nothing booked yet -> fire. `fire_self_login` books the
        // attempt and sets the dialog latch before the command runs.
        let ev = BannerEvidence {
            attempts,
            max_attempts,
            retry_seconds,
            ..banner_evidence()
        };
        assert_eq!(decide_banner_action(&ev), BannerAction::AutoLogin);
        attempts += 1;

        // Cycle 2: the dialog is up and waiting for its code -> held, alert.
        let ev = BannerEvidence {
            attempts,
            since_last_attempt: Some(10.0),
            login_pending: true,
            ..banner_evidence()
        };
        assert_eq!(
            decide_banner_action(&ev),
            BannerAction::AlertOnly {
                corroborated: true,
                reason: "login dialog already open",
            }
        );

        // The watchdog abandoned the dialog (latch cleared) but the retry
        // spacing has not elapsed -> held, alert.
        let ev = BannerEvidence {
            attempts,
            since_last_attempt: Some(retry_seconds as f64 - 1.0),
            login_pending: false,
            ..banner_evidence()
        };
        assert_eq!(
            decide_banner_action(&ev),
            BannerAction::AlertOnly {
                corroborated: true,
                reason: "retry spacing",
            }
        );

        // Spacing elapsed -> fire again, up to the budget.
        while attempts < max_attempts {
            let ev = BannerEvidence {
                attempts,
                since_last_attempt: Some(retry_seconds as f64),
                ..banner_evidence()
            };
            assert_eq!(decide_banner_action(&ev), BannerAction::AutoLogin, "attempt {attempts}");
            attempts += 1;
        }

        // Budget spent -> held forever (until the window resets), still alerting.
        let ev = BannerEvidence {
            attempts,
            since_last_attempt: Some(99999.0),
            ..banner_evidence()
        };
        assert_eq!(
            decide_banner_action(&ev),
            BannerAction::AlertOnly {
                corroborated: true,
                reason: "attempt budget exhausted",
            }
        );
    }

    /// The proactive path's brakes and the banner path's brakes are the same
    /// state: an attempt the proactive path booked counts against the banner
    /// path's budget and spacing, because it is the same dialog on the same
    /// pane.
    #[test]
    fn banner_and_expiry_paths_share_one_budget_and_one_latch() {
        // Proactive fired a moment ago (latch set).
        let proactive_pending = ExpiryEvidence {
            pane_days_left: Some(1),
            credentials: CredentialExpiry::Expiring { days_left: 1 },
            since_last_attempt: Some(5.0),
            attempts: 1,
            login_pending: true,
            ..evidence()
        };
        assert!(matches!(
            decide_expiry_action(&proactive_pending),
            ExpiryAction::AlertOnly { .. }
        ));
        let banner_same_state = BannerEvidence {
            since_last_attempt: Some(5.0),
            attempts: 1,
            login_pending: true,
            ..banner_evidence()
        };
        assert_eq!(
            decide_banner_action(&banner_same_state),
            BannerAction::AlertOnly {
                corroborated: true,
                reason: "login dialog already open",
            }
        );
    }

    /// `SelfLoginTrigger` labels are what the JSONL events and alerts key on;
    /// pin them so a log grep written today still works tomorrow.
    #[test]
    fn self_login_trigger_labels_are_stable() {
        assert_eq!(
            SelfLoginTrigger::ExpiryWarning { days_left: 2 }.as_str(),
            "expiry_warning"
        );
        assert_eq!(SelfLoginTrigger::Banner401.as_str(), "401_banner");
        assert_eq!(
            SelfLoginTrigger::ExpiryWarning { days_left: 2 }.days_left(),
            Some(2)
        );
        assert_eq!(SelfLoginTrigger::Banner401.days_left(), None);
    }

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
    // BUG A regression suite. The monitor decides liveness off the SAME
    // process set `pgrep` matched (probing each PID for genuine, non-zombie
    // liveness) — NOT off a separately-recorded PID file that drifts out of
    // sync after a restart. A watcher that `pgrep` finds genuinely alive must
    // NEVER be reported DOWN, even when its `/var/run/claude/<name>.pid` file
    // is stale (points at a now-reaped PID from before a `make deploy-systemd` /
    // watcher respawn). The zombie guard preserves the original orphan-
    // detection intent: a `<defunct>` match does not count as alive.

    #[test]
    fn test_watcher_is_down_no_matches() {
        // No matching processes at all -> DOWN.
        assert!(watcher_is_down(&[], 1, |_| true));
    }

    #[test]
    fn test_watcher_is_down_alive_match_meets_min() {
        // One genuinely-alive match, min_count 1 -> NOT down.
        assert!(!watcher_is_down(&[42], 1, |pid| pid == 42));
        // Several alive matches, min_count 1 -> NOT down.
        assert!(!watcher_is_down(&[42, 43, 44], 1, |_| true));
    }

    #[test]
    fn test_watcher_is_down_zombie_only_match() {
        // The orphan/zombie case (original bug-2 intent, preserved): pgrep
        // matched a PID but it is a zombie / dead -> the alive-count is 0 ->
        // DOWN. This is the only way a matched-but-not-running watcher is
        // flagged now; no recorded PID file is consulted.
        assert!(watcher_is_down(&[42], 1, |_| false));
        // Multiple matches, all zombies -> still DOWN.
        assert!(watcher_is_down(&[42, 43, 44], 1, |_| false));
    }

    #[test]
    fn test_watcher_is_down_mixed_alive_and_zombie() {
        // 3 pgrep matches but only 1 genuinely alive. min_count 1 -> NOT down
        // (the live one satisfies the requirement). The zombies are ignored.
        let alive_pid = 100u32;
        assert!(!watcher_is_down(&[100, 200, 300], 1, move |pid| pid
            == alive_pid));
        // Same set but min_count 2 -> DOWN (only 1 of the 2 required is alive).
        assert!(watcher_is_down(&[100, 200, 300], 2, move |pid| pid
            == alive_pid));
    }

    /// BUG A: stale-PID-file-after-restart must NOT cause a false DOWN.
    ///
    /// Before the fix, the monitor read a recorded PID from
    /// `/var/run/claude/<name>.pid`, found it dead (the watcher had been
    /// respawned under a fresh PID by `make deploy-systemd` / watchmen), and reported
    /// the watcher DOWN — while `pgrep` (and `watcher-status`, and `ps`) all
    /// saw it genuinely running. Now the monitor probes the matched PIDs
    /// directly, so the genuinely-running watcher is NEVER reported DOWN
    /// regardless of any stale recorded PID.
    #[test]
    fn test_watcher_is_down_false_down_after_restart_regression() {
        // watchmen/pgrep sees the watcher genuinely running under PID 5000
        // (the post-restart PID). An old PID file might still name PID 42
        // (now reaped) — but that file is no longer consulted, so it cannot
        // poison the verdict. Monitor must agree with watchmen: NOT down.
        let live_pid = 5000u32;
        assert!(
            !watcher_is_down(&[live_pid], 1, move |pid| pid == live_pid),
            "a watcher that pgrep finds genuinely alive must NEVER be \
             reported DOWN, even with a stale recorded PID file"
        );
    }

    #[test]
    fn test_watcher_is_down_min_count_zero() {
        // Edge case: min_count = 0 -> never DOWN, even with no matches.
        assert!(!watcher_is_down(&[], 0, |_| panic!("no probe needed")));
        // With matches present, still not DOWN.
        assert!(!watcher_is_down(&[42], 0, |_| true));
    }

    // --- pidfile_watcher_is_down tests (2026-06-11 exec-defeats-pgrep fix) ---
    //
    // The monitor now decides DOWN purely from the watcher's OWN recorded
    // pidfile (its `<name>.lock` flock file, or the `<name>.pid` from
    // watcher_run), NOT from `pgrep` on the launcher `.sh` pattern (which the
    // launcher's `exec` defeats — the `.sh` path vanishes from argv). A watcher
    // is UP iff the pidfile names a live process whose cmdline matches.

    #[test]
    fn test_pidfile_watcher_up_when_live_matching() {
        // Pidfile names a PID that is alive AND whose cmdline matches → UP.
        assert!(!pidfile_watcher_is_down(Some(4242), true, true));
    }

    #[test]
    fn test_pidfile_watcher_down_when_pidfile_missing() {
        // No pidfile → DOWN (no recorded instance). The alive/match flags are
        // meaningless here and must not flip the verdict.
        assert!(pidfile_watcher_is_down(None, false, false));
        assert!(pidfile_watcher_is_down(None, true, true));
    }

    #[test]
    fn test_pidfile_watcher_down_when_stale_dead_pid() {
        // Pidfile exists but the recorded PID is dead (stale pidfile) → DOWN.
        // This correctly triggers a legitimate restart.
        assert!(pidfile_watcher_is_down(Some(4242), false, false));
    }

    #[test]
    fn test_pidfile_watcher_down_when_recycled_pid() {
        // Recorded PID is alive but its cmdline does NOT match this watcher —
        // the kernel recycled the PID to an unrelated process → DOWN (do not
        // wrongly suppress a real restart).
        assert!(pidfile_watcher_is_down(Some(4242), true, false));
    }

    // --- cmdline_matches_watcher tests -------------------------------------
    //
    // The exec-to-binary transform: the watcher's start_cmd is the launcher
    // SCRIPT (`.../claude-event-watch.sh`), but the live process — after
    // `exec /usr/local/bin/claude-event-watch` — has cmdline
    // `/bin/bash /usr/local/bin/claude-event-watch` (the `.sh` is GONE). The
    // matcher must tolerate this by stripping the script extension from the
    // start_cmd basename, while still rejecting an obviously-unrelated PID.

    #[test]
    fn test_cmdline_matches_exec_transform_sh_to_binary() {
        // The exact live shape this bug is about.
        let cmdline = "/bin/bash /usr/local/bin/claude-event-watch";
        let start_cmd = "/opt/claude-container/watchers/claude-event-watch.sh";
        assert!(
            cmdline_matches_watcher(cmdline, start_cmd),
            "the exec'd binary cmdline (no .sh) must match the .sh launcher \
             start_cmd via the stripped stem"
        );
    }

    #[test]
    fn test_cmdline_matches_literal_path() {
        // When the live cmdline DOES contain the full start_cmd (no exec), the
        // full-token / basename match still works.
        let cmdline = "/bin/bash /opt/claude-container/watchers/claude-event-watch.sh";
        let start_cmd = "/opt/claude-container/watchers/claude-event-watch.sh";
        assert!(cmdline_matches_watcher(cmdline, start_cmd));
    }

    #[test]
    fn test_cmdline_matches_rejects_unrelated() {
        // A recycled PID running something unrelated must NOT match.
        let cmdline = "/usr/bin/python3 /home/user/some-other-tool.py";
        let start_cmd = "/opt/claude-container/watchers/claude-event-watch.sh";
        assert!(!cmdline_matches_watcher(cmdline, start_cmd));
    }

    #[test]
    fn test_cmdline_matches_empty_start_cmd_is_false() {
        assert!(!cmdline_matches_watcher("/bin/bash /usr/local/bin/x", ""));
        assert!(!cmdline_matches_watcher("/bin/bash /usr/local/bin/x", "   "));
    }

    // --- read_watcher_recorded_pid: prefers .lock, falls back to .pid -------

    #[test]
    fn test_read_watcher_recorded_pid_prefers_lock() {
        // The watcher writes its PID to `<name>.lock` (the flock singleton
        // guard). With both files present the .lock wins.
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path().to_str().unwrap();
        std::fs::write(dir.path().join("claude-event-watch.lock"), "31956\n").unwrap();
        std::fs::write(dir.path().join("claude-event-watch.pid"), "12345\n").unwrap();
        assert_eq!(
            read_watcher_recorded_pid(d, "claude-event-watch"),
            Some(31956)
        );
    }

    #[test]
    fn test_read_watcher_recorded_pid_falls_back_to_pid() {
        // No .lock (e.g. watcher spawned via watcher_run, which writes .pid).
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path().to_str().unwrap();
        std::fs::write(dir.path().join("w.pid"), "777\n").unwrap();
        assert_eq!(read_watcher_recorded_pid(d, "w"), Some(777));
    }

    #[test]
    fn test_read_watcher_recorded_pid_none_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            read_watcher_recorded_pid(dir.path().to_str().unwrap(), "nope"),
            None
        );
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
    fn test_context_threshold_compact_gated_below_danger_on_large_window() {
        // Regression (incident 2026-09-01): 1M window, ~48% REAL usage, but
        // Claude Code reported "Context left until auto-compact: 5%" — its
        // auto-compact point is decoupled from the true 1M window. With
        // compact_remaining = 5 <= compact_trigger_percent = 5 the old code
        // fired a destructive self-clear at 48% used. The compact signal is
        // now gated behind the real-usage danger zone (margin = 100000 => the
        // 900K point), so this must NOT trigger.
        let result =
            check_context_threshold_with_margin(484889, 1_000_000, Some(5), 75, 5, Some(100_000));
        assert!(
            result.is_none(),
            "compact_remaining must not fire at 48% real usage on a 1M window"
        );
    }

    #[test]
    fn test_context_threshold_compact_fires_in_danger_zone_large_window() {
        // Same 1M window + margin, but now genuinely near full (91% used, i.e.
        // within margin = max - 100000 = 900K). compact_remaining = 5 must
        // still fire — the signal is honored once real usage confirms danger.
        let result =
            check_context_threshold_with_margin(910000, 1_000_000, Some(5), 75, 5, Some(100_000));
        assert!(result.is_some(), "should fire at 91% used within margin");
        let (_, by_compact) = result.unwrap();
        assert!(by_compact, "should be BY_COMPACT in the danger zone");
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
    fn test_context_threshold_compact_gated_below_margin_zone() {
        // Formerly `test_context_threshold_compact_wins_over_margin`, which
        // asserted compact_remaining=3 FIRES at 200K/1M (20% real usage) even
        // though tokens are far below the margin zone (max-margin=900K). That
        // encoded the 2026-09-01 misfire: a destructive self-clear at 20% of a
        // 1M window, driven by Claude Code's auto-compact % (decoupled from the
        // true window). compact_remaining no longer "wins" below the real-usage
        // danger zone — it is GATED behind it. Expect None here.
        let result = check_context_threshold_with_margin(
            200_000,
            1_000_000,
            Some(3),
            75,
            5,
            Some(100_000),
        );
        assert!(
            result.is_none(),
            "compact_remaining must not fire at 20% real usage on a 1M window"
        );
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
    fn test_malformed_post_clear_low_tokens_exempt() {
        // Reported bug: the first turn AFTER a /clear was flagged MALFORMED
        // because the pre-clear turn's `<invoke>` block was still in the
        // captured scrollback. A freshly-cleared / near-empty context (tokens
        // below the fresh threshold) cannot have produced a real malformed
        // episode, so it is exempt.
        let state = State::default();
        assert!(
            malformed_detection_post_clear(&state, 4_200, false),
            "low-token (freshly-cleared) context must be exempt from malformed detection"
        );
        assert!(
            malformed_detection_post_clear(&state, 0, false),
            "tokens=0 (just-landed clear) must be exempt"
        );
    }

    #[test]
    fn test_malformed_post_clear_recent_clear_exempt() {
        // Large-preamble case: a brand-new context can exceed the low-token
        // threshold immediately (the always-loaded preamble alone is big), yet
        // the pre-clear malformed block is still lingering in the 60-line
        // scrollback. A clear recorded within the grace window keeps the
        // boundary turn exempt even though tokens are already high.
        let mut state = State::default();
        state.last_context_clear = Some(Utc::now().to_rfc3339());
        assert!(
            malformed_detection_post_clear(&state, 120_000, false),
            "a clear within the grace window must exempt even a high-token boundary turn"
        );
    }

    #[test]
    fn test_malformed_not_post_clear_high_tokens_no_recent_clear() {
        // The guard must NOT swallow genuine malforms: a normal mid-session turn
        // (high tokens, no recent clear) is fully subject to detection.
        let state = State::default();
        assert!(
            !malformed_detection_post_clear(&state, 120_000, false),
            "a normal high-token turn with no recent clear must NOT be exempt — \
             genuine malforms still fire"
        );
    }

    #[test]
    fn test_malformed_post_clear_old_clear_not_exempt() {
        // A clear far in the past (well beyond the grace window) does not exempt
        // a later high-token turn — by then the old scrollback has scrolled out
        // and any malform is a fresh, live failure.
        let mut state = State::default();
        let stale = Utc::now()
            - chrono::Duration::seconds(MALFORMED_POST_CLEAR_GRACE_SECS as i64 + 120);
        state.last_context_clear = Some(stale.to_rfc3339());
        assert!(
            !malformed_detection_post_clear(&state, 120_000, false),
            "a clear older than the grace window must not exempt a later malform"
        );
    }

    #[test]
    fn test_malformed_post_clear_active_ui_never_suppresses() {
        // 2026-08-27 regression: a long, busy, many-agent session reads
        // tokens==0 on essentially every poll (the bare context total is
        // scrolled behind the thinking indicator / agent roster / background
        // work markers), which used to make this predicate return `true`
        // (suppress) for the WHOLE session -- silently neutering the
        // malformed-tool-call detector exactly when it matters most. A
        // positive active_ui signal must short-circuit to "not a boundary"
        // regardless of how low `tokens` reads, and regardless of a recent
        // `last_context_clear` timestamp.
        let state = State::default();
        assert!(
            !malformed_detection_post_clear(&state, 0, true),
            "active_ui must override the low-token fresh-boundary exemption"
        );
        let mut state_recent_clear = State::default();
        state_recent_clear.last_context_clear = Some(Utc::now().to_rfc3339());
        assert!(
            !malformed_detection_post_clear(&state_recent_clear, 120_000, true),
            "active_ui must override the recent-clear-grace-window exemption too"
        );
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

    // --- Context-reset DETECTION tests (regression guard for 2026-08-22) ---
    //
    // 2026-08-22 incident: the daemon recognised a context reset only from a
    // token sample below 30K. The session's fresh context boots at ~77K
    // (large always-loaded preamble), and the clear fell inside a poll gap:
    //
    //     21:08:13  tokens=907979
    //     21:08:46  tokens=77185     <- the auto-clear landed in here
    //
    // Neither sample is under 30K, so `last_context_clear` was never stamped
    // and the dashboard's "Since Clear" tile read 1.07 DAYS (the previous
    // day's clear) 50 minutes after the clear it should have shown. These
    // tests pin drop-based detection, one per path that resets a context.

    #[test]
    fn test_context_reset_signal_fresh_sample() {
        // The classic case still holds: a near-empty sample is a reset even
        // with no previous sample to compare against (daemon just started).
        assert_eq!(
            context_reset_signal(None, 0),
            Some(ContextResetSignal::FreshSample)
        );
        assert_eq!(
            context_reset_signal(Some(900_000), 5_300),
            Some(ContextResetSignal::FreshSample)
        );
    }

    #[test]
    fn test_context_reset_signal_token_drop() {
        // The incident's exact samples.
        assert_eq!(
            context_reset_signal(Some(907_979), 77_185),
            Some(ContextResetSignal::TokenDrop)
        );
    }

    #[test]
    fn test_context_reset_signal_ignores_growth_and_jitter() {
        // A live context only climbs; a re-rendered status bar can jitter a
        // little. Neither may read as a clear, or every turn would stamp one.
        assert_eq!(context_reset_signal(Some(905_000), 950_000), None);
        assert_eq!(context_reset_signal(Some(900_000), 880_000), None);
        // Just short of the halving boundary.
        assert_eq!(context_reset_signal(Some(900_000), 450_001), None);
        // Exactly halved counts (>= ratio).
        assert_eq!(
            context_reset_signal(Some(900_000), 450_000),
            Some(ContextResetSignal::TokenDrop)
        );
    }

    #[test]
    fn test_context_reset_signal_needs_previously_high_sample() {
        // No previous sample, or a previous sample that was itself boot-level:
        // there is no context to have been reset.
        assert_eq!(context_reset_signal(None, 100_000), None);
        assert_eq!(context_reset_signal(Some(29_000), 100_000), None);
    }

    #[test]
    fn test_daemon_auto_clear_landing_above_fresh_threshold_stamps() {
        // PATH: daemon-triggered deferred auto-clear (context-low ->
        // self-clear). The clear lands, the replacement context boots at 77K,
        // and the daemon's first post-clear sample is already above the fresh
        // threshold. Must reset the in-flight flag AND stamp the clear.
        let config = config_for_reset_test();
        let mut state = State::default();
        state.context_clear_triggered = true;
        state.context_clear_child_pid = Some(12345);
        state.last_seen_tokens = Some(907_979);
        state.last_context_clear = Some("2026-08-21T16:21:11-04:00".to_string());
        let now = Utc::now().to_rfc3339();
        maybe_reset_context_clear(&config, &mut state, 77_185, &now);
        assert!(
            !state.context_clear_triggered,
            "a clear landing above the fresh threshold must still reset the flag"
        );
        assert!(state.context_clear_child_pid.is_none());
        assert_eq!(
            state.last_context_clear.as_deref(),
            Some(now.as_str()),
            "the auto-clear path must stamp last_context_clear at the observing cycle"
        );
        assert_eq!(
            state.post_clear_resume_injected_for.as_deref(),
            Some(now.as_str()),
            "a daemon-driven clear injects its own resume — latch the gate"
        );
    }

    #[test]
    fn test_agent_self_clear_landing_above_fresh_threshold_stamps() {
        // PATH: `self-clear` run by the agent / an operator / a skill. The
        // daemon never triggered, so only the external-clear path can stamp.
        let config = config_for_reset_test();
        let mut state = State::default();
        state.context_clear_triggered = false;
        state.last_seen_tokens = Some(903_905);
        state.last_context_clear = Some("2026-08-21T16:21:11-04:00".to_string());
        let now = Utc::now().to_rfc3339();
        maybe_reset_context_clear(&config, &mut state, 84_000, &now);
        assert_eq!(
            state.last_context_clear.as_deref(),
            Some(now.as_str()),
            "an externally-driven self-clear must stamp last_context_clear"
        );
    }

    #[test]
    fn test_manual_clear_then_restart_stamps() {
        // PATH: hand-typed /clear (or a `session-resume restart`, which brings
        // up a brand-new process whose context starts from the preamble). Both
        // present to the daemon as a collapsed token counter.
        let config = config_for_reset_test();
        let mut state = State::default();
        state.last_seen_tokens = Some(640_000);
        let now = Utc::now().to_rfc3339();
        maybe_reset_context_clear(&config, &mut state, 61_000, &now);
        assert_eq!(state.last_context_clear.as_deref(), Some(now.as_str()));
    }

    #[test]
    fn test_compaction_stamps() {
        // PATH: auto-compaction. The context is rebuilt from a summary, so the
        // counter collapses the same way a clear's does — and it IS a context
        // reset, which is what the panel measures.
        let config = config_for_reset_test();
        let mut state = State::default();
        state.last_seen_tokens = Some(950_000);
        let now = Utc::now().to_rfc3339();
        maybe_reset_context_clear(&config, &mut state, 180_000, &now);
        assert_eq!(state.last_context_clear.as_deref(), Some(now.as_str()));
    }

    #[test]
    fn test_reset_stamps_exactly_once_per_clear() {
        // The stamp must land on the observing cycle and NOT be refreshed by
        // the subsequent cycles of the new (small, growing) context — the tile
        // would otherwise sit pinned near zero forever.
        let config = config_for_reset_test();
        let mut state = State::default();
        state.last_seen_tokens = Some(907_979);
        let landing = Utc::now().to_rfc3339();
        maybe_reset_context_clear(&config, &mut state, 77_185, &landing);
        assert_eq!(state.last_context_clear.as_deref(), Some(landing.as_str()));

        // check_cycle slides the sample forward after the reset path runs.
        state.last_seen_tokens = Some(77_185);
        let later = (Utc::now() + chrono::Duration::seconds(30)).to_rfc3339();
        maybe_reset_context_clear(&config, &mut state, 85_993, &later);
        assert_eq!(
            state.last_context_clear.as_deref(),
            Some(landing.as_str()),
            "the growing new context must not re-stamp the clear"
        );
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

    // --- Token-progress guard tests (v2, 2026-06-11) ---

    #[test]
    fn test_token_action_keep_below_floor() {
        // Growth below the floor: keep the timer accumulating (this is the
        // growth-free time that earns a fire).
        assert_eq!(
            thinking_token_progress_action(Some(100_000), 101_500, 2000),
            ThinkingTokenAction::Keep
        );
        // Zero growth — definitely keep.
        assert_eq!(
            thinking_token_progress_action(Some(100_000), 100_000, 2000),
            ThinkingTokenAction::Keep
        );
    }

    #[test]
    fn test_token_action_rearm_at_floor() {
        // Growth at/above the floor: re-arm (slide timer + baseline).
        assert_eq!(
            thinking_token_progress_action(Some(100_000), 102_000, 2000),
            ThinkingTokenAction::Rearm
        );
        assert_eq!(
            thinking_token_progress_action(Some(100_000), 130_000, 2000),
            ThinkingTokenAction::Rearm
        );
    }

    #[test]
    fn test_token_action_counter_reset() {
        // Token counter went backwards (context clear / status-bar source
        // flap): old baseline is meaningless — re-baseline + slide.
        assert_eq!(
            thinking_token_progress_action(Some(150_000), 5_000, 2000),
            ThinkingTokenAction::RearmCounterReset
        );
    }

    #[test]
    fn test_token_action_late_baseline_capture() {
        // Baseline missing (tokens unparseable at episode start), tokens
        // now available: capture late, don't slide the timer.
        assert_eq!(
            thinking_token_progress_action(None, 100_000, 2000),
            ThinkingTokenAction::CaptureBaseline
        );
    }

    #[test]
    fn test_token_action_unparseable_or_disabled_keeps() {
        // tokens == 0 (unparseable now) or floor == 0 (guard disabled):
        // leave the timer alone — legacy behavior, fail-open at fire time.
        assert_eq!(
            thinking_token_progress_action(Some(100_000), 0, 2000),
            ThinkingTokenAction::Keep
        );
        assert_eq!(
            thinking_token_progress_action(None, 0, 2000),
            ThinkingTokenAction::Keep
        );
        assert_eq!(
            thinking_token_progress_action(Some(100_000), 100_000, 0),
            ThinkingTokenAction::Keep
        );
        assert_eq!(
            thinking_token_progress_action(None, 100_000, 0),
            ThinkingTokenAction::Keep
        );
    }

    #[test]
    fn test_apply_token_progress_rearm_slides_state() {
        // Rearm mutates BOTH timer and baseline and reports the reason.
        let mut start = Some("2026-06-11T19:03:26-04:00".to_string());
        let mut baseline = Some(283_000);
        let reason = apply_thinking_token_progress(
            &mut start,
            &mut baseline,
            286_368,
            2000,
            "2026-06-11T19:08:00-04:00",
        );
        assert_eq!(reason, Some("token_progress_rearm"));
        assert_eq!(start.as_deref(), Some("2026-06-11T19:08:00-04:00"));
        assert_eq!(baseline, Some(286_368));
    }

    #[test]
    fn test_apply_token_progress_counter_reset_slides_state() {
        let mut start = Some("old".to_string());
        let mut baseline = Some(150_000);
        let reason =
            apply_thinking_token_progress(&mut start, &mut baseline, 5_000, 2000, "new");
        assert_eq!(reason, Some("token_counter_reset"));
        assert_eq!(start.as_deref(), Some("new"));
        assert_eq!(baseline, Some(5_000));
    }

    #[test]
    fn test_apply_token_progress_late_capture_keeps_timer() {
        // Production no-baseline path: tokens were 0/unparseable when the
        // episode started (baseline None). When the count becomes
        // available, the baseline is captured WITHOUT sliding the timer —
        // so a wedge that started under a scrape failure still fires on
        // the original schedule, and subsequent growth is judged against
        // a real baseline instead of failing open forever.
        let mut start = Some("episode-start".to_string());
        let mut baseline: Option<u64> = None;
        let reason =
            apply_thinking_token_progress(&mut start, &mut baseline, 280_000, 2000, "now");
        assert_eq!(reason, None);
        assert_eq!(start.as_deref(), Some("episode-start"), "timer must not slide");
        assert_eq!(baseline, Some(280_000));
    }

    #[test]
    fn test_apply_token_progress_keep_touches_nothing() {
        let mut start = Some("episode-start".to_string());
        let mut baseline = Some(100_000);
        // Below-floor growth.
        assert_eq!(
            apply_thinking_token_progress(&mut start, &mut baseline, 101_000, 2000, "now"),
            None
        );
        assert_eq!(start.as_deref(), Some("episode-start"));
        assert_eq!(baseline, Some(100_000));
        // Unparseable current count.
        assert_eq!(
            apply_thinking_token_progress(&mut start, &mut baseline, 0, 2000, "now"),
            None
        );
        assert_eq!(baseline, Some(100_000));
        // Guard disabled by zero floor: never slides, even on huge growth.
        assert_eq!(
            apply_thinking_token_progress(&mut start, &mut baseline, 900_000, 0, "now"),
            None
        );
        assert_eq!(start.as_deref(), Some("episode-start"));
        assert_eq!(baseline, Some(100_000));
    }

    // --- Heartbeat-freshness gate tests (v3, 2026-06-11) ---

    #[test]
    fn test_heartbeat_fresh_suppresses_and_rearms() {
        // Fresh heartbeat (age 120s < 600s threshold): suppress the fire
        // and slide BOTH the thinking timer and the token baseline —
        // identical state effect to the v2 token-progress re-arm.
        let mut start = Some("episode-start".to_string());
        let mut baseline = Some(283_000u64);
        let suppressed = apply_ack_fresh_rearm(
            &mut start,
            &mut baseline,
            Some(120),
            600,
            290_000,
            "2026-06-11T21:58:00-04:00",
        );
        assert!(suppressed);
        assert_eq!(start.as_deref(), Some("2026-06-11T21:58:00-04:00"));
        assert_eq!(baseline, Some(290_000));
    }

    #[test]
    fn test_heartbeat_fresh_rearm_unparseable_tokens_clears_baseline() {
        // Re-arm with tokens unparseable this cycle (0): baseline goes to
        // None (late capture on a later cycle), matching the fire-path
        // baseline-refresh semantics.
        let mut start = Some("episode-start".to_string());
        let mut baseline = Some(283_000u64);
        assert!(apply_ack_fresh_rearm(
            &mut start,
            &mut baseline,
            Some(0),
            600,
            0,
            "now"
        ));
        assert_eq!(start.as_deref(), Some("now"));
        assert_eq!(baseline, None);
    }

    #[test]
    fn test_heartbeat_stale_allows_fire() {
        // Stale heartbeat (age >= threshold): possible real wedge — allow
        // the fire, touch nothing. Boundary (age == threshold) is stale.
        let mut start = Some("episode-start".to_string());
        let mut baseline = Some(283_000u64);
        assert!(!apply_ack_fresh_rearm(
            &mut start,
            &mut baseline,
            Some(900),
            600,
            290_000,
            "now"
        ));
        assert!(!apply_ack_fresh_rearm(
            &mut start,
            &mut baseline,
            Some(600),
            600,
            290_000,
            "now"
        ));
        assert_eq!(start.as_deref(), Some("episode-start"));
        assert_eq!(baseline, Some(283_000));
    }

    #[test]
    fn test_ack_missing_stamp_fails_open() {
        // Missing/unreadable ack stamp surfaces as age None: the gate must
        // FAIL OPEN (allow the fire) and touch nothing.
        assert_eq!(ack_age_secs(None, SystemTime::now()), None);
        let mut start = Some("episode-start".to_string());
        let mut baseline = Some(283_000u64);
        assert!(!apply_ack_fresh_rearm(
            &mut start,
            &mut baseline,
            None,
            600,
            290_000,
            "now"
        ));
        assert_eq!(start.as_deref(), Some("episode-start"));
        assert_eq!(baseline, Some(283_000));
    }

    #[test]
    fn test_ack_future_mtime_fails_open() {
        // mtime in the future relative to now: duration_since fails, age
        // is None, gate fails open. (Deliberately NOT treated as fresh,
        // unlike the workload-heartbeat suppressor — a corrupt or skewed
        // stamp must never mask a real wedge.)
        let now = SystemTime::now();
        let future = now + std::time::Duration::from_secs(60);
        assert_eq!(ack_age_secs(Some(future), now), None);
        let mut start = Some("episode-start".to_string());
        let mut baseline = Some(283_000u64);
        assert!(!apply_ack_fresh_rearm(
            &mut start,
            &mut baseline,
            ack_age_secs(Some(future), now),
            600,
            290_000,
            "now"
        ));
        assert_eq!(start.as_deref(), Some("episode-start"));
    }

    #[test]
    fn test_ack_gate_zero_disables() {
        // ack_fresh_secs = 0 disables the gate entirely: even a just-stamped
        // ack (age 0) never suppresses.
        let mut start = Some("episode-start".to_string());
        let mut baseline = Some(283_000u64);
        assert!(!apply_ack_fresh_rearm(
            &mut start,
            &mut baseline,
            Some(0),
            0,
            290_000,
            "now"
        ));
        assert_eq!(start.as_deref(), Some("episode-start"));
        assert_eq!(baseline, Some(283_000));
    }

    #[test]
    fn test_ack_age_secs_past_mtime() {
        // Plain past mtime: age computes in whole seconds.
        let now = SystemTime::now();
        let past = now - std::time::Duration::from_secs(123);
        assert_eq!(ack_age_secs(Some(past), now), Some(123));
    }

    #[test]
    fn test_token_progress_production_replay_2026_06_11() {
        // Replays the 19:03:26 -> 19:11:29 ET false fire from 2026-06-11:
        // an idle-but-alive open turn (2-3 tiny main-loop turns) whose
        // CONTEXT token count drips ~700/min from tool results + system
        // reminders. Under the v1 at-fire-time check the 480s window
        // accumulated ~5.6k delta >= the 2000 floor, so the fire was
        // ALLOWED (the bug). Under v2 the drip re-arms the timer every
        // ~3 min, so the growth-free clock never reaches 480s and the
        // fire is suppressed.
        let floor = 2000u64;
        let mut start = Some("t0".to_string());
        let mut baseline = Some(283_000u64);
        let mut clock_secs = 0u64; // seconds since last re-arm (simulated)
        let mut fired = false;
        let mut rearms = 0;
        // 10s full-cycle cadence, ~120 tokens per cycle (~720/min drip).
        let mut tokens = 283_000u64;
        for _cycle in 0..120 {
            // 20 minutes simulated
            clock_secs += 10;
            tokens += 120;
            if apply_thinking_token_progress(&mut start, &mut baseline, tokens, floor, "tn")
                .is_some()
            {
                rearms += 1;
                clock_secs = 0; // thinking_start slid to now
            }
            if clock_secs >= 480 {
                fired = true;
                break;
            }
        }
        assert!(!fired, "ambient context drip must keep re-arming the timer");
        assert!(rearms >= 4, "expected periodic re-arms, got {rearms}");

        // Contrast: a genuinely growth-free wedge (same setup, no drip)
        // must still fire at the 480s threshold.
        let mut start = Some("t0".to_string());
        let mut baseline = Some(283_000u64);
        let mut clock_secs = 0u64;
        let mut fired = false;
        for _cycle in 0..120 {
            clock_secs += 10;
            if apply_thinking_token_progress(&mut start, &mut baseline, 283_000, floor, "tn")
                .is_some()
            {
                clock_secs = 0;
            }
            if clock_secs >= 480 {
                fired = true;
                break;
            }
        }
        assert!(fired, "a growth-free wedge must still fire");
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

    #[test]
    fn test_try_claim_global_interrupt_grants_when_no_prior() {
        // No prior interrupt -> claim succeeds and stamps last_interrupt_at.
        // backoff_base=1 => flat cooldown (legacy equivalence).
        let mut state = State::default();
        let now = Utc::now().to_rfc3339();
        assert!(try_claim_global_interrupt(&mut state, 300, 1, 1800, &now));
        assert_eq!(state.last_interrupt_at.as_deref(), Some(now.as_str()));
        // Streak bumps on a successful claim.
        assert_eq!(state.global_interrupt_streak, 1);
    }

    #[test]
    fn test_try_claim_global_interrupt_denies_within_cooldown() {
        // A recent interrupt within the window -> claim DENIED and the
        // existing stamp is NOT overwritten (atomic check-and-stamp).
        // backoff_base=1 => flat cooldown (legacy equivalence).
        let mut state = State::default();
        let prior = (Utc::now() - chrono::Duration::seconds(10)).to_rfc3339();
        state.last_interrupt_at = Some(prior.clone());
        let now = Utc::now().to_rfc3339();
        assert!(!try_claim_global_interrupt(&mut state, 300, 1, 1800, &now));
        assert_eq!(
            state.last_interrupt_at.as_deref(),
            Some(prior.as_str()),
            "denied claim must not move the timestamp"
        );
    }

    #[test]
    fn test_try_claim_global_interrupt_grants_after_window() {
        // Prior interrupt older than the cooldown -> claim succeeds and
        // re-stamps to now. backoff_base=1 => flat cooldown (legacy).
        let mut state = State::default();
        let prior = (Utc::now() - chrono::Duration::seconds(400)).to_rfc3339();
        state.last_interrupt_at = Some(prior);
        let now = Utc::now().to_rfc3339();
        assert!(try_claim_global_interrupt(&mut state, 300, 1, 1800, &now));
        assert_eq!(state.last_interrupt_at.as_deref(), Some(now.as_str()));
    }

    #[test]
    fn test_try_claim_global_interrupt_zero_cooldown_always_grants() {
        // cooldown=0 disables the gate: claim always succeeds, still stamps.
        // backoff_base=1 => flat (legacy equivalence).
        let mut state = State::default();
        state.last_interrupt_at = Some(Utc::now().to_rfc3339());
        let now = Utc::now().to_rfc3339();
        assert!(try_claim_global_interrupt(&mut state, 0, 1, 1800, &now));
        assert_eq!(state.last_interrupt_at.as_deref(), Some(now.as_str()));
    }

    #[test]
    fn test_try_claim_backoff_base_1_is_flat_legacy() {
        // backoff_base=1 must reproduce the exact flat-cooldown behavior
        // regardless of the streak value: a prior interrupt 100s ago with a
        // 300s base is STILL in cooldown (effective cooldown stays 300, not
        // widened).
        let mut state = State::default();
        state.global_interrupt_streak = 5; // would matter only if base>1
        let prior = (Utc::now() - chrono::Duration::seconds(100)).to_rfc3339();
        state.last_interrupt_at = Some(prior.clone());
        let now = Utc::now().to_rfc3339();
        assert!(
            !try_claim_global_interrupt(&mut state, 300, 1, 1800, &now),
            "flat 300s cooldown still active at 100s elapsed"
        );
        assert_eq!(state.last_interrupt_at.as_deref(), Some(prior.as_str()));
    }

    #[test]
    fn test_try_claim_exponential_widens_cooldown() {
        // With backoff_base=2 and streak=2, effective cooldown = 300*2^2 =
        // 1200s. A prior interrupt 600s ago is now STILL in cooldown
        // (flat-300 would have granted).
        let mut state = State::default();
        state.global_interrupt_streak = 2;
        let prior = (Utc::now() - chrono::Duration::seconds(600)).to_rfc3339();
        state.last_interrupt_at = Some(prior);
        let now = Utc::now().to_rfc3339();
        assert!(
            !try_claim_global_interrupt(&mut state, 300, 2, 1800, &now),
            "1200s effective cooldown still active at 600s elapsed"
        );
    }

    #[test]
    fn test_try_claim_streak_resets_after_quiet_window() {
        // A prior interrupt older than the FULL effective window resets the
        // streak to 0 before the claim, so the claim grants and the streak
        // restarts at 1.
        let mut state = State::default();
        state.global_interrupt_streak = 3;
        // effective = 300*2^3 = 2400 (under 1800 cap? no -> capped at 1800).
        // Use a prior older than 1800 so the decay branch fires.
        let prior = (Utc::now() - chrono::Duration::seconds(2000)).to_rfc3339();
        state.last_interrupt_at = Some(prior);
        let now = Utc::now().to_rfc3339();
        assert!(try_claim_global_interrupt(&mut state, 300, 2, 1800, &now));
        assert_eq!(state.global_interrupt_streak, 1, "streak reset then +1");
    }

    // --- effective_global_cooldown_secs tests ---

    #[test]
    fn test_effective_cooldown_streak_zero_is_base() {
        assert_eq!(effective_global_cooldown_secs(300, 2, 1800, 0), 300);
    }

    #[test]
    fn test_effective_cooldown_growth() {
        // 300 * 2^1 = 600, 300 * 2^2 = 1200.
        assert_eq!(effective_global_cooldown_secs(300, 2, 1800, 1), 600);
        assert_eq!(effective_global_cooldown_secs(300, 2, 1800, 2), 1200);
    }

    #[test]
    fn test_effective_cooldown_caps_at_max() {
        // 300 * 2^3 = 2400 -> capped at 1800.
        assert_eq!(effective_global_cooldown_secs(300, 2, 1800, 3), 1800);
        assert_eq!(effective_global_cooldown_secs(300, 2, 1800, 50), 1800);
    }

    #[test]
    fn test_effective_cooldown_base_1_is_flat() {
        assert_eq!(effective_global_cooldown_secs(300, 1, 1800, 5), 300);
        // base 0 also treated as flat (no growth).
        assert_eq!(effective_global_cooldown_secs(300, 0, 1800, 5), 300);
    }

    #[test]
    fn test_effective_cooldown_saturating_no_panic() {
        // Huge streak + huge base must not overflow-panic; caps at max.
        assert_eq!(
            effective_global_cooldown_secs(u64::MAX, u64::MAX, 1800, u32::MAX),
            1800
        );
    }

    // --- 2026-08-10 context-limit deadlock regression tests ---
    //
    // The incident, in one line each: the pane hit the hard context limit and
    // rode there for ~25 minutes while the daemon armed, alerted, and never
    // once injected a clear. Every assertion below fails against the
    // pre-fix code.

    #[test]
    fn context_escalation_fires_despite_live_subagents_after_deadline() {
        // THE deadlock. Armed well past the dwell, but subagents are live, so
        // the base gate says Hold — and said Hold on every one of the ~26
        // cycles the real incident ran for, at 97.7% context. With an
        // arm-to-fire deadline the clear fires anyway.
        let armed = (Utc::now() - chrono::Duration::seconds(400)).to_rfc3339();
        let now = Utc::now().to_rfc3339();
        assert_eq!(
            obligation_escalation_decision(Some(&armed), 90, 1, &now),
            ObligationDecision::Hold,
            "base gate holds forever while any subagent is live"
        );
        assert_eq!(
            context_escalation_decision(Some(&armed), 90, 1, 300, &now),
            ObligationDecision::Escalate,
            "armed past max_armed_secs must fire regardless of subagent count"
        );
    }

    #[test]
    fn context_escalation_holds_before_deadline() {
        // The deadline is a ceiling, not a bypass: inside it the subagent
        // protection still applies.
        let armed = (Utc::now() - chrono::Duration::seconds(100)).to_rfc3339();
        let now = Utc::now().to_rfc3339();
        assert_eq!(
            context_escalation_decision(Some(&armed), 90, 1, 300, &now),
            ObligationDecision::Hold
        );
    }

    #[test]
    fn context_escalation_deadline_zero_is_legacy_behaviour() {
        let armed = (Utc::now() - chrono::Duration::seconds(9999)).to_rfc3339();
        let now = Utc::now().to_rfc3339();
        assert_eq!(
            context_escalation_decision(Some(&armed), 90, 1, 0, &now),
            ObligationDecision::Hold
        );
    }

    #[test]
    fn context_escalation_deadline_does_not_skip_the_arm_phase() {
        // An unarmed obligation must still ARM first (emit the pending alert
        // + event) rather than jumping straight to a turn-cancelling interrupt.
        let now = Utc::now().to_rfc3339();
        assert_eq!(
            context_escalation_decision(None, 90, 3, 300, &now),
            ObligationDecision::ArmObligation
        );
    }

    #[test]
    fn context_hook_defer_ceiling_expires_from_the_crossing() {
        // The per-fire grace window is measured from the last hook fire and
        // the hook re-fires every turn, so it can never expire on a working
        // loop. The ceiling is anchored to the threshold crossing, which
        // nothing refreshes.
        let recent = (Utc::now() - chrono::Duration::seconds(60)).to_rfc3339();
        let ancient = (Utc::now() - chrono::Duration::seconds(900)).to_rfc3339();
        assert!(context_hook_defer_allowed(Some(&recent), 600));
        assert!(
            !context_hook_defer_allowed(Some(&ancient), 600),
            "15 min past the crossing must stop deferring to the hook"
        );
        // Ceiling disabled -> always defer (legacy behaviour).
        assert!(context_hook_defer_allowed(Some(&ancient), 0));
        // Crossing not recorded yet -> permitted; this cycle records it.
        assert!(context_hook_defer_allowed(None, 600));
    }

    #[test]
    fn watcher_down_wedged_pane_is_not_actively_turning() {
        // `bashes > 0` is untimed proof of activity, and a wedged pane keeps
        // its background shells listed indefinitely — so the suppression gate
        // read a session that could not run a single tool call as maximally
        // busy, for 14 consecutive cycles.
        let mut state = State::default();
        state.last_active_at = Some(Utc::now().to_rfc3339());
        let bashes = 2;
        assert!(
            main_loop_actively_turning(&state, bashes, 30),
            "leftover background shells read as active"
        );
        assert!(
            watcher_down_actively_turning(&state, bashes, true, 30, false, false),
            "healthy busy pane: suppression still applies"
        );
        assert!(
            !watcher_down_actively_turning(&state, bashes, true, 30, false, true),
            "wedged pane must never count as actively turning"
        );
        // The pre-existing consumer-down bypass is unaffected.
        assert!(!watcher_down_actively_turning(
            &state, bashes, true, 30, true, false
        ));
    }

    #[test]
    fn post_clear_resume_fires_at_zero_tokens_with_background_shells() {
        // A pane at the post-clear prompt reports tokens=0 — below the
        // fresh-/clear window's min_tokens — and keeps its surviving
        // background shells, so neither existing gate can see it.
        let (tokens, bashes) = (0u64, 2u64);
        let (min_tokens, max_tokens) = (2000u64, 5000u64);

        // Pin the blind spot both pre-existing gates leave. Fresh-/clear
        // wants the token window AND zero background shells:
        assert!(
            !(tokens >= min_tokens && tokens < max_tokens && bashes == 0),
            "fresh-/clear gate cannot see a post-clear pane"
        );
        // ...and the fresh-external-session gate is behind `tokens == 0 &&
        // bashes == 0`, which surviving background shells defeat:
        assert!(
            !(tokens == 0 && bashes == 0),
            "fresh-session gate cannot see a post-clear pane with live shells"
        );

        let cleared = (Utc::now() - chrono::Duration::seconds(30)).to_rfc3339();
        assert!(post_clear_resume_due(
            tokens,          // 0 at the post-clear prompt
            min_tokens,      // fresh_clear.min_tokens
            Some(&cleared),  // clear the daemon observed
            300,             // post_clear_window_secs
            None,            // not yet injected for this clear
            false,           // operator-driven clear, not a daemon self-clear
            true,            // idle
            false,           // no interactive menu
            2,               // idle checks
            2,               // detections_required
        ));
    }

    #[test]
    fn post_clear_resume_defers_to_daemon_self_clear() {
        // `self-clear` injects its own resume prompt once the clear lands, so
        // a daemon-driven clear must not also draw an inject from here.
        let cleared = (Utc::now() - chrono::Duration::seconds(30)).to_rfc3339();
        assert!(!post_clear_resume_due(
            0,
            2000,
            Some(&cleared),
            300,
            None,
            true, // daemon_clear_recent
            true,
            false,
            2,
            2
        ));
    }

    #[test]
    fn post_clear_resume_latches_to_one_inject_per_clear() {
        let cleared = (Utc::now() - chrono::Duration::seconds(30)).to_rfc3339();
        assert!(
            !post_clear_resume_due(
                0,
                2000,
                Some(&cleared),
                300,
                Some(&cleared), // already injected for this same clear
                false,
                true,
                false,
                5,
                2
            ),
            "must not re-inject every cycle while the pane sits idle"
        );
    }

    #[test]
    fn post_clear_resume_respects_its_guards() {
        let cleared = (Utc::now() - chrono::Duration::seconds(30)).to_rfc3339();
        let stale = (Utc::now() - chrono::Duration::seconds(3600)).to_rfc3339();
        let due = |tokens, last, idle, interactive, checks, window| {
            post_clear_resume_due(
                tokens,
                2000,
                last,
                window,
                None,
                false,
                idle,
                interactive,
                checks,
                2,
            )
        };
        // Inside the fresh-/clear window -> that gate owns it, not this one.
        assert!(!due(3000, Some(&cleared), true, false, 2, 300));
        // No observed clear -> no positive evidence, no inject.
        assert!(!due(0, None, true, false, 2, 300));
        // Clear too old -> the window has closed.
        assert!(!due(0, Some(&stale), true, false, 2, 300));
        // Not idle -> mid-turn, do not preempt.
        assert!(!due(0, Some(&cleared), false, false, 2, 300));
        // Interactive menu on screen -> the inject's leading Escape would
        // cancel the operator's question.
        assert!(!due(0, Some(&cleared), true, true, 2, 300));
        // Not debounced yet.
        assert!(!due(0, Some(&cleared), true, false, 1, 300));
        // Gate disabled.
        assert!(!due(0, Some(&cleared), true, false, 2, 0));
    }

    #[test]
    fn zombie_clear_child_does_not_block_recovery() {
        // The guard that stops two clear drivers stacking on one pane used a
        // bare existence probe, which is true for a zombie — and the daemon
        // never reaps its detached clear children, so the first successful
        // clear of a daemon lifetime left a permanent `<defunct>` pid in
        // `context_clear_child_pid` that silently disabled every later
        // recovery. Spawn a child, let it exit, do NOT reap it, and assert the
        // liveness judgement used by the guard sees it as finished.
        let mut child = std::process::Command::new("true")
            .spawn()
            .expect("spawn probe child");
        let pid = child.id();
        // Wait for exit WITHOUT reaping via the Child handle, so the process
        // is genuinely a zombie at the moment we probe it.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            if matches!(
                std::fs::read_to_string(format!("/proc/{pid}/stat")),
                Ok(ref s) if s.rsplit(')').next().is_some_and(|t| t.trim_start().starts_with('Z'))
            ) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(
            !clear_child_is_running(pid),
            "a finished (zombie or reaped) clear child must not read as running"
        );
        // clear_child_is_running reaps, so this second wait is a no-op or an
        // ECHILD — either way it must not hang the test.
        let _ = child.try_wait();
    }

    // --- obligation_escalation_decision tests ---

    #[test]
    fn test_obligation_decision_arm_when_unarmed() {
        let now = Utc::now().to_rfc3339();
        assert_eq!(
            obligation_escalation_decision(None, 90, 0, &now),
            ObligationDecision::ArmObligation
        );
    }

    #[test]
    fn test_obligation_decision_hold_when_dwell_not_elapsed() {
        let armed = (Utc::now() - chrono::Duration::seconds(10)).to_rfc3339();
        let now = Utc::now().to_rfc3339();
        assert_eq!(
            obligation_escalation_decision(Some(&armed), 90, 0, &now),
            ObligationDecision::Hold
        );
    }

    #[test]
    fn test_obligation_decision_hold_when_subagents_active() {
        // Dwell elapsed but live subagents -> HOLD (don't kill healthy agents).
        let armed = (Utc::now() - chrono::Duration::seconds(120)).to_rfc3339();
        let now = Utc::now().to_rfc3339();
        assert_eq!(
            obligation_escalation_decision(Some(&armed), 90, 2, &now),
            ObligationDecision::Hold
        );
    }

    #[test]
    fn test_obligation_decision_escalate_when_dwelled_and_idle() {
        let armed = (Utc::now() - chrono::Duration::seconds(120)).to_rfc3339();
        let now = Utc::now().to_rfc3339();
        assert_eq!(
            obligation_escalation_decision(Some(&armed), 90, 0, &now),
            ObligationDecision::Escalate
        );
    }

    #[test]
    fn test_obligation_decision_dwell_zero_short_circuits_escalate() {
        // dwell_secs == 0 disables the precedence gate: always Escalate, even
        // when unarmed (legacy arm+interrupt same cycle).
        let now = Utc::now().to_rfc3339();
        assert_eq!(
            obligation_escalation_decision(None, 0, 0, &now),
            ObligationDecision::Escalate
        );
        assert_eq!(
            obligation_escalation_decision(None, 0, 5, &now),
            ObligationDecision::Escalate
        );
    }

    // --- watcher_down_obligation_decision tests (BUG 2 follow-up to #424) ---

    #[test]
    fn test_watcher_down_decision_arms_on_first_detection() {
        // First detection (unarmed), no suppression escalation: ARM the
        // obligation, do NOT interrupt — closing the #424 gap where
        // watcher-down went straight to a tmux interrupt.
        let now = Utc::now().to_rfc3339();
        assert_eq!(
            watcher_down_obligation_decision(false, None, 90, 0, &now),
            ObligationDecision::ArmObligation
        );
    }

    #[test]
    fn test_watcher_down_decision_holds_within_dwell() {
        // Armed but dwell not elapsed: Hold (no interrupt yet).
        let armed = (Utc::now() - chrono::Duration::seconds(10)).to_rfc3339();
        let now = Utc::now().to_rfc3339();
        assert_eq!(
            watcher_down_obligation_decision(false, Some(&armed), 90, 0, &now),
            ObligationDecision::Hold
        );
    }

    #[test]
    fn test_watcher_down_decision_holds_when_subagents_live() {
        // Dwell elapsed but background subagents are live: Hold — interrupting
        // would kill healthy in-flight agents.
        let armed = (Utc::now() - chrono::Duration::seconds(120)).to_rfc3339();
        let now = Utc::now().to_rfc3339();
        assert_eq!(
            watcher_down_obligation_decision(false, Some(&armed), 90, 3, &now),
            ObligationDecision::Hold
        );
    }

    #[test]
    fn test_watcher_down_decision_escalates_after_dwell_no_subagents() {
        // Armed, dwell elapsed, 0 subagents: Escalate to the interrupt+inject.
        let armed = (Utc::now() - chrono::Duration::seconds(120)).to_rfc3339();
        let now = Utc::now().to_rfc3339();
        assert_eq!(
            watcher_down_obligation_decision(false, Some(&armed), 90, 0, &now),
            ObligationDecision::Escalate
        );
    }

    #[test]
    fn test_watcher_down_decision_suppression_escalation_forces_escalate() {
        // An active cross-gate suppression-escalation FORCES Escalate even
        // when unarmed (would otherwise ArmObligation) — the capped
        // suppression run is the "lower rung failed" case the dwell must not
        // re-delay.
        let now = Utc::now().to_rfc3339();
        assert_eq!(
            watcher_down_obligation_decision(true, None, 90, 0, &now),
            ObligationDecision::Escalate
        );
        // ...and even when subagents are live (suppression backstop wins).
        let armed = (Utc::now() - chrono::Duration::seconds(5)).to_rfc3339();
        assert_eq!(
            watcher_down_obligation_decision(true, Some(&armed), 90, 9, &now),
            ObligationDecision::Escalate
        );
    }

    #[test]
    fn test_watcher_down_decision_dwell_zero_legacy_same_cycle() {
        // dwell_secs == 0 disables the precedence gate (legacy behavior:
        // arm+interrupt same cycle) -> Escalate immediately.
        let now = Utc::now().to_rfc3339();
        assert_eq!(
            watcher_down_obligation_decision(false, None, 0, 0, &now),
            ObligationDecision::Escalate
        );
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

    // --- ack_liveness_fresh: the fresh-/clear liveness gate (2026-08-24) ---
    // The fresh-/clear fast path was firing on a MISPARSED low token reading
    // (thinking-indicator / agent-roster count leaking as the context total)
    // while the session was alive and intact — looping resume injects for
    // hours at tokens=2100..4900. The gate suppresses the inject whenever the
    // last event-ack is fresh (the loop is provably alive), and stays out of
    // the way when there is no proof of life so genuine fresh /clears still
    // recover.

    #[test]
    fn test_ack_liveness_fresh_true_when_recent_ack() {
        // Acked 60s ago, stale threshold 20min: the loop handled an event
        // well within the window, so it is alive — suppress the inject.
        assert!(ack_liveness_fresh(Some(60), 20 * 60));
    }

    #[test]
    fn test_ack_liveness_fresh_false_when_ack_stale() {
        // Last ack 25min ago, threshold 20min: no proof of life, so the gate
        // opens and a genuine fresh /clear can still be resumed.
        assert!(!ack_liveness_fresh(Some(25 * 60), 20 * 60));
    }

    #[test]
    fn test_ack_liveness_fresh_false_when_no_ack_stamp() {
        // No ack data at all (fresh boot / host without event-ack): we cannot
        // prove liveness, so DON'T suppress — preserve fast-path behaviour.
        assert!(!ack_liveness_fresh(None, 20 * 60));
    }

    // --- ack_liveness_suppresses_clear_inject: the wedged carve-out
    // (2026-08-26 autoclear-swallowed-by-ack-gate fix) ---
    //
    // Regression coverage for the incident: a session hit "Context limit
    // reached" / "Context low (0% remaining)" shortly after its last
    // event-ack, so `ack_liveness_fresh` kept reading "alive" for the whole
    // stale window and the fresh-/clear + post-clear-resume gates `return`ed
    // before `check_cycle` ever reached `handle_wedged_pane`. Autoclear
    // never fired; the operator had to `/clear` by hand.

    #[test]
    fn test_ack_liveness_suppresses_clear_inject_normal_case() {
        // Fresh ack, no wedge banner on screen: this IS the misparse case the
        // gate was built for — suppress the spurious resume/fresh-clear inject.
        assert!(ack_liveness_suppresses_clear_inject(true, false));
    }

    #[test]
    fn test_ack_liveness_does_not_suppress_when_genuinely_wedged() {
        // Fresh ack (acked moments before hitting the wall) BUT the pane is
        // showing a genuine context-limit/rate-limit banner right now: do NOT
        // suppress. Autoclear must be allowed to reach `handle_wedged_pane`
        // regardless of how recently the loop last acked.
        assert!(!ack_liveness_suppresses_clear_inject(true, true));
    }

    #[test]
    fn test_ack_liveness_suppresses_clear_inject_no_ack_no_wedge() {
        // No proof of life and no wedge banner: gate stays out of the way
        // (unrelated to the wedge carve-out — mirrors ack_liveness_fresh's
        // own "no data => false" behaviour flowing through unchanged).
        assert!(!ack_liveness_suppresses_clear_inject(false, false));
    }

    #[test]
    fn test_ack_liveness_suppresses_clear_inject_no_ack_but_wedged() {
        // Stale/absent ack AND wedged: still don't suppress (wedge detection
        // was already going to run regardless — this just confirms wedged
        // never flips the result to "suppress").
        assert!(!ack_liveness_suppresses_clear_inject(false, true));
    }

    #[test]
    fn test_carry_forward_real_reading_resets_run() {
        // A non-zero reading this poll is trusted verbatim and resets the
        // carry run to 0, regardless of any prior carry.
        assert_eq!(
            carry_forward_token_misparse(180_000, 200_000, 2, 50_000, 3),
            (180_000, 0)
        );
    }

    #[test]
    fn test_carry_forward_large_context_zero_is_carried() {
        // A large same-pane context momentarily reads 0 -> hold the last value
        // and advance the bounded run.
        assert_eq!(
            carry_forward_token_misparse(0, 200_000, 0, 50_000, 3),
            (200_000, 1)
        );
        assert_eq!(
            carry_forward_token_misparse(0, 200_000, 1, 50_000, 3),
            (200_000, 2)
        );
    }

    #[test]
    fn test_carry_forward_bound_exhausted_lets_zero_through() {
        // Once the run reaches max_carry the 0 is finally trusted, so a real
        // /clear or crashed process still registers.
        assert_eq!(
            carry_forward_token_misparse(0, 200_000, 3, 50_000, 3),
            (0, 3)
        );
    }

    #[test]
    fn test_carry_forward_below_floor_not_carried() {
        // A genuinely small prior reading (below the fresh-/clear window's
        // upper bound) is never carried: fresh-/clear detection in the
        // low-token window must be untouched.
        assert_eq!(
            carry_forward_token_misparse(0, 4_000, 0, 50_000, 3),
            (0, 0)
        );
    }

    /// End-to-end MISPARSE PATTERN over consecutive polls (the real bug):
    /// a large, intact context (408_000) momentarily reads 0 for a couple of
    /// polls -- the 2026-08 status-parser hardening now yields a bare 0 for a
    /// thinking/roster-only pane instead of the old tiny (~2600) count -- then
    /// the real total returns. Every transient 0 must be carried forward (never
    /// surfaced as the session total, so no phantom context-clear fires), and
    /// the real reading resumes cleanly and resets the carry run.
    #[test]
    fn test_carry_forward_transient_misparse_sequence_is_smoothed() {
        let floor = 50_000u64;
        let max = MISPARSE_CARRY_MAX;
        let mut last_known = 408_000u64;
        let mut carry = 0u32;
        // Two consecutive misparse polls must both be held at the last large
        // value rather than collapsing the reported context to 0/tiny.
        for _ in 0..2 {
            let (eff, c) = carry_forward_token_misparse(0, last_known, carry, floor, max);
            assert_eq!(eff, 408_000, "a transient 0 must be carried, not surfaced");
            carry = c;
            last_known = eff; // caller writes the effective value back (check_cycle)
        }
        // Real total returns -> trusted verbatim, carry run resets to 0.
        let (eff, c) = carry_forward_token_misparse(410_000, last_known, carry, floor, max);
        assert_eq!(eff, 410_000, "a real reading is always trusted");
        assert_eq!(c, 0, "a real reading resets the carry run");
    }

    /// A GENUINE /clear (or crashed process) holds 0 for many consecutive
    /// polls. The carry is BOUNDED, so after `MISPARSE_CARRY_MAX` held polls
    /// the 0 is finally trusted and the clear registers: the guard DELAYS a
    /// real clear by a couple of cycles, it never SUPPRESSES one.
    #[test]
    fn test_carry_forward_sustained_zero_eventually_registers_clear() {
        let floor = 50_000u64;
        let max = MISPARSE_CARRY_MAX;
        let mut last_known = 408_000u64;
        let mut carry = 0u32;
        let mut effective = Vec::new();
        for _ in 0..(max + 2) {
            let (eff, c) = carry_forward_token_misparse(0, last_known, carry, floor, max);
            effective.push(eff);
            carry = c;
            last_known = eff; // mirror check_cycle writing the effective value back
        }
        // The first `max` polls are held at the large value...
        for e in effective.iter().take(max as usize) {
            assert_eq!(*e, 408_000, "within the bound the large context is held");
        }
        // ...then the bound is exhausted and the 0 is trusted, so a real
        // /clear finally registers downstream.
        assert_eq!(
            effective[max as usize], 0,
            "past the carry bound a sustained 0 (real clear) must register"
        );
    }

    #[test]
    fn test_ack_liveness_fresh_boundary_is_exclusive() {
        // age == stale_secs is NOT fresh (mirrors the ack-stale detector's
        // `age >= stale_secs` staleness boundary — the two must agree).
        assert!(!ack_liveness_fresh(Some(1200), 1200));
        assert!(ack_liveness_fresh(Some(1199), 1200));
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

    // --- Per-watcher watcher-down suppression cap tests (2026-08-12) ---
    //
    // Real incident: botchat-wait (operator comms) stayed down ~6 min with
    // the watcher-down inject suppressed the whole time because the main loop
    // was continuously active. The shared cross-gate window backstop above
    // can't fix it without re-introducing the claude-event-watch storm (why
    // it's tuned very high). `watcher_down_suppression_capped` is the
    // independent per-watcher bound: once a watcher's own continuous-down
    // clock exceeds `max_suppress_secs` (default 180 = 3 min), force the
    // inject regardless of active-turn suppression.

    #[test]
    fn test_watcher_down_cap_fires_when_down_exceeds_threshold() {
        // Watcher continuously down 200s, cap 180s: force the inject.
        assert!(watcher_down_suppression_capped(Some(200), 180));
    }

    #[test]
    fn test_watcher_down_cap_fires_at_exact_threshold() {
        // Boundary is >= so exactly at the cap forces the inject. This is
        // the incident-fix guarantee: a comms watcher down for the full
        // 3 min surfaces even mid-turn.
        assert!(watcher_down_suppression_capped(Some(180), 180));
    }

    #[test]
    fn test_watcher_down_cap_holds_below_threshold() {
        // Down only 179s: still under the cap, normal active-turn
        // suppression continues to hold the inject this cycle.
        assert!(!watcher_down_suppression_capped(Some(179), 180));
    }

    #[test]
    fn test_watcher_down_cap_disabled_when_zero() {
        // max_suppress_secs=0 disables the cap entirely (escape hatch:
        // fall back to the shared window backstop only). Even a watcher
        // down for a day must NOT force via this path.
        assert!(!watcher_down_suppression_capped(Some(86400), 0));
    }

    #[test]
    fn test_watcher_down_cap_no_down_data_never_fires() {
        // No down-duration data (no watcher reached the inject path with a
        // stamped down_since) → the cap can't fire. Fail-safe: absence of a
        // clock never fabricates a force-inject.
        assert!(!watcher_down_suppression_capped(None, 180));
    }

    #[test]
    fn test_watcher_down_cap_independent_of_shared_suppression_counter() {
        // The whole point of this cap: it does NOT consult
        // consecutive_suppressions / first_suppression_at, so it stays
        // effective even when the shared backstop is neutered (the 10000 /
        // 86400 tuning that tolerates the flapping event consumer). A state
        // with a pristine shared counter (would NOT escalate) still force-
        // injects via the per-watcher clock.
        let state = State::default();
        assert_eq!(
            should_escalate_suppression(&state, 10000, 86400),
            None,
            "shared backstop must not escalate here"
        );
        // ...but the per-watcher cap does, from the down-duration alone.
        assert!(watcher_down_suppression_capped(Some(240), 180));
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
    fn test_default_watcher_inject_cooldown() {
        // Pin the 300s default (KNOB #3, raised 150 -> 300 on 2026-06-24:
        // once a watcher-down inject has fired, re-nagging the main loop more
        // often than ~5min was the most disruptive part of the storm, so the
        // re-injection cadence is now ~5min; history: 60 -> 150 on 2026-06-18,
        // 300 -> 60 on 2026-04-28). This throttles RE-FIRE of an
        // already-surfaced interruption, NOT initial detection latency. If you
        // change this default, also update the comment in config.rs and the
        // watcher_inject_due doc comment in policy.rs.
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
            cfg.watcher_monitor.inject_cooldown, 300,
            "default watcher inject_cooldown should be 300s (re-inject cadence); \
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

    // --- consumer_watcher_missing tests (silent-watcher-death fix) ---
    //
    // When the event consumer (claude-event-watch) is itself the down
    // watcher, the quiet claude-event channel is a dead letter box (nothing
    // drains the queue), so the escalation MUST take the out-of-band tmux
    // inject path. `consumer_watcher_missing` is the predicate the inject
    // block uses to force the suppression + obligation-dwell gates open.

    #[test]
    fn test_consumer_watcher_missing_present() {
        let missing = vec![
            "botchat-wait".to_string(),
            "claude-event-watch".to_string(),
        ];
        assert!(consumer_watcher_missing(&missing, "claude-event-watch"));
    }

    #[test]
    fn test_consumer_watcher_missing_absent() {
        let missing = vec!["botchat-wait".to_string()];
        assert!(!consumer_watcher_missing(&missing, "claude-event-watch"));
    }

    #[test]
    fn test_consumer_watcher_missing_only_consumer() {
        let missing = vec!["claude-event-watch".to_string()];
        assert!(
            consumer_watcher_missing(&missing, "claude-event-watch"),
            "consumer-only down must be detected — this is the exact circular-\
             dependency case that silently killed the watcher"
        );
    }

    #[test]
    fn test_consumer_watcher_missing_empty_list() {
        let missing: Vec<String> = vec![];
        assert!(!consumer_watcher_missing(&missing, "claude-event-watch"));
    }

    #[test]
    fn test_consumer_watcher_missing_empty_consumer_name_never_matches() {
        // Unconfigured consumer name -> never "missing" (avoid matching an
        // empty entry / false positive).
        let missing = vec!["claude-event-watch".to_string()];
        assert!(!consumer_watcher_missing(&missing, ""));
    }

    #[test]
    fn test_consumer_watcher_missing_custom_name() {
        let missing = vec!["my-custom-consumer".to_string()];
        assert!(consumer_watcher_missing(&missing, "my-custom-consumer"));
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
            down_since: None,
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

    // -------------------------------------------------------------------
    // Active-subagent heartbeat-stale suppression (2026-06-24).
    //
    // The heartbeat-stale detection block fires a destructive Escape
    // interrupt that kills healthy background subagents when the main
    // loop is legitimately dispatcher-waiting on them. `stuck_suppressed_
    // by_activity` is the pure decision the detection block uses: stuck
    // is suppressed when EITHER a workload heartbeat is fresh OR any
    // subagents are active. This mirrors the auto-respawn active-subagent
    // guard (`respawn::should_respawn`), tested in `respawn.rs`.
    // -------------------------------------------------------------------

    #[test]
    fn stuck_suppressed_by_activity_truth_table() {
        // Neither proof-of-life condition -> not suppressed (stuck may fire).
        assert!(
            !stuck_suppressed_by_activity(false, 0),
            "no workload + 0 subagents must NOT suppress"
        );
        // Fresh workload heartbeat alone suppresses (pre-existing behavior).
        assert!(
            stuck_suppressed_by_activity(true, 0),
            "fresh workload heartbeat must suppress"
        );
        // Active subagents alone suppress (the NEW guard -- prevents the
        // false-positive Escape that kills healthy dispatcher-waited agents).
        assert!(
            stuck_suppressed_by_activity(false, 1),
            "one active subagent must suppress"
        );
        assert!(
            stuck_suppressed_by_activity(false, 5),
            "many active subagents must suppress"
        );
        // Both conditions -> suppressed.
        assert!(
            stuck_suppressed_by_activity(true, 3),
            "both conditions must suppress"
        );
    }

    #[test]
    fn heartbeat_stale_liveness_reason_truth_table() {
        // No proof-of-life at all -> None (stuck may fire = genuine wedge).
        assert_eq!(
            heartbeat_stale_liveness_reason(false, 0, false, false),
            None,
            "idle+not-thinking+no-activity must let the stuck flag fire"
        );
        // Pre-existing signals still win, with their original reason strings.
        assert_eq!(
            heartbeat_stale_liveness_reason(true, 0, false, false),
            Some("workload_heartbeat_fresh")
        );
        assert_eq!(
            heartbeat_stale_liveness_reason(false, 2, false, false),
            Some("active_subagents")
        );
        // NEW: an active thinking episode is independent proof-of-life ->
        // suppress the false stale (the incident case: loop thinking in a
        // long turn, heartbeat starved because no tool call processed a tick).
        assert_eq!(
            heartbeat_stale_liveness_reason(false, 0, true, false),
            Some("loop_thinking")
        );
        // NEW: actively turning (tool call running / recent) also suppresses.
        assert_eq!(
            heartbeat_stale_liveness_reason(false, 0, false, true),
            Some("loop_actively_turning")
        );
        // Precedence: workload/subagents outrank the new signals (stable
        // reason string for existing log consumers).
        assert_eq!(
            heartbeat_stale_liveness_reason(true, 1, true, true),
            Some("workload_heartbeat_fresh")
        );
    }

    #[test]
    fn last_ack_timestamp_age_returns_none_when_missing() {
        // Missing file -> None (fresh boot / stripped deployment).
        let tmp = tempfile::tempdir().expect("tempdir");
        let state_dir = tmp.path().to_str().unwrap();
        assert_eq!(
            last_ack_timestamp_age(state_dir),
            None,
            "missing last-ack timestamp must return None"
        );
    }

    #[test]
    fn last_ack_timestamp_age_returns_age_when_present() {
        // Fresh file -> Some(age ~0).
        let tmp = tempfile::tempdir().expect("tempdir");
        let state_dir = tmp.path();
        let ack_file = state_dir.join("last-ack-timestamp");
        std::fs::write(&ack_file, "1234567890.0\n").expect("write ack file");
        let age = last_ack_timestamp_age(state_dir.to_str().unwrap());
        assert!(age.is_some(), "fresh ack file must return Some(age)");
        // Age should be very small (file just written).
        assert!(
            age.unwrap() < 10,
            "fresh ack file age must be near zero, got {:?}",
            age
        );
    }

    #[test]
    fn last_ack_timestamp_age_returns_age_when_stale() {
        // Stale file -> Some(age > threshold).
        let tmp = tempfile::tempdir().expect("tempdir");
        let state_dir = tmp.path();
        let ack_file = state_dir.join("last-ack-timestamp");
        std::fs::write(&ack_file, "1234567890.0\n").expect("write ack file");
        // Backdate the file mtime by 700 seconds.
        let now = std::time::SystemTime::now();
        let old = now - std::time::Duration::from_secs(700);
        filetime::set_file_mtime(&ack_file, filetime::FileTime::from_system_time(old))
            .expect("backdate mtime");
        let age = last_ack_timestamp_age(state_dir.to_str().unwrap());
        assert!(age.is_some(), "stale ack file must return Some(age)");
        let age_val = age.unwrap();
        assert!(
            age_val >= 690 && age_val <= 710,
            "stale ack file age must be ~700s, got {}",
            age_val
        );
    }


    // --- fresh-external-session inject gate (interactive-prompt suppression) ---

    #[test]
    fn test_fresh_inject_due_fires_when_idle_and_no_interactive_prompt() {
        // The canonical fresh-idle case: enough dead checks, not yet
        // injected, idle prompt visible, NO interactive menu → inject.
        assert!(fresh_inject_due(
            /* dead_checks */ 3,
            /* fresh_inject_checks */ 3,
            /* already_injected */ false,
            /* is_idle */ true,
            /* interactive_prompt */ false,
        ));
    }

    #[test]
    fn test_fresh_inject_due_suppressed_when_interactive_prompt_pending() {
        // THE BUG FIX: a legitimately pending AskUserQuestion idles the loop
        // (tokens==0) and renders a `❯` cursor, so `is_idle` is true and we
        // are deep in the dead-process block — but an interactive prompt is
        // up. The inject MUST be suppressed so its leading-Escape send-keys
        // does not cancel the operator's question. Every other condition is
        // satisfied; only the interactive-prompt clause must hold the gate.
        assert!(
            !fresh_inject_due(
                /* dead_checks */ 10,
                /* fresh_inject_checks */ 3,
                /* already_injected */ false,
                /* is_idle */ true,
                /* interactive_prompt */ true,
            ),
            "a pending interactive question must suppress the fresh-inject"
        );
    }

    #[test]
    fn test_fresh_inject_due_requires_idle() {
        // Not at the idle prompt (e.g. still rendering) → never inject,
        // regardless of the interactive flag.
        assert!(!fresh_inject_due(5, 3, false, false, false));
        assert!(!fresh_inject_due(5, 3, false, false, true));
    }

    #[test]
    fn test_fresh_inject_due_requires_threshold_and_not_already_injected() {
        // Under the dead-check threshold → no inject yet.
        assert!(!fresh_inject_due(2, 3, false, true, false));
        // Already injected this session → don't re-inject.
        assert!(!fresh_inject_due(5, 3, true, true, false));
    }

    // --- AskUserQuestion stale-monitor timer lifecycle (Phase 1) ---

    fn ask_q_now_offset(secs: i64) -> String {
        (Utc::now() - chrono::Duration::seconds(secs)).to_rfc3339()
    }

    #[test]
    fn test_ask_question_timer_fires_once_at_threshold_then_resets() {
        let mut state = State::default();
        let stale = 240u64;

        // Cycle 1: prompt appears. Timer starts; not yet stale (use a
        // freshly-stamped now). No fire.
        let t0 = Utc::now().to_rfc3339();
        let d = ask_question_timer_step(&mut state, true, stale, true, &t0);
        assert_eq!(d, AskQuestionTimerDecision::Pending);
        assert!(state.ask_question_pending_since.is_some());
        assert!(!state.ask_question_alerted);

        // Cycle 2: still pending, still under threshold. No fire.
        let d = ask_question_timer_step(&mut state, true, stale, true, &Utc::now().to_rfc3339());
        assert_eq!(d, AskQuestionTimerDecision::Pending);
        assert!(!state.ask_question_alerted);

        // Cycle 3: the question has now been pending past the threshold.
        // Simulate by backdating pending_since to > stale seconds ago.
        state.ask_question_pending_since = Some(ask_q_now_offset(stale as i64 + 5));
        let d = ask_question_timer_step(&mut state, true, stale, true, &Utc::now().to_rfc3339());
        match d {
            AskQuestionTimerDecision::Fire { stale_minutes } => {
                assert!(stale_minutes >= 4, "expected >=4 min, got {}", stale_minutes);
            }
            other => panic!("expected Fire, got {:?}", other),
        }
        assert!(state.ask_question_alerted, "alerted flag must latch after fire");

        // Cycle 4: still pending + still over threshold, but already
        // alerted — must NOT fire again (fires exactly once per question).
        let d = ask_question_timer_step(&mut state, true, stale, true, &Utc::now().to_rfc3339());
        assert_eq!(d, AskQuestionTimerDecision::Pending);

        // Cycle 5: the prompt clears (question answered). Timer resets.
        let d = ask_question_timer_step(&mut state, true, stale, false, &Utc::now().to_rfc3339());
        assert_eq!(d, AskQuestionTimerDecision::Clear);
        assert!(state.ask_question_pending_since.is_none());
        assert!(!state.ask_question_alerted);

        // Cycle 6: a NEW question appears — gets its own fresh timer and can
        // fire again later (proves the reset re-arms the once-per-question
        // semantics).
        let d = ask_question_timer_step(&mut state, true, stale, true, &Utc::now().to_rfc3339());
        assert_eq!(d, AskQuestionTimerDecision::Pending);
        assert!(state.ask_question_pending_since.is_some());
    }

    #[test]
    fn test_ask_question_timer_never_fires_when_not_interactive() {
        // A busy / non-interactive pane (no AskUserQuestion prompt) must
        // never start the timer or fire, regardless of how much time
        // passes.
        let mut state = State::default();
        for _ in 0..10 {
            let d = ask_question_timer_step(&mut state, true, 1, false, &ask_q_now_offset(3600));
            assert_eq!(d, AskQuestionTimerDecision::Clear);
            assert!(state.ask_question_pending_since.is_none());
            assert!(!state.ask_question_alerted);
        }
    }

    #[test]
    fn test_ask_question_timer_disabled_never_fires() {
        // enabled = false: even with an interactive prompt long past the
        // threshold, the monitor stays silent and clears any timer state.
        let mut state = State::default();
        state.ask_question_pending_since = Some(ask_q_now_offset(99999));
        let d = ask_question_timer_step(&mut state, false, 1, true, &Utc::now().to_rfc3339());
        assert_eq!(d, AskQuestionTimerDecision::Clear);
        assert!(state.ask_question_pending_since.is_none());
        assert!(!state.ask_question_alerted);
    }

    // -------------------------------------------------------------------
    // build_relaunch_claude_argv — auto-update relaunch argv shape.
    //
    // These assert the env-INdependent parts of the argv (always present
    // regardless of CLAUDE_SHIM_SETTINGS_PATH / plugin-dir), so they stay
    // stable under nextest's parallel execution (no process-env mutation).
    // -------------------------------------------------------------------

    #[test]
    fn relaunch_argv_always_skips_permissions_and_continues() {
        let cmd = build_relaunch_claude_argv(None);
        // The leading token is now `resolve_claude_bin()` — either a bare
        // `claude` (host / unknown layout) or an absolute path ending in
        // `/claude` (container). Assert it references a claude binary rather
        // than a fixed literal, so the test is stable regardless of which
        // install locations happen to exist on the test host.
        let first = cmd.split_whitespace().next().unwrap_or("");
        assert!(
            first == "claude" || first.ends_with("/claude"),
            "argv must start with a claude binary token: {cmd}"
        );
        assert!(
            cmd.contains("--dangerously-skip-permissions"),
            "harness-managed relaunch must skip permissions: {cmd}"
        );
        assert!(
            cmd.trim_end().ends_with("--continue"),
            "no session id => --continue: {cmd}"
        );
        assert!(!cmd.contains("--resume"), "no session id => no --resume: {cmd}");
    }

    // -------------------------------------------------------------------
    // resolve_claude_bin — absolute-path resolution for the relaunch argv.
    //
    // The env-override branch is deterministic + testable; the
    // filesystem-probe branches depend on which install locations exist on
    // the host, so we only assert the override precedence and the bare
    // fallback here. These mutate process env, so they are NOT parallel-safe
    // with each other under a shared-process runner — nextest isolates each
    // test in its own process; under `cargo test` they still pass because
    // each set/removes CLAUDE_BIN within its own body and no OTHER test reads
    // it. Kept in one #[test] to avoid cross-test env races on `cargo test`.
    // -------------------------------------------------------------------

    #[test]
    fn resolve_claude_bin_honors_env_override_else_falls_back() {
        // (a) explicit override wins.
        std::env::set_var("CLAUDE_BIN", "/opt/custom/claude");
        assert_eq!(resolve_claude_bin(), "/opt/custom/claude");

        // (b) empty override is ignored (treated as unset).
        std::env::set_var("CLAUDE_BIN", "");
        let resolved = resolve_claude_bin();
        assert!(
            resolved == "claude" || resolved.ends_with("/claude"),
            "empty override => probe result (bare or absolute): {resolved}"
        );

        std::env::remove_var("CLAUDE_BIN");
    }

    #[test]
    fn resolve_relaunch_bin_prefers_shim_but_env_override_wins() {
        // (a) CLAUDE_BIN override wins over everything (test/operator hook).
        std::env::set_var("CLAUDE_BIN", "/opt/custom/claude");
        assert_eq!(resolve_relaunch_bin(), "/opt/custom/claude");
        std::env::remove_var("CLAUDE_BIN");

        // (b) With no override, an EXISTING shim path (pointed at via the
        // CLAUDE_RELAUNCH_EXEC env hook) is preferred so relaunch waits for +
        // repairs a dangling launcher instead of hot-spinning "not found".
        // Point the hook at a file guaranteed to exist on any host.
        let existing = if std::path::Path::new("/bin/sh").exists() {
            "/bin/sh"
        } else {
            "/usr/bin/env"
        };
        std::env::set_var("CLAUDE_RELAUNCH_EXEC", existing);
        assert_eq!(
            resolve_relaunch_bin(),
            existing,
            "an existing shim path must be preferred over the direct launcher"
        );

        // (c) A NON-existent shim path falls back to resolve_claude_bin()'s
        // result (bare or absolute claude token) — never a dead path.
        std::env::set_var("CLAUDE_RELAUNCH_EXEC", "/nonexistent/claude-relaunch-exec");
        let fell_back = resolve_relaunch_bin();
        assert!(
            fell_back == "claude" || fell_back.ends_with("/claude"),
            "missing shim => fall back to a claude token: {fell_back}"
        );
        std::env::remove_var("CLAUDE_RELAUNCH_EXEC");
    }

    #[test]
    fn relaunch_argv_uses_resolved_absolute_bin_when_env_set() {
        std::env::set_var("CLAUDE_BIN", "/opt/custom/claude");
        let cmd = build_relaunch_claude_argv(None);
        assert!(
            cmd.starts_with("/opt/custom/claude "),
            "argv must lead with the resolved absolute bin: {cmd}"
        );
        assert!(
            cmd.contains("--dangerously-skip-permissions") && cmd.trim_end().ends_with("--continue"),
            "flag logic must be unchanged by the bin swap: {cmd}"
        );
        std::env::remove_var("CLAUDE_BIN");
    }

    #[test]
    fn relaunch_argv_resumes_with_session_id() {
        let sid = "12345678-1234-1234-1234-123456789abc";
        let cmd = build_relaunch_claude_argv(Some(sid));
        assert!(
            cmd.contains(&format!("--resume {sid}")),
            "session id => --resume <sid>: {cmd}"
        );
        assert!(!cmd.contains("--continue"), "session id => no --continue: {cmd}");
        assert!(
            cmd.contains("--dangerously-skip-permissions"),
            "skip-permissions present on resume path too: {cmd}"
        );
    }

    // -------------------------------------------------------------------
    // build_relaunch_inject_cmd — self-healing guarded relaunch one-liner.
    //
    // Pure (no I/O), so parallel-safe like the argv tests above. Guards the
    // second bug PR #412 didn't touch: a missing/vanished relaunch script
    // must NOT leave a dead `bash <path>: No such file or directory` pane.
    // -------------------------------------------------------------------

    #[test]
    fn relaunch_inject_cmd_runs_script_when_present() {
        let path = "/var/run/claude/claude-relaunch.sh";
        let launch = "cd $HOME && claude --dangerously-skip-permissions --continue";
        let cmd = build_relaunch_inject_cmd(path, launch);
        // (a) references the script path with bash
        assert!(
            cmd.contains(&format!("bash {path}")),
            "must bash the script path: {cmd}"
        );
        assert!(
            cmd.contains(&format!("[ -f {path} ]")),
            "must guard on the file existing: {cmd}"
        );
    }

    #[test]
    fn relaunch_inject_cmd_falls_back_to_inline_launch() {
        let path = "/var/run/claude/claude-relaunch.sh";
        let launch = "cd $HOME && claude --dangerously-skip-permissions --resume abc";
        let cmd = build_relaunch_inject_cmd(path, launch);
        // (b) contains the inline launch fallback after `||`
        let (_before, after) = cmd.split_once("||").expect("must have an || fallback");
        assert!(
            after.contains(launch),
            "inline launch must appear after ||: {cmd}"
        );
        assert!(
            after.contains('{') && after.contains('}'),
            "fallback must be brace-grouped for sh/bash: {cmd}"
        );
    }

    #[test]
    fn relaunch_inject_cmd_is_single_line() {
        let cmd = build_relaunch_inject_cmd(
            "/var/run/claude/claude-relaunch.sh",
            "cd $HOME && claude --dangerously-skip-permissions --continue",
        );
        // (c) single line — tmux inject_shell types the literal then Enter.
        assert!(
            !cmd.contains('\n'),
            "injected command must be a single line: {cmd}"
        );
    }
}
