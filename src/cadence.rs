//! Daemon-emitted cadence signals: `keepalive` and `memory-reminder`.
//!
//! The claude-watch daemon is already a long-running monitor loop — the
//! natural place to source periodic "cadence" signals for the main loop.
//! Previously these were produced by a separate self-rescheduling
//! background task that the main loop had to keep restarting every cycle
//! (a treadmill). Moving the *cadence source* into the daemon removes that
//! restart churn: the daemon ticks on its own monotonic clock.
//!
//! 1. `keepalive` — a POKE FOR QUIET PERIODS, not a schedule. The tracker
//!    ticks every [`KEEPALIVE_INTERVAL_SECS`] (300s, 5 min), but the daemon
//!    only EMITS the event when the main loop has not acked anything in that
//!    window. Liveness is the age of the last ack of ANY event (`event-ack`
//!    stamps `last-ack-timestamp` on every ack, batch acks included), so a
//!    loop that is busy handling real events never sees a keepalive at all.
//!    The event exists solely so an IDLE loop still has something to ack
//!    before [`crate::config::AckConfig::stale_minutes`] elapses.
//!
//!    (Renamed from `heartbeat-tick` 2026-08-22. The old tag is still
//!    accepted by consumers for one release — see [`KEEPALIVE_TAG`].)
//!
//! 2. `memory-reminder` — every [`MEMORY_REMINDER_INTERVAL_SECS`] (30min),
//!    carrying the action checklist text ([`MEMORY_REMINDER_CHECKLIST`]).
//!    Written to the event queue (`~/claude-events/`) as an AMBIENT
//!    `claude-watch/memory-reminder` event, surfaced via the next
//!    `UserPromptSubmit`. Memory hygiene is not urgent enough to justify a
//!    mid-generation tmux-inject interruption, so it lives at the lowest
//!    (event) tier of the alerting hierarchy.
//!
//! ## Delivery choice: event queue vs. tmux-inject
//!
//! Writing JSON files to `~/claude-events/` can, under load, contribute to a
//! watcher-restart treadmill: `claude-event-watch` fires on a new file,
//! drains it, exits; the watcher-monitor restarts it; if another event has
//! already landed, repeat. That treadmill is driven by event *bursts* during
//! active threads — not by a single steady periodic signal. And a keepalive
//! is emitted ONLY when the bus has been quiet for the whole interval, which
//! is precisely when a restart cycle costs nothing.
//!
//! ## Why the daemon must NOT ack on the main loop's behalf
//!
//! The last-ack timestamp's entire value is that the *main loop* writes it:
//! a wedged loop stops acking, the timestamp goes stale, and the daemon's
//! stale-detection fires a nudge. If the daemon stamped it directly it would
//! stay fresh even while the loop is dead, defeating wedge detection. This
//! module never writes any liveness state.
//!
//! ## Config reload does not re-arm the timers
//!
//! Both timers fire on the daemon's FIRST loop pass — that startup keepalive
//! + reminder is deliberate (see [`CadenceTracker`]). A config RELOAD is not
//! a start: the daemon calls [`CadenceTracker::apply_intervals`], which swaps
//! the intervals but preserves the last-fired instants, so N reloads inside
//! an interval emit nothing and a shortened interval is measured from the
//! real last emission. Rebuilding the tracker instead would reset both timers
//! to "never fired" and emit a full set of events per reload — the 2026-08-22
//! regression, where seven config saves in 52 seconds produced seven
//! `memory-reminder` events against a 30-minute interval.
//!
//! ## Cadence decision is pure
//!
//! [`CadenceTracker`] holds the monotonic instant of the last emission for
//! each timer and decides, given "now", whether each timer is due. It is a
//! pure value type (no I/O), so the interval logic is unit-tested directly.
//! The daemon owns one `CadenceTracker`, calls [`CadenceTracker::due`] each
//! loop pass, and acts on whichever signals are due. Whether a due keepalive
//! is actually EMITTED is a separate, I/O-dependent decision the daemon makes
//! from the last-ack age (see `crate::policy::last_ack_timestamp_age`).

use std::time::{Duration, Instant};

