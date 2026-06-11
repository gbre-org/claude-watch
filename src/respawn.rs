//! Auto-respawn-on-hang: kill + relaunch the Claude Code dashboard when
//! multiple independent signals indicate the process is hung.
//!
//! Background. claude-watch already detects + emits banners for individual
//! failure modes (heartbeat stale, watcher down, prolonged thinking, wedged
//! pane). On 2026-04-30 the Claude Code main loop wedged silently
//! mid-thread — the dashboard pane showed "Brewing... (23s)" but no
//! progress was being made and watchers had timed out. The existing
//! interrupt-and-inject paths could not unstick it; only a full kill +
//! relaunch of the `claude` process worked, and even that needed manual
//! cleanup of orphaned subagent processes and stale tmux panes.
//!
//! This module implements the "process unresponsive → kill + respawn fresh
//! dashboard" decision path. It deliberately requires MULTIPLE independent
//! signals to fire within a short window before acting — a single signal
//! can be a benign blip (long agent return, single watcher restart, brief
//! API retry), but two or more concurrent failures are diagnostic of a
//! genuinely wedged main loop.
//!
//! Default-OFF. Auto-killing the dashboard is destructive and Andrew opts
//! in explicitly via `[auto_respawn_on_hang] enabled = true` in his
//! `~/.config/claude-watch/config.toml`. The cooldown gate prevents
//! respawn loops if the new dashboard also hangs immediately.

use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use tracing::{info, warn};

/// One observed indicator of "claude-code is hung". Each variant carries
/// just enough metadata for the multi-signal coalescer to decide whether
/// it represents an independent observation or a duplicate.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum HangSignal {
    /// memory-remind has been unable to touch the heartbeat file for
    /// `heartbeat.stale_minutes`. This is the strongest single signal —
    /// when memory-remind itself can't run, the main loop is wedged
    /// hard enough that no tool calls are landing.
    HeartbeatStale,
    /// At least one critical watcher has been confirmed missing for
    /// claude-watch's normal `inject_threshold` cycles, AND a watcher-down
    /// inject was already attempted within the recent window without
    /// recovery (i.e. claude-watch already poked the loop and got no
    /// response).
    WatcherDownPersistent,
    /// claude-watch fired the prolonged-thinking interrupt (Escape +
    /// inject) within the recent window AND the pane is still in the
    /// thinking state when re-checked — the interrupt didn't land, the
    /// loop is genuinely wedged on a single thought.
    ProlongedThinkingNoProgress,
    /// The pane content (capture-pane output) hash has been unchanged for
    /// `pane_unchanged_secs` continuously. Independent of claude-watch's
    /// other interrupt paths — covers cases where claude-code is between
    /// thinking and tool-running states (or the status bar is locked).
    PaneCaptureUnchanged,
    /// claude-watch issued a wedged-pane self-clear within the recent
    /// window AND the pane still shows the wedged banner — the self-clear
    /// command itself didn't get processed, indicating the loop is
    /// rejecting all input.
    WedgedClearNoProgress,
}

impl HangSignal {
    pub fn name(&self) -> &'static str {
        match self {
            HangSignal::HeartbeatStale => "heartbeat_stale",
            HangSignal::WatcherDownPersistent => "watcher_down_persistent",
            HangSignal::ProlongedThinkingNoProgress => "prolonged_thinking_no_progress",
            HangSignal::PaneCaptureUnchanged => "pane_capture_unchanged",
            HangSignal::WedgedClearNoProgress => "wedged_clear_no_progress",
        }
    }
}

