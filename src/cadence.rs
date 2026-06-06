//! Daemon-emitted cadence signals: `heartbeat-tick` and `memory-reminder`.
//!
//! The claude-watch daemon is already a long-running monitor loop — the
//! natural place to source periodic "cadence" signals for the main loop.
//! Previously these were produced by a separate self-rescheduling
//! background task that the main loop had to keep restarting every cycle
//! (a treadmill). Moving the *cadence source* into the daemon removes that
//! restart churn: the daemon ticks on its own monotonic clock.
//!
//! 1. `heartbeat-tick` — every [`HEARTBEAT_TICK_INTERVAL_SECS`] (300s, 5 min).
//!    Written to the event queue (`~/claude-events/`) so the main loop is
//!    reminded — via the next `UserPromptSubmit` — to touch the host
//!    heartbeat file. That file is the wedge-detector; if the loop never
//!    gets a recurring prompt to refresh it while idle, it goes stale at the
//!    ~10-min threshold and the daemon fires a spurious "heartbeat stale"
//!    alert. So heartbeat-tick *must* reach the loop, and the event queue is
//!    the delivery path.
//!
//! 2. `memory-reminder` — every [`MEMORY_REMINDER_INTERVAL_SECS`] (15min),
//!    carrying the action checklist text ([`MEMORY_REMINDER_CHECKLIST`]).
//!    Delivered via tmux-inject into the main loop pane (same mechanism as
//!    other daemon interventions), NOT the event queue.
//!
//! ## Delivery choice: event queue vs. tmux-inject
//!
//! Writing JSON files to `~/claude-events/` can, under load, contribute to a
//! watcher-restart treadmill: `claude-event-watch` fires on a new file,
//! drains it, exits; the watcher-monitor restarts it; if another event has
//! already landed, repeat. That treadmill is driven by event *bursts* during
//! active threads — not by a single steady periodic signal. A lone
//! heartbeat-tick every 5 minutes is well within the debounce window and is
//! an acceptable cost for the thing it buys: a reliable idle-loop reminder to
//! refresh the heartbeat. `memory-reminder` carries a large checklist and
//! wants to land as a user-typed prompt, so it is tmux-injected instead.
//!
//! (Historical note: an earlier change routed *both* cadence signals away
//! from the event queue to fight the treadmill, which silently dropped the
//! heartbeat-tick reminder and re-introduced the stale-heartbeat alerts.
//! Only memory-reminder needed to leave the queue.)
//!
//! ## Why the daemon must NOT write the heartbeat file itself
//!
//! The host heartbeat file's entire value is that the *main loop* writes
//! it: a wedged loop stops writing, the file goes stale, and the daemon's
//! existing stale-detection fires a nudge. If the daemon wrote that file
//! directly it would stay fresh even while the loop is dead, defeating
//! wedge detection. This module never touches any heartbeat file.
//!
//! ## Cadence decision is pure
//!
//! [`CadenceTracker`] holds the monotonic instant of the last emission for
//! each timer and decides, given "now", whether each timer is due. It is a
//! pure value type (no I/O), so the interval logic is unit-tested directly.
//! The daemon owns one `CadenceTracker`, calls [`CadenceTracker::due`] each
//! loop pass, and acts on whichever signals are due.

use std::time::{Duration, Instant};

/// Interval between `heartbeat-tick` events. 300 seconds (5 min).
pub const HEARTBEAT_TICK_INTERVAL_SECS: u64 = 300;

/// Interval between `memory-reminder` events. 15 minutes.
pub const MEMORY_REMINDER_INTERVAL_SECS: u64 = 900;

/// claude-event tag for the heartbeat tick.
pub const HEARTBEAT_TICK_TAG: &str = "heartbeat-tick";

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
    pub heartbeat_tick: bool,
    pub memory_reminder: bool,
}

