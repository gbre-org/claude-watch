//! Claude Code subagent management — list and kill agents.
//!
//! Replaces the Python `agent-ctl` script. Identifies agent child processes by
//! finding the main Claude Code PID, listing children, filtering out watchers,
//! and cross-referencing with agent JSONL metadata files.

use regex_lite::Regex;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Built-in watcher command patterns — children matching these are
/// classified as long-running watchers, NOT subagents. `watcher-ctl`
/// covers the canonical supervisor form (`watcher-ctl run <name>`).
/// The remaining `*-wait` / `*-remind` / `*-watch` entries cover the
/// stock watcher binaries shipped under `tools/watchers/`, plus the
/// generic `request-wait` / `task-watch` wrappers.
///
/// Operators with additional site-specific watcher names should extend
/// this list via `CLAUDE_WATCH_EXTRA_WATCHER_PATTERNS` (colon-separated)
/// — see `is_watcher` for the merge logic.
pub const WATCHER_PATTERNS: &[&str] = &[
    "watcher-ctl",
    "watchmen",
    "memory-remind",
    "context-watch",
    "task-watch",
    "request-wait",
];

/// Read additional watcher patterns from the
/// `CLAUDE_WATCH_EXTRA_WATCHER_PATTERNS` env var (colon-separated).
/// Returns an empty Vec when unset / empty.
pub fn extra_watcher_patterns() -> Vec<String> {
    std::env::var("CLAUDE_WATCH_EXTRA_WATCHER_PATTERNS")
        .ok()
        .map(|s| {
            s.split(':')
                .map(|p| p.trim().to_string())
                .filter(|p| !p.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// Claude tasks base directory PARENT (per-uid).
///
/// Claude Code writes per-session task output files under
/// `/tmp/claude-<uid>/<project-slug>/<session-uuid>/tasks/`. The
/// project slug is derived from the cwd at session start, with `/`
/// replaced by `-` (e.g. `/home/alice` -> `-home-alice`). We compute
/// the slug at runtime from `$HOME` so the same binary works on any
/// account without a hardcoded path.
const CLAUDE_TASKS_PARENT: &str = "/tmp/claude-1000";

/// Compute the project-slug-derived tasks base for the current user.
///
/// Returns e.g. `/tmp/claude-1000/-home-alice` for `$HOME=/home/alice`.
/// Defaults to `/tmp/claude-1000/-home-user` if `$HOME` is unset (which
/// keeps prior behavior in test environments).
pub fn claude_tasks_base() -> String {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/home/user".to_string());
    let slug: String = home
        .chars()
        .map(|c| if c == '/' { '-' } else { c })
        .collect();
    format!("{}/{}", CLAUDE_TASKS_PARENT, slug)
}

/// A child process of Claude Code.
#[derive(Debug, Clone)]
pub struct ChildProcess {
    pub pid: u32,
    pub cmd: String,
}

/// Agent metadata loaded from JSONL files.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct AgentInfo {
    pub description: String,
    pub agent_type: String,
    pub last_bash_cmd: Option<String>,
    pub jsonl_path: PathBuf,
    pub jsonl_mtime: f64,
}

/// Agent metadata from the .meta.json file.
#[derive(Debug, Deserialize)]
struct AgentMeta {
    #[serde(default)]
    description: String,
    #[serde(rename = "agentType", default)]
    agent_type: String,
}

/// Extract the eval'd command from a zsh wrapper command string.
///
/// Matches patterns like:
///   - `... eval 'COMMAND' < /dev/null ...` (quoted)
///   - `... eval COMMAND \< /dev/null ...` (unquoted, escaped redirect)
pub fn extract_eval_command(cmd: &str) -> String {
    let re_single = Regex::new(r"eval '([^']+)'").unwrap();
    if let Some(caps) = re_single.captures(cmd) {
        return caps[1].to_string();
    }
    let re_double = Regex::new(r#"eval "([^"]+)""#).unwrap();
    if let Some(caps) = re_double.captures(cmd) {
        return caps[1].to_string();
    }
    // Unquoted eval: "eval COMMAND \< /dev/null" or "eval COMMAND < /dev/null"
    let re_unquoted = Regex::new(r"eval\s+(\S+)").unwrap();
    if let Some(caps) = re_unquoted.captures(cmd) {
        return caps[1].to_string();
    }
    cmd.to_string()
}

/// Check if a command matches known watcher patterns. Honors both the
/// built-in `WATCHER_PATTERNS` and any operator-supplied additions
/// via `CLAUDE_WATCH_EXTRA_WATCHER_PATTERNS`.
pub fn is_watcher(cmd: &str) -> bool {
    let eval_cmd = extract_eval_command(cmd);
    for pattern in WATCHER_PATTERNS {
        if eval_cmd.starts_with(pattern) || eval_cmd.starts_with(&format!("'{}", pattern)) {
            return true;
        }
    }
    for pattern in extra_watcher_patterns() {
        if eval_cmd.starts_with(&pattern) || eval_cmd.starts_with(&format!("'{}", pattern)) {
            return true;
        }
    }
    false
}

/// Check if this is our own command (agent-ctl or ps).
pub fn is_own_command(cmd: &str) -> bool {
    cmd.contains("agent-ctl") || cmd.contains("claude-watch agent") || cmd.starts_with("ps ")
}

/// Find the main Claude Code process PID by checking /proc/PID/exe.
///
/// Matches against `${HOME}/.local/share/claude/versions/` (Claude Code's
/// native-install path).
///
/// When `CLAUDE_WATCH_CONTAINER_MODE=1` is set in the daemon's
/// environment, ALSO matches against the npm-global install path used
/// by the in-container Claude Code build:
/// `/home/hndrewaall/.npm-global/lib/node_modules/@anthropic-ai/claude-code/`.
/// The container image bakes Claude Code via `npm install -g` with the
/// npm prefix pointing at $HOME/.npm-global (the canonical unprivileged
/// npm-global pattern), so before any in-container self-update lands a
/// versions/<ver>/ entry into the claude-container-versions named
/// volume, the running exe lives at the npm-global path. The diagnostic
/// at claude-watch-container-diagnostic.md flagged this mismatch as one
/// of the reasons the host daemon could see the container's claude PID
/// via `/proc` (PID namespace flat-visible) but never returned it from
/// this function. Path moved from /usr/lib/node_modules/ to
/// $HOME/.npm-global/lib/node_modules/ in the autoupdate-v2 fix
/// (2026-05-15) so `claude update` can write to its install tree as
/// uid 1000 without privilege escalation.
pub fn find_claude_pid() -> Option<u32> {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/home/user".to_string());
    let versions_dir = format!("{}/.local/share/claude/versions", home);
    find_claude_pid_with_versions_dir(&versions_dir)
}

/// Container-mode npm-global install path. Constant so the test suite
/// can assert against it without duplicating the literal. Path is the
/// uid-1000-owned npm prefix under $HOME — see autoupdate-v2 fix
/// 2026-05-15 (container Dockerfile npm-install block).
pub const CONTAINER_CLAUDE_EXE_PREFIX: &str =
    "/home/hndrewaall/.npm-global/lib/node_modules/@anthropic-ai/claude-code/";

/// Testable version of find_claude_pid with configurable versions dir.
pub fn find_claude_pid_with_versions_dir(versions_dir: &str) -> Option<u32> {
    let container_mode = std::env::var("CLAUDE_WATCH_CONTAINER_MODE")
        .ok()
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    find_claude_pid_with_paths(versions_dir, container_mode)
}

/// Walk a process subtree rooted at `root_pid` (typically a tmux pane's
/// pane_pid, i.e. the shell) looking for a claude binary descendant.
/// Returns the first matching PID found, or `None` if no descendant
/// matches the configured claude installation paths within `max_depth`.
///
/// Used by the inject dispatcher to map a tmux pane to the actual
/// claude PID whose deployment mode we want to interrogate — terminal-
/// mode panes have claude running directly as the pane's child of
/// child; IDE panel mode never goes through tmux, so this helper
/// inherently returns `None` for those cases (the dispatcher uses that
/// signal to fall through to claude-event).
///
/// Honors `CLAUDE_WATCH_CONTAINER_MODE` like `find_claude_pid`.
pub fn find_claude_pid_in_tree(root_pid: u32, max_depth: u32) -> Option<u32> {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/home/user".to_string());
    let versions_dir = format!("{}/.local/share/claude/versions", home);
    let container_mode = std::env::var("CLAUDE_WATCH_CONTAINER_MODE")
        .ok()
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    find_claude_pid_in_tree_with_paths(root_pid, max_depth, &versions_dir, container_mode)
}

/// Pure variant: caller chooses the versions-dir prefix + container-mode
/// toggle. Walks the process subtree via `/proc/PID/task/PID/children`
/// (cheap, no fork). Returns the first claude descendant PID, depth-first.
pub fn find_claude_pid_in_tree_with_paths(
    root_pid: u32,
    max_depth: u32,
    versions_dir: &str,
    container_mode: bool,
) -> Option<u32> {
    walk_for_claude(root_pid, 0, max_depth, versions_dir, container_mode)
}

fn walk_for_claude(
    pid: u32,
    depth: u32,
    max_depth: u32,
    versions_dir: &str,
    container_mode: bool,
) -> Option<u32> {
    if depth > max_depth {
        return None;
    }
    if pid_is_claude(pid, versions_dir, container_mode) {
        return Some(pid);
    }
    // Read children via /proc/PID/task/PID/children. This file lists
    // direct children space-separated. Falls back to pgrep -P if the
    // children file is missing (e.g. on a kernel without
    // CONFIG_PROC_CHILDREN, vanishingly rare).
    let children = read_proc_children(pid);
    for child in children {
        if let Some(found) =
            walk_for_claude(child, depth + 1, max_depth, versions_dir, container_mode)
        {
            return Some(found);
        }
    }
    None
}

fn pid_is_claude(pid: u32, versions_dir: &str, container_mode: bool) -> bool {
    let exe_path = format!("/proc/{}/exe", pid);
    if let Ok(target) = std::fs::read_link(&exe_path) {
        let target_str = target.to_string_lossy();
        if target_str.starts_with(versions_dir) {
            return true;
        }
        if container_mode && target_str.starts_with(CONTAINER_CLAUDE_EXE_PREFIX) {
            return true;
        }
    }
    false
}

fn read_proc_children(pid: u32) -> Vec<u32> {
    let path = format!("/proc/{}/task/{}/children", pid, pid);
    match std::fs::read_to_string(&path) {
        Ok(content) => content
            .split_whitespace()
            .filter_map(|s| s.parse::<u32>().ok())
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// Pure variant: caller chooses both the versions-dir prefix and whether
/// to ALSO accept the container npm-global install path. Used directly
/// by tests so they don't need to mutate process env.
pub fn find_claude_pid_with_paths(versions_dir: &str, container_mode: bool) -> Option<u32> {
    let proc_dir = match std::fs::read_dir("/proc") {
        Ok(d) => d,
        Err(_) => return None,
    };

    for entry in proc_dir.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if !name_str.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let exe_path = format!("/proc/{}/exe", name_str);
        if let Ok(target) = std::fs::read_link(&exe_path) {
            let target_str = target.to_string_lossy();
            if target_str.starts_with(versions_dir) {
                return name_str.parse().ok();
            }
            if container_mode && target_str.starts_with(CONTAINER_CLAUDE_EXE_PREFIX) {
                return name_str.parse().ok();
            }
        }
    }
    None
}

/// Get all direct child processes of a PID.
pub fn get_children(ppid: u32) -> Vec<ChildProcess> {
    let output = match std::process::Command::new("ps")
        .args(["--ppid", &ppid.to_string(), "-o", "pid=,cmd="])
        .output()
    {
        Ok(o) => o,
        Err(_) => return Vec::new(),
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_ps_output(&stdout)
}

/// Pure function: parse ps output into child processes.
pub fn parse_ps_output(output: &str) -> Vec<ChildProcess> {
    output
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() {
                return None;
            }
            let parts: Vec<&str> = line.splitn(2, char::is_whitespace).collect();
            let pid: u32 = parts[0].trim().parse().ok()?;
            let cmd = if parts.len() >= 2 {
                parts[1].trim().to_string()
            } else {
                String::new()
            };
            Some(ChildProcess { pid, cmd })
        })
        .collect()
}

/// Find the active Claude Code session directory (most recently modified .output file).
pub fn find_session_dir() -> Option<PathBuf> {
    find_session_dir_in(&claude_tasks_base())
}

/// Testable version with configurable base path.
pub fn find_session_dir_in(base: &str) -> Option<PathBuf> {
    let base_path = Path::new(base);
    let mut best_dir: Option<PathBuf> = None;
    let mut best_mtime: f64 = 0.0;

    let entries = match std::fs::read_dir(base_path) {
        Ok(e) => e,
        Err(_) => return None,
    };

    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        // Session IDs are UUIDs (36 chars with dashes)
        if name_str.len() != 36 || !name_str.contains('-') {
            continue;
        }
        if !entry.path().is_dir() {
            continue;
        }

        let tasks_path = entry.path().join("tasks");
        if !tasks_path.is_dir() {
            continue;
        }

        if let Ok(task_entries) = std::fs::read_dir(&tasks_path) {
            for f in task_entries.flatten() {
                if f.file_name().to_string_lossy().ends_with(".output") {
                    if let Ok(meta) = f.metadata() {
                        if let Ok(mtime) = meta.modified() {
                            let mt = mtime
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_secs_f64();
                            if mt > best_mtime {
                                best_mtime = mt;
                                best_dir = Some(entry.path());
                            }
                        }
                    }
                }
            }
        }
    }

    best_dir
}

/// Find the subagents directory for a session.
pub fn find_subagents_dir(session_dir: &Path) -> Option<PathBuf> {
    let session_id = session_dir.file_name()?.to_string_lossy().to_string();
    let home = std::env::var("HOME").unwrap_or_else(|_| "/home/user".to_string());
    let projects_base = PathBuf::from(home).join(".claude").join("projects");

    let entries = std::fs::read_dir(&projects_base).ok()?;
    for entry in entries.flatten() {
        let subagents = entry.path().join(&session_id).join("subagents");
        if subagents.is_dir() {
            return Some(subagents);
        }
    }
    None
}

/// Load agent metadata from JSONL files in the subagents directory.
pub fn load_agents(subagents_dir: &Path) -> HashMap<String, AgentInfo> {
    let mut agents = HashMap::new();

    let entries = match std::fs::read_dir(subagents_dir) {
        Ok(e) => e,
        Err(_) => return agents,
    };

    for entry in entries.flatten() {
        let fname = entry.file_name().to_string_lossy().to_string();
        if !fname.ends_with(".meta.json") {
            continue;
        }

        let agent_id = fname
            .strip_prefix("agent-")
            .unwrap_or(&fname)
            .strip_suffix(".meta.json")
            .unwrap_or(&fname)
            .to_string();

        let meta_path = entry.path();
        let jsonl_path = subagents_dir.join(format!("agent-{}.jsonl", agent_id));

        // Read meta
        let meta: AgentMeta = std::fs::read_to_string(&meta_path)
            .ok()
            .and_then(|content| serde_json::from_str(&content).ok())
            .unwrap_or(AgentMeta {
                description: "unknown".to_string(),
                agent_type: "unknown".to_string(),
            });

        // Extract last bash command from JSONL
        let last_bash_cmd = extract_last_bash_cmd(&jsonl_path);

        let jsonl_mtime = std::fs::metadata(&jsonl_path)
            .ok()
            .and_then(|m| m.modified().ok())
            .map(|t| {
                t.duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs_f64()
            })
            .unwrap_or(0.0);

        agents.insert(
            agent_id,
            AgentInfo {
                description: meta.description,
                agent_type: meta.agent_type,
                last_bash_cmd,
                jsonl_path,
                jsonl_mtime,
            },
        );
    }

    agents
}

/// Extract the most recent Bash command from a JSONL file.
fn extract_last_bash_cmd(jsonl_path: &Path) -> Option<String> {
    let content = std::fs::read_to_string(jsonl_path).ok()?;
    extract_last_bash_cmd_from_str(&content)
}

/// Pure function: extract the most recent Bash command from JSONL content.
pub fn extract_last_bash_cmd_from_str(content: &str) -> Option<String> {
    let mut last_cmd: Option<String> = None;

    for line in content.lines() {
        let entry: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let content_arr = entry
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_array());

        if let Some(blocks) = content_arr {
            for block in blocks {
                if block.get("name").and_then(|n| n.as_str()) == Some("Bash") {
                    if let Some(cmd) = block
                        .get("input")
                        .and_then(|i| i.get("command"))
                        .and_then(|c| c.as_str())
                    {
                        if !cmd.is_empty() {
                            last_cmd = Some(cmd.to_string());
                        }
                    }
                }
            }
        }
    }

    last_cmd
}

/// Match agent IDs to child PIDs by command matching.
pub fn match_agent_to_pid(
    agents: &HashMap<String, AgentInfo>,
    children: &[ChildProcess],
) -> (HashMap<String, Vec<u32>>, Vec<ChildProcess>) {
    let mut matches: HashMap<String, Vec<u32>> = HashMap::new();
    let mut unmatched: Vec<ChildProcess> = Vec::new();

    for child in children {
        let mut matched = false;
        for (agent_id, agent) in agents {
            if let Some(ref bash_cmd) = agent.last_bash_cmd {
                if child.cmd.contains(bash_cmd.as_str()) {
                    matches.entry(agent_id.clone()).or_default().push(child.pid);
                    matched = true;
                    break;
                }
            }
        }
        if !matched {
            unmatched.push(child.clone());
        }
    }

    (matches, unmatched)
}

/// Get all descendant PIDs of a process recursively.
pub fn get_descendant_pids(pid: u32) -> Vec<u32> {
    let mut descendants = Vec::new();

    let output = match std::process::Command::new("ps")
        .args(["--ppid", &pid.to_string(), "-o", "pid="])
        .output()
    {
        Ok(o) => o,
        Err(_) => return descendants,
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Ok(child_pid) = line.parse::<u32>() {
            descendants.push(child_pid);
            descendants.extend(get_descendant_pids(child_pid));
        }
    }

    descendants
}

/// Kill a process and all its descendants. Returns list of PIDs targeted.
pub fn kill_process_tree(pid: u32, sig: nix::sys::signal::Signal) -> Vec<u32> {
    let descendants = get_descendant_pids(pid);

    // Kill children first (bottom up)
    for &dpid in descendants.iter().rev() {
        let _ = nix::sys::signal::kill(nix::unistd::Pid::from_raw(dpid as i32), sig);
    }
    // Then the parent
    let _ = nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), sig);

    let mut all = vec![pid];
    all.extend(descendants);
    all
}