/// Configuration for the auto-respawn feature.
///
/// Default `enabled = false` — auto-killing the dashboard is destructive
/// and Andrew must opt in explicitly via the config file.
#[derive(Debug, Deserialize, Clone)]
pub struct AutoRespawnConfig {
    /// Master switch. Default: false (the feature is opt-in).
    #[serde(default)]
    pub enabled: bool,
    /// Number of distinct HangSignal variants that must be observed within
    /// `signal_window_secs` before respawn fires. Default: 2.
    #[serde(default = "default_signals_required")]
    pub signals_required: u32,
    /// Sliding-window length (seconds) over which signals are coalesced.
    /// Older fires are ignored. Default: 300 (5 min).
    #[serde(default = "default_signal_window_secs")]
    pub signal_window_secs: u64,
    /// Minimum interval between successive respawns (seconds). Prevents a
    /// hung newly-spawned dashboard from being respawned again immediately.
    /// Default: 1800 (30 min).
    #[serde(default = "default_respawn_cooldown_secs")]
    pub cooldown_secs: u64,
    /// Seconds between SIGTERM and SIGKILL when killing the old `claude`
    /// process. Default: 5.
    #[serde(default = "default_kill_grace_secs")]
    pub kill_grace_secs: u64,
    /// Seconds to wait after issuing the respawn before declaring it a
    /// failure. The new `claude` process must have appeared in /proc by
    /// then. Default: 30.
    #[serde(default = "default_respawn_verify_secs")]
    pub respawn_verify_secs: u64,
    /// How long the pane capture must stay unchanged before the
    /// PaneCaptureUnchanged signal fires. Default: 600 (10 min) — short
    /// enough to catch real hangs, long enough not to fire on legitimate
    /// long-running thoughts.
    #[serde(default = "default_pane_unchanged_secs")]
    pub pane_unchanged_secs: u64,
    /// Command + argv to launch a fresh Claude Code session after the
    /// hung process tree is killed. Default: `["dashboard", "--detach"]`
    /// — the host's systemd-boot path (bare tmux session +
    /// `claude --continue`).
    ///
    /// Container deployments override this to `["cwsr", "--no-upgrade"]`
    /// so respawn rolls the inner pane via `tmux respawn-pane -k`
    /// instead of trying to invoke the host-side `dashboard` script
    /// (which doesn't exist inside the container — the diagnostic at
    /// claude-watch-container-diagnostic.md flagged this as one of the
    /// host-tmux-bound features that silently no-op'd from the host
    /// daemon's perspective when the operator was actually running
    /// inside the cw container).
    #[serde(default = "default_respawn_command")]
    pub respawn_command: Vec<String>,
}

impl Default for AutoRespawnConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            signals_required: default_signals_required(),
            signal_window_secs: default_signal_window_secs(),
            cooldown_secs: default_respawn_cooldown_secs(),
            kill_grace_secs: default_kill_grace_secs(),
            respawn_verify_secs: default_respawn_verify_secs(),
            pane_unchanged_secs: default_pane_unchanged_secs(),
            respawn_command: default_respawn_command(),
        }
    }
}

fn default_signals_required() -> u32 {
    2
}
fn default_signal_window_secs() -> u64 {
    300
}
fn default_respawn_cooldown_secs() -> u64 {
    1800
}
fn default_kill_grace_secs() -> u64 {
    5
}
fn default_respawn_verify_secs() -> u64 {
    30
}
fn default_pane_unchanged_secs() -> u64 {
    600
}
fn default_respawn_command() -> Vec<String> {
    vec!["dashboard".to_string(), "--detach".to_string()]
}

/// Per-signal observation history. Persisted in State so the
/// signal-window evaluation survives daemon check cycles.
///
/// Stored as a small ring of (signal_name, rfc3339_timestamp) tuples.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct HangSignalHistory {
    /// Most recent observation timestamp per signal name. Older entries
    /// are pruned by `prune_window`.
    pub last_seen: std::collections::HashMap<String, String>,
}

impl HangSignalHistory {
    /// Record that `signal` was observed at `at_iso`.
    pub fn observe(&mut self, signal: &HangSignal, at_iso: &str) {
        self.last_seen
            .insert(signal.name().to_string(), at_iso.to_string());
    }

    /// Drop entries older than `window_secs` measured from `now`. Returns
    /// the count of entries that remain.
    pub fn prune_window(&mut self, now_iso: &str, window_secs: u64) -> usize {
        let now = match chrono::DateTime::parse_from_rfc3339(now_iso) {
            Ok(t) => t.with_timezone(&Utc),
            Err(_) => return self.last_seen.len(),
        };
        self.last_seen.retain(|_name, ts| {
            chrono::DateTime::parse_from_rfc3339(ts)
                .ok()
                .map(|dt| (now - dt.with_timezone(&Utc)).num_seconds().abs() <= window_secs as i64)
                .unwrap_or(false)
        });
        self.last_seen.len()
    }

    /// Distinct signal names currently in the window.
    pub fn distinct_active(&self) -> HashSet<String> {
        self.last_seen.keys().cloned().collect()
    }
}

