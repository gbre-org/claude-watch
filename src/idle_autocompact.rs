//! Idle auto-compaction: reset the main loop's context when the session is
//! genuinely idle, to stop paying `cache_read` freight on a large,
//! ever-growing context that is re-read on every keepalive wake.
//!
//! ## Why this exists
//!
//! claude-watch's own main loop runs on Opus. When the operator is away the
//! loop still wakes on every `keepalive` (~1 per [`crate::cadence::KEEPALIVE_INTERVAL_SECS`],
//! kept under the prompt-cache TTL on purpose) and re-reads its entire
//! context from cache. Most of the idle spend is that `cache_read` of a
//! context that only grows across a long idle stretch — a ~700K-and-growing
//! window re-read ~17×/hr. If nothing is happening, that context is pure
//! carry cost: there is no in-flight work that needs it. Resetting the
//! context (a `/clear` + resume prompt, via the existing `self-clear`
//! machinery) drops the carried freight back to a fresh-boot preamble.
//!
//! ## Opt-in, disabled by default
//!
//! The whole feature is gated on [`IdleAutocompactConfig::enabled`], which
//! defaults to **false**. A fresh or default config never auto-compacts.
//! An operator turns it on explicitly. When it is off, [`decide`] short-
//! circuits to [`IdleAutocompactDecision::Disabled`] before any I/O.
//!
//! ## Trigger conditions (ALL must hold)
//!
//! 1. **Feature enabled** (`enabled = true`).
//! 2. **Operator AWAY** — never clear mid-conversation while someone is at
//!    the desk. Uses the same presence carrier the rest of the daemon reads
//!    ([`crate::metrics::operator_is_away`]).
//! 3. **Queue empty** — no `running` and no `ready`/`pending` session-task
//!    items. Clearing while work is queued or in flight would throw away the
//!    context that work depends on, and would race a subagent.
//! 4. **N consecutive idle keepalives** — the loop has emitted
//!    [`IdleAutocompactConfig::after_keepalives`] keepalives in a row with
//!    the operator away and the queue empty and nothing acked in between
//!    (i.e. no real work happened). This is the "genuinely, sustainedly
//!    idle" bar; a single quiet tick is not enough.
//!
//! ## Safety
//!
//! * **Never while busy**: a non-empty queue (running OR ready) resets the
//!   idle streak to zero — the accumulation must restart from scratch.
//! * **Never while present**: presence returning resets the streak.
//! * **Never loops**: after a clear fires the streak is reset to zero, so the
//!   next clear needs a fresh full accumulation of `after_keepalives` idle
//!   ticks. Combined with a cooldown ([`IdleAutocompactConfig::cooldown_secs`])
//!   this bounds clears to at most one per cooldown window even if the
//!   accounting is perturbed.
//! * **State-save-before-clear is the daemon's contract, enforced by the
//!   caller**: the caller saves a resume pointer (`session-task set`) and
//!   only then spawns `self-clear` with a resume prompt. If the state save
//!   fails, the caller does NOT clear (see `crate::policy`/`main`).
//!
//! ## Purity
//!
//! [`decide`] is a pure function of already-gathered inputs
//! ([`IdleInputs`]) so the trigger logic is unit-tested without touching the
//! presence carrier, the queue CLI, or tmux. The daemon gathers the inputs
//! (presence probe, queue probe) and calls `decide` once per keepalive
//! emission.

use serde::Deserialize;

/// Default number of consecutive idle keepalives before an auto-compaction
/// fires. At [`crate::cadence::KEEPALIVE_INTERVAL_SECS`] (210s) this is
/// ~10.5 minutes of sustained idle before the first clear — long enough that
/// a brief lull never triggers, short enough to reclaim most of an overnight
/// idle stretch.
pub const DEFAULT_AFTER_KEEPALIVES: u32 = 3;

/// Default cooldown between idle auto-compactions (seconds). A backstop on
/// top of the streak-reset-on-fire rule: even if the streak accounting is
/// perturbed, no second clear fires within this window. 1800s = 30 min.
pub const DEFAULT_COOLDOWN_SECS: u64 = 1800;

