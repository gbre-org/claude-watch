#![allow(dead_code)]
//! Shared test harness for claude-watch e2e tests.
//!
//! Provides:
//!   - Isolated tmux session management
//!   - Mock `claude-status` script with configurable output
//!   - Temp directories for state, logs, config
//!   - Config generation with short intervals for fast tests
//!   - Cleanup on drop

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Unique test environment with its own tmux session, mock scripts, and config.
pub struct TestEnv {
    /// Unique test name used for tmux session and temp paths.
    pub name: String,
    /// Temp directory for all test artifacts.
    pub tmp_dir: PathBuf,
    /// Path to the generated test config.toml.
    pub config_path: PathBuf,
    /// Path to the mock claude-status script.
    pub mock_status_script: PathBuf,
    /// Path to the mock status data file (JSON written by tests, read by mock script).
    pub mock_status_data: PathBuf,
    /// Path to the mock pingme script (logs calls instead of sending notifications).
    pub mock_pingme_script: PathBuf,
    /// Path to the pingme call log.
    pub pingme_log: PathBuf,
    /// Tmux session name for this test.
    pub tmux_session: String,
    /// Tmux pane identifier (session:window.pane).
    pub tmux_pane: String,
    /// State file path.
    pub state_file: PathBuf,
    /// JSONL log file path.
    pub log_file: PathBuf,
    /// Legacy log file path.
    pub legacy_log_file: PathBuf,
    /// Heartbeat file path.
    pub heartbeat_file: PathBuf,
    /// Path to mock bin directory (prepended to PATH).
    pub mock_bin_dir: PathBuf,
    /// Watchers config file path.
    pub watchers_config: PathBuf,
    /// Path to the mock self-clear invocation log (one line per invocation).
    pub self_clear_log: PathBuf,
}

/// Configuration for mock claude-status output.
#[derive(Debug, Clone)]
pub struct MockStatus {
    pub pane: String,
    pub tokens: u64,
    pub bashes: u64,
    pub compact_remaining: Option<u32>,
    pub version: Option<String>,
}

impl MockStatus {
    /// A status representing a healthy, running Claude Code instance.
    pub fn healthy(pane: &str) -> Self {
        Self {
            pane: pane.to_string(),
            tokens: 50000,
            bashes: 10,
            compact_remaining: None,
            version: Some("1.0.0".to_string()),
        }
    }

    /// A status representing a dead Claude Code process.
    pub fn dead() -> Self {
        Self {
            pane: String::new(),
            tokens: 0,
            bashes: 0,
            compact_remaining: None,
            version: None,
        }
    }

    /// A status representing a fresh /clear (low tokens, zero bashes).
    pub fn fresh_clear(pane: &str) -> Self {
        Self {
            pane: pane.to_string(),
            tokens: 3000,
            bashes: 0,
            compact_remaining: None,
            version: Some("1.0.0".to_string()),
        }
    }

    /// A status representing high context usage (for token stall tests).
    pub fn high_context(pane: &str, tokens: u64, bashes: u64) -> Self {
        Self {
            pane: pane.to_string(),
            tokens,
            bashes,
            compact_remaining: None,
            version: Some("1.0.0".to_string()),
        }
    }

    /// Serialize to JSON string.
    pub fn to_json(&self) -> String {
        let compact = self
            .compact_remaining
            .map(|v| format!("{}", v))
            .unwrap_or_else(|| "null".to_string());
        let version = self
            .version
            .as_ref()
            .map(|v| format!("\"{}\"", v))
            .unwrap_or_else(|| "null".to_string());
        format!(
            r#"{{"pane":"{}","tokens":{},"bashes":{},"compact_remaining":{},"version":{}}}"#,
            self.pane, self.tokens, self.bashes, compact, version
        )
    }
}

