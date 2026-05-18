//! Configuration structs and TOML loading.

use serde::Deserialize;

#[derive(Debug, Deserialize, Clone)]
pub struct Config {
    pub general: GeneralConfig,
    #[serde(default)]
    pub tmux: TmuxConfig,
    pub claude: ClaudeConfig,
    pub dead_process: DeadProcessConfig,
    pub fresh_clear: FreshClearConfig,
    pub heartbeat: HeartbeatConfig,
    pub alerts: AlertsConfig,
    pub foreground_monitor: ForegroundMonitorConfig,
    pub watcher_monitor: WatcherMonitorConfig,
    pub context_monitor: ContextMonitorConfig,
    #[serde(default)]
    pub auto_update: AutoUpdateConfig,
    #[serde(default)]
    pub reauth: ReauthConfig,
    #[serde(default)]
    pub task_watch: TaskWatchConfig,
    #[serde(default)]
    pub hybrid: HybridConfig,
    #[serde(default)]
    pub suppression: SuppressionConfig,
    #[serde(default)]
    pub api_retry: ApiRetryConfig,
    /// Auto-respawn-on-hang. Default off; opt in via config to allow
    /// claude-watch to kill + relaunch the dashboard when multiple
    /// independent signals indicate Claude Code is wedged. See
    /// `crate::respawn` for the design.
    #[serde(default)]
    pub auto_respawn_on_hang: crate::respawn::AutoRespawnConfig,
    /// Stuck-detection suppression knobs. Default-on, sensible defaults
    /// so existing configs work without edits. See `StuckDetectionConfig`.
    #[serde(default)]
    pub stuck_detection: StuckDetectionConfig,
}

#[derive(Debug, Deserialize, Clone)]
pub struct GeneralConfig {
    pub check_interval: u64,
    pub state_file: String,
    pub log_file: String,
    pub legacy_log_file: String,
    /// Global post-interrupt cooldown (seconds). After ANY interrupt fires
    /// (prolonged-thinking, watcher-down, context-warning), suppress all
    /// new interrupts for this many seconds. Prevents cascading interrupts
    /// where e.g. a watcher-down interrupt fires mid-thought, resets the
    /// thinking timer, and a prolonged-thinking interrupt fires immediately
    /// on the newly-started thought. 0 disables the gate.
    #[serde(default = "default_post_interrupt_cooldown_secs")]
    pub post_interrupt_cooldown_secs: u64,
}

fn default_post_interrupt_cooldown_secs() -> u64 {
    60
}

#[derive(Debug, Default, Deserialize, Clone)]
pub struct TmuxConfig {
    /// Known dashboard pane for Claude Code (e.g. "dashboard:0.2").
    /// Empty string = auto-detect via find_claude_pane().
    #[serde(default)]
    pub dashboard_pane: String,
    /// Tmux session name where Claude Code runs (e.g. "dashboard").
    /// Empty string = auto-detect via find_claude_pane().
    #[serde(default)]
    pub dashboard_session: String,
    /// Settle delay (milliseconds) inserted between the ESC -> NORMAL-mode
    /// transition and the dd/i/text sequence inside `inject_text`. Default:
    /// 0 (disabled — fast path). Tune up only if a particular environment
    /// shows follow-up keystrokes being garbled or eaten because Claude
    /// Code's pane hasn't finished processing the Escape before the next
    /// keys arrive. Most setups don't need this — the ESC loop's
    /// per-iteration `is_insert_mode()` check already confirms each
    /// Escape was processed before the next is sent (and PR #46 adds
    /// explicit INSERT-mode verification after the `i` keystroke).
    #[serde(default)]
    pub post_escape_settle_ms: u64,
}