/// Configuration for idle auto-compaction. Default is fully inert
/// (`enabled = false`).
#[derive(Debug, Deserialize, Clone)]
pub struct IdleAutocompactConfig {
    /// MASTER GATE. Default **false** — the feature is opt-in. When false,
    /// [`decide`] returns [`IdleAutocompactDecision::Disabled`] and the
    /// daemon does nothing. A fresh/default config never auto-compacts.
    #[serde(default = "default_idle_autocompact_enabled")]
    pub enabled: bool,

    /// Number of consecutive idle keepalives (operator away, queue empty,
    /// no work acked) required before a clear fires. Default
    /// [`DEFAULT_AFTER_KEEPALIVES`]. A value of 0 is treated as 1 (never
    /// fire on a zero-length streak) by [`decide`].
    #[serde(default = "default_after_keepalives")]
    pub after_keepalives: u32,

    /// Minimum seconds between two idle auto-compactions. Default
    /// [`DEFAULT_COOLDOWN_SECS`]. Backstop against a clear loop on top of
    /// the streak-reset-on-fire rule.
    #[serde(default = "default_cooldown_secs")]
    pub cooldown_secs: u64,

    /// Resume prompt injected after the `/clear` lands. Kept intentionally
    /// generic and low-key: the operator is away and there is no work
    /// queued, so it just re-establishes liveness (ack the next keepalive)
    /// and points at the queue for any work that arrived during the clear.
    #[serde(default = "default_idle_resume_prompt")]
    pub resume_prompt: String,
}

impl Default for IdleAutocompactConfig {
    fn default() -> Self {
        Self {
            enabled: default_idle_autocompact_enabled(),
            after_keepalives: default_after_keepalives(),
            cooldown_secs: default_cooldown_secs(),
            resume_prompt: default_idle_resume_prompt(),
        }
    }
}

fn default_idle_autocompact_enabled() -> bool {
    false
}

fn default_after_keepalives() -> u32 {
    DEFAULT_AFTER_KEEPALIVES
}

fn default_cooldown_secs() -> u64 {
    DEFAULT_COOLDOWN_SECS
}

fn default_idle_resume_prompt() -> String {
    "[CLAUDE-WATCH] Idle auto-compaction: your context was reset because the \
     session was idle (operator away, queue empty). Nothing was in flight. \
     Ack the next keepalive to re-establish liveness, then check \
     `session-task queue list` for any work that arrived, and resume normal \
     dispatching."
        .to_string()
}

/// Inputs to the pure trigger decision, gathered by the daemon each time a
/// keepalive is actually EMITTED (i.e. the bus has been quiet — no acks in
/// the window, which is itself a necessary idle signal).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdleInputs {
    /// Is the feature enabled? (`config.enabled`.)
    pub enabled: bool,
    /// Is the operator away? (`metrics::operator_is_away()`.)
    pub operator_away: bool,
    /// Is the session-task queue empty (no running AND no ready/pending)?
    pub queue_empty: bool,
    /// The idle-keepalive streak count BEFORE this keepalive is counted.
    pub prior_streak: u32,
    /// Required streak length (`config.after_keepalives`, clamped to >= 1).
    pub after_keepalives: u32,
    /// Whether a clear is currently within its cooldown window (a prior idle
    /// clear fired less than `cooldown_secs` ago). True => suppress.
    pub in_cooldown: bool,
}

/// The decision [`decide`] returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdleAutocompactDecision {
    /// Feature is off (master gate false). Fully inert.
    Disabled,
    /// Not idle this tick (operator present, or queue non-empty). The daemon
    /// resets the streak to 0.
    NotIdle,
    /// Idle this tick but the streak has not yet reached the threshold (or a
    /// cooldown is active). The daemon sets the streak to `new_streak` and
    /// does nothing else.
    Accumulate { new_streak: u32 },
    /// Fire an auto-compaction now. The daemon saves state, then spawns
    /// `self-clear`, then resets the streak to 0.
    Fire,
}