/// Tick interval for the `keepalive` probe. 210 seconds (3.5 min).
///
/// This is a *ceiling on how often a keepalive can be emitted*, not a
/// schedule: the daemon suppresses the emission unless the last ack is at
/// least this old, so an actively-acking loop never sees one.
///
/// Sized to keep an IDLE loop's prompt cache warm. The Anthropic prompt
/// cache TTL is ~5 min (300s); a keepalive that lands after the TTL pays a
/// full cache-miss re-read on every idle turn. The *effective* turn-to-turn
/// cadence is this interval plus the daemon loop granularity (~10s) plus the
/// `claude-event-watch` debounce (~30s) ≈ 250s, comfortably under the 300s
/// TTL. (Previously 300s, which — combined with the timer-advance-on-suppress
/// bug fixed in `due` / `record_keepalive_emitted` — produced an effective
/// cadence of ~2× ≈ 10 min, so ~half of idle keepalive turns missed the
/// cache. See analyze-session-spend, 2026-09.)
pub const KEEPALIVE_INTERVAL_SECS: u64 = 210;

/// Interval between `memory-reminder` events when the operator is PRESENT.
/// 30 minutes.
///
/// Unlike `keepalive` (whose 210s cadence is tuned to stay under the ~5min
/// Anthropic prompt-cache TTL so idle wakes hit the WARM cache — see
/// [`KEEPALIVE_INTERVAL_SECS`] and PR #794), the memory-reminder is a
/// non-liveness, non-urgent context-hygiene nudge (update logs, commit + push
/// repos). Its cost lever is therefore turn COUNT, not cache warmth: each
/// reminder that fires while the operator is away drives a work-cascade
/// (git status across every repo, log update) that is pure idle spend when
/// nothing has changed and nobody is waiting on it. The base interval is kept
/// at 30min so hygiene stays responsive whenever someone is actually at the
/// desk.
pub const MEMORY_REMINDER_INTERVAL_SECS: u64 = 1800;

/// Interval between `memory-reminder` events when the operator is AWAY (idle).
/// 2 hours.
///
/// This is the idle-spend lever measured on the 2026-09-23/24 overnight
/// windows (analyze-session-spend, `#9352`): with keepalive already warm-tuned
/// by PR #794 (measured 0% cache-miss overnight), the remaining idle cost is
/// per-turn cache_read FREIGHT × wake frequency. keepalive frequency cannot be
/// safely cut (stretching it past the ~5min TTL turns every wake COLD at ~12×
/// the warm price, and past [`crate::config::AckConfig`]'s `stale_minutes`=20
/// it trips the wedge alarm). The memory-reminder has neither constraint: it is
/// not a liveness signal, so lengthening it when the operator is away trips no
/// alarm and loses no responsiveness (it snaps back to
/// [`MEMORY_REMINDER_INTERVAL_SECS`] the moment presence returns). Stretching
/// 30min → 2h while away removes ~3 of every 4 overnight reminder wakes and
/// their downstream commit/log cascades. `0` (the config default for the field
/// that selects this) means "no idle backoff — always use the base interval".
///
/// `#[allow(dead_code)]`: a documentary single-source-of-truth value mirrored
/// by the container's `[cadence] memory_reminder_idle_interval_secs` (config is
/// TOML data and cannot reference a Rust const). The field's code default is
/// `0` (off), so nothing in non-test code reads this const directly.
#[allow(dead_code)]
pub const MEMORY_REMINDER_IDLE_INTERVAL_SECS: u64 = 7200;

/// Pick the effective memory-reminder interval for this loop pass.
///
/// Pure decision, unit-testable without I/O: the daemon passes the current
/// operator-presence state and the two configured intervals. When the operator
/// is AWAY and an idle interval is configured (`idle > 0`), the LONGER of the
/// two is used (a mis-set `idle < base` can never SHORTEN the base); otherwise
/// the base interval stands. Applied via
/// [`CadenceTracker::apply_intervals`], which preserves the last-fired instant,
/// so switching regimes never replays a startup burst and a shortened interval
/// (operator returns) takes effect measured from the real last emission.
pub fn effective_memory_interval_secs(
    base_secs: u64,
    idle_secs: u64,
    operator_away: bool,
) -> u64 {
    if operator_away && idle_secs > 0 {
        base_secs.max(idle_secs)
    } else {
        base_secs
    }
}