/// Check if a process is still alive.
pub fn is_process_alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{}", pid)).exists()
}

/// Find orphaned agent-like processes (reparented to PID 1, matching "sidechain").
pub fn find_orphaned_agents(claude_pid: Option<u32>) -> Vec<ChildProcess> {
    let output = match std::process::Command::new("pgrep")
        .args(["-af", "sidechain"])
        .output()
    {
        Ok(o) => o,
        Err(_) => return Vec::new(),
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut orphans = Vec::new();

    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let parts: Vec<&str> = line.splitn(2, char::is_whitespace).collect();
        if parts.len() < 2 {
            continue;
        }
        let pid: u32 = match parts[0].parse() {
            Ok(p) => p,
            Err(_) => continue,
        };
        let cmd = parts[1].to_string();

        // Skip pgrep itself
        if cmd.contains("pgrep") {
            continue;
        }

        // Skip if it's a direct child of claude (handled by main kill-all)
        if let Some(cpid) = claude_pid {
            if let Ok(ppid_out) = std::process::Command::new("ps")
                .args(["-o", "ppid=", "-p", &pid.to_string()])
                .output()
            {
                let ppid_str = String::from_utf8_lossy(&ppid_out.stdout);
                if let Ok(ppid) = ppid_str.trim().parse::<u32>() {
                    if ppid == cpid {
                        continue;
                    }
                }
            }
        }

        orphans.push(ChildProcess { pid, cmd });
    }

    orphans
}