#[derive(Debug, Deserialize, Clone)]
pub struct ClaudeConfig {
    pub max_context_tokens: u64,
    pub heartbeat_file: String,
    pub relaunch_script: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct DeadProcessConfig {
    pub checks_required: u32,
    pub restart_cooldown: u64,
    /// After this many dead checks without a shell prompt, check if Claude Code's
    /// idle prompt is visible and inject "resume" to kick-start a fresh session.
    /// This handles the case where dashboard --fresh launches Claude Code externally
    /// (not via claude-watch restart), so pending_resume_inject is never set.
    #[serde(default = "default_fresh_inject_checks")]
    pub fresh_inject_checks: u32,
    /// When true, suppress the `restart_claude` action (and the
    /// `claude-crashed` alert it fires) when the main loop is actively
    /// turning — a tool call ran within `active_window_secs`. The
    /// `tokens == 0 && bashes == 0` predicate is point-in-time and can
    /// briefly satisfy during a tmux pane swap, status-parser miss, or
    /// the gap between two tool calls; restarting Claude in those moments
    /// kills an active session. The shell-prompt confirmation is the
    /// other safety belt and remains required, but a recent tool call is
    /// equally strong evidence the process is alive. Default: true.
    #[serde(default = "default_suppress_when_active")]
    pub suppress_when_active: bool,
    /// Window (seconds) of recent tool-call activity that counts as
    /// "actively turning" for `suppress_when_active`. If `bashes > 0` was
    /// last observed within this many seconds, the restart is suppressed.
    /// Default: 60 — wider than the watcher-down window because a
    /// dead-process false positive is more destructive (kills a live
    /// Claude Code session) than a missed inject.
    #[serde(default = "default_dead_process_active_window_secs")]
    pub active_window_secs: u64,
}

fn default_fresh_inject_checks() -> u32 {
    5 // ~60s at 12s intervals
}

fn default_dead_process_active_window_secs() -> u64 {
    60
}

#[derive(Debug, Deserialize, Clone)]
pub struct FreshClearConfig {
    pub min_tokens: u64,
    pub max_tokens: u64,
    pub detections_required: u32,
    pub cooldown: u64,
    /// When true, suppress the `fresh-clear-stuck` alert and inject when
    /// the main loop is actively turning. The token range
    /// `[min_tokens, max_tokens)` overlaps with normal mid-turn token
    /// counts (a small turn that has just received a few thousand tokens
    /// from a tool call), and `bashes == 0` is point-in-time and can be
    /// transiently true between two tool calls. Without this gate the
    /// alert fires while Claude is mid-turn and injects "resume" into
    /// active work. Default: true.
    #[serde(default = "default_suppress_when_active")]
    pub suppress_when_active: bool,
    /// Window (seconds) of recent tool-call activity that counts as
    /// "actively turning" for `suppress_when_active`. If `bashes > 0`
    /// was last observed within this many seconds, the inject is
    /// suppressed. Default: 60 — wider than the watcher-down window
    /// because a fresh-clear false positive injects "resume" mid-turn,
    /// which derails the active task.
    #[serde(default = "default_fresh_clear_active_window_secs")]
    pub active_window_secs: u64,
}

fn default_suppress_when_active() -> bool {
    true
}

fn default_fresh_clear_active_window_secs() -> u64 {
    60
}

#[derive(Debug, Deserialize, Clone)]
pub struct HeartbeatConfig {
    pub stale_minutes: u64,
}

#[derive(Debug, Deserialize, Clone)]
#[allow(dead_code)]
pub struct AlertsConfig {
    pub initial_cooldown: u64,
    pub escalation_tiers: Vec<u64>,
    pub max_pingme_alerts: u32,
    pub resume_prompt: String,
}

#[derive(Debug, Deserialize, Clone)]
#[allow(dead_code)]
pub struct ForegroundMonitorConfig {
    pub enabled: bool,
    pub threshold_seconds: u64,
    pub check_interval: u64,
    #[serde(default = "default_interrupt_enabled")]
    pub interrupt_enabled: bool,
    #[serde(default = "default_interrupt_message")]
    pub interrupt_message: String,
    /// Maximum backoff for thinking interrupts in seconds (default: 960 = 16 min)
    #[serde(default = "default_max_thinking_backoff")]
    pub max_thinking_backoff: u64,
    /// Multiplier applied to the thinking-interrupt threshold on each
    /// successive interrupt. With base=300 and multiplier=3 the sequence
    /// is 300, 900, 2700 (capped at max_thinking_backoff). Default 2 preserves
    /// the original doubling behaviour.
    #[serde(default = "default_thinking_backoff_multiplier")]
    pub thinking_backoff_multiplier: u64,
}

fn default_interrupt_enabled() -> bool {
    true
}

fn default_interrupt_message() -> String {
    "The foreground command was backgrounded by claude-watch because it exceeded the timeout. Use run_in_background for long commands.".to_string()
}

fn default_max_thinking_backoff() -> u64 {
    960 // 16 minutes
}

fn default_thinking_backoff_multiplier() -> u64 {
    2
}

#[derive(Debug, Deserialize, Clone)]
#[allow(dead_code)]
pub struct WatcherMonitorConfig {
    pub enabled: bool,
    pub watchers_config: String,
    pub expected_watchmen: u32,
    /// Consecutive missing checks before injecting a restart prompt (default: 6 = ~60s)
    #[serde(default = "default_watcher_inject_threshold")]
    pub inject_threshold: u32,
    /// Cooldown in seconds between watcher-missing injections (default: 60).
    /// Tightened 300 -> 60 on 2026-04-28: a down watcher is a hard liveness
    /// failure (no signal, no events, no torrents getting through), so when
    /// the previous inject didn't land we want to re-inject quickly rather
    /// than wait 5 minutes while the user is silent.
    #[serde(default = "default_watcher_inject_cooldown")]
    pub inject_cooldown: u64,
    /// When true, suppress the tmux-INJECT (interrupt + prompt) part of
    /// the watcher-down alert when the main loop is actively turning
    /// (a tool call is running, or one ran within `active_window_secs`).
    /// The structured claude-event STILL fires so Andrew is notified
    /// out-of-band — only the in-pane preemption is skipped. Heartbeat-
    /// stale and other alert paths are unaffected. Default: true.
    #[serde(default = "default_suppress_inject_when_active")]
    pub suppress_inject_when_active: bool,
    /// Window (seconds) of recent tool-call activity that counts as
    /// "actively turning" for the purposes of `suppress_inject_when_active`.
    /// If `bashes > 0` was last observed within this many seconds, the
    /// watcher-down INJECT is suppressed. Default: 30.
    #[serde(default = "default_active_window_secs")]
    pub active_window_secs: u64,
    /// Grace period (seconds) after `last_seen_running` during which a
    /// missing watcher is NOT counted toward `consecutive_missing`. Short-
    /// lived watchers (e.g. a `*-wait` watcher that exits when an event
    /// arrives) have a natural gap between exit and the main loop's
    /// restart, so we
    /// avoid firing spurious "watcher missing" alerts every time a message
    /// arrives. Default: 90 seconds. Lowered to 0 in e2e tests so a freshly
    /// killed watcher fires within the inject_threshold window.
    #[serde(default = "default_watcher_grace_secs")]
    pub grace_secs: u64,
    /// Quiet path (PR #48): emit a `watcher-down` claude-event after this
    /// many consecutive missing checks. Set lower than `inject_threshold`
    /// so the quiet path runs first; the heavyweight tmux-inject is the
    /// fallback. Default: 3 (~30s at 10s interval).
    #[serde(default = "default_watcher_event_threshold")]
    pub event_threshold: u32,
    /// Grace period (seconds) after a `watcher-down` event has been emitted
    /// during which the tmux-inject path is suppressed. If the watcher is
    /// still down after this many seconds, fall through to the inject path
    /// as a fallback. Default: 60.
    #[serde(default = "default_watcher_event_grace_secs")]
    pub event_grace_secs: u64,
    /// Path to the `claude-event` CLI used by the quiet path. Defaults to
    /// `claude-event` (resolved via $PATH). Override for tests or non-standard
    /// installs.
    #[serde(default = "default_watcher_event_command")]
    pub event_command: String,
    /// The watcher name that consumes claude-events. If THIS watcher goes
    /// down, the quiet path is useless (no consumer) and we fall straight
    /// through to the tmux-inject path. Default: "claude-event-watch".
    #[serde(default = "default_watcher_event_consumer_name")]
    pub event_consumer_watcher_name: String,
}

fn default_watcher_inject_threshold() -> u32 {
    6
}

fn default_watcher_inject_cooldown() -> u64 {
    60
}

fn default_watcher_grace_secs() -> u64 {
    90
}

fn default_suppress_inject_when_active() -> bool {
    true
}

fn default_active_window_secs() -> u64 {
    30
}

fn default_watcher_event_threshold() -> u32 {
    3
}

fn default_watcher_event_grace_secs() -> u64 {
    60
}

fn default_watcher_event_command() -> String {
    "claude-event".to_string()
}

fn default_watcher_event_consumer_name() -> String {
    "claude-event-watch".to_string()
}

#[derive(Debug, Deserialize, Clone)]
pub struct AutoUpdateConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Minute of hour (0-59) to check for updates (default: 10)
    #[serde(default = "default_check_minute")]
    pub check_minute: u32,
    /// Minimum hours between update attempts (default: 1)
    #[serde(default = "default_cooldown_hours")]
    pub cooldown_hours: u64,
    /// Resume prompt injected after update restart
    #[serde(default = "default_update_resume_prompt")]
    pub resume_prompt: String,
}