/// claude-event tag for the keepalive probe.
///
/// Renamed from `heartbeat-tick` 2026-08-22. Nothing in the daemon routes on
/// the tag STRING, so there is no legacy constant here; the compat aliases
/// live in the two consumers that do route on it — `event-classify`'s rule
/// table and `claude-event-watch`'s monitor-line lead map — so an event
/// emitted by an older binary (or still sitting on a queue across the
/// upgrade) still classifies and renders.
pub const KEEPALIVE_TAG: &str = "keepalive";

/// claude-event tag for the memory reminder.
pub const MEMORY_REMINDER_TAG: &str = "memory-reminder";

/// `source` / `source_name` used on both cadence events.
pub const CADENCE_SOURCE: &str = "claude-watch";

/// Checklist body carried by the `memory-reminder` event.
///
/// Reproduces the action checklist from the host's standalone reminder
/// script, genericized: integration-agnostic wording, no host-specific
/// paths or private references. The consuming main loop maps these generic
/// steps onto its own concrete files/repos.
pub const MEMORY_REMINDER_CHECKLIST: &str = "\
=== MEMORY REMINDER — ACTION REQUIRED ===

STOP what you are doing and perform ALL of these steps NOW:

1. UPDATE the session log with a summary of work done since the last update
2. CHECK for any pending requests and update their status if fulfilled
3. UPDATE long-term memory if you learned any new patterns, preferences, or gotchas
4. UPDATE notes on any new collaborator info or pending requests
5. RUN git status across all working repositories
6. COMMIT and PUSH every repository with uncommitted changes

Do NOT dismiss this reminder without completing the checklist.
Do NOT just read the output and continue working — actually do the steps.";

/// Which cadence events are due on a given loop pass. Either, both, or
/// neither may be true.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CadenceDue {
    pub keepalive: bool,
    pub memory_reminder: bool,
}

impl CadenceDue {
    /// True if neither event is due (the common case — nothing to emit).
    pub fn is_empty(self) -> bool {
        !self.keepalive && !self.memory_reminder
    }
}

/// Tracks when each cadence timer last fired and decides what is due.
///
/// Uses monotonic [`Instant`]s, so it is immune to wall-clock jumps. On
/// construction both timers are armed to fire on the first `due()` call —
/// matching the host script's "touch/emit immediately, then sleep" shape
/// (the main loop gets a tick and a reminder right away on daemon start /
/// restart, instead of waiting a full interval for the first signal).
#[derive(Debug, Clone)]
pub struct CadenceTracker {
    keepalive_interval: Duration,
    memory_interval: Duration,
    /// Last emission instant per timer. `None` => never emitted yet
    /// (fire on first `due()` call).
    last_keepalive: Option<Instant>,
    last_memory: Option<Instant>,
}

impl CadenceTracker {
    /// Construct with the default intervals (5min / 15min).
    pub fn new() -> Self {
        Self::with_intervals(
            Duration::from_secs(KEEPALIVE_INTERVAL_SECS),
            Duration::from_secs(MEMORY_REMINDER_INTERVAL_SECS),
        )
    }

    /// Construct with explicit intervals (config override / tests).
    pub fn with_intervals(keepalive_interval: Duration, memory_interval: Duration) -> Self {
        Self {
            keepalive_interval,
            memory_interval,
            last_keepalive: None,
            last_memory: None,
        }
    }

