//! Watcher supervision: list, status, run, enable/disable, restart.
//!
//! Replaces the shell scripts `watcher-ctl`, `watcher-status`, and
//! `watcher-restart` with native Rust implementations.

use crate::cmd::run_cmd_any;
use crate::status::{parse_watchers_config, WatcherEntry};
use serde::Serialize;
use std::io::Write;
use std::os::unix::process::ExitStatusExt;

/// Default config path for watchers.
const DEFAULT_CONFIG: &str = ".config/watchmen/watchers.conf";

/// PID file directory for watcher liveness tracking.
pub const PID_DIR: &str = "/var/run/claude";

/// Resolve the watchers.conf path (respects $WATCHERS_CONFIG for testing).
pub fn config_path() -> String {
    if let Ok(p) = std::env::var("WATCHERS_CONFIG") {
        return p;
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/home/user".to_string());
    format!("{}/{}", home, DEFAULT_CONFIG)
}

/// Resolve the optional extra watchers.conf path (respects $WATCHERS_CONFIG_EXTRA).
/// Returns None when the env var is unset or empty.
pub fn config_path_extra() -> Option<String> {
    std::env::var("WATCHERS_CONFIG_EXTRA")
        .ok()
        .filter(|s| !s.is_empty())
}

/// Status of a single watcher.
///
/// `status` values:
/// - `"ok"` — exactly the right number of pollers running, no duplicate
///   supervisors
/// - `"DOWN"` — poller count is below `required` (min_count from
///   watchers.conf)
/// - `"DUPLICATE"` — at least one of:
///     * more than one underlying poller process matches the watcher pattern
///     * more than one `watcher-ctl run <name>` supervisor process is alive
///   `DOWN` takes precedence over `DUPLICATE` if both apply (because a dead
///   poller is the more urgent failure mode).
/// - `"off"` — disabled in watchers.conf
///
/// `dup_supervisors` and `dup_pollers` are populated (non-empty) only when the
/// corresponding duplicate condition is detected. The lists carry the PIDs so
/// the human can `kill` them by hand. We deliberately do NOT auto-kill — the
/// wrong choice could take out the canonical poller.
#[derive(Debug, Serialize)]
pub struct WatcherStatus {
    pub name: String,
    pub status: String, // "ok", "DOWN", "DUPLICATE", "off"
    pub count: u32,
    pub required: u32,
    pub pids: String,
    pub enabled: bool,
    /// PIDs of duplicate `watcher-ctl run <name>` supervisor wrappers.
    /// Empty when only one (canonical) supervisor is alive.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dup_supervisors: Vec<u32>,
    /// PIDs of duplicate underlying poller processes. Empty when count == 1.
    /// (When count > min_count > 1 we still report it; users can audit.)
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dup_pollers: Vec<u32>,
}

/// Get process count for a pattern via `pgrep -fc`.
///
/// Currently unused inside this module (`watcher_status` derives the count
/// from the pid list to halve fork count) but kept on the public surface
/// for any external caller that needs a count-only check.
#[allow(dead_code)]
pub async fn process_count(pattern: &str) -> u32 {
    let (out, _) = run_cmd_any(&["pgrep", "-fc", "--", pattern], 5).await;
    out.trim().parse().unwrap_or(0)
}

/// Get PIDs matching a pattern via `pgrep -f`.
pub async fn process_pids(pattern: &str) -> Vec<u32> {
    let (out, _) = run_cmd_any(&["pgrep", "-f", "--", pattern], 5).await;
    out.lines()
        .filter_map(|l| l.trim().parse::<u32>().ok())
        .collect()
}

/// Get PIDs of `watcher-ctl run <name>` supervisor processes.
///
/// `pgrep -f "watcher-ctl run <name>"` would also pick up the shell wrappers
/// that LAUNCHED the supervisor (e.g. a `/bin/zsh -c 'watcher-ctl run X'`
/// tail-end of an interactive eval), so we filter the matches by reading
/// `/proc/PID/comm` and keeping only those whose process name is
/// `watcher-ctl` (or its multicall alias `claude-watch`).
///
/// This returns the canonical list of live supervisors. Length > 1 means a
/// duplicate supervisor stack — the bug pattern caught on a prior
/// regression, where multiple nested `watcher-ctl run <name>` parents
/// stay alive `wait()`ing on the same descendant.
pub async fn supervisor_pids(name: &str) -> Vec<u32> {
    let pattern = format!("watcher-ctl run {}", name);
    let candidates = process_pids(&pattern).await;
    candidates
        .into_iter()
        .filter(|pid| is_supervisor_comm(*pid))
        .collect()
}

/// Read `/proc/PID/comm` and return true if it is a supervisor binary name
/// (`watcher-ctl` or `claude-watch`). False on any I/O error or unrelated
/// comm. Used to filter `pgrep -f` matches that would otherwise include
/// shell wrappers that ran the same command line.
fn is_supervisor_comm(pid: u32) -> bool {
    let path = format!("/proc/{}/comm", pid);
    match std::fs::read_to_string(&path) {
        Ok(s) => {
            let trimmed = s.trim();
            trimmed == "watcher-ctl" || trimmed == "claude-watch"
        }
        Err(_) => false,
    }
}

/// Load watcher entries from the primary config and an optional extra config,
/// concatenating entries from both. Missing extra file is silently ignored
/// (parse_watchers_config already returns an empty vec for missing files).
fn load_entries(config_path: &str, extra_config_path: Option<&str>) -> Vec<WatcherEntry> {
    let mut entries = parse_watchers_config(config_path);
    if let Some(extra) = extra_config_path {
        entries.extend(parse_watchers_config(extra));
    }
    entries
}

/// List all watcher entries from config.
pub fn watcher_list(config_path: &str, extra_config_path: Option<&str>) -> Vec<WatcherEntry> {
    load_entries(config_path, extra_config_path)
}

/// Get status for all watchers.
///
/// Runs the per-watcher `pgrep` lookups in parallel. For each enabled watcher
/// we issue TWO pgrep calls in parallel:
///   * pattern from watchers.conf → underlying poller PIDs (count + dup check)
///   * `watcher-ctl run <name>` → supervisor wrapper PIDs (dup check only)
/// Both fans run as `tokio::spawn` tasks so the wall-clock per status call
/// stays near one pgrep round-trip even with many watchers configured.
///
/// The supervisor lookup catches a known regression pattern: nested
/// `watcher-ctl run <name>` parents accumulating because each redundant
/// `watcher-ctl run` invocation spawns a fresh wrapper that doesn't
/// clean up its predecessors. The PID-file check that `watcher-status`
/// USED to do was completely blind to this — we'd report `ok` while
/// four supervisors raced on the same PID file.
pub async fn watcher_status(config_path: &str, extra_config_path: Option<&str>) -> Vec<WatcherStatus> {
    let entries = load_entries(config_path, extra_config_path);

    // Fan out: for each enabled watcher, spawn BOTH a poller-pid lookup and
    // a supervisor-pid lookup. Disabled watchers get `None` placeholders so
    // the result vec stays index-aligned with `entries`.
    let mut handles: Vec<Option<(_, _)>> = Vec::with_capacity(entries.len());
    for entry in &entries {
        if !entry.enabled {
            handles.push(None);
            continue;
        }
        let pattern = entry.pattern.clone();
        let name = entry.name.clone();
        let poller_h = tokio::spawn(async move { process_pids(&pattern).await });
        let sup_h = tokio::spawn(async move { supervisor_pids(&name).await });
        handles.push(Some((poller_h, sup_h)));
    }

    let mut joined: Vec<Option<(Vec<u32>, Vec<u32>)>> = Vec::with_capacity(handles.len());
    for handle in handles {
        match handle {
            Some((poller_h, sup_h)) => {
                let poller = poller_h.await.unwrap_or_default();
                let sup = sup_h.await.unwrap_or_default();
                joined.push(Some((poller, sup)));
            }
            None => joined.push(None),
        }
    }

    let mut results = Vec::with_capacity(entries.len());
    for (entry, joined_opt) in entries.iter().zip(joined.into_iter()) {
        if !entry.enabled {
            results.push(WatcherStatus {
                name: entry.name.clone(),
                status: "off".to_string(),
                count: 0,
                required: entry.min_count,
                pids: String::new(),
                enabled: false,
                dup_supervisors: Vec::new(),
                dup_pollers: Vec::new(),
            });
            continue;
        }

        let (pids, supervisors) = joined_opt.unwrap_or_default();
        let count = pids.len() as u32;
        let pid_str = pids
            .iter()
            .map(|p| p.to_string())
            .collect::<Vec<_>>()
            .join(" ");

        let dup_pollers = if pids.len() > 1 {
            pids.clone()
        } else {
            Vec::new()
        };
        let dup_supervisors = if supervisors.len() > 1 {
            supervisors
        } else {
            Vec::new()
        };

        // Status precedence: DOWN > DUPLICATE > ok. A dead poller is the more
        // urgent failure; duplicates are a state-cleanliness issue. If both
        // apply (e.g. min_count=2, only 1 poller, but 3 supervisors), the
        // dup_supervisors vec is still populated so the human sees both.
        let status = if count < entry.min_count {
            "DOWN".to_string()
        } else if !dup_pollers.is_empty() || !dup_supervisors.is_empty() {
            "DUPLICATE".to_string()
        } else {
            "ok".to_string()
        };

        results.push(WatcherStatus {
            name: entry.name.clone(),
            status,
            count,
            required: entry.min_count,
            pids: pid_str,
            enabled: true,
            dup_supervisors,
            dup_pollers,
        });
    }

    results
}

/// Run a watcher by name. Looks up the entry, rejects if disabled or no
/// start_cmd, then execs the start_cmd and waits for it to complete.
/// Returns the exit code of the child process.
pub async fn watcher_run(config_path: &str, extra_config_path: Option<&str>, name: &str) -> Result<i32, String> {
    let entries = load_entries(config_path, extra_config_path);
    let entry = entries
        .iter()
        .find(|e| e.name == name)
        .ok_or_else(|| format!("watcher '{}' not found in config", name))?;

    if !entry.enabled {
        return Err(format!("watcher '{}' is disabled", name));
    }

    let start_cmd = entry
        .start_cmd
        .as_deref()
        .ok_or_else(|| format!("no start command configured for '{}'", name))?;

    // Create PID directory if needed
    let _ = std::fs::create_dir_all(PID_DIR);

    // Print history on restart (PID file exists from previous run)
    let pid_file = format!("{}/{}.pid", PID_DIR, name);
    if std::path::Path::new(&pid_file).exists() {
        // Fire the watcher's optional on_restart_cmd handler so its
        // recent state lands in the task output. Operators wire whatever
        // history-dumping command makes sense for their integration via
        // the 6th `|`-separated field in `watchers.conf`. Daemon stays
        // integration-agnostic.
        if let Some(on_restart_cmd) = entry.on_restart_cmd.as_deref() {
            let parts: Vec<&str> = on_restart_cmd.split_whitespace().collect();
            if !parts.is_empty() {
                let _ = run_cmd_any(&parts, 10).await;
            }
        }
    }

    // Parse start_cmd into args (shell-style split)
    let args: Vec<&str> = start_cmd.split_whitespace().collect();
    if args.is_empty() {
        return Err(format!("empty start command for '{}'", name));
    }

    // Spawn child process
    let mut child = tokio::process::Command::new(args[0])
        .args(&args[1..])
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .map_err(|e| format!("failed to start '{}': {}", start_cmd, e))?;

    // Write PID file
    let pid = child.id().unwrap_or(0);
    let _ = std::fs::write(&pid_file, pid.to_string());

    // Wait for child to exit
    let status = child
        .wait()
        .await
        .map_err(|e| format!("failed to wait for '{}': {}", name, e))?;

    Ok(exit_code_from_status(
        status.code(),
        ExitStatusExt::signal(&status),
    ))
}

/// Translate a child `ExitStatus` into a Unix-conventional integer exit code.
///
/// - Normal exit: returns the child's exit code (0..=255).
/// - Signal-killed exit: returns `128 + signal_number`, matching the standard
///   shell convention (e.g. SIGTERM=15 -> 143, SIGKILL=9 -> 137).
/// - Neither code nor signal (should be impossible on Unix): returns 1.
///
/// The previous implementation collapsed signal-killed children into a flat
/// exit code of 1, indistinguishable from a real `exit 1` from the script.
/// That made every signal-terminated watcher (e.g. memory-remind getting
/// SIGTERM during /clear, watcher-restart, or compaction) look like a real
/// failure. With this translation the caller can tell exit-1 (logic failure)
/// from exit-143 (SIGTERM during normal shutdown) apart.
pub fn exit_code_from_status(code: Option<i32>, signal: Option<i32>) -> i32 {
    if let Some(c) = code {
        return c;
    }
    if let Some(s) = signal {
        return 128 + s;
    }
    1
}

/// Enable or disable a watcher by rewriting the config file.
///
/// **Cardinal rule (2026-05-01):** watchers can ONLY be started by Claude
/// Code's main loop, in the main loop's process tree. `enable` therefore
/// flips the config bit and stops there — the next `watcher-restart` /
/// session-resume run *by the main loop* is what actually spawns the
/// watcher. We do NOT `nohup` (or any other supervisor mechanism) the
/// start_cmd from this process: a daemon-spawned watcher would live in the
/// wrong process tree and become invisible to the main loop's obligation
/// gate. See `feedback_watcher-architecture-cardinal.md` in claude-config.
///
/// On disable, kills matching processes (this side is fine — the main loop
/// owns the watcher, killing it cleanly is not the same as spawning).
///
/// Watchers that must never be disabled (guardrails).
const PROTECTED_WATCHERS: &[&str] = &["memory-remind"];

pub async fn watcher_toggle(config_path: &str, name: &str, enable: bool) -> Result<String, String> {
    if !enable && PROTECTED_WATCHERS.contains(&name) {
        return Err(format!(
            "watcher '{}' is protected and cannot be disabled. \
             Edit ~/.config/watchmen/watchers.conf manually if you really mean it.",
            name
        ));
    }

    let content = std::fs::read_to_string(config_path)
        .map_err(|e| format!("failed to read config: {}", e))?;

    let new_val = if enable { "true" } else { "false" };
    let mut found = false;
    let mut target_pattern = String::new();
    let mut output_lines = Vec::new();

    for line in content.lines() {
        if line.starts_with('#') || line.trim().is_empty() {
            output_lines.push(line.to_string());
            continue;
        }

        let parts: Vec<&str> = line.split('|').collect();
        if parts.len() >= 2 && parts[0] == name {
            found = true;
            target_pattern = parts[1].to_string();
            let min_count = parts.get(2).unwrap_or(&"1");
            let start_cmd = parts.get(4).unwrap_or(&"");
            output_lines.push(format!(
                "{}|{}|{}|{}|{}",
                parts[0], parts[1], min_count, new_val, start_cmd
            ));
        } else {
            output_lines.push(line.to_string());
        }
    }

    if !found {
        return Err(format!("watcher '{}' not found in config", name));
    }

    // Write updated config
    let new_content = output_lines.join("\n") + "\n";
    let mut file =
        std::fs::File::create(config_path).map_err(|e| format!("failed to write config: {}", e))?;
    file.write_all(new_content.as_bytes())
        .map_err(|e| format!("failed to write config: {}", e))?;

    if enable {
        // Config-only flip. The main loop is responsible for spawning the
        // watcher (e.g. via `watcher-restart` or a fresh
        // `watcher-ctl run <name>` background task). We deliberately do not
        // spawn it here — see the doc comment above.
        Ok(format!(
            "{}: enabled (config flipped — main loop must spawn via \
             `watcher-ctl run {}` or `watcher-restart`)",
            name, name
        ))
    } else {
        // Kill matching processes
        let pids = process_pids(&target_pattern).await;
        if !pids.is_empty() {
            let count = pids.len();
            for pid in &pids {
                let _ = run_cmd_any(&["kill", &pid.to_string()], 5).await;
            }
            Ok(format!("{}: disabled (killed {} process(es))", name, count))
        } else {
            Ok(format!("{}: disabled (no processes running)", name))
        }
    }
}

// ---------------------------------------------------------------------------
// REMOVED 2026-05-01: daemon-side watcher auto-restart.
//
// Previous shape: `auto_restart_watcher` + a stack of `systemd-run --user`
// helpers (`supervised_unit_name`, `supervised_unit_main_pid`,
// `supervised_unit_is_active`, `supervised_unit_is_healthy_steady`,
// `user_bus_env`, `run_systemctl_user`) that the daemon's check loop called
// to spawn `watcher-ctl run <name>` as a transient user systemd unit.
//
// Why it was removed: it violated the cardinal rule that watchers can ONLY
// be started by Claude Code's main loop, in the main loop's process tree.
// A watcher inside a `claude-watch-watcher-<name>.service` user unit lives
// in `user@1000.service` slice, NOT as a descendant of Claude Code — which
// makes it invisible to the obligation gate, orphaned from the main loop's
// process model, and a surprise to the next session ("ghost watcher: alive
// but no one in claude-code spawned it"). See
// `feedback_watcher-architecture-cardinal.md` in claude-config.
//
// What replaces it: nothing in this file. The daemon's only emergency
// recovery action is now the existing tmux-inject path in `policy.rs`,
// which types `watcher-ctl run <name>` into the Claude Code pane so the
// MAIN LOOP spawns the watcher in its own process tree. claude-watch
// (the daemon) never touches the watcher process directly.
// ---------------------------------------------------------------------------

/// Kill all enabled watcher processes and clean PID files.
pub async fn watcher_restart(config_path: &str, extra_config_path: Option<&str>) -> String {
    let entries = load_entries(config_path, extra_config_path);
    let mut total = 0u32;
    let mut messages = Vec::new();

    for entry in &entries {
        if !entry.enabled {
            continue;
        }
        let pids = process_pids(&entry.pattern).await;
        if !pids.is_empty() {
            let count = pids.len() as u32;
            for pid in &pids {
                let _ = run_cmd_any(&["kill", &pid.to_string()], 5).await;
            }
            messages.push(format!("Killed {} {} process(es)", count, entry.name));
            total += count;
        }
    }

    // Clean PID files
    if let Ok(dir) = std::fs::read_dir(PID_DIR) {
        for entry in dir.flatten() {
            if entry.path().extension().is_some_and(|ext| ext == "pid") {
                let _ = std::fs::remove_file(entry.path());
            }
        }
        messages.push("Cleaned PID files".to_string());
    }

    if total == 0 {
        messages.push("No watchers running.".to_string());
    } else {
        messages.push(format!(
            "\nKilled {} total process(es). All watchers stopped.",
            total
        ));
    }

    messages.join("\n")
}

// --- CLI command handlers ---

/// `claude-watch watcher list [--json]`
pub fn cmd_list(config_path: &str, extra_config_path: Option<&str>, json: bool) {
    let entries = watcher_list(config_path, extra_config_path);

    if json {
        let items: Vec<serde_json::Value> = entries
            .iter()
            .map(|e| {
                serde_json::json!({
                    "name": e.name,
                    "pattern": e.pattern,
                    "min_count": e.min_count,
                    "enabled": e.enabled,
                    "start_cmd": e.start_cmd,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&items).unwrap());
    } else {
        println!("{:<20} {:<8} PATTERN", "NAME", "ENABLED");
        println!("{:<20} {:<8} -------", "----", "-------");
        for e in &entries {
            println!("{:<20} {:<8} {}", e.name, e.enabled, e.pattern);
        }
    }
}

/// `claude-watch watcher status [--json] [--unhealthy-only]`
///
/// `unhealthy_only`: when set, the command emits NOTHING and returns exit 0
/// if every enabled watcher is `ok`. If any enabled watcher is `DOWN` *or*
/// `DUPLICATE` the full status output is printed (same format as the default
/// case) so the caller can see what's wrong. Designed for the PostToolUse
/// hook that surfaces watcher health on every tool call.
pub async fn cmd_status(config_path: &str, extra_config_path: Option<&str>, json: bool, unhealthy_only: bool) {
    let statuses = watcher_status(config_path, extra_config_path).await;

    if unhealthy_only && !any_unhealthy(&statuses) {
        // Stay silent when everything is healthy. JSON mode gets the same
        // silence treatment so the hook stays non-spammy in either case.
        return;
    }

    if json {
        println!("{}", serde_json::to_string_pretty(&statuses).unwrap());
    } else {
        print!("{}", format_status(&statuses));
    }
}

/// True iff at least one watcher is unhealthy (`DOWN` or `DUPLICATE`).
/// Disabled (`off`) and `ok` watchers do not count.
pub fn any_unhealthy(statuses: &[WatcherStatus]) -> bool {
    statuses
        .iter()
        .any(|s| s.status == "DOWN" || s.status == "DUPLICATE")
}

/// `claude-watch watcher run <name>`
pub async fn cmd_run(config_path: &str, extra_config_path: Option<&str>, name: &str) -> i32 {
    match watcher_run(config_path, extra_config_path, name).await {
        Ok(code) => code,
        Err(msg) => {
            eprintln!("Error: {}", msg);
            1
        }
    }
}

/// `claude-watch watcher enable <name>` / `claude-watch watcher disable <name>`
pub async fn cmd_toggle(config_path: &str, name: &str, enable: bool) -> i32 {
    match watcher_toggle(config_path, name, enable).await {
        Ok(msg) => {
            println!("{}", msg);
            0
        }
        Err(msg) => {
            eprintln!("Error: {}", msg);
            1
        }
    }
}

/// `claude-watch watcher restart`
pub async fn cmd_restart(config_path: &str, extra_config_path: Option<&str>) {
    let output = watcher_restart(config_path, extra_config_path).await;
    println!("{}", output);
}

// --- Pure function tests ---

/// Pure function: format watcher list output (for testing without I/O).
#[allow(dead_code)]
pub fn format_list(entries: &[WatcherEntry]) -> String {
    let mut out = String::new();
    out.push_str(&format!("{:<20} {:<8} {}\n", "NAME", "ENABLED", "PATTERN"));
    out.push_str(&format!("{:<20} {:<8} {}\n", "----", "-------", "-------"));
    for e in entries {
        out.push_str(&format!("{:<20} {:<8} {}\n", e.name, e.enabled, e.pattern));
    }
    out
}

/// Pure function: format watcher status output.
///
/// Used by `cmd_status` for the human-readable text rendering, and by tests
/// for I/O-free assertions.
///
/// Output shape:
///
/// ```text
/// alerts-watcher       ok        (1/1)  783136
/// claude-event-watch   DOWN      (0/1)
/// alerts-watcher       DUPLICATE (3/1)  783136 1234567 8901234
///                      duplicate pollers: 783136 1234567 8901234
///                      duplicate supervisors: 358036 359170 705775
/// ```
///
/// The duplicate-detail lines are indented under the affected watcher and
/// only emitted when the corresponding list is non-empty. They are
/// machine-greppable via the literal substrings `duplicate pollers:` /
/// `duplicate supervisors:`.
///
/// Healthy-state output (`ok` / `off`) is byte-for-byte unchanged from the
/// pre-DUPLICATE rendering so downstream parsers (cron jobs, dashboards)
/// that grep for `ok` keep working. The status column widens from 4 to 9
/// characters to fit the literal `DUPLICATE` (and the `DOWN` / `ok` rows
/// just get a few extra trailing spaces — still parses fine).
pub fn format_status(statuses: &[WatcherStatus]) -> String {
    let mut out = String::new();
    let mut all_healthy = true;
    let mut down_names: Vec<String> = Vec::new();
    let mut has_duplicate = false;
    for s in statuses {
        if s.status == "off" {
            out.push_str(&format!("{:<20} {:<9} (disabled)\n", s.name, s.status));
        } else {
            if s.status == "DOWN" || s.status == "DUPLICATE" {
                all_healthy = false;
            }
            if s.status == "DOWN" {
                down_names.push(s.name.clone());
            }
            if s.status == "DUPLICATE" {
                has_duplicate = true;
            }
            out.push_str(&format!(
                "{:<20} {:<9} ({}/{})  {}\n",
                s.name, s.status, s.count, s.required, s.pids
            ));
            // Indented detail lines for duplicates. The 21-space gutter
            // (column 22) lines up under the status column so the output
            // is scannable.
            if !s.dup_pollers.is_empty() {
                let pids = s
                    .dup_pollers
                    .iter()
                    .map(|p| p.to_string())
                    .collect::<Vec<_>>()
                    .join(" ");
                out.push_str(&format!("{:<21}duplicate pollers: {}\n", "", pids));
            }
            if !s.dup_supervisors.is_empty() {
                let pids = s
                    .dup_supervisors
                    .iter()
                    .map(|p| p.to_string())
                    .collect::<Vec<_>>()
                    .join(" ");
                out.push_str(&format!(
                    "{:<21}duplicate supervisors: {}\n",
                    "", pids
                ));
            }
        }
    }
    if all_healthy {
        out.push_str("\nAll watchers healthy.\n");
    } else {
        out.push_str("\nWARNING: Some watchers are down or duplicated!\n");
        // State-aware recovery suggestion. The footer is the canonical
        // place for an actionable next step; the per-row text above stays
        // pure status data so existing parsers (cron jobs, dashboards)
        // don't have to filter prose. DUPLICATE always wins because
        // `watcher-restart` is a superset fix (kills everything, lets
        // supervisors respawn DOWN watchers from a clean slate); a per-
        // watcher `watcher-ctl run <name>` wouldn't clear the duplicate
        // pollers/supervisors.
        if has_duplicate {
            out.push_str(
                "Recovery for DUPLICATE state: `watcher-restart` \
                 (kills all watchers + cleans PID files; supervisors will respawn).\n",
            );
        } else if !down_names.is_empty() {
            // DOWN-only: per-watcher restart is the surgical fix.
            let names = down_names.join(" ");
            out.push_str(&format!(
                "Recovery for DOWN state: `watcher-ctl run <name>` (e.g. {}). \
                 Or `watcher-restart` to reset everything.\n",
                names
            ));
        }
    }
    out
}

/// Pure function: rewrite config content toggling the enabled field for a watcher.
/// Returns the new config content, or None if the watcher was not found.
#[allow(dead_code)]
pub fn rewrite_config_toggle(content: &str, name: &str, enable: bool) -> Option<String> {
    let new_val = if enable { "true" } else { "false" };
    let mut found = false;
    let mut output_lines = Vec::new();

    for line in content.lines() {
        if line.starts_with('#') || line.trim().is_empty() {
            output_lines.push(line.to_string());
            continue;
        }

        let parts: Vec<&str> = line.split('|').collect();
        if parts.len() >= 2 && parts[0] == name {
            found = true;
            let min_count = parts.get(2).unwrap_or(&"1");
            let start_cmd = parts.get(4).unwrap_or(&"");
            output_lines.push(format!(
                "{}|{}|{}|{}|{}",
                parts[0], parts[1], min_count, new_val, start_cmd
            ));
        } else {
            output_lines.push(line.to_string());
        }
    }

    if found {
        Some(output_lines.join("\n") + "\n")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_list_basic() {
        let entries = vec![
            WatcherEntry {
                name: "alerts".to_string(),
                pattern: "alerts$".to_string(),
                min_count: 1,
                enabled: true,
                start_cmd: Some("alerts-watcher".to_string()),
                on_restart_cmd: None,
            },
            WatcherEntry {
                name: "torrent".to_string(),
                pattern: "torrent$".to_string(),
                min_count: 1,
                enabled: false,
                start_cmd: None,
                on_restart_cmd: None,
            },
        ];
        let output = format_list(&entries);
        assert!(output.contains("alerts"));
        assert!(output.contains("torrent"));
        assert!(output.contains("true"));
        assert!(output.contains("false"));
    }

    /// Test helper: build a healthy `ok` watcher status.
    fn ok_status(name: &str, count: u32, required: u32, pids: &str) -> WatcherStatus {
        WatcherStatus {
            name: name.to_string(),
            status: "ok".to_string(),
            count,
            required,
            pids: pids.to_string(),
            enabled: true,
            dup_supervisors: Vec::new(),
            dup_pollers: Vec::new(),
        }
    }

    /// Test helper: build a `DOWN` watcher status.
    fn down_status(name: &str, required: u32) -> WatcherStatus {
        WatcherStatus {
            name: name.to_string(),
            status: "DOWN".to_string(),
            count: 0,
            required,
            pids: String::new(),
            enabled: true,
            dup_supervisors: Vec::new(),
            dup_pollers: Vec::new(),
        }
    }

    #[test]
    fn test_format_status_all_ok() {
        let statuses = vec![ok_status("alerts", 1, 1, "1234")];
        let output = format_status(&statuses);
        assert!(output.contains("ok"));
        assert!(output.contains("All watchers healthy."));
        // Healthy-state output must NOT mention "duplicate" — that's the
        // whole point of keeping the existing format byte-stable for healthy
        // rows.
        assert!(!output.contains("duplicate"));
    }

    #[test]
    fn test_format_status_some_down() {
        let statuses = vec![ok_status("alerts", 1, 1, "1234"), down_status("torrent", 1)];
        let output = format_status(&statuses);
        assert!(output.contains("DOWN"));
        assert!(output.contains("WARNING: Some watchers are down or duplicated!"));
    }

    #[test]
    fn test_format_status_disabled() {
        let statuses = vec![WatcherStatus {
            name: "ctx".to_string(),
            status: "off".to_string(),
            count: 0,
            required: 1,
            pids: String::new(),
            enabled: false,
            dup_supervisors: Vec::new(),
            dup_pollers: Vec::new(),
        }];
        let output = format_status(&statuses);
        assert!(output.contains("off"));
        assert!(output.contains("disabled"));
        assert!(output.contains("All watchers healthy."));
    }

    #[test]
    fn test_rewrite_config_enable() {
        let config =
            "# comment\nalerts|alerts$|1|false|alerts-watcher\ntorrent|torrent$|1|true|torrent-wait\n";
        let result = rewrite_config_toggle(config, "alerts", true).unwrap();
        assert!(result.contains("alerts|alerts$|1|true|alerts-watcher"));
        assert!(result.contains("torrent|torrent$|1|true|torrent-wait"));
    }

    #[test]
    fn test_rewrite_config_disable() {
        let config = "alerts|alerts$|1|true|alerts-watcher\n";
        let result = rewrite_config_toggle(config, "alerts", false).unwrap();
        assert!(result.contains("alerts|alerts$|1|false|alerts-watcher"));
    }

    #[test]
    fn test_rewrite_config_not_found() {
        let config = "alerts|alerts$|1|true|alerts-watcher\n";
        let result = rewrite_config_toggle(config, "nonexistent", true);
        assert!(result.is_none());
    }

    #[test]
    fn test_rewrite_config_preserves_comments() {
        let config = "# header comment\n\nsig|sig$|1|true|cmd\n# footer\n";
        let result = rewrite_config_toggle(config, "sig", false).unwrap();
        assert!(result.contains("# header comment"));
        assert!(result.contains("# footer"));
        assert!(result.contains("false"));
    }

    #[test]
    fn test_protected_watchers_includes_memory_remind() {
        // memory-remind is a guardrail and must never be removable from
        // the protected list without a deliberate code change.
        assert!(super::PROTECTED_WATCHERS.contains(&"memory-remind"));
    }

    #[test]
    fn test_rewrite_config_minimal_fields() {
        let config = "alerts|alerts$\n";
        let result = rewrite_config_toggle(config, "alerts", false).unwrap();
        assert!(result.contains("alerts|alerts$|1|false|"));
    }

    #[test]
    fn test_format_list_empty() {
        let entries: Vec<WatcherEntry> = vec![];
        let output = format_list(&entries);
        assert!(output.contains("NAME"));
        // Just headers, no entries
        assert_eq!(output.lines().count(), 2);
    }

    // --- DUPLICATE detection tests -------------------------
    //
    // These guard the regression pattern where nested `watcher-ctl run
    // <name>` supervisors accumulate, all alive, racing on one PID file.
    // The old `watcher-status` was completely blind because it only
    // checked the single PID written to /var/run/claude/<name>.pid.

    #[test]
    fn test_format_status_duplicate_pollers() {
        // 3 pollers running when min_count is 1 → DUPLICATE row + a
        // "duplicate pollers:" detail line listing all three PIDs.
        let statuses = vec![WatcherStatus {
            name: "alerts-watcher".to_string(),
            status: "DUPLICATE".to_string(),
            count: 3,
            required: 1,
            pids: "111 222 333".to_string(),
            enabled: true,
            dup_supervisors: Vec::new(),
            dup_pollers: vec![111, 222, 333],
        }];
        let output = format_status(&statuses);
        assert!(output.contains("DUPLICATE"));
        assert!(
            output.contains("duplicate pollers: 111 222 333"),
            "expected the offending poller PIDs to be printed verbatim under \
             the affected watcher row, got:\n{}",
            output
        );
        // Must NOT mention supervisors (none reported)
        assert!(!output.contains("duplicate supervisors"));
        assert!(output.contains("WARNING: Some watchers are down or duplicated!"));
    }

    #[test]
    fn test_format_status_duplicate_supervisors_only() {
        // The 2026-04-27 case: poller count is 1 (healthy) but the
        // `watcher-ctl run` supervisor wrappers have piled up (4 nested
        // parents, all alive). Status is DUPLICATE; the offending wrapper
        // PIDs are listed.
        let statuses = vec![WatcherStatus {
            name: "alerts-watcher".to_string(),
            status: "DUPLICATE".to_string(),
            count: 1,
            required: 1,
            pids: "783136".to_string(),
            enabled: true,
            dup_supervisors: vec![358036, 359170, 705775, 761576],
            dup_pollers: Vec::new(),
        }];
        let output = format_status(&statuses);
        assert!(output.contains("DUPLICATE"));
        assert!(
            output.contains("duplicate supervisors: 358036 359170 705775 761576"),
            "expected supervisor PIDs to be printed verbatim, got:\n{}",
            output
        );
        // Single poller → no poller-dup line
        assert!(!output.contains("duplicate pollers"));
    }

    #[test]
    fn test_format_status_duplicate_both() {
        // Pathological: dup pollers AND dup supervisors. Both detail lines
        // must appear under the affected watcher.
        let statuses = vec![WatcherStatus {
            name: "alerts-watcher".to_string(),
            status: "DUPLICATE".to_string(),
            count: 2,
            required: 1,
            pids: "100 200".to_string(),
            enabled: true,
            dup_supervisors: vec![10, 20],
            dup_pollers: vec![100, 200],
        }];
        let output = format_status(&statuses);
        assert!(output.contains("duplicate pollers: 100 200"));
        assert!(output.contains("duplicate supervisors: 10 20"));
    }

    #[test]
    fn test_format_status_down_takes_precedence_over_duplicate() {
        // Scenario constructed by the orchestrator: poller count is 0
        // (DOWN) but the supervisor wrappers are still alive. We want the
        // top-line status to show DOWN (more urgent) yet still print the
        // supervisor-dup detail line so Andrew sees the full picture.
        let statuses = vec![WatcherStatus {
            name: "alerts-watcher".to_string(),
            status: "DOWN".to_string(),
            count: 0,
            required: 1,
            pids: String::new(),
            enabled: true,
            dup_supervisors: vec![10, 20],
            dup_pollers: Vec::new(),
        }];
        let output = format_status(&statuses);
        // DOWN appears as the headline status
        assert!(
            output.contains("DOWN"),
            "DOWN must be the visible top-line status when both DOWN and \
             dup-supervisors are present"
        );
        // Supervisor-dup detail still surfaces
        assert!(output.contains("duplicate supervisors: 10 20"));
    }

    #[test]
    fn test_any_unhealthy_includes_duplicate() {
        // `--unhealthy-only` MUST trigger on DUPLICATE rows, not just DOWN.
        let dup = vec![WatcherStatus {
            name: "x".to_string(),
            status: "DUPLICATE".to_string(),
            count: 2,
            required: 1,
            pids: "1 2".to_string(),
            enabled: true,
            dup_supervisors: Vec::new(),
            dup_pollers: vec![1, 2],
        }];
        assert!(any_unhealthy(&dup), "DUPLICATE must count as unhealthy");

        let down = vec![down_status("x", 1)];
        assert!(any_unhealthy(&down), "DOWN must count as unhealthy");

        let healthy = vec![ok_status("x", 1, 1, "1")];
        assert!(
            !any_unhealthy(&healthy),
            "all-ok must NOT trigger unhealthy"
        );

        let off = vec![WatcherStatus {
            name: "x".to_string(),
            status: "off".to_string(),
            count: 0,
            required: 1,
            pids: String::new(),
            enabled: false,
            dup_supervisors: Vec::new(),
            dup_pollers: Vec::new(),
        }];
        assert!(!any_unhealthy(&off), "disabled (off) must NOT trigger");
    }

    #[test]
    fn test_format_status_machine_greppable() {
        // The detail-line literals are an external interface — the q-7950
        // PostToolUse hook (or any future watcher dashboard) needs stable
        // substrings to grep on. Lock the spelling.
        let statuses = vec![WatcherStatus {
            name: "x".to_string(),
            status: "DUPLICATE".to_string(),
            count: 2,
            required: 1,
            pids: "1 2".to_string(),
            enabled: true,
            dup_supervisors: vec![3, 4],
            dup_pollers: vec![1, 2],
        }];
        let output = format_status(&statuses);
        // These exact substrings are part of the public contract
        assert!(output.contains("duplicate pollers:"));
        assert!(output.contains("duplicate supervisors:"));
        // DUPLICATE keyword in the status column is also greppable
        assert!(output.contains("DUPLICATE"));
    }

    // --- State-aware recovery suggestion tests (q-2026-05-01-d487) -------
    //
    // The footer must DIFFERENTIATE the recovery command by the failure
    // state. DUPLICATE => `watcher-restart` (the only thing that clears
    // duplicate pollers/supervisors); DOWN-only => per-watcher
    // `watcher-ctl run <name>` (surgical), with `watcher-restart` as a
    // secondary option.

    #[test]
    fn test_format_status_duplicate_suggests_watcher_restart() {
        let statuses = vec![WatcherStatus {
            name: "alerts-watcher".to_string(),
            status: "DUPLICATE".to_string(),
            count: 3,
            required: 1,
            pids: "111 222 333".to_string(),
            enabled: true,
            dup_supervisors: Vec::new(),
            dup_pollers: vec![111, 222, 333],
        }];
        let output = format_status(&statuses);
        assert!(
            output.contains("Recovery for DUPLICATE state:"),
            "expected 'Recovery for DUPLICATE state:' footer, got:\n{}",
            output
        );
        assert!(
            output.contains("`watcher-restart`"),
            "expected the literal `watcher-restart` (backticks) as the \
             recovery command for DUPLICATE state, got:\n{}",
            output
        );
        // DUPLICATE-only must NOT recommend `watcher-ctl run <name>` as
        // the primary path: that command can't kill duplicate
        // supervisors/pollers, so it would just leave the user in the
        // same state.
        assert!(
            !output.contains("Recovery for DOWN state:"),
            "DUPLICATE-only must not surface the DOWN recovery line, \
             got:\n{}",
            output
        );
    }

    #[test]
    fn test_format_status_down_only_suggests_watcher_ctl_run() {
        let statuses = vec![down_status("claude-event-watch", 1)];
        let output = format_status(&statuses);
        assert!(
            output.contains("Recovery for DOWN state:"),
            "expected 'Recovery for DOWN state:' footer, got:\n{}",
            output
        );
        assert!(
            output.contains("`watcher-ctl run <name>`"),
            "expected `watcher-ctl run <name>` as the surgical recovery \
             command for DOWN state, got:\n{}",
            output
        );
        // The footer should name the actually-DOWN watcher in the
        // example.
        assert!(
            output.contains("claude-event-watch"),
            "expected the DOWN watcher's name to appear in the recovery \
             example, got:\n{}",
            output
        );
        // `watcher-restart` should still appear as a fallback.
        assert!(
            output.contains("`watcher-restart`"),
            "expected `watcher-restart` mentioned as the fallback, got:\n{}",
            output
        );
    }

    #[test]
    fn test_format_status_mixed_down_and_duplicate_prefers_watcher_restart() {
        // When DOWN and DUPLICATE coexist, `watcher-restart` is the
        // superset fix (clears duplicates AND the supervisors will
        // respawn the DOWN ones). The per-watcher `watcher-ctl run`
        // path would still leave the duplicates in place, so the
        // primary recommendation should be `watcher-restart`.
        let statuses = vec![
            down_status("claude-event-watch", 1),
            WatcherStatus {
                name: "alerts-watcher".to_string(),
                status: "DUPLICATE".to_string(),
                count: 3,
                required: 1,
                pids: "111 222 333".to_string(),
                enabled: true,
                dup_supervisors: Vec::new(),
                dup_pollers: vec![111, 222, 333],
            },
        ];
        let output = format_status(&statuses);
        assert!(
            output.contains("Recovery for DUPLICATE state:"),
            "DUPLICATE wins precedence in mixed state, got:\n{}",
            output
        );
        assert!(
            output.contains("`watcher-restart`"),
            "expected `watcher-restart` as the recovery command, got:\n{}",
            output
        );
    }

    #[test]
    fn test_format_status_healthy_no_recovery_footer() {
        // The recovery hints must only appear when something is wrong;
        // an all-healthy run should print only "All watchers healthy."
        let statuses = vec![ok_status("alerts-watcher", 1, 1, "1234")];
        let output = format_status(&statuses);
        assert!(output.contains("All watchers healthy."));
        assert!(
            !output.contains("Recovery for"),
            "healthy state must not include any 'Recovery for ...' line, \
             got:\n{}",
            output
        );
    }

    #[test]
    fn test_is_supervisor_comm_self() {
        // Read our own /proc/self/comm — should NOT match watcher-ctl /
        // claude-watch when the test runner is `cargo test`. This sanity-
        // checks the comm-filter logic against a known non-supervisor
        // process.
        let pid = std::process::id();
        // The test binary's comm is something like `watcher_status-<hash>`
        // or `cargo-test`. Either way, NOT `watcher-ctl`.
        assert!(
            !is_supervisor_comm(pid),
            "test runner should not be classified as a supervisor"
        );
    }

    #[test]
    fn test_is_supervisor_comm_nonexistent_pid() {
        // PID 0 doesn't have a /proc entry on Linux → should return false
        // without panicking. Same for any PID that isn't currently alive.
        assert!(!is_supervisor_comm(0));
    }

    // --- watcher_toggle::enable: config-only flip (cardinal-rule guard) ---
    //
    // Andrew's cardinal rule (2026-05-01): watchers can ONLY be started by
    // Claude Code's main loop. `watcher_toggle(_, _, true)` therefore must
    // NOT spawn the start_cmd via `nohup` (or any other mechanism). It only
    // flips the config bit — a subsequent `watcher-ctl run <name>` from the
    // main loop is what actually starts the process.

    #[tokio::test]
    async fn test_watcher_toggle_enable_is_config_only() {
        // The watcher's pattern is a unique sentinel. After enabling we must
        // NOT see any process matching that pattern: enable is config-only.
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("watchers.conf");
        let sentinel = format!("cw-test-enable-sentinel-{}", std::process::id());
        // start_cmd is a no-op `true` invocation; even if we accidentally
        // spawned it, no `pgrep -f` for the sentinel would match. We use the
        // sentinel as the *pattern* so a buggy spawn (which would have used
        // the start_cmd) wouldn't show up here either — what we're actually
        // asserting is the success-message text and the absence of a
        // `started, pid` substring that the old nohup path emitted.
        std::fs::write(
            &cfg,
            format!("toggle-test|{}|1|false|true\n", sentinel),
        )
        .unwrap();

        let msg = watcher_toggle(cfg.to_str().unwrap(), "toggle-test", true)
            .await
            .expect("enable should succeed for a known watcher");
        // Config-only flip — no `started, pid` substring, which was the
        // signature of the old nohup spawn path.
        assert!(
            !msg.contains("started, pid"),
            "enable must NOT report a spawn pid (cardinal rule), got: {}",
            msg
        );
        // Confirm the new config-only message structure.
        assert!(
            msg.contains("config flipped") && msg.contains("main loop must spawn"),
            "enable must clearly indicate config-only behavior, got: {}",
            msg
        );

        // Verify the file actually got the enabled flag flipped.
        let content = std::fs::read_to_string(&cfg).unwrap();
        assert!(
            content.contains("toggle-test|") && content.contains("|true|"),
            "config file should have enabled=true, got: {}",
            content
        );
    }

    #[tokio::test]
    async fn test_watcher_toggle_enable_does_not_spawn_process() {
        // Stronger guard: after `enable`, there must be no descendant
        // process matching the watcher's pattern. This is the test that
        // would catch a regression where someone re-introduces the nohup
        // spawn path.
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("watchers.conf");
        let sentinel = format!("cw-test-no-spawn-{}-{}", std::process::id(), std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0));
        // start_cmd that, IF spawned, would be visible to pgrep.
        let start = format!("sleep 30 # {}", sentinel);
        std::fs::write(
            &cfg,
            format!("toggle-test|{}|1|false|{}\n", sentinel, start),
        )
        .unwrap();

        let _ = watcher_toggle(cfg.to_str().unwrap(), "toggle-test", true)
            .await
            .expect("enable should succeed");

        // Give any rogue spawn a chance to actually fire.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let pids = process_pids(&sentinel).await;
        assert!(
            pids.is_empty(),
            "watcher_toggle enable must NOT spawn the start_cmd (cardinal \
             rule). Found PIDs: {:?}",
            pids
        );
    }

    // --- exit_code_from_status tests ---
    //
    // Regression suite for memory-remind exit-1 bug: when bash gets SIGTERM
    // (during /clear, watcher-restart, or compaction) we used to collapse the
    // signal-killed exit into a flat `1` via `unwrap_or(1)`, indistinguishable
    // from a real script `exit 1`. The fix returns `128 + signo` (Unix
    // convention) so SIGTERM surfaces as 143 instead.

    #[test]
    fn test_exit_code_from_status_normal_zero() {
        assert_eq!(super::exit_code_from_status(Some(0), None), 0);
    }

    #[test]
    fn test_exit_code_from_status_normal_nonzero() {
        // A real `exit 1` from the script should still be reported as 1.
        assert_eq!(super::exit_code_from_status(Some(1), None), 1);
        assert_eq!(super::exit_code_from_status(Some(2), None), 2);
        assert_eq!(super::exit_code_from_status(Some(127), None), 127);
    }

    #[test]
    fn test_exit_code_from_status_sigterm() {
        // SIGTERM (15) — this is the case that bit memory-remind. Must NOT
        // collapse to 1; must report 143 so the caller can see "killed by
        // SIGTERM" rather than mistake it for a logic failure.
        assert_eq!(super::exit_code_from_status(None, Some(15)), 143);
    }

    #[test]
    fn test_exit_code_from_status_sigkill() {
        // SIGKILL (9) — surfaces as 137.
        assert_eq!(super::exit_code_from_status(None, Some(9)), 137);
    }

    #[test]
    fn test_exit_code_from_status_sigint() {
        // SIGINT (2) — surfaces as 130.
        assert_eq!(super::exit_code_from_status(None, Some(2)), 130);
    }

    #[test]
    fn test_exit_code_from_status_neither_falls_back_to_one() {
        // Defensive: if neither code nor signal is present (should be
        // impossible on Unix), preserve the old fallback of 1.
        assert_eq!(super::exit_code_from_status(None, None), 1);
    }

    #[test]
    fn test_exit_code_from_status_normal_takes_precedence() {
        // If both are somehow present, prefer the explicit exit code.
        assert_eq!(super::exit_code_from_status(Some(0), Some(15)), 0);
        assert_eq!(super::exit_code_from_status(Some(7), Some(15)), 7);
    }
}