/// Options for creating a test environment.
pub struct TestEnvOptions {
    /// Check interval in seconds (default: 1 for fast tests).
    pub check_interval: u64,
    /// Dead process checks required (default: 2).
    pub dead_checks_required: u32,
    /// Fresh clear detections required (default: 2).
    pub fresh_clear_detections: u32,
    /// Heartbeat stale minutes (default: 1 for fast tests).
    pub heartbeat_stale_minutes: u64,
    /// Foreground threshold seconds (default: 3 for fast tests).
    pub foreground_threshold: u64,
    /// Whether foreground interrupt is enabled (default: false for existing tests).
    pub foreground_interrupt_enabled: bool,
    /// Message to inject after foreground interrupt.
    pub foreground_interrupt_message: String,
    /// Foreground monitor check interval (default: 1 for fast tests).
    pub foreground_check_interval: u64,
    /// Whether to show a shell prompt in the tmux pane.
    pub show_shell_prompt: bool,
    /// Whether to show the Claude idle prompt in the tmux pane.
    pub show_idle_prompt: bool,
    /// Watcher monitor: consecutive missing checks before injecting (default: 6).
    /// Lower this in tests that exercise the watcher-down inject path so the
    /// daemon fires within a few seconds rather than the production ~60s.
    pub watcher_inject_threshold: u32,
    /// Watcher monitor: cooldown between injections (default: 60).
    pub watcher_inject_cooldown: u64,
    /// Watcher monitor: grace period (seconds) after last_seen_running during
    /// which a missing watcher does not count toward consecutive_missing.
    /// Default 90 matches production; lower to 0 in tests where you start
    /// the watcher then kill it and want immediate detection.
    pub watcher_grace_secs: u64,
    /// Don't create the test tmux session in TestEnv::new (caller will
    /// create it themselves with custom layout). When true, set_status's
    /// default healthy MockStatus still references the configured pane,
    /// but no tmux session is spawned.
    pub skip_tmux_session: bool,
    /// Enable the [context_monitor] block (default: false). The wedged-pane
    /// detector lives under this block; tests that exercise wedged recovery
    /// turn it on.
    pub context_monitor_enabled: bool,
    /// Consecutive wedged-pattern observations required before self-clear
    /// fires (default: 3, mirroring production).
    pub wedged_consecutive: u32,
}

impl Default for TestEnvOptions {
    fn default() -> Self {
        Self {
            check_interval: 1,
            dead_checks_required: 2,
            fresh_clear_detections: 2,
            heartbeat_stale_minutes: 1,
            foreground_threshold: 3,
            foreground_interrupt_enabled: false,
            foreground_interrupt_message: "[TEST-INTERRUPT] Foreground command was backgrounded."
                .to_string(),
            foreground_check_interval: 1,
            show_shell_prompt: false,
            show_idle_prompt: false,
            watcher_inject_threshold: 6,
            watcher_inject_cooldown: 60,
            watcher_grace_secs: 90,
            skip_tmux_session: false,
            context_monitor_enabled: false,
            wedged_consecutive: 3,
        }
    }
}

impl TestEnv {
    /// Create a new isolated test environment.
    ///
    /// Sets up:
    /// - Temp directory under /tmp/claude-watch-test-<name>-<pid>/
    /// - Mock `claude-status` script
    /// - Mock `pingme` script
    /// - Test tmux session
    /// - Test config.toml with short intervals
    pub fn new(name: &str, opts: TestEnvOptions) -> Self {
        let pid = std::process::id();
        let session_name = format!("cw-test-{}-{}", name, pid);
        let tmp_dir = PathBuf::from(format!("/tmp/claude-watch-test-{}-{}", name, pid));

        // Clean up any leftover from a previous failed run
        let _ = fs::remove_dir_all(&tmp_dir);
        fs::create_dir_all(&tmp_dir).expect("create tmp dir");

        let mock_bin_dir = tmp_dir.join("bin");
        fs::create_dir_all(&mock_bin_dir).expect("create mock bin dir");

        let log_dir = tmp_dir.join("logs");
        fs::create_dir_all(&log_dir).expect("create log dir");

        let env = TestEnv {
            name: name.to_string(),
            config_path: tmp_dir.join("config.toml"),
            mock_status_script: mock_bin_dir.join("claude-status"),
            mock_status_data: tmp_dir.join("mock-status.json"),
            mock_pingme_script: mock_bin_dir.join("pingme"),
            pingme_log: tmp_dir.join("pingme.log"),
            tmux_session: session_name.clone(),
            tmux_pane: format!("{}:0.0", session_name),
            state_file: tmp_dir.join("state.json"),
            log_file: log_dir.join("claude-watch.jsonl"),
            legacy_log_file: log_dir.join("claude-watch.log"),
            heartbeat_file: tmp_dir.join("heartbeat"),
            self_clear_log: tmp_dir.join("self-clear.log"),
            mock_bin_dir,
            watchers_config: tmp_dir.join("watchers.conf"),
            tmp_dir,
        };

        // Write mock claude-status script
        env.write_mock_status_script();

        // Write mock pingme script
        env.write_mock_pingme_script();

        // Write mock tmux-healthcheck script
        env.write_mock_tmux_healthcheck();

        // Write mock self-clear script (logs invocation instead of injecting /clear)
        env.write_mock_self_clear_script();

        // Write empty watchers config
        fs::write(&env.watchers_config, "# test watchers\n").expect("write watchers config");

        // Set initial mock status (healthy)
        env.set_status(&MockStatus::healthy(&env.tmux_pane));

        // Create tmux session (unless caller wants to manage it themselves)
        if !opts.skip_tmux_session {
            env.create_tmux_session(&opts);
        }

        // Write test config
        env.write_config(&opts);

        env
    }

