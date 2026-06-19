//! Deployment-mode-aware inject dispatch.
//!
//! The daemon's interruption mechanism has historically been a single path
//! — `tmux::inject_text` types a prompt fragment into the dashboard pane.
//! That works when the agent is hosted in a pty (the common case: native
//! CLI, integrated terminal, workbot container). It silently no-ops when
//! the agent was spawned by an IDE extension with piped stdio (VSCode
//! panel mode), because the pane doesn't contain a running claude — only
//! the extension host's child process tree does, and no descendant has a
//! pty for `send-keys` to land on.
//!
//! This module routes the inject to the right channel based on the agent
//! process's deployment mode (see `proc_util::agent_deployment_mode`):
//!
//! | Mode      | Channel                                       |
//! |-----------|-----------------------------------------------|
//! | Terminal  | `tmux::inject_text` (historical default)      |
//! | IdePanel  | `inject_probe::inject` (pidfd_getfd)          |
//! | Unknown   | tmux if pane present, else claude-event       |
//!
//! ## Limitations
//!
//! The pidfd-inject path APPENDS a user message to the agent's stdin —
//! it does NOT cancel mid-generation the way `tmux send-keys Escape`
//! does for a terminal-mode pane. Operators reviewing the daemon log
//! see this called out explicitly each time the panel-mode path fires:
//! `intervening on panel-mode agent — will append, not interrupt`.
//!
//! ## Error fallback
//!
//! When the agent is classified as `IdePanel` but `pidfd_open` fails
//! with EPERM (kernel.yama.ptrace_scope > 0, cross-uid agent, etc.),
//! the dispatcher emits a claude-event so the main loop has a chance
//! to surface the inability to interrupt — and logs a single one-line
//! warning per agent PID (idempotent across the daemon's lifetime).
//!
//! `Unknown` mode (claude PID not yet resolvable — e.g. the post-boot
//! window where /proc/PID/exe isn't yet the final claude binary, or
//! /proc unreadable) defaults to the historical Terminal behavior when
//! the tmux pane is present: `tmux::inject_text`. A tmux-hosted pane is
//! always a terminal, and the only reason to suppress tmux-inject is a
//! positively-detected IDE panel (the distinct `IdePanel` mode), never
//! `Unknown`. This honors the `proc_util::AgentDeploymentMode::Unknown`
//! contract ("default to Terminal behavior"). Only when NO pane is
//! present (empty spec / pane gone) does the dispatcher fall back to
//! claude-event escalation, since there is then no in-band channel.

use std::collections::HashSet;
use std::sync::Mutex;

use tracing::{info, warn};

use crate::alert;
use crate::event_bus::{ClaudeWatchAlert, Severity};
use crate::inject_probe::{self, ProbeOutcome};
use crate::proc_util::{agent_deployment_mode, AgentDeploymentMode};

/// Backend an inject took. Returned by `dispatch_inject` for callers
/// (and unit tests) that want to verify which channel handled a given
/// call without inspecting side effects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InjectBackend {
    /// Terminal mode — text was written via tmux send-keys.
    Tmux,
    /// IdePanel mode — text was written via pidfd_getfd to the agent
    /// stdin socketpair.
    Pidfd,
    /// IdePanel detected but pidfd write failed; fell through to a
    /// claude-event emit.
    PidfdFailedEvent,
    /// Unknown mode — emitted a claude-event instead of trying any
    /// in-band channel.
    UnknownEvent,
}