    /// Adopt new intervals WITHOUT resetting the timers.
    ///
    /// Called on a config reload. The distinction matters: rebuilding the
    /// tracker with [`CadenceTracker::with_intervals`] would reset
    /// `last_keepalive` / `last_memory` to `None`, and a `None` timer is
    /// armed to fire on the very next `due()` call. A config file that is
    /// saved several times in a minute would then emit one cadence event
    /// per save — which is exactly what happened on 2026-08-22, when seven
    /// `memory-reminder` events landed in 52 seconds against a 30-minute
    /// interval.
    ///
    /// Preserving the last-fired instants means the next fire is measured
    /// from the real last emission, against the NEW interval. Shortening
    /// the interval therefore takes effect immediately (a timer whose
    /// elapsed time already exceeds the new interval is due on the next
    /// pass) without replaying a startup burst; lengthening it pushes the
    /// next fire out. Only a genuine process start fires on construction.
    pub fn apply_intervals(&mut self, keepalive_interval: Duration, memory_interval: Duration) {
        self.keepalive_interval = keepalive_interval;
        self.memory_interval = memory_interval;
    }

    /// Decide which cadence events are due as of `now`, and record the
    /// emission for any that are. This both reports AND advances the timer
    /// state — call it once per loop pass and emit whatever it returns.
    ///
    /// A timer is due when it has never fired (`None`) or when at least its
    /// interval has elapsed since its last fire. The recorded "last fire"
    /// is set to `now` (not `last + interval`), which means a slow loop
    /// pass does not try to "catch up" by firing repeatedly — at most one
    /// event of each kind per call. That is the desired behaviour: these
    /// are cadence signals, not a billing meter.
    ///
    /// ## Keepalive: due != advance
    ///
    /// The keepalive timer is NOT advanced here. Whether a due keepalive is
    /// actually EMITTED is a separate, I/O-dependent decision the daemon makes
    /// from the last-ack age (see `crate::policy::last_ack_timestamp_age`): a
    /// due tick whose acks are still flowing is SUPPRESSED, not emitted. If
    /// `due` advanced the timer on every due pass (as it once did), a
    /// suppressed tick would still reset the clock, so the NEXT emit could
    /// only happen a full interval later — doubling the effective cadence
    /// (~2× the configured interval) and letting ~half of idle keepalive
    /// turns land after the prompt-cache TTL. The daemon therefore calls
    /// [`CadenceTracker::record_keepalive_emitted`] only when it actually
    /// emits, keeping the timer measured from real emissions. The
    /// memory-reminder timer has no suppression gate, so it is still advanced
    /// inline on its due pass.
    pub fn due(&mut self, now: Instant) -> CadenceDue {
        let keepalive_due = match self.last_keepalive {
            None => true,
            Some(last) => now.duration_since(last) >= self.keepalive_interval,
        };
        let memory_due = match self.last_memory {
            None => true,
            Some(last) => now.duration_since(last) >= self.memory_interval,
        };
        // NOTE: the keepalive timer is intentionally NOT advanced here — see
        // the doc comment above. The daemon calls `record_keepalive_emitted`
        // when (and only when) it actually emits. The memory-reminder timer
        // has no emission gate, so advance it inline.
        if memory_due {
            self.last_memory = Some(now);
        }
        CadenceDue {
            keepalive: keepalive_due,
            memory_reminder: memory_due,
        }
    }

    /// Record that a `keepalive` event was actually EMITTED at `now`,
    /// advancing the keepalive timer.
    ///
    /// Paired with [`CadenceTracker::due`], which reports keepalive-due but
    /// does NOT advance the timer (the emission is gated on last-ack age in
    /// the daemon). Call this only on a real emit; a due-but-suppressed tick
    /// must leave the timer untouched so the next due pass re-reports
    /// promptly rather than a full interval later.
    pub fn record_keepalive_emitted(&mut self, now: Instant) {
        self.last_keepalive = Some(now);
    }
}

impl Default for CadenceTracker {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effective_memory_interval_uses_base_when_present() {
        // Operator PRESENT => always the base interval, even if an idle
        // interval is configured. Responsiveness is never harmed by the
        // backoff while someone is at the desk.
        assert_eq!(
            effective_memory_interval_secs(1800, 7200, false),
            1800
        );
    }

    #[test]
    fn effective_memory_interval_backs_off_when_away() {
        // Operator AWAY + idle configured => the longer idle interval.
        assert_eq!(
            effective_memory_interval_secs(1800, 7200, true),
            7200
        );
    }