/// Pure decision: given a signal history (already pruned to the window),
/// the configured threshold, and the count of currently-active subagents,
/// return whether a respawn should fire NOW.
///
/// **Active-subagent guard (added 2026-05-01).** If `active_subagents > 0`,
/// respawn is short-circuited to `false` regardless of all other signals.
/// Rationale: when subagents are running, the main loop looks idle/silent
/// from outside (no tool calls land while the parent waits on the child)
/// but is legitimately blocked on agent work — killing it would also kill
/// the agents and lose their state mid-task. Andrew flagged this 2026-05-01.
/// The guard runs FIRST so even a fully-tripped multi-signal threshold
/// cannot fire while agents are alive.
///
/// `last_respawn_at`, if present, gates the decision against the cooldown.
pub fn should_respawn(
    history: &HangSignalHistory,
    last_respawn_at: Option<&str>,
    now_iso: &str,
    signals_required: u32,
    cooldown_secs: u64,
    active_subagents: u32,
) -> bool {
    // Active-subagent guard — short-circuits regardless of signal state.
    if active_subagents > 0 {
        return false;
    }
    if history.distinct_active().len() < signals_required as usize {
        return false;
    }
    if let Some(last) = last_respawn_at {
        if let (Ok(last_dt), Ok(now_dt)) = (
            chrono::DateTime::parse_from_rfc3339(last),
            chrono::DateTime::parse_from_rfc3339(now_iso),
        ) {
            let elapsed = (now_dt.with_timezone(&Utc) - last_dt.with_timezone(&Utc)).num_seconds();
            if elapsed >= 0 && (elapsed as u64) < cooldown_secs {
                return false;
            }
        }
    }
    true
}

/// Count currently-active subagents of the running Claude Code main process.
///
/// **Heuristic**: a subagent is active iff it appears as a live child PID
/// of the Claude Code main process AND its command does NOT match a known
/// watcher pattern (any `watcher-ctl run <name>` supervisor or one of
/// the built-in `WATCHER_PATTERNS`) AND it is not one of our own
/// introspection commands (agent-ctl / claude-watch agent / ps). This
/// mirrors the existing `agent-ctl list` / `agent-ctl kill-all` logic.
///
/// We deliberately use **live child processes** rather than `agent-*.meta.json`
/// files on disk, because meta.json files persist after the subagent process
/// exits — counting those would over-count and never let the auto-respawn
/// fire after the first agent ever ran in this Claude session. A live
/// process is the single most reliable signal that a subagent is doing work.
///
/// Fail-open: if `find_claude_pid()` returns None (no Claude PID detectable
/// from /proc) or `ps --ppid` fails, this returns 0 and the auto-respawn
/// path proceeds with its existing signal-based decision. The point of the
/// guard is to prevent killing live agents — when we can't see /proc at all,
/// we also can't auto-respawn anyway (`execute_respawn` aborts on missing
/// PID), so 0 is the safe default.
///
/// Cheap: one /proc scan + one `ps --ppid` invocation. Called at most once
/// per check_auto_respawn cycle (default ~5s).
///
/// Production callers pass `None` for `versions_dir_override`, which uses
/// the real `~/.local/share/claude/versions` dir. Tests pass
/// `Some("/nonexistent/path")` to force the fail-open (no-PID) branch
/// without touching real /proc PIDs.
pub fn count_active_subagents_with_versions_dir(versions_dir_override: Option<&str>) -> u32 {
    let claude_pid_opt = match versions_dir_override {
        Some(dir) => crate::agent::find_claude_pid_with_versions_dir(dir),
        None => crate::agent::find_claude_pid(),
    };
    let Some(claude_pid) = claude_pid_opt else {
        // No Claude PID — fail-open to 0 (existing respawn predicate gates).
        return 0;
    };
    let children = crate::agent::get_children(claude_pid);
    let agent_children = children
        .iter()
        .filter(|c| !crate::agent::is_watcher(&c.cmd) && !crate::agent::is_own_command(&c.cmd))
        .count();
    u32::try_from(agent_children).unwrap_or(u32::MAX)
}