/// Pure trigger decision. See the module docs for the full condition set.
///
/// Ordering of the gates matters for the safety guarantees:
///
/// 1. `!enabled`            => `Disabled` (before any other consideration).
/// 2. present OR queue busy => `NotIdle` (streak resets — the "never while
///    busy / never while present" rule, and the anti-loop reset).
/// 3. idle, cooldown active => `Accumulate` (bounded: never a second clear
///    inside the cooldown window; the streak still advances so a clear fires
///    promptly once the cooldown expires).
/// 4. idle, streak+1 >= N   => `Fire`.
/// 5. idle, streak+1  < N   => `Accumulate`.
pub fn decide(inp: IdleInputs) -> IdleAutocompactDecision {
    if !inp.enabled {
        return IdleAutocompactDecision::Disabled;
    }
    // "Idle this tick" == operator away AND queue empty. Anything else
    // resets the streak — this is the never-while-busy / never-while-present
    // guarantee AND the anti-loop reset (a clear that produces real work, or
    // a returning operator, breaks the streak).
    if !inp.operator_away || !inp.queue_empty {
        return IdleAutocompactDecision::NotIdle;
    }
    let threshold = inp.after_keepalives.max(1);
    let new_streak = inp.prior_streak.saturating_add(1);
    // Cooldown backstop: never fire a second clear inside the window. Keep
    // advancing the streak so the first tick after the cooldown expires can
    // fire without re-accumulating from zero.
    if inp.in_cooldown {
        return IdleAutocompactDecision::Accumulate { new_streak };
    }
    if new_streak >= threshold {
        IdleAutocompactDecision::Fire
    } else {
        IdleAutocompactDecision::Accumulate { new_streak }
    }
}

/// Probe the session-task queue for emptiness: returns `Some(true)` when
/// there are no `running` and no `ready` (pending, unblocked) items,
/// `Some(false)` when at least one exists, and `None` when the queue state
/// could not be determined (CLI missing, exec error, parse error) — the
/// caller treats `None` as "do NOT fire" (fail-safe: an unreadable queue is
/// never assumed empty).
///
/// Reuses the same `session-task ... --json` shell-out contract as
/// [`crate::stale_ready`]: `queue list --all --json` for the running set and
/// `queue ready --json` for the ready set.
pub fn queue_is_empty(timeout_secs: u64) -> Option<bool> {
    let cli = find_session_task_cli()?;

    // Running items: any item in the full list with status "running".
    let all = run_session_task_json(&cli, &["queue", "list", "--all", "--json"], timeout_secs)
        .ok()?;
    let any_running = all.iter().any(|it| it.status == "running");
    if any_running {
        return Some(false);
    }

    // Ready items: pending + unblocked. Non-empty => not idle.
    let ready = run_session_task_json(&cli, &["queue", "ready", "--json"], timeout_secs).ok()?;
    Some(ready.is_empty())
}

/// Minimal queue-item shape for the emptiness probe — only `status` matters.
#[derive(Debug, Clone, Deserialize)]
struct ProbeItem {
    #[serde(default)]
    status: String,
}