impl Default for AutoUpdateConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            check_minute: default_check_minute(),
            cooldown_hours: default_cooldown_hours(),
            resume_prompt: default_update_resume_prompt(),
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct ReauthConfig {
    #[serde(default = "default_reauth_enabled")]
    pub enabled: bool,
    /// Interval between repeated reauth alerts in seconds (default: 10800 = 3 hours)
    #[serde(default = "default_reauth_alert_interval")]
    pub alert_interval_seconds: u64,
}

impl Default for ReauthConfig {
    fn default() -> Self {
        Self {
            enabled: default_reauth_enabled(),
            alert_interval_seconds: default_reauth_alert_interval(),
        }
    }
}

fn default_reauth_enabled() -> bool {
    true
}

fn default_reauth_alert_interval() -> u64 {
    10800 // 3 hours
}

#[derive(Debug, Deserialize, Clone)]
pub struct TaskWatchConfig {
    #[serde(default = "default_tw_enabled")]
    pub enabled: bool,
    #[serde(default = "default_tw_session")]
    pub session: String,
    #[serde(default = "default_tw_poll_interval")]
    pub poll_interval: u64,
    #[serde(default = "default_tw_done_delay")]
    pub done_delay: u64,
    #[serde(default = "default_tw_agent_done_delay")]
    pub agent_done_delay: u64,
    #[serde(default = "default_tw_max_panes")]
    pub max_panes: usize,
    #[serde(default)]
    pub show_all: bool,
    /// Override tasks directory for testing (bypasses find_tasks_dir discovery).
    #[serde(skip)]
    pub tasks_dir_override: Option<std::path::PathBuf>,
}

impl Default for TaskWatchConfig {
    fn default() -> Self {
        Self {
            enabled: default_tw_enabled(),
            session: default_tw_session(),
            poll_interval: default_tw_poll_interval(),
            done_delay: default_tw_done_delay(),
            agent_done_delay: default_tw_agent_done_delay(),
            max_panes: default_tw_max_panes(),
            show_all: false,
            tasks_dir_override: None,
        }
    }
}

fn default_tw_enabled() -> bool {
    false
}

fn default_tw_session() -> String {
    "tasks".to_string()
}

fn default_tw_poll_interval() -> u64 {
    5
}

fn default_tw_done_delay() -> u64 {
    10
}

fn default_tw_agent_done_delay() -> u64 {
    120
}

fn default_tw_max_panes() -> usize {
    20
}

fn default_check_minute() -> u32 {
    10
}

fn default_cooldown_hours() -> u64 {
    1
}

fn default_update_resume_prompt() -> String {
    // Post-restart injection: bare "resume" wasn't enough — after a
    // claude-watch auto-update restart, any subagent that was running
    // at /exit time is orphaned (its tmux pane is gone, its
    // `claude-watch active-agents` entry is stale), but its queue item
    // is still marked running and its PR (if any) is still open. The
    // main loop has to discover those orphans before resuming normal
    // dispatch, or in-flight PR-shipping work sits unmerged until
    // something external (WorkQueueOrphaned alert, Andrew) flags it.
    //
    // This prompt directs the main loop to: (1) run its normal
    // session-resume entry, (2) audit `session-task queue list` for
    // running items, (3) for each orphaned repo-scope item recover
    // via PR-state probe (green CI → merge agent; no PR → abandon),
    // (4) leave workload:* items alone (they survive restarts by
    // design), then (5) resume normal dispatch. Single line so
    // tmux-inject's vim-mode dd/i pipeline handles it atomically.
    //
    // 2026-05-15: q-6477 PR #203 sat green-and-unmerged for ~30 min
    // post-restart until WorkQueueOrphaned fired; this prompt makes
    // that recovery deterministic instead of alert-driven.
    "post-restart recovery: run `session-resume restart`, then for each `session-task queue list` running item whose agent is missing from `claude-watch active-agents` and scope is repo:* (NOT workload:* — workloads survive restart): probe PR state — open PR + green CI → spawn a merge-and-redeploy recovery agent (pass PR # + queue id); open PR + CI in-progress → spawn a CI-watch recovery agent; no PR → `session-task queue abandon <id>` (reason: agent orphaned across restart). Then resume normal dispatch.".to_string()
}

#[derive(Debug, Deserialize, Clone)]
pub struct ContextMonitorConfig {
    pub enabled: bool,
    /// Token percentage threshold (legacy fallback, ignored if threshold_margin is set)
    #[serde(default = "default_threshold_percent")]
    pub threshold_percent: u64,
    /// Fixed token margin from max_context_tokens at which to trigger (e.g., 30000 = trigger at max - 30K)
    /// When set, overrides threshold_percent.
    #[serde(default)]
    pub threshold_margin: Option<u64>,
    /// Compact remaining % at which to trigger (primary trigger)
    pub compact_trigger_percent: u32,
    /// Grace period before forced self-clear (seconds)
    pub grace_period: u64,
    /// Minimum interval between context clear triggers (seconds)
    pub cooldown: u64,
    /// Detect "Context limit reached" / "Request rejected (429)" banners in the
    /// pane and run `self-clear` immediately, without waiting for the agent to
    /// cooperate. This is the recovery path for when the agent is too wedged
    /// to run any tool call (and so the normal compact-prep checklist can't
    /// fire). Defaults to enabled.
    #[serde(default = "default_wedged_detection_enabled")]
    pub wedged_detection_enabled: bool,
    /// Number of consecutive check cycles a wedged pattern must be observed
    /// before claude-watch runs `self-clear`. Avoids tripping on stale chat
    /// history references to the strings. At a 10s general interval, the
    /// default of 3 corresponds to ~30s of sustained wedge.
    #[serde(default = "default_wedged_consecutive")]
    pub wedged_consecutive: u32,
    /// Cooldown in seconds between wedged-triggered self-clears. Prevents
    /// rapid retriggering if /clear takes a moment to land.
    #[serde(default = "default_wedged_cooldown")]
    pub wedged_cooldown: u64,
}