    /// Write the mock claude-status script that reads from the data file.
    fn write_mock_status_script(&self) {
        let script = format!(
            r#"#!/bin/bash
# Mock claude-status for e2e tests
# Reads pre-configured JSON from the data file
if [ "$1" = "--json" ]; then
    cat "{data}"
else
    echo "mock claude-status (use --json)"
fi
"#,
            data = self.mock_status_data.display()
        );
        fs::write(&self.mock_status_script, &script).expect("write mock claude-status");
        make_executable(&self.mock_status_script);
    }

    /// Write mock pingme that logs calls instead of sending notifications.
    fn write_mock_pingme_script(&self) {
        let script = format!(
            r#"#!/bin/bash
# Mock pingme for e2e tests -- logs call instead of sending notification
echo "$(date -Is) $@" >> "{log}"
"#,
            log = self.pingme_log.display()
        );
        fs::write(&self.mock_pingme_script, &script).expect("write mock pingme");
        make_executable(&self.mock_pingme_script);
    }

    /// Write mock self-clear that records its invocation instead of injecting
    /// `/clear` into a real pane. The wedged-recovery path spawns `self-clear`
    /// (via PATH), so a test can assert recovery fired by checking this log.
    fn write_mock_self_clear_script(&self) {
        let script = format!(
            r#"#!/bin/bash
# Mock self-clear for e2e tests -- records invocation instead of clearing.
echo "$(date -Is) self-clear $@" >> "{log}"
"#,
            log = self.self_clear_log.display()
        );
        let path = self.mock_bin_dir.join("self-clear");
        fs::write(&path, &script).expect("write mock self-clear");
        make_executable(&path);
    }

    /// Read the mock self-clear invocation log (empty string if never invoked).
    pub fn read_self_clear_log(&self) -> String {
        fs::read_to_string(&self.self_clear_log).unwrap_or_default()
    }

    /// Write mock tmux-healthcheck that returns OK.
    fn write_mock_tmux_healthcheck(&self) {
        let script = r#"#!/bin/bash
echo "tmux: ok (mock)"
"#;
        let path = self.mock_bin_dir.join("tmux-healthcheck");
        fs::write(&path, script).expect("write mock tmux-healthcheck");
        make_executable(&path);
    }

    /// Create the test tmux session.
    fn create_tmux_session(&self, opts: &TestEnvOptions) {
        // Kill any existing session with this name
        let _ = Command::new("tmux")
            .args(["kill-session", "-t", &self.tmux_session])
            .output();

        // Create new detached session
        let status = Command::new("tmux")
            .args([
                "new-session",
                "-d",
                "-s",
                &self.tmux_session,
                "-x",
                "200",
                "-y",
                "50",
            ])
            .status()
            .expect("create tmux session");
        assert!(status.success(), "failed to create tmux session");

        // Optionally show a shell prompt or idle prompt
        if opts.show_shell_prompt {
            self.set_pane_content("user@testhost:~$ ");
        }
        if opts.show_idle_prompt {
            self.set_pane_content(&format!("Claude Code\n\u{276f} "));
        }
    }