/// Locate the `session-task` CLI (PATH, then `~/bin`), honouring the
/// `SESSION_TASK_CLI` test-injection env var. Mirrors
/// [`crate::stale_ready`]'s resolver so both agree on which binary to call.
fn find_session_task_cli() -> Option<std::path::PathBuf> {
    use std::path::PathBuf;
    if let Ok(p) = std::env::var("SESSION_TASK_CLI") {
        let pb = PathBuf::from(p);
        if pb.exists() {
            return Some(pb);
        }
    }
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            let candidate = dir.join("session-task");
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        let candidate = PathBuf::from(home).join("bin/session-task");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Shell out to `session-task <args>` with a timeout and parse stdout as a
/// `Vec<ProbeItem>`. Same timeout-guarded pattern as
/// [`crate::stale_ready`]'s helper.
fn run_session_task_json(
    cli: &std::path::Path,
    args: &[&str],
    timeout_secs: u64,
) -> Result<Vec<ProbeItem>, String> {
    use std::process::Command;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    let cli_owned = cli.to_path_buf();
    let args_owned: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let out = Command::new(&cli_owned).args(&args_owned).output();
        let _ = tx.send(out);
    });
    let out = match rx.recv_timeout(Duration::from_secs(timeout_secs)) {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => return Err(format!("session-task exec failed: {e}")),
        Err(_) => return Err(format!("session-task timed out after {timeout_secs}s")),
    };
    if !out.status.success() {
        return Err(format!(
            "session-task exited non-zero (rc={:?})",
            out.status.code()
        ));
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    serde_json::from_str(trimmed).map_err(|e| format!("session-task JSON parse failed: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inp() -> IdleInputs {
        IdleInputs {
            enabled: true,
            operator_away: true,
            queue_empty: true,
            prior_streak: 0,
            after_keepalives: 3,
            in_cooldown: false,
        }
    }

    #[test]
    fn disabled_is_fully_inert() {
        // The master gate: even with every other idle condition satisfied
        // and the streak already past threshold, a disabled feature never
        // fires — it returns Disabled before any other consideration.
        let d = decide(IdleInputs {
            enabled: false,
            prior_streak: 100,
            ..inp()
        });
        assert_eq!(d, IdleAutocompactDecision::Disabled);
    }

    #[test]
    fn default_config_is_disabled() {
        // A fresh/default config must NOT auto-compact.
        assert!(!IdleAutocompactConfig::default().enabled);
    }

    #[test]
    fn present_operator_never_fires_and_resets_streak() {
        // Never while present: even a long streak resets when the operator
        // is at the desk.
        let d = decide(IdleInputs {
            operator_away: false,
            prior_streak: 10,
            ..inp()
        });
        assert_eq!(d, IdleAutocompactDecision::NotIdle);
    }

    #[test]
    fn busy_queue_never_fires_and_resets_streak() {
        // Never while busy: a non-empty queue resets the streak, even at a
        // streak that would otherwise fire.
        let d = decide(IdleInputs {
            queue_empty: false,
            prior_streak: 10,
            ..inp()
        });
        assert_eq!(d, IdleAutocompactDecision::NotIdle);
    }

    #[test]
    fn accumulates_below_threshold() {
        // First idle tick of a 3-tick threshold: accumulate to 1, no fire.
        let d = decide(IdleInputs {
            prior_streak: 0,
            after_keepalives: 3,
            ..inp()
        });
        assert_eq!(d, IdleAutocompactDecision::Accumulate { new_streak: 1 });
    }

    #[test]
    fn fires_at_threshold() {
        // prior_streak 2 + this tick = 3 == threshold => Fire.
        let d = decide(IdleInputs {
            prior_streak: 2,
            after_keepalives: 3,
            ..inp()
        });
        assert_eq!(d, IdleAutocompactDecision::Fire);
    }

    #[test]
    fn does_not_loop_cooldown_suppresses_immediate_refire() {
        // Anti-loop: right after a clear the daemon resets the streak AND is
        // in cooldown. Even if the streak somehow re-reaches threshold, the
        // cooldown suppresses a second fire (Accumulate, not Fire).
        let d = decide(IdleInputs {
            prior_streak: 5,
            after_keepalives: 3,
            in_cooldown: true,
            ..inp()
        });
        assert_eq!(d, IdleAutocompactDecision::Accumulate { new_streak: 6 });
    }

    #[test]
    fn zero_threshold_clamped_to_one() {
        // after_keepalives=0 must not fire on a zero-length streak; it is
        // clamped to 1, so the first idle tick fires.
        let d = decide(IdleInputs {
            prior_streak: 0,
            after_keepalives: 0,
            ..inp()
        });
        assert_eq!(d, IdleAutocompactDecision::Fire);
    }

    #[test]
    fn present_takes_precedence_over_disabled_is_false() {
        // Ordering guard: disabled is checked FIRST. A disabled feature with
        // operator present is still Disabled, not NotIdle.
        let d = decide(IdleInputs {
            enabled: false,
            operator_away: false,
            ..inp()
        });
        assert_eq!(d, IdleAutocompactDecision::Disabled);
    }
}