    #[test]
    fn effective_memory_interval_idle_zero_disables_backoff() {
        // idle == 0 means "no backoff": use the base even when away. This is
        // the field default, so a deployment that does not opt in behaves
        // exactly as before this change.
        assert_eq!(
            effective_memory_interval_secs(1800, 0, true),
            1800
        );
    }

    #[test]
    fn effective_memory_interval_uses_the_canonical_idle_const() {
        // The idle const is the recommended idle interval; the container config
        // mirrors it as a literal. Assert the helper backs off to exactly it
        // from the base when the operator is away (this also keeps the const a
        // live single-source-of-truth value rather than dead code).
        assert_eq!(
            effective_memory_interval_secs(
                MEMORY_REMINDER_INTERVAL_SECS,
                MEMORY_REMINDER_IDLE_INTERVAL_SECS,
                true,
            ),
            MEMORY_REMINDER_IDLE_INTERVAL_SECS
        );
        assert!(
            MEMORY_REMINDER_IDLE_INTERVAL_SECS > MEMORY_REMINDER_INTERVAL_SECS,
            "idle backoff must be longer than the base interval"
        );
    }

    #[test]
    fn effective_memory_interval_never_shortens_base() {
        // A mis-set idle interval SHORTER than the base must never shorten the
        // base (that would INCREASE idle spend). The max() guards it.
        assert_eq!(
            effective_memory_interval_secs(1800, 600, true),
            1800
        );
    }


    /// Model the daemon's loop pass with keepalive emission UNCONDITIONAL
    /// (as if the ack gate always let it through): call `due` and, when
    /// keepalive is reported due, record the emit. `due` no longer advances
    /// the keepalive timer itself (the advance is gated on a real emit in the
    /// daemon — see `due`'s doc comment and `record_keepalive_emitted`), so a
    /// test that wants the classic "keepalive fires every interval" behaviour
    /// must model the record. This keeps the reminder-focused tests' intent
    /// (their `is_empty()` assertions) exactly as before the split.
    fn tick(t: &mut CadenceTracker, at: Instant) -> CadenceDue {
        let d = t.due(at);
        if d.keepalive {
            t.record_keepalive_emitted(at);
        }
        d
    }

    #[test]
    fn first_call_fires_both() {
        let mut t = CadenceTracker::new();
        let now = Instant::now();
        let due = t.due(now);
        assert!(due.keepalive, "first keepalive should fire");
        assert!(due.memory_reminder, "first reminder should fire");
        assert!(!due.is_empty());
    }

    #[test]
    fn immediate_second_call_fires_neither() {
        let mut t = CadenceTracker::new();
        let start = Instant::now();
        let _ = tick(&mut t, start);
        // Same instant again: nothing has elapsed.
        let due = t.due(start);
        assert!(!due.keepalive);
        assert!(!due.memory_reminder);
        assert!(due.is_empty());
    }

    #[test]
    fn keepalive_fires_at_its_interval_but_not_reminder() {
        let mut t = CadenceTracker::with_intervals(
            Duration::from_secs(60),
            Duration::from_secs(900),
        );
        let start = Instant::now();
        let _ = tick(&mut t, start); // arm both

        // 60s later: keepalive due, reminder not.
        let due = tick(&mut t, start + Duration::from_secs(60));
        assert!(due.keepalive);
        assert!(!due.memory_reminder);

        // A bit before the next keepalive interval: neither.
        let due = t.due(start + Duration::from_secs(60 + 59));
        assert!(!due.keepalive);
        assert!(!due.memory_reminder);
    }

    #[test]
    fn reminder_fires_at_its_interval() {
        let mut t = CadenceTracker::with_intervals(
            Duration::from_secs(60),
            Duration::from_secs(900),
        );
        let start = Instant::now();
        let _ = tick(&mut t, start); // arm both

        // Just before 15min: reminder not yet due.
        let due = tick(&mut t, start + Duration::from_secs(899));
        assert!(!due.memory_reminder);

        // At 15min: reminder due. (Keepalive fired at 899 in the call
        // above, so only 1s has elapsed for it here — not due, and that's
        // fine: the timers are independent.)
        let due = t.due(start + Duration::from_secs(900));
        assert!(due.memory_reminder);
    }