fn default_wedged_detection_enabled() -> bool {
    true
}

fn default_wedged_consecutive() -> u32 {
    3
}

fn default_wedged_cooldown() -> u64 {
    300 // 5 minutes
}

fn default_threshold_percent() -> u64 {
    75
}

/// Hybrid hooks + daemon-fallback tuning.
///
/// When enabled, the daemon defers its heavy-handed injections (tmux
/// `/clear`, `claude update`) for a grace window after a Claude Code hook
/// fires the corresponding reminder. This lets the conversational
/// reminder (low-friction) succeed most of the time, falling back to the
/// daemon only when Claude ignores or can't act on the hint.
#[derive(Debug, Deserialize, Clone)]
pub struct HybridConfig {
    /// Master switch. Default: true (the feature is opt-out).
    #[serde(default = "default_hybrid_enabled")]
    pub enabled: bool,
    /// Seconds to wait after a `context_high` hook fire before falling back
    /// to tmux-injecting `/clear`. Default: 300 (5 min).
    #[serde(default = "default_context_fallback_secs")]
    pub context_fallback_secs: u64,
    /// Seconds to wait after a `version_update` hook fire before falling
    /// back to running `claude update`. Default: 900 (15 min) — Claude
    /// often needs a few turns to hit a stopping point before restarting.
    #[serde(default = "default_version_fallback_secs")]
    pub version_fallback_secs: u64,
}

impl Default for HybridConfig {
    fn default() -> Self {
        Self {
            enabled: default_hybrid_enabled(),
            context_fallback_secs: default_context_fallback_secs(),
            version_fallback_secs: default_version_fallback_secs(),
        }
    }
}

fn default_hybrid_enabled() -> bool {
    true
}
fn default_context_fallback_secs() -> u64 {
    300
}
fn default_version_fallback_secs() -> u64 {
    900
}

/// Cross-gate suppression-escalation tuning. The watcher-down, fresh-/clear,
/// and dead-process suppression gates each defer their inject when the main
/// loop is "actively turning". That's correct in the common case but creates
/// a starvation hazard: a long-running dispatcher window (e.g. 30+ minutes
/// of back-to-back agents) can hold the gate open indefinitely, swallowing
/// genuine watcher-down / context-clear / dead-process conditions for the
/// duration. The two knobs here cap the suppression run.
///
/// When EITHER limit is reached on the next gate-fire, the fire force-injects
/// regardless of `actively_turning`. Counters are shared across all three
/// gates (a single counter on `State`) so a busy mix of fires across paths
/// still hits the cap.
///
/// This was added 2026-04-28 (q-2026-04-28-2449) after `claude-event-watch`
/// was suppressed for 33 minutes during a sustained dispatcher window;
/// alertmanager fired ClaudeEventStale before the gate ever reopened.
#[derive(Debug, Deserialize, Clone)]
pub struct SuppressionConfig {
    /// After this many consecutive suppressed fires (across all three
    /// gates), the next fire force-injects regardless of `actively_turning`.
    /// Default: 3 — three suppressed fires at the watcher-down 30s window
    /// is ~3 minutes of confirmed-busy + already-failed-to-inject before
    /// we override.
    #[serde(default = "default_max_consecutive_suppressions")]
    pub max_consecutive_suppressions: u32,
    /// Wall-clock backstop (seconds) since the FIRST suppression in the
    /// current run. If `now - first_suppression_at` exceeds this, the
    /// next gate fire force-injects. Catches the slow-drip case where
    /// suppressions land less often than the consecutive-counter cap
    /// would suggest (e.g. a check that satisfies the gate every other
    /// cycle).
    /// Default: 600 (10 min). The 33-min real-world incident this is
    /// designed to fix would have triggered at the 10-min mark.
    #[serde(default = "default_max_suppression_window_secs")]
    pub max_suppression_window_secs: u64,
}

impl Default for SuppressionConfig {
    fn default() -> Self {
        Self {
            max_consecutive_suppressions: default_max_consecutive_suppressions(),
            max_suppression_window_secs: default_max_suppression_window_secs(),
        }
    }
}

fn default_max_consecutive_suppressions() -> u32 {
    3
}

fn default_max_suppression_window_secs() -> u64 {
    600
}

/// Upstream-API retry detection.
///
/// When Anthropic's API returns 529 (overloaded) or transient 5xx errors,
/// Claude Code retries with exponential backoff and prints lines like
/// "Retrying in 24s · attempt 3/10". During the retry window the daemon's
/// normal interrupt sites (prolonged-thinking, watcher-down, context-warning,
/// wedged-clear) MUST suppress fires — every inject during retry resets the
/// retry state machine and the loop never recovers.
///
/// Detection in `tmux::check_lines_for_api_retry()` requires both a retry
/// marker ("Retrying in Ns" / "attempt N/M") AND an upstream-API error cue
/// ("API Error: 5xx", "Overloaded", etc.) in the LAST ~25 pane lines, so
/// chat-history references to the strings don't trip it.
#[derive(Debug, Deserialize, Clone)]
pub struct ApiRetryConfig {
    /// Master switch. Default: true (the feature is opt-out).
    #[serde(default = "default_api_retry_enabled")]
    pub enabled: bool,
    /// Number of consecutive check cycles a retry pattern must be observed
    /// before suppression activates. Single-cycle blips would otherwise
    /// suppress legitimate interrupts on a flicker. Default: 1 (suppress
    /// immediately on first detection — the cost of a missed interrupt is
    /// negligible compared to the cost of resetting the retry loop).
    #[serde(default = "default_api_retry_consecutive")]
    pub consecutive: u32,
    /// Maximum seconds api_retrying state may persist before claude-watch
    /// stops suppressing and resumes normal monitoring. Guards against
    /// "stuck retry" where Claude Code hangs in the retry banner forever
    /// (e.g. a network split kills outgoing requests so the retry loop
    /// can't make progress and we still need to alert/recover). Default:
    /// 1800 (30 min).
    #[serde(default = "default_api_retry_max_stuck_secs")]
    pub max_stuck_secs: u64,
}

impl Default for ApiRetryConfig {
    fn default() -> Self {
        Self {
            enabled: default_api_retry_enabled(),
            consecutive: default_api_retry_consecutive(),
            max_stuck_secs: default_api_retry_max_stuck_secs(),
        }
    }
}