/// Format the `list` command output. Returns the formatted string.
pub fn format_list(
    claude_pid: u32,
    agents: &HashMap<String, AgentInfo>,
    matches: &HashMap<String, Vec<u32>>,
    unmatched: &[ChildProcess],
    watcher_children: &[ChildProcess],
    show_all: bool,
) -> String {
    let mut out = format!("Claude Code PID: {}\n", claude_pid);

    if !agents.is_empty() {
        out.push_str(&format!("\n=== Agents ({}) ===", agents.len()));

        let mut sorted_agents: Vec<_> = agents.iter().collect();
        sorted_agents.sort_by_key(|(id, _)| (*id).clone());

        for (agent_id, info) in sorted_agents {
            let pids = matches
                .get(agent_id.as_str())
                .or_else(|| matches.get(agent_id));
            let age = if info.jsonl_mtime > 0.0 {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs_f64();
                now - info.jsonl_mtime
            } else {
                0.0
            };

            let (status, pid_str) = match pids {
                Some(p) if !p.is_empty() => (
                    "RUNNING".to_string(),
                    p.iter()
                        .map(|p| p.to_string())
                        .collect::<Vec<_>>()
                        .join(", "),
                ),
                _ => ("no child process".to_string(), "-".to_string()),
            };

            out.push_str(&format!("\n\n  {}", agent_id));
            out.push_str(&format!("\n    Description: {}", info.description));
            out.push_str(&format!("\n    Type: {}", info.agent_type));
            out.push_str(&format!("\n    Status: {}", status));
            out.push_str(&format!("\n    PIDs: {}", pid_str));
            out.push_str(&format!("\n    Last JSONL write: {:.0}s ago", age));
            if let Some(ref cmd) = info.last_bash_cmd {
                let preview: String = cmd.chars().take(100).collect();
                out.push_str(&format!("\n    Last command: {}", preview));
            }
        }

        if !unmatched.is_empty() {
            out.push_str(&format!(
                "\n\n=== Unmatched child processes ({}) ===",
                unmatched.len()
            ));
            for child in unmatched {
                let eval_cmd = extract_eval_command(&child.cmd);
                let preview: String = eval_cmd.chars().take(120).collect();
                out.push_str(&format!("\n  PID {}: {}", child.pid, preview));
            }
        }
    } else {
        out.push_str("\nNo agent metadata found.");
        if !unmatched.is_empty() {
            out.push_str(&format!(
                "\n\n=== Non-watcher child processes ({}) ===",
                unmatched.len()
            ));
            for child in unmatched {
                let eval_cmd = extract_eval_command(&child.cmd);
                let preview: String = eval_cmd.chars().take(120).collect();
                out.push_str(&format!("\n  PID {}: {}", child.pid, preview));
            }
        }
    }

    if !show_all {
        out.push_str(&format!(
            "\n\n({} watcher processes hidden, use --all to show)",
            watcher_children.len()
        ));
    } else {
        out.push_str(&format!(
            "\n\n=== Watchers ({}) ===",
            watcher_children.len()
        ));
        for child in watcher_children {
            let eval_cmd = extract_eval_command(&child.cmd);
            let preview: String = eval_cmd.chars().take(100).collect();
            out.push_str(&format!("\n  PID {}: {}", child.pid, preview));
        }
    }

    out
}