/// Trait wrapping the side-effecting channels so unit tests can mock
/// each backend without spinning up tmux or a live agent. Production
/// uses `RealBackends`; tests use a `RecordingBackends` that captures
/// what was called.
#[async_trait::async_trait]
pub trait InjectBackends: Send + Sync {
    /// Call into `tmux::inject_text`.
    async fn tmux_inject(&self, pane: &str, text: &str);
    /// Call into `inject_probe::inject` for a panel-mode agent.
    /// Returns the raw `ProbeOutcome` so the dispatcher can decide
    /// whether to fall through on EPERM / WrongMode.
    fn pidfd_inject(&self, agent_pid: u32, text: &str) -> ProbeOutcome;
    /// Emit a claude-event surfacing the inability to deliver an
    /// in-band interruption. Default impl uses `alert::emit_event`.
    fn emit_event(&self, alert: ClaudeWatchAlert<'_>);
}

/// Production wiring — delegates to the real `tmux`, `inject_probe`,
/// and `alert::emit_event` paths.
pub struct RealBackends;

#[async_trait::async_trait]
impl InjectBackends for RealBackends {
    async fn tmux_inject(&self, pane: &str, text: &str) {
        crate::tmux::inject_text(pane, text).await;
    }
    fn pidfd_inject(&self, agent_pid: u32, text: &str) -> ProbeOutcome {
        inject_probe::inject(agent_pid, text)
    }
    fn emit_event(&self, alert: ClaudeWatchAlert<'_>) {
        alert::emit_event(alert);
    }
}

/// Idempotent log-throttling cache for the per-process "intervening on
/// panel-mode agent" warning. A `Mutex<HashSet<u32>>` is sufficient —
/// the cache is bounded by the number of distinct agent PIDs the daemon
/// touches over its lifetime (small), and contention is negligible
/// (every inject is a slow tmux/pidfd op compared to a HashSet lookup).
static PANEL_WARNED_PIDS: Mutex<Option<HashSet<u32>>> = Mutex::new(None);

/// Reset the warning cache. Test-only: production code never resets;
/// the cache lives for the daemon's lifetime. Callable from tests
/// across the crate boundary so the unit suite can isolate warning
/// throttling tests from one another.
#[cfg(test)]
pub fn reset_panel_warn_cache() {
    if let Ok(mut guard) = PANEL_WARNED_PIDS.lock() {
        *guard = Some(HashSet::new());
    }
}

/// Returns `true` the FIRST time we see this PID; `false` thereafter.
/// Used to gate the "will append, not interrupt" warning so it fires
/// once per process instead of every loop tick.
fn record_and_check_first_warn(pid: u32) -> bool {
    let mut guard = match PANEL_WARNED_PIDS.lock() {
        Ok(g) => g,
        Err(_) => return false, // poisoned; just suppress
    };
    let set = guard.get_or_insert_with(HashSet::new);
    set.insert(pid)
}

/// Resolve a tmux pane spec to the actual claude PID running under it,
/// if any. Returns `None` when:
///   * `pane` is empty (caller passed no pane — happens at boot before
///     dashboard discovery).
///   * tmux returns no pane_pid (pane gone).
///   * No descendant of the pane's shell is a claude binary (panel-
///     mode setup; claude is a child of the IDE extension host, not
///     of any tmux pane).
///
/// The fail-mode is deliberately "treat unknown as Unknown" — the
/// dispatcher then takes the claude-event path. We never guess a PID.
pub async fn resolve_agent_pid_for_pane(pane: &str) -> Option<u32> {
    if pane.is_empty() {
        return None;
    }
    let pane_pid = crate::tmux::get_pane_pid(pane).await?;
    // Depth 5 mirrors the existing `is_service_process` walk; the
    // claude binary is usually the pane's grandchild (shell → claude),
    // sometimes great-grandchild via `bash -c "claude ..."`.
    crate::agent::find_claude_pid_in_tree(pane_pid, 5)
}

/// Decide the deployment mode for a (pane, agent_pid) pair. Pure
/// w.r.t. the agent_pid input — pulls /proc state via `proc_util`.
/// `None` agent_pid yields `Unknown`.
pub fn mode_for(agent_pid: Option<u32>) -> AgentDeploymentMode {
    match agent_pid {
        Some(pid) => agent_deployment_mode(&pid.to_string()),
        None => AgentDeploymentMode::Unknown,
    }
}

/// Core dispatch — takes pre-resolved mode + agent PID + backends and
/// performs the inject. Pulled out of `inject_to_agent` so the mode
/// dispatch can be exercised in unit tests without mocking proc-walks.
pub async fn dispatch_inject(
    backends: &dyn InjectBackends,
    pane: &str,
    text: &str,
    mode: AgentDeploymentMode,
    agent_pid: Option<u32>,
    pane_present: bool,
) -> InjectBackend {
    match mode {
        AgentDeploymentMode::Terminal => {
            backends.tmux_inject(pane, text).await;
            InjectBackend::Tmux
        }
        AgentDeploymentMode::IdePanel => {
            // `IdePanel` is only ever returned when proc_util resolved
            // an agent PID, so `agent_pid` is Some here. Defensive
            // fallback to claude-event keeps the function total.
            let pid = match agent_pid {
                Some(p) => p,
                None => {
                    emit_unknown_event(backends, pane);
                    return InjectBackend::UnknownEvent;
                }
            };
            // Surface the limitation: pidfd APPENDS, does not cancel
            // mid-generation. Throttle to one log per process so the
            // operator log isn't spammed every tick.
            if record_and_check_first_warn(pid) {
                info!(
                    agent_pid = pid,
                    pane = %pane,
                    "intervening on panel-mode agent — will append, not interrupt"
                );
            }
            match backends.pidfd_inject(pid, text) {
                ProbeOutcome::Ok { .. } => InjectBackend::Pidfd,
                ProbeOutcome::SyscallFailed { stage, errno, ref msg }
                    if stage == "pidfd_open" =>
                {
                    warn!(
                        agent_pid = pid,
                        stage = stage,
                        errno = errno,
                        err = msg,
                        "panel-mode inject pidfd_open failed (EPERM / ptrace_scope?); falling back to claude-event escalation"
                    );
                    emit_panel_fallback_event(backends, pane, pid, errno);
                    InjectBackend::PidfdFailedEvent
                }
                other => {
                    warn!(
                        agent_pid = pid,
                        outcome = ?other,
                        "panel-mode inject failed (non-EPERM); falling back to claude-event escalation"
                    );
                    emit_panel_fallback_event(backends, pane, pid, -1);
                    InjectBackend::PidfdFailedEvent
                }
            }
        }
        AgentDeploymentMode::Unknown => {
            // `Unknown` means we couldn't positively resolve the claude
            // PID (e.g. during the post-boot window where /proc/PID/exe
            // isn't yet the final claude binary while node bootstraps).
            // Per the proc_util::AgentDeploymentMode::Unknown contract,
            // Unknown should default to Terminal behavior — it's strictly
            // broader than IdePanel, and a tmux-hosted pane is always a
            // terminal. The only legitimate reason to suppress tmux inject
            // is a positively-detected IDE panel (pty=false + SSE_PORT),
            // which is the distinct `IdePanel` mode, never `Unknown`. So
            // when the pane exists, honor the historical Terminal default
            // and inject via tmux rather than escalating to a claude-event
            // that never types the prompt into the pane.
            if pane_present {
                info!(
                    pane = %pane,
                    "agent deployment mode unknown but pane present; defaulting to tmux inject (Terminal contract)"
                );
                backends.tmux_inject(pane, text).await;
                InjectBackend::Tmux
            } else {
                info!(
                    pane = %pane,
                    "agent deployment mode unknown and no pane present; falling back to claude-event escalation (no in-band channel)"
                );
                emit_unknown_event(backends, pane);
                InjectBackend::UnknownEvent
            }
        }
    }
}

/// Convenience: full pipeline — resolve agent PID, pick mode, dispatch.
/// This is the function callers in `policy.rs` / `alert.rs` should use.
pub async fn inject_to_agent(pane: &str, text: &str) {
    let backends = RealBackends;
    let agent_pid = resolve_agent_pid_for_pane(pane).await;
    let mode = mode_for(agent_pid);
    // "Pane present" = a non-empty pane spec that tmux still tracks as a
    // live pane. This is DECOUPLED from the agent-PID probe on purpose:
    // both `mode` (via `resolve_agent_pid_for_pane` -> `get_pane_pid` ->
    // `find_claude_pid_in_tree`) and the older pane_present derivation
    // shared the same `get_pane_pid` query, which stalls/returns None when
    // the in-pane claude loop is wedged -- the very condition we are trying
    // to recover from. When that probe failed, mode collapsed to Unknown
    // AND pane_present collapsed to false together, so the Unknown arm
    // concluded "no in-band channel" and emitted an unactionable
    // `inject-dispatch-unknown-mode` event instead of attempting the
    // `tmux send-keys` that recovers the pane. `tmux::pane_exists` asks a
    // question independent of the pane's process state (does a pane with
    // this id exist?), with a one-shot retry, so a wedged-but-live pane is
    // correctly reported present and the Unknown arm takes the tmux
    // Terminal recovery path. See the `AgentDeploymentMode::Unknown`
    // contract and `tmux::pane_exists`.
    let pane_present = crate::tmux::pane_exists(pane).await;
    let _ = dispatch_inject(&backends, pane, text, mode, agent_pid, pane_present).await;
}

fn emit_panel_fallback_event(
    backends: &dyn InjectBackends,
    pane: &str,
    agent_pid: u32,
    errno: i32,
) {
    let stuck_reason = format!(
        "panel-mode inject failed (agent_pid={}, errno={})",
        agent_pid, errno
    );
    let message = format!(
        "claude-watch: panel-mode agent inject failed on pane {}. Operator must intervene manually.",
        pane
    );
    backends.emit_event(ClaudeWatchAlert {
        alert_type: "inject-dispatch-failed",
        stuck_reason: &stuck_reason,
        stale_minutes: None,
        affected_watchers: vec![],
        severity: Severity::High,
        message: &message,
    });
}

fn emit_unknown_event(backends: &dyn InjectBackends, pane: &str) {
    let message = format!(
        "claude-watch: cannot interrupt agent on pane {} — deployment mode unknown.",
        pane
    );
    backends.emit_event(ClaudeWatchAlert {
        alert_type: "inject-dispatch-unknown-mode",
        stuck_reason: "agent deployment mode unknown; no in-band channel available",
        stale_minutes: None,
        affected_watchers: vec![],
        severity: Severity::Medium,
        message: &message,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

    /// Recording backends used by every dispatch test. Captures which
    /// backend was called and the args, so assertions stay declarative.
    struct RecordingBackends {
        calls: StdMutex<Vec<Call>>,
        pidfd_outcome: ProbeOutcome,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Call {
        Tmux { pane: String, text: String },
        Pidfd { pid: u32, text: String },
        Event { alert_type: String },
    }

    impl RecordingBackends {
        fn new(pidfd_outcome: ProbeOutcome) -> Self {
            Self {
                calls: StdMutex::new(Vec::new()),
                pidfd_outcome,
            }
        }
        fn ok_pidfd() -> Self {
            Self::new(ProbeOutcome::Ok {
                bytes: 42,
                parent_pid: 1234,
                parent_fd: 5,
            })
        }
        fn calls(&self) -> Vec<Call> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl InjectBackends for RecordingBackends {
        async fn tmux_inject(&self, pane: &str, text: &str) {
            self.calls.lock().unwrap().push(Call::Tmux {
                pane: pane.to_string(),
                text: text.to_string(),
            });
        }
        fn pidfd_inject(&self, agent_pid: u32, text: &str) -> ProbeOutcome {
            self.calls.lock().unwrap().push(Call::Pidfd {
                pid: agent_pid,
                text: text.to_string(),
            });
            // Clone the configured outcome. ProbeOutcome doesn't impl
            // Clone (it's a Serialize-only diagnostic enum), so we
            // reconstruct by-variant.
            clone_outcome(&self.pidfd_outcome)
        }
        fn emit_event(&self, alert: ClaudeWatchAlert<'_>) {
            self.calls.lock().unwrap().push(Call::Event {
                alert_type: alert.alert_type.to_string(),
            });
        }
    }

    fn clone_outcome(o: &ProbeOutcome) -> ProbeOutcome {
        match o {
            ProbeOutcome::Ok {
                bytes,
                parent_pid,
                parent_fd,
            } => ProbeOutcome::Ok {
                bytes: *bytes,
                parent_pid: *parent_pid,
                parent_fd: *parent_fd,
            },
            ProbeOutcome::WrongMode { stdin_target } => ProbeOutcome::WrongMode {
                stdin_target: stdin_target.clone(),
            },
            ProbeOutcome::AgentUnreadable { reason } => ProbeOutcome::AgentUnreadable {
                reason: reason.clone(),
            },
            ProbeOutcome::ParentFdNotFound {
                agent_pid,
                expected_inode,
            } => ProbeOutcome::ParentFdNotFound {
                agent_pid: *agent_pid,
                expected_inode: *expected_inode,
            },
            ProbeOutcome::SyscallFailed { stage, errno, msg } => ProbeOutcome::SyscallFailed {
                stage: *stage,
                errno: *errno,
                msg: msg.clone(),
            },
        }
    }

    // Tests that exercise the dispatch path share the global
    // PANEL_WARNED_PIDS cache and must run serially within a single
    // test binary process. (nextest spawns each test in its own
    // process so the lock is a no-op there; cargo-test's built-in
    // runner shares process state, hence the explicit lock.) The
    // pure mode-helper / cache unit tests below don't take this lock
    // — they only check single-PID invariants on a freshly reset
    // cache.
    static SERIAL: StdMutex<()> = StdMutex::new(());

    fn reset_for_test() -> std::sync::MutexGuard<'static, ()> {
        let guard = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        reset_panel_warn_cache();
        guard
    }

    #[tokio::test]
    async fn dispatch_terminal_routes_to_tmux() {
        let _g = reset_for_test();
        let b = RecordingBackends::ok_pidfd();
        let out = dispatch_inject(
            &b,
            "dashboard:0.0",
            "hello",
            AgentDeploymentMode::Terminal,
            Some(9999),
            true,
        )
        .await;
        assert_eq!(out, InjectBackend::Tmux);
        assert_eq!(
            b.calls(),
            vec![Call::Tmux {
                pane: "dashboard:0.0".to_string(),
                text: "hello".to_string()
            }]
        );
    }

    #[tokio::test]
    async fn dispatch_ide_panel_routes_to_pidfd() {
        let _g = reset_for_test();
        let b = RecordingBackends::ok_pidfd();
        let out = dispatch_inject(
            &b,
            "dashboard:0.0",
            "hello",
            AgentDeploymentMode::IdePanel,
            Some(4321),
            true,
        )
        .await;
        assert_eq!(out, InjectBackend::Pidfd);
        assert_eq!(
            b.calls(),
            vec![Call::Pidfd {
                pid: 4321,
                text: "hello".to_string()
            }]
        );
    }

    #[tokio::test]
    async fn dispatch_unknown_no_pane_routes_to_event() {
        let _g = reset_for_test();
        let b = RecordingBackends::ok_pidfd();
        let out = dispatch_inject(
            &b,
            "dashboard:0.0",
            "hello",
            AgentDeploymentMode::Unknown,
            None,
            false,
        )
        .await;
        assert_eq!(out, InjectBackend::UnknownEvent);
        assert_eq!(
            b.calls(),
            vec![Call::Event {
                alert_type: "inject-dispatch-unknown-mode".to_string()
            }]
        );
    }

    #[tokio::test]
    async fn dispatch_unknown_with_pane_present_routes_to_tmux() {
        // Load-bearing regression: during the post-boot window the claude
        // PID isn't yet resolvable, so mode_for() yields Unknown — but the
        // tmux pane exists. Per the AgentDeploymentMode::Unknown contract
        // ("default to Terminal behavior"), this MUST tmux-inject the
        // resume prompt, not escalate to a claude-event that never types
        // into the pane.
        let _g = reset_for_test();
        let b = RecordingBackends::ok_pidfd();
        let out = dispatch_inject(
            &b,
            "claude-container:0.0",
            "hello",
            AgentDeploymentMode::Unknown,
            None,
            true,
        )
        .await;
        assert_eq!(out, InjectBackend::Tmux);
        assert_eq!(
            b.calls(),
            vec![Call::Tmux {
                pane: "claude-container:0.0".to_string(),
                text: "hello".to_string()
            }]
        );
    }

    #[tokio::test]
    async fn dispatch_ide_panel_eperm_falls_through_to_event() {
        let _g = reset_for_test();
        let b = RecordingBackends::new(ProbeOutcome::SyscallFailed {
            stage: "pidfd_open",
            errno: 1, // EPERM
            msg: "Operation not permitted".to_string(),
        });
        let out = dispatch_inject(
            &b,
            "dashboard:0.0",
            "hello",
            AgentDeploymentMode::IdePanel,
            Some(8765),
            true,
        )
        .await;
        assert_eq!(out, InjectBackend::PidfdFailedEvent);
        // pidfd was attempted, then event was emitted
        let calls = b.calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(
            calls[0],
            Call::Pidfd {
                pid: 8765,
                text: "hello".to_string()
            }
        );
        assert_eq!(
            calls[1],
            Call::Event {
                alert_type: "inject-dispatch-failed".to_string()
            }
        );
    }

    #[tokio::test]
    async fn dispatch_ide_panel_other_failure_falls_through_to_event() {
        let _g = reset_for_test();
        // ParentFdNotFound is NOT pidfd_open EPERM — still falls
        // through (covers the "non-EPERM" arm).
        let b = RecordingBackends::new(ProbeOutcome::ParentFdNotFound {
            agent_pid: 8765,
            expected_inode: 12345,
        });
        let out = dispatch_inject(
            &b,
            "dashboard:0.0",
            "hello",
            AgentDeploymentMode::IdePanel,
            Some(8765),
            true,
        )
        .await;
        assert_eq!(out, InjectBackend::PidfdFailedEvent);
    }

    #[tokio::test]
    async fn dispatch_ide_panel_with_none_agent_pid_emits_unknown() {
        let _g = reset_for_test();
        // Defensive path — IdePanel without a PID is the dispatcher
        // signalling "I lost the PID between mode_for() and dispatch".
        // Emit an Unknown-style event rather than panicking.
        let b = RecordingBackends::ok_pidfd();
        let out = dispatch_inject(
            &b,
            "dashboard:0.0",
            "hello",
            AgentDeploymentMode::IdePanel,
            None,
            true,
        )
        .await;
        assert_eq!(out, InjectBackend::UnknownEvent);
        assert_eq!(
            b.calls(),
            vec![Call::Event {
                alert_type: "inject-dispatch-unknown-mode".to_string()
            }]
        );
    }

    #[test]
    fn panel_warn_cache_first_call_returns_true() {
        let _g = reset_for_test();
        assert!(record_and_check_first_warn(111));
        // Second call for the same PID returns false (already warned).
        assert!(!record_and_check_first_warn(111));
    }

    #[test]
    fn panel_warn_cache_per_pid() {
        let _g = reset_for_test();
        assert!(record_and_check_first_warn(111));
        // Different PID — separate slot, still first-time.
        assert!(record_and_check_first_warn(222));
        // Both already warned now.
        assert!(!record_and_check_first_warn(111));
        assert!(!record_and_check_first_warn(222));
    }

    #[test]
    fn panel_warn_cache_reset_clears_state() {
        let _g = reset_for_test();
        assert!(record_and_check_first_warn(111));
        // Reset the cache directly without re-taking the serial lock
        // (we already hold it via `_g`).
        reset_panel_warn_cache();
        // After reset the same PID is "first-time" again.
        assert!(record_and_check_first_warn(111));
    }

    #[test]
    fn mode_for_none_pid_is_unknown() {
        let m = mode_for(None);
        assert_eq!(m, AgentDeploymentMode::Unknown);
    }

    #[test]
    fn mode_for_bogus_pid_is_unknown() {
        // A PID that almost certainly doesn't exist resolves to Unknown
        // (proc_util::agent_deployment_mode falls through to Unknown).
        let m = mode_for(Some(99_999_999));
        assert_eq!(m, AgentDeploymentMode::Unknown);
    }
}