fn default_api_retry_enabled() -> bool {
    true
}

fn default_api_retry_consecutive() -> u32 {
    1
}

fn default_api_retry_max_stuck_secs() -> u64 {
    1800 // 30 minutes
}

/// Stuck-detection suppression for active long-running workloads.
///
/// When a `workload run` invocation is active, its wrapper script
/// writes + touches `<workload_heartbeat_dir>/<label>.heartbeat` every
/// 30s as a fast-cadence proof-of-life. Before firing a "stuck" alert
/// (heartbeat-stale, prolonged-thinking) the daemon scans the dir;
/// if any heartbeat file has mtime within `workload_heartbeat_max_age_secs`,
/// the alert is SUPPRESSED — there's an out-of-band workload providing
/// liveness that the main loop's idleness can't explain on its own.
///
/// Distinct from the existing 15-min
/// `/var/run/claude/workload-state/<label>.heartbeat` which
/// `cron-workload-stale-check` consumes to detect wedged workloads
/// (1h stale threshold). The two heartbeats serve different purposes
/// and live in different subdirs of `/var/run/claude/`:
///   * `/run/claude/workloads/` (this): fast cadence (30s), daemon-side
///     suppression of false-positive stuck alerts.
///   * `/var/run/claude/workload-state/`: slow cadence (15min), cron-side
///     detection of stalled workloads. The legacy `/tmp/claude-workloads`
///     path is symlinked to it for back-compat with out-of-tree consumers.
#[derive(Debug, Deserialize, Clone)]
pub struct StuckDetectionConfig {
    /// Master switch. Default: true. Set to false to disable workload-
    /// heartbeat suppression and revert to the old behaviour (every
    /// stuck-state fire regardless of in-flight workloads).
    #[serde(default = "default_stuck_detection_enabled")]
    pub enabled: bool,
    /// Directory scanned for `<label>.heartbeat` files. Defaults to
    /// `/run/claude/workloads` — same `tmpfs` mount as the main-loop
    /// heartbeat at `/run/claude/heartbeat`, uid 1000 writable.
    #[serde(default = "default_workload_heartbeat_dir")]
    pub workload_heartbeat_dir: String,
    /// Maximum age (seconds) of a workload heartbeat to count as
    /// "fresh" (proof-of-life). Default: 60. Must be >= the wrapper's
    /// touch interval (default 30s) plus headroom for missed ticks.
    /// Set to 0 to require an exact-now match (mostly useful for
    /// tests).
    #[serde(default = "default_workload_heartbeat_max_age_secs")]
    pub workload_heartbeat_max_age_secs: u64,
}

impl Default for StuckDetectionConfig {
    fn default() -> Self {
        Self {
            enabled: default_stuck_detection_enabled(),
            workload_heartbeat_dir: default_workload_heartbeat_dir(),
            workload_heartbeat_max_age_secs: default_workload_heartbeat_max_age_secs(),
        }
    }
}

fn default_stuck_detection_enabled() -> bool {
    true
}

fn default_workload_heartbeat_dir() -> String {
    "/run/claude/workloads".to_string()
}

fn default_workload_heartbeat_max_age_secs() -> u64 {
    60
}

/// Load config from well-known paths or CLAUDE_WATCH_CONFIG env var.
/// Exits the process on failure — suitable for the daemon, not for
/// best-effort subcommands. Use `try_load_config` for those.
pub fn load_config() -> Config {
    match try_load_config() {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("FATAL: {}", e);
            std::process::exit(1);
        }
    }
}

/// Non-exiting config loader. Returns an Err with a human-readable
/// reason if no valid config file is found. The hybrid `hook-fire`
/// subcommand uses this to fail gracefully — a Claude Code session
/// must not break just because the host hasn't set up a config file.
pub fn try_load_config() -> Result<Config, String> {
    let config_paths = [
        std::env::var("CLAUDE_WATCH_CONFIG").unwrap_or_default(),
        format!(
            "{}/.config/claude-watch/config.toml",
            std::env::var("HOME").unwrap_or_else(|_| "/home/user".to_string())
        ),
        "config.toml".to_string(), // fallback: look in current directory
    ];

    for path in &config_paths {
        if path.is_empty() {
            continue;
        }
        if let Ok(content) = std::fs::read_to_string(path) {
            // Expand ~ to $HOME in config values before parsing
            let home = std::env::var("HOME").unwrap_or_else(|_| "/home/user".to_string());
            let content = content.replace("~/", &format!("{}/", home));
            match toml::from_str::<Config>(&content) {
                Ok(config) => {
                    tracing::info!(path, "loaded config");
                    return Ok(config);
                }
                Err(e) => {
                    return Err(format!("Failed to parse config {}: {}", path, e));
                }
            }
        }
    }
    Err(format!(
        "no config file found. Tried: {:?}",
        config_paths
    ))
}