    #[test]
    fn timers_are_independent() {
        let mut t = CadenceTracker::with_intervals(
            Duration::from_secs(60),
            Duration::from_secs(900),
        );
        let start = Instant::now();
        let _ = tick(&mut t, start);

        // Fire keepalive several times across the reminder window; the
        // reminder must only fire once it crosses 900s, regardless of how
        // many keepalives fired in between.
        let mut reminder_fires = 0;
        for sec in (60..=900).step_by(60) {
            let due = tick(&mut t, start + Duration::from_secs(sec));
            if due.memory_reminder {
                reminder_fires += 1;
            }
        }
        assert_eq!(reminder_fires, 1, "reminder fires exactly once over 15min");
    }

    #[test]
    fn slow_loop_does_not_replay_missed_ticks() {
        // If the loop stalls and we call due() once after a long gap, we
        // get at most one event of each kind — not one per missed interval.
        let mut t = CadenceTracker::with_intervals(
            Duration::from_secs(60),
            Duration::from_secs(900),
        );
        let start = Instant::now();
        let _ = tick(&mut t, start);

        // Jump 10 minutes ahead in a single call.
        let due = tick(&mut t, start + Duration::from_secs(600));
        assert!(due.keepalive);
        assert!(!due.memory_reminder); // 600 < 900

        // Immediately again — nothing replays.
        let due = t.due(start + Duration::from_secs(600));
        assert!(due.is_empty());
    }

    #[test]
    fn suppressed_keepalive_does_not_advance_the_timer() {
        // REGRESSION (analyze-session-spend, 2026-09): `due` used to advance
        // the keepalive timer on EVERY due pass, including one the daemon then
        // SUPPRESSED because acks were still flowing. A suppressed tick would
        // burn the clock, so the next real emit could only land a full
        // interval later — DOUBLING the effective cadence (~2× the configured
        // interval) and pushing ~half of idle keepalive turns past the
        // prompt-cache TTL. The split (due reports; record advances) fixes it:
        // a due-but-suppressed tick must leave the timer untouched.
        let interval = Duration::from_secs(210);
        let mut t = CadenceTracker::with_intervals(interval, Duration::from_secs(1800));
        let start = Instant::now();

        // Startup emit at t=0.
        let d = t.due(start);
        assert!(d.keepalive);
        t.record_keepalive_emitted(start);

        // t=210: due again — but the daemon SUPPRESSES (acks flowing), so it
        // does NOT record. The timer must stay at 0.
        let d = t.due(start + interval);
        assert!(d.keepalive, "interval elapsed => reported due");
        // (no record_keepalive_emitted: suppressed)

        // A short loop-granularity later the acks have gone quiet, so the next
        // pass emits. With the OLD bug the timer would have advanced to 210 on
        // the suppressed pass and this pass (t=220) would NOT be due; the fix
        // keeps it due so the emit lands promptly — NOT a full interval later.
        let d = t.due(start + interval + Duration::from_secs(10));
        assert!(
            d.keepalive,
            "a suppressed tick must not consume the timer — next pass still due"
        );
        t.record_keepalive_emitted(start + interval + Duration::from_secs(10));

        // And after a real emit the timer IS measured from it: not due again
        // until one interval past t=220.
        let d = t.due(start + interval + Duration::from_secs(10 + 209));
        assert!(!d.keepalive, "measured from the real emit at t=220");
        let d = t.due(start + interval + Duration::from_secs(10 + 210));
        assert!(d.keepalive, "due one full interval after the real emit");
    }