/// Run the `list` command. Returns exit code.
pub fn cmd_list(show_all: bool) -> i32 {
    let claude_pid = match find_claude_pid() {
        Some(pid) => pid,
        None => {
            println!("No Claude Code process found.");
            return 1;
        }
    };

    let children = get_children(claude_pid);
    let agent_children: Vec<ChildProcess> = children
        .iter()
        .filter(|c| !is_watcher(&c.cmd) && !is_own_command(&c.cmd))
        .cloned()
        .collect();
    let watcher_children: Vec<ChildProcess> = children
        .iter()
        .filter(|c| is_watcher(&c.cmd))
        .cloned()
        .collect();

    let session_dir = find_session_dir();
    let subagents_dir = session_dir.as_ref().and_then(|d| find_subagents_dir(d));
    let agents = subagents_dir
        .as_ref()
        .map(|d| load_agents(d))
        .unwrap_or_default();

    if !agents.is_empty() {
        let (matches, unmatched) = match_agent_to_pid(&agents, &agent_children);
        let output = format_list(
            claude_pid,
            &agents,
            &matches,
            &unmatched,
            &watcher_children,
            show_all,
        );
        println!("{}", output);
    } else {
        let output = format_list(
            claude_pid,
            &agents,
            &HashMap::new(),
            &agent_children,
            &watcher_children,
            show_all,
        );
        println!("{}", output);
    }

    0
}