/// Result of an attempted respawn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RespawnOutcome {
    /// Old process killed (TERM, escalated to KILL if necessary), tmux
    /// session reset, new dashboard launched, new claude PID observed
    /// in /proc within `respawn_verify_secs`.
    Success { new_pid: Option<u32> },
    /// Cleanup completed but the new dashboard didn't bring up a claude
    /// process within the verify window. We log + alert; the daemon does
    /// NOT loop (the cooldown gate prevents re-fire).
    LaunchFailed,
    /// Something went wrong before we could even attempt the kill
    /// (couldn't find the existing PID, command exec failed, etc.). Old
    /// state is left untouched.
    Aborted { reason: String },
}

/// Compute a stable hash of pane content for the unchanged-pane signal.
/// We use the full string with default Rust hasher — collision probability
/// across two consecutive captures is negligible and false positives only
/// hurt by *failing to fire* a single signal (not by causing an
/// inappropriate respawn).
pub fn hash_pane_content(s: &str) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

/// Execute the cleanup-and-respawn sequence. This is the destructive
/// I/O path. Steps:
///
///   1. Look up the current `claude` PID via `agent::find_claude_pid()`.
///   2. SIGTERM the claude process tree (kill children first, then root).
///   3. Wait `kill_grace_secs` seconds, then SIGKILL anything still alive.
///   4. Kill the dashboard tmux session (`tmux kill-session -t <session>`).
///   5. Run any registered cleanup callbacks (stale state files, pid
///      files). Best-effort — failures are logged but do not abort.
///   6. Spawn a fresh dashboard via `dashboard --detach` (the systemd
///      boot path — bare tmux session + `claude --continue`). Detached
///      via setsid so it survives the daemon being SIGTERMed.
///   7. Poll `agent::find_claude_pid()` for `respawn_verify_secs` seconds.
///      Return Success on detection, LaunchFailed on timeout.
///
/// Logs every step via tracing + (if `claude-event` is on PATH) emits a
/// structured event so Andrew sees a notification.
///
/// Production callers pass `None` for `versions_dir_override`, which uses
/// the real `~/.local/share/claude/versions` dir. Tests pass
/// `Some("/nonexistent/path")` to force the "no claude PID found" abort
/// path without ever scanning /proc for a real Claude process — critical
/// safety: an uninstrumented test would otherwise fire SIGTERM at the
/// live Claude PID running the dev session.
pub async fn execute_respawn_with_versions_dir(
    config: &AutoRespawnConfig,
    dashboard_session: &str,
    versions_dir_override: Option<&str>,
) -> RespawnOutcome {
    let claude_pid_opt = match versions_dir_override {
        Some(dir) => crate::agent::find_claude_pid_with_versions_dir(dir),
        None => crate::agent::find_claude_pid(),
    };
    let claude_pid = match claude_pid_opt {
        Some(pid) => pid,
        None => {
            warn!(
                "auto-respawn: no Claude Code PID found via /proc — \
                aborting (nothing to kill)"
            );
            return RespawnOutcome::Aborted {
                reason: "no claude PID found".into(),
            };
        }
    };

    info!(
        claude_pid,
        "auto-respawn: SIGTERM Claude Code process tree (multi-signal hang detected)"
    );
    let term_killed = crate::agent::kill_process_tree(
        claude_pid,
        nix::sys::signal::Signal::SIGTERM,
    );

    // Wait the grace period for graceful exit.
    tokio::time::sleep(std::time::Duration::from_secs(config.kill_grace_secs)).await;

    // Escalate to SIGKILL on anything still alive.
    let still_alive: Vec<u32> = term_killed
        .iter()
        .filter(|&&pid| crate::agent::is_process_alive(pid))
        .copied()
        .collect();
    if !still_alive.is_empty() {
        warn!(
            still_alive_count = still_alive.len(),
            "auto-respawn: TERM grace expired, escalating to SIGKILL"
        );
        for pid in &still_alive {
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(*pid as i32),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
        // Brief settle so /proc reflects the kills before we start the new session
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }

    // Tear down the old tmux dashboard session so the fresh `dashboard
    // --detach` doesn't trip over a half-dead session of the same name.
    let session = if dashboard_session.is_empty() {
        "dashboard"
    } else {
        dashboard_session
    };
    info!(session, "auto-respawn: tearing down old dashboard tmux session");
    let _ = crate::cmd::run_cmd_any(&["tmux", "kill-session", "-t", session], 5).await;

    // Best-effort cleanup of stale claude-watch state files that can
    // confuse the post-restart resume-inject path. The fresh dashboard
    // will write its own pane-id file. We do NOT delete the heartbeat
    // file — the new claude+memory-remind chain will refresh it.
    let _ = std::fs::remove_file("/var/run/claude/claude-relaunch.sh");

    // Emit a claude-event so Andrew's notification stream picks up the
    // respawn, and so the dashboard's session log gets a breadcrumb.
    let event_msg = format!(
        "[CLAUDE-WATCH] AUTO-RESPAWN: dashboard relaunched after multi-signal hang \
         (pid {} killed)",
        claude_pid
    );
    let _ = crate::cmd::run_cmd_any(
        &[
            "claude-event",
            &event_msg,
            "--tag",
            "auto-respawn",
            "--source",
            "claude-watch",
            "--source-name",
            "respawn",
            "--priority",
            "high",
        ],
        10,
    )
    .await;

    // Launch a fresh Claude Code session via the configured respawn
    // command. Default: `dashboard --detach` (host systemd-boot path —
    // bare tmux session + `claude --continue` with the resume-inject
    // prompt). Container deployments override to `cwsr --no-upgrade`
    // so the in-container claude-watch rolls the inner pane via
    // `tmux respawn-pane -k` instead of trying to shell out to the
    // host-side `dashboard` script (which does not exist inside the
    // container). setsid via spawn_detached so the process survives a
    // daemon SIGTERM mid-respawn.
    let respawn_argv: Vec<&str> = config
        .respawn_command
        .iter()
        .map(|s| s.as_str())
        .collect();
    info!(
        respawn_command = ?config.respawn_command,
        "auto-respawn: launching fresh Claude Code via configured respawn_command"
    );
    if respawn_argv.is_empty() {
        warn!("auto-respawn: respawn_command is empty — aborting launch");
        return RespawnOutcome::LaunchFailed;
    }
    if let Err(e) = spawn_detached(&respawn_argv) {
        warn!(error = %e, "auto-respawn: respawn_command launch failed");
        return RespawnOutcome::LaunchFailed;
    }

    // Verify the new claude PID appears within the verify window.
    let deadline = std::time::Instant::now()
        + std::time::Duration::from_secs(config.respawn_verify_secs);
    while std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        if let Some(new_pid) = crate::agent::find_claude_pid() {
            if new_pid != claude_pid {
                info!(new_pid, "auto-respawn: fresh Claude Code process confirmed");
                return RespawnOutcome::Success {
                    new_pid: Some(new_pid),
                };
            }
        }
    }

    warn!(
        respawn_verify_secs = config.respawn_verify_secs,
        "auto-respawn: fresh Claude Code did not appear within verify window"
    );
    RespawnOutcome::LaunchFailed
}