    #[test]
    fn reloads_inside_the_interval_emit_nothing() {
        // REGRESSION (2026-08-22): a config save triggers a reload, and the
        // reload used to REBUILD the tracker. Seven saves in 52 seconds
        // produced seven `memory-reminder` events against a 30-minute
        // interval. Applying the intervals in place must emit nothing.
        let keepalive = Duration::from_secs(300);
        let memory = Duration::from_secs(1800);
        let mut t = CadenceTracker::with_intervals(keepalive, memory);
        let start = Instant::now();
        let due = tick(&mut t, start);
        assert!(!due.is_empty(), "startup fires both (documented behaviour)");

        // Seven reloads spread over the next ~50 seconds, unchanged intervals.
        for n in 1..=7 {
            t.apply_intervals(keepalive, memory);
            let due = tick(&mut t, start + Duration::from_secs(n * 7));
            assert!(
                due.is_empty(),
                "reload at +{}s must not re-arm the timers",
                n * 7
            );
        }

        // And the real schedule still holds afterwards: the reminder fires
        // once its own interval has elapsed since the ORIGINAL emission, not
        // since the last reload.
        let due = t.due(start + memory);
        assert!(due.memory_reminder, "reminder still due at its interval");
    }

    #[test]
    fn shortened_interval_measures_from_the_preserved_last_fire() {
        let mut t =
            CadenceTracker::with_intervals(Duration::from_secs(300), Duration::from_secs(1800));
        let start = Instant::now();
        let _ = tick(&mut t, start); // arm both

        // 600s in, the operator shortens the reminder to 5min. The new
        // interval has ALREADY elapsed relative to the preserved last-fire,
        // so the reminder is due on the next pass — but only once, and it is
        // the interval change (not the reload) that made it due.
        t.apply_intervals(Duration::from_secs(300), Duration::from_secs(300));
        let due = tick(&mut t, start + Duration::from_secs(600));
        assert!(due.memory_reminder, "shortened interval already elapsed");

        // Immediately after, nothing replays.
        let due = t.due(start + Duration::from_secs(600));
        assert!(due.is_empty());

        // Next fire is one NEW interval after that emission.
        let due = tick(&mut t, start + Duration::from_secs(600 + 299));
        assert!(!due.memory_reminder);
        let due = t.due(start + Duration::from_secs(600 + 300));
        assert!(due.memory_reminder);
    }

    #[test]
    fn lengthened_interval_pushes_the_next_fire_out() {
        let mut t =
            CadenceTracker::with_intervals(Duration::from_secs(300), Duration::from_secs(900));
        let start = Instant::now();
        let _ = tick(&mut t, start);

        // Operator lengthens the reminder to 30min.
        t.apply_intervals(Duration::from_secs(300), Duration::from_secs(1800));

        // The OLD interval boundary passes with no emission...
        let due = tick(&mut t, start + Duration::from_secs(900));
        assert!(!due.memory_reminder, "old 15min boundary must not fire");
        // ...and the new one fires, measured from the preserved last-fire.
        let due = t.due(start + Duration::from_secs(1800));
        assert!(due.memory_reminder);
    }

    #[test]
    fn apply_intervals_before_first_due_keeps_the_startup_fire() {
        // A reload that lands before the daemon's first loop pass must not
        // swallow the documented startup emission (cold start is unchanged).
        let mut t = CadenceTracker::new();
        t.apply_intervals(Duration::from_secs(60), Duration::from_secs(900));
        let due = t.due(Instant::now());
        assert!(due.keepalive);
        assert!(due.memory_reminder);
    }

    #[test]
    fn checklist_is_generic_no_private_paths() {
        // Guard against re-introducing host-specific paths/names into the
        // public repo's reminder text.
        let c = MEMORY_REMINDER_CHECKLIST;
        for needle in ["/mnt/", "Raiden", "ADHPrivate", "/home/", "signal-admin"] {
            assert!(
                !c.contains(needle),
                "checklist must not contain host-specific token: {needle}"
            );
        }
        assert!(c.contains("MEMORY REMINDER"));
        assert!(c.contains("COMMIT and PUSH"));
    }

    #[test]
    fn tags_match_protocol() {
        // The tag strings are the wire contract with event-classify /
        // claude-event-watch, so pin them. The interval *values* are NOT
        // asserted here — that would just restate the literal in a second
        // place (a maintenance tax, not a test). The intervals are plain
        // tunables; their single source of truth is the const above.
        assert_eq!(KEEPALIVE_TAG, "keepalive");
        assert_eq!(MEMORY_REMINDER_TAG, "memory-reminder");
    }
}