impl CadenceDue {
    /// True if neither event is due (the common case — nothing to emit).
    pub fn is_empty(self) -> bool {
        !self.heartbeat_tick && !self.memory_reminder
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
    heartbeat_interval: Duration,
    memory_interval: Duration,
    /// Last emission instant per timer. `None` => never emitted yet
    /// (fire on first `due()` call).
    last_heartbeat: Option<Instant>,
    last_memory: Option<Instant>,
}

impl CadenceTracker {
    /// Construct with the default intervals (5min / 15min).
    pub fn new() -> Self {
        Self::with_intervals(
            Duration::from_secs(HEARTBEAT_TICK_INTERVAL_SECS),
            Duration::from_secs(MEMORY_REMINDER_INTERVAL_SECS),
        )
    }

    /// Construct with explicit intervals (config override / tests).
    pub fn with_intervals(heartbeat_interval: Duration, memory_interval: Duration) -> Self {
        Self {
            heartbeat_interval,
            memory_interval,
            last_heartbeat: None,
            last_memory: None,
        }
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
    pub fn due(&mut self, now: Instant) -> CadenceDue {
        let heartbeat_due = match self.last_heartbeat {
            None => true,
            Some(last) => now.duration_since(last) >= self.heartbeat_interval,
        };
        let memory_due = match self.last_memory {
            None => true,
            Some(last) => now.duration_since(last) >= self.memory_interval,
        };
        if heartbeat_due {
            self.last_heartbeat = Some(now);
        }
        if memory_due {
            self.last_memory = Some(now);
        }
        CadenceDue {
            heartbeat_tick: heartbeat_due,
            memory_reminder: memory_due,
        }
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
    fn first_call_fires_both() {
        let mut t = CadenceTracker::new();
        let now = Instant::now();
        let due = t.due(now);
        assert!(due.heartbeat_tick, "first heartbeat should fire");
        assert!(due.memory_reminder, "first reminder should fire");
        assert!(!due.is_empty());
    }

    #[test]
    fn immediate_second_call_fires_neither() {
        let mut t = CadenceTracker::new();
        let start = Instant::now();
        let _ = t.due(start);
        // Same instant again: nothing has elapsed.
        let due = t.due(start);
        assert!(!due.heartbeat_tick);
        assert!(!due.memory_reminder);
        assert!(due.is_empty());
    }

    #[test]
    fn heartbeat_fires_at_its_interval_but_not_reminder() {
        let mut t = CadenceTracker::with_intervals(
            Duration::from_secs(60),
            Duration::from_secs(900),
        );
        let start = Instant::now();
        let _ = t.due(start); // arm both

        // 60s later: heartbeat due, reminder not.
        let due = t.due(start + Duration::from_secs(60));
        assert!(due.heartbeat_tick);
        assert!(!due.memory_reminder);

        // A bit before the next heartbeat interval: neither.
        let due = t.due(start + Duration::from_secs(60 + 59));
        assert!(!due.heartbeat_tick);
        assert!(!due.memory_reminder);
    }

    #[test]
    fn reminder_fires_at_its_interval() {
        let mut t = CadenceTracker::with_intervals(
            Duration::from_secs(60),
            Duration::from_secs(900),
        );
        let start = Instant::now();
        let _ = t.due(start); // arm both

        // Just before 15min: reminder not yet due.
        let due = t.due(start + Duration::from_secs(899));
        assert!(!due.memory_reminder);

        // At 15min: reminder due. (Heartbeat fired at 899 in the call
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
        let _ = t.due(start);

        // Fire heartbeat several times across the reminder window; the
        // reminder must only fire once it crosses 900s, regardless of how
        // many heartbeats fired in between.
        let mut reminder_fires = 0;
        for sec in (60..=900).step_by(60) {
            let due = t.due(start + Duration::from_secs(sec));
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
        let _ = t.due(start);

        // Jump 10 minutes ahead in a single call.
        let due = t.due(start + Duration::from_secs(600));
        assert!(due.heartbeat_tick);
        assert!(!due.memory_reminder); // 600 < 900

        // Immediately again — nothing replays.
        let due = t.due(start + Duration::from_secs(600));
        assert!(due.is_empty());
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
    fn constants_match_design() {
        assert_eq!(HEARTBEAT_TICK_INTERVAL_SECS, 300);
        assert_eq!(MEMORY_REMINDER_INTERVAL_SECS, 900);
        assert_eq!(HEARTBEAT_TICK_TAG, "heartbeat-tick");
        assert_eq!(MEMORY_REMINDER_TAG, "memory-reminder");
    }
}