/// Parse config from a TOML string. Useful for testing.
#[cfg(test)]
pub fn parse_config(toml_str: &str) -> Result<Config, toml::de::Error> {
    toml::from_str(toml_str)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_CONFIG: &str = r#"
[general]
check_interval = 10
state_file = "/tmp/test-state.json"
log_file = "/tmp/test.jsonl"
legacy_log_file = "/tmp/test.log"

[tmux]
dashboard_pane = "dashboard:0.0"
dashboard_session = "dashboard"

[claude]
max_context_tokens = 200000
heartbeat_file = "/tmp/heartbeat"
relaunch_script = "/tmp/relaunch.sh"

[dead_process]
checks_required = 3
restart_cooldown = 300

[fresh_clear]
min_tokens = 1000
max_tokens = 50000
detections_required = 2
cooldown = 120

[heartbeat]
stale_minutes = 15

[alerts]
initial_cooldown = 60
escalation_tiers = [60, 120, 300, 600, 3600]
max_pingme_alerts = 3
resume_prompt = "Resume your work."

[foreground_monitor]
enabled = true
threshold_seconds = 120
check_interval = 10
interrupt_enabled = true
interrupt_message = "Test interrupt message"

[watcher_monitor]
enabled = true
watchers_config = "/tmp/watchers.conf"
expected_watchmen = 3

[context_monitor]
enabled = true
threshold_percent = 75
compact_trigger_percent = 5
grace_period = 120
cooldown = 300

[auto_update]
enabled = false
check_minute = 10
cooldown_hours = 1
resume_prompt = "resume"

[reauth]
enabled = true
alert_interval_seconds = 10800

[task_watch]
enabled = true
session = "tasks"
poll_interval = 5
done_delay = 10
agent_done_delay = 120
max_panes = 20
show_all = false
"#;

    #[test]
    fn test_tmux_config_defaults_when_omitted() {
        // Config without [tmux] section should parse with empty defaults
        let config_no_tmux = r#"
[general]
check_interval = 10
state_file = "/tmp/test-state.json"
log_file = "/tmp/test.jsonl"
legacy_log_file = "/tmp/test.log"

[claude]
max_context_tokens = 200000
heartbeat_file = "/tmp/heartbeat"
relaunch_script = "/tmp/relaunch.sh"

[dead_process]
checks_required = 3
restart_cooldown = 300

[fresh_clear]
min_tokens = 1000
max_tokens = 50000
detections_required = 2
cooldown = 120

[heartbeat]
stale_minutes = 15

[alerts]
initial_cooldown = 60
escalation_tiers = [60, 120, 300, 600, 3600]
max_pingme_alerts = 3
resume_prompt = "Resume your work."

[foreground_monitor]
enabled = true
threshold_seconds = 120
check_interval = 10

[watcher_monitor]
enabled = true
watchers_config = "/tmp/watchers.conf"
expected_watchmen = 3

[context_monitor]
enabled = true
threshold_percent = 75
compact_trigger_percent = 5
grace_period = 120
cooldown = 300
"#;
        let config = parse_config(config_no_tmux).expect("should parse without [tmux] section");
        assert_eq!(config.tmux.dashboard_pane, "");
        assert_eq!(config.tmux.dashboard_session, "");
    }

    #[test]
    fn test_tmux_config_partial_override() {
        // Config with only dashboard_session set (dashboard_pane defaults to empty)
        let config_partial_tmux = r#"
[general]
check_interval = 10
state_file = "/tmp/test-state.json"
log_file = "/tmp/test.jsonl"
legacy_log_file = "/tmp/test.log"

[tmux]
dashboard_session = "my-session"

[claude]
max_context_tokens = 200000
heartbeat_file = "/tmp/heartbeat"
relaunch_script = "/tmp/relaunch.sh"

[dead_process]
checks_required = 3
restart_cooldown = 300

[fresh_clear]
min_tokens = 1000
max_tokens = 50000
detections_required = 2
cooldown = 120

[heartbeat]
stale_minutes = 15

[alerts]
initial_cooldown = 60
escalation_tiers = [60, 120, 300, 600, 3600]
max_pingme_alerts = 3
resume_prompt = "Resume your work."

[foreground_monitor]
enabled = true
threshold_seconds = 120
check_interval = 10

[watcher_monitor]
enabled = true
watchers_config = "/tmp/watchers.conf"
expected_watchmen = 3

[context_monitor]
enabled = true
threshold_percent = 75
compact_trigger_percent = 5
grace_period = 120
cooldown = 300
"#;
        let config =
            parse_config(config_partial_tmux).expect("should parse with partial [tmux] section");
        assert_eq!(config.tmux.dashboard_pane, "");
        assert_eq!(config.tmux.dashboard_session, "my-session");
    }

    #[test]
    fn test_tmux_config_empty_section() {
        // Config with empty [tmux] section should use defaults
        let config_empty_tmux = r#"
[general]
check_interval = 10
state_file = "/tmp/test-state.json"
log_file = "/tmp/test.jsonl"
legacy_log_file = "/tmp/test.log"

[tmux]

[claude]
max_context_tokens = 200000
heartbeat_file = "/tmp/heartbeat"
relaunch_script = "/tmp/relaunch.sh"

[dead_process]
checks_required = 3
restart_cooldown = 300

[fresh_clear]
min_tokens = 1000
max_tokens = 50000
detections_required = 2
cooldown = 120

[heartbeat]
stale_minutes = 15

[alerts]
initial_cooldown = 60
escalation_tiers = [60, 120, 300, 600, 3600]
max_pingme_alerts = 3
resume_prompt = "Resume your work."

[foreground_monitor]
enabled = true
threshold_seconds = 120
check_interval = 10

[watcher_monitor]
enabled = true
watchers_config = "/tmp/watchers.conf"
expected_watchmen = 3

[context_monitor]
enabled = true
threshold_percent = 75
compact_trigger_percent = 5
grace_period = 120
cooldown = 300
"#;
        let config =
            parse_config(config_empty_tmux).expect("should parse with empty [tmux] section");
        assert_eq!(config.tmux.dashboard_pane, "");
        assert_eq!(config.tmux.dashboard_session, "");
        // New knob (added 2026-04-25; combined-PR default lowered 500 -> 0,
        // 2026-04-28 PR #43+#46): should default to 0ms (fast-path) when
        // omitted. PR #46 adds explicit INSERT-mode verification after the
        // `i` keystroke, so the prior 500ms cushion is no longer needed.
        assert_eq!(config.tmux.post_escape_settle_ms, 0);
    }

    #[test]
    fn test_post_escape_settle_ms_default_when_tmux_section_missing() {
        // No [tmux] section at all -> TmuxConfig::default() -> 0ms (fast-path).
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
enabled = false
watchers_config = "/tmp/w.conf"
expected_watchmen = 0

[context_monitor]
enabled = true
threshold_percent = 75
compact_trigger_percent = 5
grace_period = 120
cooldown = 300
"#;
        let config = parse_config(cfg).expect("should parse without [tmux] section");
        assert_eq!(config.tmux.post_escape_settle_ms, 0);
    }

    #[test]
    fn test_post_escape_settle_ms_explicit_override() {
        // Explicit override in [tmux] should win over the 500ms default.
        let cfg = r#"
[general]
check_interval = 10
state_file = "/tmp/s.json"
log_file = "/tmp/s.jsonl"
legacy_log_file = "/tmp/s.log"

[tmux]
post_escape_settle_ms = 1500

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
enabled = false
watchers_config = "/tmp/w.conf"
expected_watchmen = 0

[context_monitor]
enabled = true
threshold_percent = 75
compact_trigger_percent = 5
grace_period = 120
cooldown = 300
"#;
        let config = parse_config(cfg).expect("should parse with override");
        assert_eq!(config.tmux.post_escape_settle_ms, 1500);
        // Other tmux defaults should still apply (untouched in the override).
        assert_eq!(config.tmux.dashboard_pane, "");
        assert_eq!(config.tmux.dashboard_session, "");
    }

    #[test]
    fn test_parse_valid_config() {
        let config = parse_config(SAMPLE_CONFIG).expect("should parse valid config");
        assert_eq!(config.general.check_interval, 10);
        // New field: default should be applied when not present in TOML.
        assert_eq!(config.general.post_interrupt_cooldown_secs, 60);
        // New field: thinking_backoff_multiplier default is 2 (legacy doubling).
        assert_eq!(config.foreground_monitor.thinking_backoff_multiplier, 2);
        assert_eq!(config.tmux.dashboard_pane, "dashboard:0.0");
        assert_eq!(config.claude.max_context_tokens, 200000);
        assert_eq!(config.dead_process.checks_required, 3);
        assert_eq!(config.fresh_clear.min_tokens, 1000);
        assert_eq!(config.heartbeat.stale_minutes, 15);
        assert_eq!(config.alerts.escalation_tiers.len(), 5);
        assert!(config.foreground_monitor.enabled);
        assert!(config.watcher_monitor.enabled);
        // Quiet-path defaults (no event_* keys in SAMPLE_CONFIG -> defaults).
        assert_eq!(config.watcher_monitor.event_threshold, 3);
        assert_eq!(config.watcher_monitor.event_grace_secs, 60);
        assert_eq!(config.watcher_monitor.event_command, "claude-event");
        assert_eq!(
            config.watcher_monitor.event_consumer_watcher_name,
            "claude-event-watch"
        );
        assert!(config.reauth.enabled);
        assert_eq!(config.reauth.alert_interval_seconds, 10800);
        assert!(config.task_watch.enabled);
        assert_eq!(config.task_watch.session, "tasks");
        assert_eq!(config.task_watch.poll_interval, 5);
        assert_eq!(config.task_watch.done_delay, 10);
        assert_eq!(config.task_watch.agent_done_delay, 120);
        assert_eq!(config.task_watch.max_panes, 20);
        assert!(!config.task_watch.show_all);
    }

    #[test]
    fn test_stuck_detection_defaults() {
        // No [stuck_detection] in SAMPLE_CONFIG → all defaults applied.
        let config = parse_config(SAMPLE_CONFIG).unwrap();
        assert!(config.stuck_detection.enabled);
        assert_eq!(
            config.stuck_detection.workload_heartbeat_dir,
            "/run/claude/workloads"
        );
        assert_eq!(config.stuck_detection.workload_heartbeat_max_age_secs, 60);
    }

    #[test]
    fn test_stuck_detection_override() {
        let cfg_str = format!(
            "{}\n[stuck_detection]\nenabled = false\nworkload_heartbeat_dir = \"/tmp/wl-hb\"\nworkload_heartbeat_max_age_secs = 120\n",
            SAMPLE_CONFIG
        );
        let config = parse_config(&cfg_str).unwrap();
        assert!(!config.stuck_detection.enabled);
        assert_eq!(config.stuck_detection.workload_heartbeat_dir, "/tmp/wl-hb");
        assert_eq!(config.stuck_detection.workload_heartbeat_max_age_secs, 120);
    }

    #[test]
    fn test_parse_minimal_values() {
        let config = parse_config(SAMPLE_CONFIG).unwrap();
        assert_eq!(config.alerts.max_pingme_alerts, 3);
        assert_eq!(config.alerts.resume_prompt, "Resume your work.");
        assert!(!config.auto_update.enabled);
        assert_eq!(config.auto_update.check_minute, 10);
        assert_eq!(config.auto_update.cooldown_hours, 1);
        assert_eq!(config.auto_update.resume_prompt, "resume");
        assert!(config.reauth.enabled);
        assert_eq!(config.reauth.alert_interval_seconds, 10800);
        // Hybrid defaults (no [hybrid] in SAMPLE_CONFIG -> defaults applied)
        assert!(config.hybrid.enabled);
        assert_eq!(config.hybrid.context_fallback_secs, 300);
        assert_eq!(config.hybrid.version_fallback_secs, 900);
    }

    #[test]
    fn test_hybrid_config_override() {
        let cfg_str = r#"
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
min_tokens = 2000
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
enabled = false
watchers_config = "/tmp/w.conf"
expected_watchmen = 0

[context_monitor]
enabled = true
threshold_percent = 75
compact_trigger_percent = 5
grace_period = 120
cooldown = 300

[hybrid]
enabled = false
context_fallback_secs = 60
version_fallback_secs = 120
"#;
        let cfg = parse_config(cfg_str).unwrap();
        assert!(!cfg.hybrid.enabled);
        assert_eq!(cfg.hybrid.context_fallback_secs, 60);
        assert_eq!(cfg.hybrid.version_fallback_secs, 120);
    }

    #[test]
    fn test_parse_config_without_auto_update_section() {
        // Config without [auto_update] should still parse with defaults
        let config_without_auto_update = r#"
[general]
check_interval = 10
state_file = "/tmp/test-state.json"
log_file = "/tmp/test.jsonl"
legacy_log_file = "/tmp/test.log"

[tmux]
dashboard_pane = "dashboard:0.0"
dashboard_session = "dashboard"

[claude]
max_context_tokens = 200000
heartbeat_file = "/tmp/heartbeat"
relaunch_script = "/tmp/relaunch.sh"

[dead_process]
checks_required = 3
restart_cooldown = 300

[fresh_clear]
min_tokens = 1000
max_tokens = 50000
detections_required = 2
cooldown = 120

[heartbeat]
stale_minutes = 15

[alerts]
initial_cooldown = 60
escalation_tiers = [60, 120, 300, 600, 3600]
max_pingme_alerts = 3
resume_prompt = "Resume your work."

[foreground_monitor]
enabled = true
threshold_seconds = 120
check_interval = 10

[watcher_monitor]
enabled = true
watchers_config = "/tmp/watchers.conf"
expected_watchmen = 3

[context_monitor]
enabled = true
threshold_percent = 75
compact_trigger_percent = 5
grace_period = 120
cooldown = 300
"#;
        let config = parse_config(config_without_auto_update)
            .expect("should parse without [auto_update] section");
        // Defaults should be applied
        assert!(!config.auto_update.enabled);
        assert_eq!(config.auto_update.check_minute, 10);
        assert_eq!(config.auto_update.cooldown_hours, 1);
        // Default post-restart prompt instructs orphan-recovery (not bare
        // "resume" — see default_update_resume_prompt()). Pin a stable
        // anchor substring rather than the full text so updates to the
        // procedure prose don't churn this assertion; the dedicated
        // test_default_update_resume_prompt_includes_orphan_recovery
        // test below pins the full shape.
        assert!(
            config
                .auto_update
                .resume_prompt
                .contains("post-restart recovery"),
            "default resume_prompt should begin the post-restart recovery checklist, got: {}",
            config.auto_update.resume_prompt
        );
        // reauth defaults should also be applied
        assert!(config.reauth.enabled);
        assert_eq!(config.reauth.alert_interval_seconds, 10800);
    }

    #[test]
    fn test_default_update_resume_prompt_includes_orphan_recovery() {
        // The post-restart resume prompt must include the orphan-recovery
        // procedure: session-resume entry + queue audit + repo/workload
        // discrimination + PR-state probe + abandon-on-no-PR. Anchors
        // pinned individually so a wording tweak doesn't require
        // rewriting the test, but a regression that drops a step is
        // caught.
        //
        // 2026-05-15 q-6477 regression test: bare "resume" left
        // orphaned PR-shipping agents stranded post-restart until an
        // external alert flagged them. This prompt makes recovery
        // deterministic. If you're tempted to revert to "resume", read
        // memory/feedback_post-restart-orphan-recovery.md first.
        let prompt = default_update_resume_prompt();
        assert!(
            prompt.contains("session-resume restart"),
            "must invoke session-resume restart"
        );
        assert!(
            prompt.contains("session-task queue list"),
            "must audit running queue items"
        );
        assert!(
            prompt.contains("claude-watch active-agents"),
            "must probe agent liveness via claude-watch active-agents"
        );
        assert!(
            prompt.contains("repo:*") && prompt.contains("workload:*"),
            "must discriminate repo:* vs workload:* scopes"
        );
        assert!(
            prompt.contains("PR")
                && (prompt.contains("CI") || prompt.contains("green")),
            "must mention PR state + CI for recovery probe"
        );
        assert!(
            prompt.contains("session-task queue abandon"),
            "must abandon orphaned items with no PR"
        );
        // Single-line invariant: tmux inject_text's vim-mode pipeline
        // sends the payload as one literal send_keys -l call. Embedded
        // newlines would either land as a multi-line typed message
        // (Claude Code interprets Enter as submit) or get eaten. Keep
        // the prompt single-line.
        assert!(
            !prompt.contains('\n'),
            "resume_prompt must be single-line (tmux inject pipeline assumes single-line); got newlines in: {:?}",
            prompt
        );
        // Single-character sanity: never accidentally land back at
        // bare "resume" (the pre-fix regression target).
        assert_ne!(
            prompt, "resume",
            "regression guard: bare \"resume\" was the q-6477 failure mode"
        );
    }

    #[test]
    fn test_parse_config_suppression_defaults() {
        // [suppression] section is optional; defaults must apply when
        // it's absent. Pin the documented defaults so future drift
        // requires updating tests + docs together.
        let config_no_suppression = r#"
[general]
check_interval = 10
state_file = "/tmp/test-state.json"
log_file = "/tmp/test.jsonl"
legacy_log_file = "/tmp/test.log"

[tmux]
dashboard_pane = "dashboard:0.0"
dashboard_session = "dashboard"

[claude]
max_context_tokens = 200000
heartbeat_file = "/tmp/heartbeat"
relaunch_script = "/tmp/relaunch.sh"

[dead_process]
checks_required = 3
restart_cooldown = 300

[fresh_clear]
min_tokens = 1000
max_tokens = 50000
detections_required = 2
cooldown = 120

[heartbeat]
stale_minutes = 15

[alerts]
initial_cooldown = 60
escalation_tiers = [60, 120, 300, 600, 3600]
max_pingme_alerts = 3
resume_prompt = "Resume your work."

[foreground_monitor]
enabled = true
threshold_seconds = 120
check_interval = 10

[watcher_monitor]
enabled = true
watchers_config = "/tmp/watchers.conf"
expected_watchmen = 3

[context_monitor]
enabled = true
threshold_percent = 75
compact_trigger_percent = 5
grace_period = 120
cooldown = 300
"#;
        let config = parse_config(config_no_suppression)
            .expect("should parse without [suppression] section");
        assert_eq!(config.suppression.max_consecutive_suppressions, 3);
        assert_eq!(config.suppression.max_suppression_window_secs, 600);
    }

    #[test]
    fn test_parse_config_suppression_overrides() {
        // Explicit [suppression] values must override the defaults.
        let config_custom = r#"
[general]
check_interval = 10
state_file = "/tmp/test-state.json"
log_file = "/tmp/test.jsonl"
legacy_log_file = "/tmp/test.log"

[tmux]
dashboard_pane = "dashboard:0.0"
dashboard_session = "dashboard"

[claude]
max_context_tokens = 200000
heartbeat_file = "/tmp/heartbeat"
relaunch_script = "/tmp/relaunch.sh"

[dead_process]
checks_required = 3
restart_cooldown = 300

[fresh_clear]
min_tokens = 1000
max_tokens = 50000
detections_required = 2
cooldown = 120

[heartbeat]
stale_minutes = 15

[alerts]
initial_cooldown = 60
escalation_tiers = [60, 120, 300, 600, 3600]
max_pingme_alerts = 3
resume_prompt = "Resume your work."

[foreground_monitor]
enabled = true
threshold_seconds = 120
check_interval = 10

[watcher_monitor]
enabled = true
watchers_config = "/tmp/watchers.conf"
expected_watchmen = 3

[context_monitor]
enabled = true
threshold_percent = 75
compact_trigger_percent = 5
grace_period = 120
cooldown = 300

[suppression]
max_consecutive_suppressions = 5
max_suppression_window_secs = 1200
"#;
        let config = parse_config(config_custom).expect("should parse with [suppression] section");
        assert_eq!(config.suppression.max_consecutive_suppressions, 5);
        assert_eq!(config.suppression.max_suppression_window_secs, 1200);
    }

    #[test]
    fn test_parse_invalid_config() {
        let result = parse_config("not valid toml [[[");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_missing_section() {
        let partial = r#"
[general]
check_interval = 10
state_file = "/tmp/s"
log_file = "/tmp/l"
legacy_log_file = "/tmp/ll"
"#;
        let result = parse_config(partial);
        assert!(result.is_err(), "missing sections should fail");
    }
}