    /// Write the test config.toml.
    fn write_config(&self, opts: &TestEnvOptions) {
        let config = format!(
            r#"[general]
check_interval = {check_interval}
state_file = "{state_file}"
log_file = "{log_file}"
legacy_log_file = "{legacy_log_file}"

[tmux]
dashboard_pane = "{pane}"
dashboard_session = "{session}"

[claude]
max_context_tokens = 200000
heartbeat_file = "{heartbeat_file}"
relaunch_script = "{tmp_dir}/relaunch.sh"

[dead_process]
checks_required = {dead_checks}
restart_cooldown = 5

[fresh_clear]
min_tokens = 2000
max_tokens = 5000
detections_required = {fresh_clear_detections}
cooldown = 5

[heartbeat]
stale_minutes = {heartbeat_stale_minutes}

[alerts]
initial_cooldown = 5
escalation_tiers = [5, 10, 30]
max_pingme_alerts = 3
resume_prompt = "[TEST-RESUME] Test resume prompt"

[foreground_monitor]
enabled = true
threshold_seconds = {foreground_threshold}
check_interval = {foreground_check_interval}
interrupt_enabled = {foreground_interrupt_enabled}
interrupt_message = "{foreground_interrupt_message}"

[watcher_monitor]
enabled = true
watchers_config = "{watchers_config}"
expected_watchmen = 0
inject_threshold = {watcher_inject_threshold}
inject_cooldown = {watcher_inject_cooldown}
grace_secs = {watcher_grace_secs}

[context_monitor]
enabled = {context_monitor_enabled}
threshold_percent = 75
compact_trigger_percent = 5
grace_period = 120
cooldown = 300
wedged_consecutive = {wedged_consecutive}
wedged_cooldown = 300

[auto_update]
enabled = false
check_minute = 10
cooldown_hours = 1
resume_prompt = "resume"
"#,
            check_interval = opts.check_interval,
            state_file = self.state_file.display(),
            log_file = self.log_file.display(),
            legacy_log_file = self.legacy_log_file.display(),
            pane = self.tmux_pane,
            session = self.tmux_session,
            heartbeat_file = self.heartbeat_file.display(),
            tmp_dir = self.tmp_dir.display(),
            dead_checks = opts.dead_checks_required,
            fresh_clear_detections = opts.fresh_clear_detections,
            heartbeat_stale_minutes = opts.heartbeat_stale_minutes,
            foreground_threshold = opts.foreground_threshold,
            foreground_check_interval = opts.foreground_check_interval,
            foreground_interrupt_enabled = opts.foreground_interrupt_enabled,
            foreground_interrupt_message = opts.foreground_interrupt_message,
            watchers_config = self.watchers_config.display(),
            watcher_inject_threshold = opts.watcher_inject_threshold,
            watcher_inject_cooldown = opts.watcher_inject_cooldown,
            watcher_grace_secs = opts.watcher_grace_secs,
            context_monitor_enabled = opts.context_monitor_enabled,
            wedged_consecutive = opts.wedged_consecutive,
        );
        fs::write(&self.config_path, &config).expect("write test config");
    }

    /// Update the mock claude-status data file with new status values.
    pub fn set_status(&self, status: &MockStatus) {
        fs::write(&self.mock_status_data, status.to_json()).expect("write mock status data");
    }

    /// Make the mock `claude-status --json` emit unparseable output so
    /// `get_claude_status()` returns `None` — simulating the real-world case
    /// where a wedged context-limit/429 banner covers the status bar and the
    /// parse misses, even though Claude Code is still running.
    pub fn set_status_unparseable(&self) {
        fs::write(&self.mock_status_data, "not json — banner covering status bar\n")
            .expect("write unparseable mock status data");
    }

    /// Send text to the tmux pane to simulate pane content.
    pub fn set_pane_content(&self, text: &str) {
        // Clear pane first, then echo the text
        let _ = Command::new("tmux")
            .args(["send-keys", "-t", &self.tmux_pane, "C-c", ""])
            .output();
        let _ = Command::new("tmux")
            .args(["send-keys", "-t", &self.tmux_pane, "C-l", ""])
            .output();
        std::thread::sleep(std::time::Duration::from_millis(100));
        // Use printf to output text without newline issues
        let cmd = format!("printf '{}\\n'", text.replace('\'', "'\\''"));
        let _ = Command::new("tmux")
            .args(["send-keys", "-t", &self.tmux_pane, "-l", &cmd])
            .output();
        let _ = Command::new("tmux")
            .args(["send-keys", "-t", &self.tmux_pane, "Enter"])
            .output();
        std::thread::sleep(std::time::Duration::from_millis(200));
    }

    /// Send raw tmux keys to the test pane.
    pub fn send_keys(&self, keys: &[&str]) {
        let mut args = vec!["send-keys", "-t", &self.tmux_pane];
        args.extend_from_slice(keys);
        let _ = Command::new("tmux").args(&args).output();
    }

    /// Capture current pane content.
    pub fn capture_pane(&self) -> String {
        let output = Command::new("tmux")
            .args(["capture-pane", "-t", &self.tmux_pane, "-p"])
            .output()
            .expect("capture pane");
        String::from_utf8_lossy(&output.stdout).to_string()
    }

    /// Touch the heartbeat file (make it fresh).
    pub fn touch_heartbeat(&self) {
        fs::write(&self.heartbeat_file, "").expect("touch heartbeat");
    }

    /// Set heartbeat file mtime to N seconds in the past.
    pub fn age_heartbeat(&self, seconds: u64) {
        // Create the file if it doesn't exist
        if !self.heartbeat_file.exists() {
            fs::write(&self.heartbeat_file, "").expect("create heartbeat");
        }
        let past = filetime::FileTime::from_system_time(
            std::time::SystemTime::now() - std::time::Duration::from_secs(seconds),
        );
        filetime::set_file_mtime(&self.heartbeat_file, past).expect("set heartbeat mtime");
    }