/// Spawn a child process detached from the daemon's session so it
/// survives a daemon SIGTERM. Same setsid trick used by
/// `spawn_deferred_clear` and `spawn_immediate_clear` in policy.rs.
fn spawn_detached(args: &[&str]) -> std::io::Result<()> {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    if args.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "empty args",
        ));
    }
    let mut cmd = Command::new(args[0]);
    cmd.args(&args[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // SAFETY: setsid() is async-signal-safe.
    unsafe {
        cmd.pre_exec(|| {
            nix::unistd::setsid()
                .map(|_| ())
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))
        });
    }
    cmd.spawn()?;
    Ok(())
}

/// Pure decision: given the current pane hash and the previously-stored
/// hash + first-seen timestamp, return:
///   - the (possibly updated) stored hash and first_seen,
///   - whether the PaneCaptureUnchanged signal should fire NOW.
///
/// The signal fires when (now - first_seen) >= unchanged_secs AND the
/// hash is stable. If the hash changes, first_seen resets to None and we
/// store the new hash so the next stable run is measured from the change.
pub fn evaluate_pane_unchanged(
    current_hash: u64,
    prev_hash: Option<u64>,
    prev_first_seen: Option<&str>,
    now_iso: &str,
    unchanged_secs: u64,
) -> (Option<u64>, Option<String>, bool) {
    if prev_hash != Some(current_hash) {
        // Pane changed — reset
        return (Some(current_hash), Some(now_iso.to_string()), false);
    }
    let first_seen = match prev_first_seen {
        Some(s) => s.to_string(),
        None => return (Some(current_hash), Some(now_iso.to_string()), false),
    };
    let elapsed = match (
        chrono::DateTime::parse_from_rfc3339(&first_seen),
        chrono::DateTime::parse_from_rfc3339(now_iso),
    ) {
        (Ok(a), Ok(b)) => (b.with_timezone(&Utc) - a.with_timezone(&Utc)).num_seconds(),
        _ => 0,
    };
    let fire = elapsed >= 0 && (elapsed as u64) >= unchanged_secs;
    (Some(current_hash), Some(first_seen), fire)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn iso_at(offset_secs: i64) -> String {
        (Utc::now() + Duration::seconds(offset_secs)).to_rfc3339()
    }

    #[test]
    fn signals_required_is_two_by_default() {
        let cfg = AutoRespawnConfig::default();
        assert_eq!(cfg.signals_required, 2);
        assert!(!cfg.enabled, "auto-respawn must default OFF");
    }

    #[test]
    fn respawn_command_defaults_to_dashboard_detach() {
        // Host-side default — preserves the systemd-boot path so
        // upgrade lands the new field as a no-op for existing operators
        // who don't override it. Container deployments override to
        // `["cwsr", "--no-upgrade"]` via /etc/claude-watch/config.toml.
        let cfg = AutoRespawnConfig::default();
        assert_eq!(cfg.respawn_command, vec!["dashboard", "--detach"]);
    }

    #[test]
    fn respawn_command_deserialises_from_toml_array() {
        // Container override shape: an operator-supplied array replaces
        // the default. The container ships
        // `respawn_command = ["cwsr", "--no-upgrade"]` so the daemon
        // rolls the inner pane via tmux respawn-pane instead of
        // shelling out to the host-only `dashboard` script.
        let toml_src = r#"
            enabled = true
            respawn_command = ["cwsr", "--no-upgrade"]
        "#;
        let cfg: AutoRespawnConfig = toml::from_str(toml_src).expect("parse");
        assert_eq!(cfg.respawn_command, vec!["cwsr", "--no-upgrade"]);
        assert!(cfg.enabled);
    }

    #[test]
    fn single_signal_does_not_trigger_respawn() {
        let mut h = HangSignalHistory::default();
        let now = iso_at(0);
        h.observe(&HangSignal::HeartbeatStale, &now);

        let fire = should_respawn(&h, None, &now, 2, 1800, 0);
        assert!(!fire, "one signal alone must NOT trigger respawn");
    }

    #[test]
    fn two_distinct_signals_trigger_respawn() {
        let mut h = HangSignalHistory::default();
        let now = iso_at(0);
        h.observe(&HangSignal::HeartbeatStale, &now);
        h.observe(&HangSignal::WatcherDownPersistent, &now);

        let fire = should_respawn(&h, None, &now, 2, 1800, 0);
        assert!(fire, "two distinct signals MUST trigger respawn");
    }

    #[test]
    fn duplicate_signal_does_not_count_twice() {
        let mut h = HangSignalHistory::default();
        let now = iso_at(0);
        // Same signal observed twice — still counts as ONE distinct signal.
        h.observe(&HangSignal::HeartbeatStale, &iso_at(-30));
        h.observe(&HangSignal::HeartbeatStale, &now);

        let fire = should_respawn(&h, None, &now, 2, 1800, 0);
        assert!(
            !fire,
            "duplicate signal of same kind must not satisfy the threshold"
        );
    }

    #[test]
    fn signals_outside_window_are_pruned() {
        let mut h = HangSignalHistory::default();
        // 10 minutes ago — outside a 5 minute window
        h.observe(&HangSignal::HeartbeatStale, &iso_at(-600));
        h.observe(&HangSignal::WatcherDownPersistent, &iso_at(0));

        let now = iso_at(0);
        h.prune_window(&now, 300);
        assert_eq!(
            h.distinct_active().len(),
            1,
            "stale signal should be pruned"
        );
    }

    #[test]
    fn cooldown_blocks_re_fire() {
        let mut h = HangSignalHistory::default();
        let now = iso_at(0);
        h.observe(&HangSignal::HeartbeatStale, &now);
        h.observe(&HangSignal::WatcherDownPersistent, &now);

        // Last respawn 10 minutes ago, cooldown 30 minutes — still in cooldown.
        let last = iso_at(-600);
        let fire = should_respawn(&h, Some(&last), &now, 2, 1800, 0);
        assert!(!fire, "cooldown should block re-fire");
    }

    #[test]
    fn cooldown_expires_allows_re_fire() {
        let mut h = HangSignalHistory::default();
        let now = iso_at(0);
        h.observe(&HangSignal::HeartbeatStale, &now);
        h.observe(&HangSignal::WatcherDownPersistent, &now);

        // Last respawn 60 minutes ago — past the 30-minute cooldown.
        let last = iso_at(-3600);
        let fire = should_respawn(&h, Some(&last), &now, 2, 1800, 0);
        assert!(fire, "expired cooldown should allow re-fire");
    }

    #[test]
    fn three_signals_trigger_respawn_when_required_two() {
        let mut h = HangSignalHistory::default();
        let now = iso_at(0);
        h.observe(&HangSignal::HeartbeatStale, &now);
        h.observe(&HangSignal::WatcherDownPersistent, &now);
        h.observe(&HangSignal::ProlongedThinkingNoProgress, &now);

        let fire = should_respawn(&h, None, &now, 2, 1800, 0);
        assert!(fire, "exceeding the threshold also fires");
    }

    #[test]
    fn higher_threshold_requires_more_signals() {
        let mut h = HangSignalHistory::default();
        let now = iso_at(0);
        h.observe(&HangSignal::HeartbeatStale, &now);
        h.observe(&HangSignal::WatcherDownPersistent, &now);

        // With required=3, two signals must NOT trigger.
        let fire = should_respawn(&h, None, &now, 3, 1800, 0);
        assert!(!fire, "two signals must not satisfy a 3-signal requirement");
    }

    // -------------------------------------------------------------------
    // Active-subagent guard (2026-05-01).
    //
    // When subagents are alive, the main loop is legitimately blocked on
    // their work — killing it would also kill them and lose state. The
    // guard short-circuits should_respawn() to false even when the
    // multi-signal threshold + cooldown would otherwise fire.
    // -------------------------------------------------------------------

    #[test]
    fn active_subagent_blocks_respawn_even_when_threshold_met() {
        // Scenario mirrors `two_distinct_signals_trigger_respawn` (which
        // returns true with 0 subagents) — the ONLY difference is one
        // active subagent, which must flip the decision to false.
        let mut h = HangSignalHistory::default();
        let now = iso_at(0);
        h.observe(&HangSignal::HeartbeatStale, &now);
        h.observe(&HangSignal::WatcherDownPersistent, &now);

        let fire_with_agent = should_respawn(&h, None, &now, 2, 1800, 1);
        assert!(
            !fire_with_agent,
            "active subagent must veto respawn even when 2 distinct signals fired"
        );

        // Sanity: same scenario with 0 subagents still fires.
        let fire_no_agent = should_respawn(&h, None, &now, 2, 1800, 0);
        assert!(
            fire_no_agent,
            "0 subagents + 2 distinct signals must fire respawn (control)"
        );
    }

    #[test]
    fn many_active_subagents_block_respawn() {
        // Even with three signals AND no cooldown, any positive
        // subagent count must veto the fire.
        let mut h = HangSignalHistory::default();
        let now = iso_at(0);
        h.observe(&HangSignal::HeartbeatStale, &now);
        h.observe(&HangSignal::WatcherDownPersistent, &now);
        h.observe(&HangSignal::ProlongedThinkingNoProgress, &now);

        let fire = should_respawn(&h, None, &now, 2, 1800, 5);
        assert!(!fire, "5 active subagents must block respawn");
    }

    #[test]
    fn zero_subagents_does_not_change_existing_behavior() {
        // Two distinct signals + zero agents → fire (regression sanity:
        // the new parameter must be additive — passing 0 must not break
        // any existing decision path).
        let mut h = HangSignalHistory::default();
        let now = iso_at(0);
        h.observe(&HangSignal::HeartbeatStale, &now);
        h.observe(&HangSignal::WatcherDownPersistent, &now);

        let fire = should_respawn(&h, None, &now, 2, 1800, 0);
        assert!(fire, "0 subagents + threshold met must fire");
    }

    #[test]
    fn agent_guard_runs_before_threshold_check() {
        // Even a single signal (which would NOT satisfy the threshold)
        // produces the same answer (false) regardless of agent count.
        // This isn't a behavioral change — it just confirms the guard
        // doesn't accidentally make a false-firing scenario fire.
        let mut h = HangSignalHistory::default();
        let now = iso_at(0);
        h.observe(&HangSignal::HeartbeatStale, &now);

        assert!(!should_respawn(&h, None, &now, 2, 1800, 0));
        assert!(!should_respawn(&h, None, &now, 2, 1800, 1));
    }

    #[test]
    fn count_active_subagents_no_claude_pid_returns_zero() {
        // Fail-open: when find_claude_pid_with_versions_dir returns None
        // (because the versions_dir doesn't exist on disk), the count
        // helper must return 0, not panic. Production code calls
        // find_claude_pid (default location); this test forces the
        // None path via a guaranteed-non-existent directory so we never
        // touch real /proc PIDs.
        let count = count_active_subagents_with_versions_dir(Some(
            "/nonexistent/claude/versions/test/path",
        ));
        assert_eq!(count, 0, "missing claude PID must yield 0 active subagents");
    }

    #[test]
    fn signal_names_are_stable_strings() {
        // Used for state-file persistence — renaming would break stored
        // histories silently. Pin the names.
        assert_eq!(HangSignal::HeartbeatStale.name(), "heartbeat_stale");
        assert_eq!(
            HangSignal::WatcherDownPersistent.name(),
            "watcher_down_persistent"
        );
        assert_eq!(
            HangSignal::ProlongedThinkingNoProgress.name(),
            "prolonged_thinking_no_progress"
        );
        assert_eq!(
            HangSignal::PaneCaptureUnchanged.name(),
            "pane_capture_unchanged"
        );
        assert_eq!(
            HangSignal::WedgedClearNoProgress.name(),
            "wedged_clear_no_progress"
        );
    }

    #[test]
    fn pane_unchanged_first_observation_does_not_fire() {
        let now = iso_at(0);
        let (h, fs, fire) =
            evaluate_pane_unchanged(0xdeadbeef, None, None, &now, 600);
        assert_eq!(h, Some(0xdeadbeef));
        assert!(fs.is_some());
        assert!(!fire, "first observation should never fire — no elapsed time");
    }

    #[test]
    fn pane_unchanged_fires_after_threshold() {
        let now = iso_at(0);
        let earlier = iso_at(-700);
        let (h, fs, fire) = evaluate_pane_unchanged(
            0xdeadbeef,
            Some(0xdeadbeef),
            Some(&earlier),
            &now,
            600,
        );
        assert_eq!(h, Some(0xdeadbeef));
        assert_eq!(fs, Some(earlier));
        assert!(fire, "after 700s with same hash and 600s threshold, must fire");
    }

    #[test]
    fn pane_unchanged_change_resets_timer() {
        let now = iso_at(0);
        let earlier = iso_at(-700);
        let (h, fs, fire) = evaluate_pane_unchanged(
            0xfeedface, // new hash
            Some(0xdeadbeef),
            Some(&earlier),
            &now,
            600,
        );
        assert_eq!(h, Some(0xfeedface));
        // first_seen should be reset to now
        assert_ne!(fs.as_deref(), Some(earlier.as_str()));
        assert!(!fire, "hash change must reset and not fire");
    }

    #[test]
    fn pane_unchanged_below_threshold_does_not_fire() {
        let now = iso_at(0);
        let earlier = iso_at(-100);
        let (_h, _fs, fire) = evaluate_pane_unchanged(
            0xdeadbeef,
            Some(0xdeadbeef),
            Some(&earlier),
            &now,
            600,
        );
        assert!(!fire, "100s elapsed < 600s threshold => no fire");
    }

    #[test]
    fn hash_is_deterministic_for_same_input() {
        let a = hash_pane_content("line one\nline two\n");
        let b = hash_pane_content("line one\nline two\n");
        assert_eq!(a, b);
    }

    #[test]
    fn hash_differs_for_different_input() {
        let a = hash_pane_content("line one\n");
        let b = hash_pane_content("line two\n");
        assert_ne!(a, b);
    }

    #[test]
    fn config_defaults_match_spec() {
        let cfg = AutoRespawnConfig::default();
        assert!(!cfg.enabled);
        assert_eq!(cfg.signals_required, 2);
        assert_eq!(cfg.signal_window_secs, 300);
        assert_eq!(cfg.cooldown_secs, 1800);
        assert_eq!(cfg.kill_grace_secs, 5);
        assert_eq!(cfg.respawn_verify_secs, 30);
        assert_eq!(cfg.pane_unchanged_secs, 600);
    }
}