/// Run the `kill` command for a specific target (agent ID or PID). Returns exit code.
pub fn cmd_kill(target: &str, dry_run: bool) -> i32 {
    let claude_pid = match find_claude_pid() {
        Some(pid) => pid,
        None => {
            println!("No Claude Code process found.");
            return 1;
        }
    };

    // Check if target is a PID
    if let Ok(target_pid) = target.parse::<u32>() {
        let children = get_children(claude_pid);
        let child_pids: std::collections::HashSet<u32> = children.iter().map(|c| c.pid).collect();
        if !child_pids.contains(&target_pid) {
            println!(
                "PID {} is not a child of Claude Code (PID {}).",
                target_pid, claude_pid
            );
            return 1;
        }
        if dry_run {
            let descendants = get_descendant_pids(target_pid);
            println!(
                "Would kill PID {} + {} descendants",
                target_pid,
                descendants.len()
            );
            return 0;
        }
        let killed = kill_process_tree(target_pid, nix::sys::signal::Signal::SIGTERM);
        println!("Killed process tree: {:?}", killed);
        return 0;
    }

    // Target is an agent ID (or prefix)
    let session_dir = find_session_dir();
    let subagents_dir = session_dir.as_ref().and_then(|d| find_subagents_dir(d));
    let agents = subagents_dir
        .as_ref()
        .map(|d| load_agents(d))
        .unwrap_or_default();

    let matching: Vec<&String> = agents
        .keys()
        .filter(|aid| *aid == target || aid.starts_with(target))
        .collect();

    if matching.is_empty() {
        println!("No agent matching '{}' found.", target);
        if !agents.is_empty() {
            let keys: Vec<&String> = agents.keys().collect();
            println!(
                "Known agents: {}",
                keys.iter()
                    .map(|k| k.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        } else {
            println!("Known agents: none");
        }
        return 1;
    }

    if matching.len() > 1 {
        println!("Ambiguous target '{}' matches: {:?}", target, matching);
        return 1;
    }

    let agent_id = matching[0].clone();
    let agent = &agents[&agent_id];
    println!("Agent: {} ({})", agent_id, agent.description);

    let children = get_children(claude_pid);
    let agent_children: Vec<ChildProcess> = children
        .iter()
        .filter(|c| !is_watcher(&c.cmd) && !is_own_command(&c.cmd))
        .cloned()
        .collect();

    let (matches, _) = match_agent_to_pid(&agents, &agent_children);
    let pids = matches.get(&agent_id).cloned().unwrap_or_default();

    if pids.is_empty() {
        println!("No running child processes found for this agent.");
        println!("The agent may be between API calls (no active bash command).");
        return 1;
    }

    let mut total_killed = Vec::new();
    for pid in &pids {
        if dry_run {
            let descendants = get_descendant_pids(*pid);
            println!("Would kill PID {} + {} descendants", pid, descendants.len());
        } else {
            let killed = kill_process_tree(*pid, nix::sys::signal::Signal::SIGTERM);
            println!("Killed process tree: {:?}", killed);
            total_killed.extend(killed);
        }
    }

    if !dry_run && !total_killed.is_empty() {
        std::thread::sleep(std::time::Duration::from_millis(500));
        let still_alive: Vec<u32> = total_killed
            .iter()
            .filter(|&&pid| is_process_alive(pid))
            .copied()
            .collect();

        if !still_alive.is_empty() {
            println!(
                "WARNING: {} processes still alive: {:?}",
                still_alive.len(),
                still_alive
            );
            println!("Sending SIGKILL...");
            for pid in &still_alive {
                let _ = nix::sys::signal::kill(
                    nix::unistd::Pid::from_raw(*pid as i32),
                    nix::sys::signal::Signal::SIGKILL,
                );
            }
        } else {
            println!("All processes confirmed dead.");
        }
    }

    0
}

/// Run the `kill-all` command. Returns exit code.
pub fn cmd_kill_all(dry_run: bool) -> i32 {
    let claude_pid = match find_claude_pid() {
        Some(pid) => pid,
        None => {
            println!("No Claude Code process found.");
            return 1;
        }
    };

    let children = get_children(claude_pid);
    let agent_children: Vec<ChildProcess> = children
        .iter()
        .filter(|c| !is_watcher(&c.cmd) && !is_own_command(&c.cmd))
        .cloned()
        .collect();
    let orphans = find_orphaned_agents(Some(claude_pid));

    if agent_children.is_empty() && orphans.is_empty() {
        println!("No agent processes found.");
        return 0;
    }

    if !agent_children.is_empty() {
        println!("Found {} agent child process(es):", agent_children.len());
        for child in &agent_children {
            let eval_cmd = extract_eval_command(&child.cmd);
            let preview: String = eval_cmd.chars().take(100).collect();
            println!("  PID {}: {}", child.pid, preview);
        }
    }

    if !orphans.is_empty() {
        println!("\nFound {} orphaned agent process(es):", orphans.len());
        for orphan in &orphans {
            let preview: String = orphan.cmd.chars().take(100).collect();
            println!("  PID {}: {}", orphan.pid, preview);
        }
    }

    if dry_run {
        println!("\nDry run — no processes killed.");
        return 0;
    }

    let mut total_killed = Vec::new();
    for child in &agent_children {
        let killed = kill_process_tree(child.pid, nix::sys::signal::Signal::SIGTERM);
        total_killed.extend(killed);
    }
    for orphan in &orphans {
        let killed = kill_process_tree(orphan.pid, nix::sys::signal::Signal::SIGTERM);
        total_killed.extend(killed);
    }

    println!("\nKilled {} process(es).", total_killed.len());

    // Verify
    std::thread::sleep(std::time::Duration::from_millis(500));
    let still_alive: Vec<u32> = total_killed
        .iter()
        .filter(|&&pid| is_process_alive(pid))
        .copied()
        .collect();

    if !still_alive.is_empty() {
        println!(
            "WARNING: {} still alive, sending SIGKILL...",
            still_alive.len()
        );
        for pid in &still_alive {
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(*pid as i32),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
    } else {
        println!("All confirmed dead.");
    }

    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_eval_command_single_quotes() {
        let cmd = "zsh -c eval 'watcher-ctl run alerts-watcher' < /dev/null 2>&1";
        assert_eq!(extract_eval_command(cmd), "watcher-ctl run alerts-watcher");
    }

    #[test]
    fn test_extract_eval_command_double_quotes() {
        let cmd = r#"zsh -c eval "torrent-wait" < /dev/null"#;
        assert_eq!(extract_eval_command(cmd), "torrent-wait");
    }

    #[test]
    fn test_extract_eval_command_unquoted() {
        let cmd = "/bin/zsh -c source /home/user/.claude/shell-snapshots/snapshot-zsh.sh && setopt NO_EXTENDED_GLOB 2>/dev/null || true && eval watchmen \\< /dev/null && pwd -P >| /tmp/claude-5601-cwd";
        assert_eq!(extract_eval_command(cmd), "watchmen");
    }

    #[test]
    fn test_extract_eval_command_no_eval() {
        let cmd = "python3 /some/script.py";
        assert_eq!(extract_eval_command(cmd), cmd);
    }

    #[test]
    fn test_is_watcher_via_watcher_ctl() {
        // `watcher-ctl run <name>` is the canonical supervisor wrapper,
        // so any watcher invoked through it is recognised regardless of
        // the watcher's own name.
        assert!(is_watcher(
            "zsh -c eval 'watcher-ctl run alerts-watcher' < /dev/null"
        ));
        assert!(is_watcher("watcher-ctl run torrent-wait"));
    }

    #[test]
    fn test_is_watcher_watchmen() {
        assert!(is_watcher("zsh -c eval 'watchmen' < /dev/null"));
    }

    #[test]
    fn test_is_watcher_context_watch() {
        assert!(is_watcher("context-watch --foo"));
    }

    #[test]
    fn test_is_watcher_extra_patterns_env_var() {
        // Site-specific watcher names extend the built-in list via env var.
        let prev = std::env::var("CLAUDE_WATCH_EXTRA_WATCHER_PATTERNS").ok();
        // SAFETY: tests run in parallel but env vars are process-global.
        // We restore on the way out; collisions across tests would just
        // produce a flake here, not silently miscompile.
        std::env::set_var(
            "CLAUDE_WATCH_EXTRA_WATCHER_PATTERNS",
            "my-bespoke-watcher:another-watcher",
        );
        assert!(is_watcher("my-bespoke-watcher --flag"));
        assert!(is_watcher("zsh -c eval 'another-watcher' < /dev/null"));
        assert!(!is_watcher("some-other-cmd"));
        match prev {
            Some(v) => std::env::set_var("CLAUDE_WATCH_EXTRA_WATCHER_PATTERNS", v),
            None => std::env::remove_var("CLAUDE_WATCH_EXTRA_WATCHER_PATTERNS"),
        }
    }

    #[test]
    fn test_is_not_watcher() {
        assert!(!is_watcher("python3 /some/agent-script.py"));
        assert!(!is_watcher("cargo test"));
    }

    #[test]
    fn test_is_own_command() {
        assert!(is_own_command("agent-ctl list"));
        assert!(is_own_command("claude-watch agent list"));
        assert!(is_own_command("ps --ppid 1234 -o pid=,cmd="));
        assert!(!is_own_command("python3 /some/script.py"));
    }

    #[test]
    fn test_parse_ps_output_basic() {
        let output = "  1234 some-command --flag\n  5678 another-command\n";
        let children = parse_ps_output(output);
        assert_eq!(children.len(), 2);
        assert_eq!(children[0].pid, 1234);
        assert_eq!(children[0].cmd, "some-command --flag");
        assert_eq!(children[1].pid, 5678);
        assert_eq!(children[1].cmd, "another-command");
    }

    #[test]
    fn test_parse_ps_output_empty() {
        let children = parse_ps_output("");
        assert!(children.is_empty());
    }

    #[test]
    fn test_parse_ps_output_whitespace_only() {
        let children = parse_ps_output("   \n  \n");
        assert!(children.is_empty());
    }

    #[test]
    fn test_container_claude_exe_prefix_constant() {
        // The constant must point at the uid-1000-owned npm-global
        // @anthropic-ai/claude-code dir — that's where the container's
        // `npm install -g @anthropic-ai/claude-code` lands the binary
        // (autoupdate-v2: prefix is /home/hndrewaall/.npm-global under
        // $HOME so `claude update` can write as uid 1000 without
        // privilege escalation), before any in-container self-update
        // populates the named-volume versions/<ver>/ tree. Changing
        // this constant in isolation would silently break
        // find_claude_pid_with_paths(container_mode=true) for fresh
        // container boots. Diagnostic context:
        // claude-watch-container-diagnostic.md confirms
        // /proc/<container-claude-pid>/exe ->
        // <npm-prefix>/lib/node_modules/@anthropic-ai/claude-code/bin/claude.exe.
        assert_eq!(
            CONTAINER_CLAUDE_EXE_PREFIX,
            "/home/hndrewaall/.npm-global/lib/node_modules/@anthropic-ai/claude-code/"
        );
    }

    #[test]
    fn test_find_claude_pid_with_paths_no_proc_panic_safety() {
        // Pure-data sanity: calling find_claude_pid_with_paths with bogus
        // versions_dir + container_mode=false must not panic. The function
        // walks /proc which exists on Linux test runners; the assertion
        // here is just "doesn't crash and returns Option<u32>".
        let _ = find_claude_pid_with_paths("/nonexistent/path/that/does/not/exist", false);
        let _ = find_claude_pid_with_paths("/nonexistent/path/that/does/not/exist", true);
    }

    #[test]
    fn test_extract_last_bash_cmd_from_str() {
        let content = r#"{"message":{"content":[{"name":"Bash","input":{"command":"ls -la"}}]}}
{"message":{"content":[{"name":"Read","input":{"file_path":"/tmp/x"}}]}}
{"message":{"content":[{"name":"Bash","input":{"command":"cargo test"}}]}}
"#;
        assert_eq!(
            extract_last_bash_cmd_from_str(content),
            Some("cargo test".to_string())
        );
    }

    #[test]
    fn test_extract_last_bash_cmd_from_str_no_bash() {
        let content = r#"{"message":{"content":[{"name":"Read","input":{"file_path":"/tmp/x"}}]}}"#;
        assert_eq!(extract_last_bash_cmd_from_str(content), None);
    }

    #[test]
    fn test_extract_last_bash_cmd_from_str_empty() {
        assert_eq!(extract_last_bash_cmd_from_str(""), None);
    }

    #[test]
    fn test_extract_last_bash_cmd_from_str_corrupt() {
        let content = "not json\n{broken\n";
        assert_eq!(extract_last_bash_cmd_from_str(content), None);
    }

    #[test]
    fn test_match_agent_to_pid_basic() {
        let mut agents = HashMap::new();
        agents.insert(
            "abc123".to_string(),
            AgentInfo {
                description: "test agent".to_string(),
                agent_type: "general".to_string(),
                last_bash_cmd: Some("cargo test".to_string()),
                jsonl_path: PathBuf::from("/tmp/test.jsonl"),
                jsonl_mtime: 0.0,
            },
        );

        let children = vec![
            ChildProcess {
                pid: 1234,
                cmd: "zsh -c eval 'cargo test' < /dev/null".to_string(),
            },
            ChildProcess {
                pid: 5678,
                cmd: "python3 /unrelated/script.py".to_string(),
            },
        ];

        let (matches, unmatched) = match_agent_to_pid(&agents, &children);
        assert_eq!(matches.get("abc123").unwrap(), &vec![1234u32]);
        assert_eq!(unmatched.len(), 1);
        assert_eq!(unmatched[0].pid, 5678);
    }

    #[test]
    fn test_match_agent_to_pid_no_match() {
        let mut agents = HashMap::new();
        agents.insert(
            "abc123".to_string(),
            AgentInfo {
                description: "test".to_string(),
                agent_type: "general".to_string(),
                last_bash_cmd: Some("unique-command".to_string()),
                jsonl_path: PathBuf::from("/tmp/test.jsonl"),
                jsonl_mtime: 0.0,
            },
        );

        let children = vec![ChildProcess {
            pid: 1234,
            cmd: "totally-different-cmd".to_string(),
        }];

        let (matches, unmatched) = match_agent_to_pid(&agents, &children);
        assert!(matches.is_empty() || matches.get("abc123").map(|v| v.is_empty()).unwrap_or(true));
        assert_eq!(unmatched.len(), 1);
    }

    #[test]
    fn test_format_list_no_agents() {
        let agents = HashMap::new();
        let matches = HashMap::new();
        let unmatched = vec![ChildProcess {
            pid: 1234,
            cmd: "some-cmd".to_string(),
        }];
        let watchers = vec![];

        let output = format_list(9999, &agents, &matches, &unmatched, &watchers, false);
        assert!(output.contains("Claude Code PID: 9999"));
        assert!(output.contains("No agent metadata found"));
        assert!(output.contains("PID 1234"));
    }

    #[test]
    fn test_format_list_with_agents() {
        let mut agents = HashMap::new();
        agents.insert(
            "abc123".to_string(),
            AgentInfo {
                description: "test agent".to_string(),
                agent_type: "general".to_string(),
                last_bash_cmd: Some("cargo test".to_string()),
                jsonl_path: PathBuf::from("/tmp/test.jsonl"),
                jsonl_mtime: 0.0,
            },
        );
        let mut matches = HashMap::new();
        matches.insert("abc123".to_string(), vec![1234u32]);

        let output = format_list(9999, &agents, &matches, &[], &[], false);
        assert!(output.contains("=== Agents (1) ==="));
        assert!(output.contains("abc123"));
        assert!(output.contains("test agent"));
        assert!(output.contains("RUNNING"));
        assert!(output.contains("1234"));
    }

    #[test]
    fn test_watcher_patterns_comprehensive() {
        for pattern in WATCHER_PATTERNS {
            assert!(
                is_watcher(&format!("zsh -c eval '{}' < /dev/null", pattern)),
                "Pattern '{}' should be detected as watcher",
                pattern
            );
        }
    }
}