    /// Read the JSONL log file and return parsed entries.
    pub fn read_log_entries(&self) -> Vec<serde_json::Value> {
        match fs::read_to_string(&self.log_file) {
            Ok(content) => content
                .lines()
                .filter_map(|line| serde_json::from_str(line).ok())
                .collect(),
            Err(_) => Vec::new(),
        }
    }

    /// Read the legacy log file.
    pub fn read_legacy_log(&self) -> String {
        fs::read_to_string(&self.legacy_log_file).unwrap_or_default()
    }

    /// Read the state file.
    pub fn read_state(&self) -> serde_json::Value {
        match fs::read_to_string(&self.state_file) {
            Ok(content) => serde_json::from_str(&content).unwrap_or(serde_json::Value::Null),
            Err(_) => serde_json::Value::Null,
        }
    }

    /// Read pingme log entries.
    pub fn read_pingme_log(&self) -> Vec<String> {
        match fs::read_to_string(&self.pingme_log) {
            Ok(content) => content.lines().map(|s| s.to_string()).collect(),
            Err(_) => Vec::new(),
        }
    }

    /// Get the PATH with mock bin dir prepended.
    pub fn test_path(&self) -> String {
        let current_path = std::env::var("PATH").unwrap_or_default();
        format!("{}:{}", self.mock_bin_dir.display(), current_path)
    }

    /// Build the daemon binary and return the path.
    pub fn daemon_binary() -> PathBuf {
        // Build in test mode — use CARGO_MANIFEST_DIR to find the project root
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let status = Command::new("cargo")
            .args(["build"])
            .current_dir(manifest_dir)
            .status()
            .expect("cargo build");
        assert!(status.success(), "cargo build failed");

        PathBuf::from(format!("{}/target/debug/claude-watch", manifest_dir))
    }

    /// Run the daemon for a specified number of check cycles, then kill it.
    /// Returns the process exit status.
    pub fn run_daemon_cycles(&self, cycles: u32, extra_wait_ms: u64) -> DaemonRun {
        let binary = Self::daemon_binary();
        let wait_ms = (self.read_config_interval() * 1000 * cycles as u64) + extra_wait_ms;

        let child = Command::new(&binary)
            .env("CLAUDE_WATCH_CONFIG", &self.config_path)
            .env("PATH", self.test_path())
            .env("CLAUDE_STATUS_CMD", "1")
            .env("RUST_LOG", "debug")
            // Hermeticity: point HOME at the test tmp dir so the layered
            // user config at ~/.config/claude-watch/config.toml does NOT
            // overlay (and override e.g. [tmux] dashboard_pane onto) the
            // test config. Without this, a developer's real config leaks
            // into the daemon under test.
            .env("HOME", &self.tmp_dir)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("spawn daemon");

        std::thread::sleep(std::time::Duration::from_millis(wait_ms));

        // Send SIGTERM for graceful shutdown
        unsafe {
            libc::kill(child.id() as i32, libc::SIGTERM);
        }

        // Wait for exit with timeout
        let output = match child.wait_with_output() {
            Ok(o) => o,
            Err(e) => panic!("failed to wait on daemon: {}", e),
        };

        DaemonRun {
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
            exit_code: output.status.code(),
        }
    }

    /// Read check interval from config (for calculating wait times).
    fn read_config_interval(&self) -> u64 {
        let content = fs::read_to_string(&self.config_path).unwrap_or_default();
        for line in content.lines() {
            if line.starts_with("check_interval") {
                if let Some(val) = line.split('=').nth(1) {
                    return val.trim().parse().unwrap_or(1);
                }
            }
        }
        1
    }

    /// Count log entries matching a specific event type.
    pub fn count_log_events(&self, event_type: &str) -> usize {
        self.read_log_entries()
            .iter()
            .filter(|e| e["event"].as_str() == Some(event_type))
            .count()
    }

    /// Find log entries matching a specific event type.
    pub fn find_log_events(&self, event_type: &str) -> Vec<serde_json::Value> {
        self.read_log_entries()
            .into_iter()
            .filter(|e| e["event"].as_str() == Some(event_type))
            .collect()
    }
}

impl Drop for TestEnv {
    fn drop(&mut self) {
        // Kill tmux session
        let _ = Command::new("tmux")
            .args(["kill-session", "-t", &self.tmux_session])
            .output();

        // Clean up temp directory
        let _ = fs::remove_dir_all(&self.tmp_dir);
    }
}

/// Result from running the daemon.
pub struct DaemonRun {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: Option<i32>,
}

/// Make a file executable.
fn make_executable(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o755));
    }
}
