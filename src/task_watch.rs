//! Task-watch daemon loop — auto-manages tmux panes for Claude Code background tasks.
//!
//! Ports the Python `task-watch` daemon into the claude-watch Rust binary.
//! Monitors /tmp/claude-1000/ for task output files, creates tmux panes with
//! `tail -f` for active tasks, and removes them after tasks complete.

use crate::cmd::run_cmd;
use crate::config::TaskWatchConfig;
use crate::proc_util;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use notify::{
    Config as NotifyConfig, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher,
};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

/// Base directory for Claude Code task output files.
const TASKS_BASE: &str = "/tmp/claude-1000/-home-hndrewaall";

/// Mtime threshold for considering an agent "active" (seconds).
const AGENT_MTIME_THRESHOLD: u64 = 600;

/// A tracked task pane in the tmux session.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct TrackedTask {
    pub pane_id: String,
    pub label: String,
    pub added_at: Instant,
    pub is_agent: bool,
}

/// Main task-watch state.
pub struct TaskWatchState {
    pub tracked: HashMap<String, TrackedTask>,
    pub pending_removal: HashMap<String, Instant>,
    pub tasks_dir: PathBuf,
    pub session: String,
    pub max_panes: usize,
}

/// Find the active Claude Code tasks directory.
///
/// Claude Code uses UUID-scoped directories:
///   /tmp/claude-1000/-home-hndrewaall/<uuid>/tasks/
/// Pick the one with the most recently modified .output file.
pub fn find_tasks_dir() -> Option<PathBuf> {
    find_tasks_dir_in(TASKS_BASE)
}

/// Testable version with configurable base path.
pub fn find_tasks_dir_in(base: &str) -> Option<PathBuf> {
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
                                .duration_since(UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_secs_f64();
                            if mt > best_mtime {
                                best_mtime = mt;
                                best_dir = Some(tasks_path.clone());
                            }
                        }
                    }
                }
            }
        }
    }

    best_dir
}

/// Check if a task output file is an agent JSONL file (symlinked).
pub fn is_agent_output(tasks_dir: &Path, task_id: &str) -> bool {
    let output_file = tasks_dir.join(format!("{}.output", task_id));
    if let Ok(meta) = std::fs::symlink_metadata(&output_file) {
        if meta.file_type().is_symlink() {
            if let Ok(target) = std::fs::read_link(&output_file) {
                return target.to_string_lossy().ends_with(".jsonl");
            }
        }
    }
    false
}

/// Check if a task's output file has any content.
pub fn has_output(tasks_dir: &Path, task_id: &str) -> bool {
    let output_file = tasks_dir.join(format!("{}.output", task_id));
    match std::fs::metadata(&output_file) {
        Ok(meta) => meta.len() > 0,
        Err(_) => false,
    }
}

/// Infer a human-readable label from the first line of a task output file.
pub fn infer_label(tasks_dir: &Path, task_id: &str) -> String {
    let output_file = tasks_dir.join(format!("{}.output", task_id));
    match std::fs::read_to_string(&output_file) {
        Ok(content) => infer_label_from_content(task_id, &content),
        Err(_) => task_id.chars().take(12).collect(),
    }
}

/// Pure function: extract label from file content.
pub fn infer_label_from_content(task_id: &str, content: &str) -> String {
    let first_line = match content.lines().next() {
        Some(line) => line.trim(),
        None => return task_id.chars().take(12).collect(),
    };

    if first_line.is_empty() {
        return task_id.chars().take(12).collect();
    }

    // Agent JSONL: try to extract slug or agentId
    if first_line.starts_with('{') {
        if let Ok(obj) = serde_json::from_str::<serde_json::Value>(first_line) {
            if let Some(slug) = obj.get("slug").and_then(|s| s.as_str()) {
                if !slug.is_empty() {
                    return format!("agent:{}", slug);
                }
            }
            if let Some(agent_id) = obj.get("agentId").and_then(|s| s.as_str()) {
                if !agent_id.is_empty() {
                    let short: String = agent_id.chars().take(12).collect();
                    return format!("agent:{}", short);
                }
            }
        }
    }

    // Trim long lines
    if first_line.len() > 40 {
        let truncated: String = first_line.chars().take(37).collect();
        format!("{}...", truncated)
    } else {
        first_line.to_string()
    }
}

/// Scan /proc for processes with writable fds pointing to .output files in tasks_dir.
///
/// Returns a map of task_id -> is_service for active writers.
/// This is the core /proc scanning logic ported from the Python `get_open_output_files`.
pub fn scan_active_writers(tasks_dir: &Path, include_watchers: bool) -> HashMap<String, bool> {
    let prefix = match tasks_dir.canonicalize() {
        Ok(p) => format!("{}/", p.display()),
        Err(_) => format!("{}/", tasks_dir.display()),
    };

    let mut active: HashMap<String, bool> = HashMap::new();

    // Build reverse map: realpath -> task_id for symlinked output files (agents)
    let mut realpath_map: HashMap<String, String> = HashMap::new();
    if let Ok(entries) = std::fs::read_dir(tasks_dir) {
        for entry in entries.flatten() {
            let fname = entry.file_name().to_string_lossy().to_string();
            if !fname.ends_with(".output") {
                continue;
            }
            let fpath = entry.path();
            if let Ok(meta) = std::fs::symlink_metadata(&fpath) {
                if meta.file_type().is_symlink() {
                    if let Ok(real) = std::fs::canonicalize(&fpath) {
                        let tid = fname.strip_suffix(".output").unwrap_or(&fname).to_string();
                        realpath_map.insert(real.to_string_lossy().to_string(), tid);
                    }
                }
            }
        }
    }

    // Scan /proc for writers
    let proc_dir = match std::fs::read_dir("/proc") {
        Ok(d) => d,
        Err(_) => return active,
    };

    for proc_entry in proc_dir.flatten() {
        let pid_name = proc_entry.file_name();
        let pid_str = pid_name.to_string_lossy();
        if !pid_str.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }

        let fd_dir = format!("/proc/{}/fd", pid_str);
        let fd_entries = match std::fs::read_dir(&fd_dir) {
            Ok(e) => e,
            Err(_) => continue,
        };

        for fd_entry in fd_entries.flatten() {
            let fd_name = fd_entry.file_name();
            let fd_str = fd_name.to_string_lossy().to_string();

            let link_path = format!("/proc/{}/fd/{}", pid_str, fd_str);
            let target = match std::fs::read_link(&link_path) {
                Ok(t) => t.to_string_lossy().to_string(),
                Err(_) => continue,
            };

            let mut tid: Option<String> = None;

            // Direct match: fd points to tasks dir .output file
            if target.starts_with(&prefix) && target.ends_with(".output") {
                let rest = &target[prefix.len()..];
                if let Some(id) = rest.strip_suffix(".output") {
                    tid = Some(id.to_string());
                }
            }
            // Symlink match: fd points to real file behind a symlink
            else if let Some(id) = realpath_map.get(&target) {
                tid = Some(id.clone());
            }

            if let Some(task_id) = tid {
                if proc_util::fd_is_writable(&pid_str, &fd_str) && !active.contains_key(&task_id) {
                    let is_service = proc_util::is_service_process(&pid_str)
                        || proc_util::is_service_output(tasks_dir, &task_id);
                    active.insert(task_id, is_service);
                }
            }
        }
    }

    if !include_watchers {
        active.retain(|_, is_svc| !*is_svc);
    }
    active
}

/// Check if an agent's JSONL conversation is complete.
///
/// A completed conversation has a final assistant message with only text
/// content (no tool_use blocks).
pub fn agent_conversation_complete(tasks_dir: &Path, task_id: &str) -> bool {
    let output_file = tasks_dir.join(format!("{}.output", task_id));
    let real_path = match std::fs::canonicalize(&output_file) {
        Ok(p) => p,
        Err(_) => return false,
    };
    let content = match std::fs::read_to_string(&real_path) {
        Ok(c) => c,
        Err(_) => return false,
    };

    agent_conversation_complete_from_str(&content)
}

/// Pure function: check if JSONL content represents a completed agent conversation.
pub fn agent_conversation_complete_from_str(content: &str) -> bool {
    let mut last_role: Option<String> = None;
    let mut last_has_tool_use = false;

    for line in content.lines() {
        let entry: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        if let Some(role) = entry
            .get("message")
            .and_then(|m| m.get("role"))
            .and_then(|r| r.as_str())
        {
            last_role = Some(role.to_string());
            last_has_tool_use = false;
            if let Some(content_arr) = entry
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(|c| c.as_array())
            {
                for block in content_arr {
                    if block.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                        last_has_tool_use = true;
                    }
                }
            }
        }
    }

    last_role.as_deref() == Some("assistant") && !last_has_tool_use
}

/// Check if an agent output file is still active.
///
/// Uses conversation completion, mtime, and child process detection.
pub fn agent_is_active(tasks_dir: &Path, task_id: &str) -> bool {
    // Fast check: if conversation is complete, agent is dead
    if agent_conversation_complete(tasks_dir, task_id) {
        return false;
    }

    let output_file = tasks_dir.join(format!("{}.output", task_id));
    let real_path = match std::fs::canonicalize(&output_file) {
        Ok(p) => p,
        Err(_) => return false,
    };

    if let Ok(meta) = std::fs::metadata(&real_path) {
        if let Ok(mtime) = meta.modified() {
            let elapsed = SystemTime::now()
                .duration_since(mtime)
                .unwrap_or_default()
                .as_secs();
            if elapsed < AGENT_MTIME_THRESHOLD {
                return true;
            }
        }
    }

    // Mtime stale — check for running child processes
    agent_has_child_process(tasks_dir, task_id)
}

/// Check if an agent has a running child process under the Claude PID.
fn agent_has_child_process(tasks_dir: &Path, task_id: &str) -> bool {
    let claude_pid = match crate::agent::find_claude_pid() {
        Some(pid) => pid,
        None => return false,
    };

    // Get the agent's last bash command from JSONL
    let output_file = tasks_dir.join(format!("{}.output", task_id));
    let real_path = match std::fs::canonicalize(&output_file) {
        Ok(p) => p,
        Err(_) => return false,
    };

    let content = match std::fs::read_to_string(&real_path) {
        Ok(c) => c,
        Err(_) => return false,
    };

    let last_bash_cmd = match crate::agent::extract_last_bash_cmd_from_str(&content) {
        Some(cmd) => cmd,
        None => return false,
    };

    // Check children of claude PID
    let children = crate::agent::get_children(claude_pid);
    children.iter().any(|c| c.cmd.contains(&last_bash_cmd))
}

/// Get the set of all alive pane IDs in a tmux session.
async fn get_alive_panes(session: &str) -> std::collections::HashSet<String> {
    let args: Vec<&str> = vec!["tmux", "list-panes", "-t", session, "-F", "#{pane_id}"];
    match run_cmd(&args, 5).await {
        Some(output) => output.lines().map(|l| l.trim().to_string()).collect(),
        None => std::collections::HashSet::new(),
    }
}

/// Info about an existing pane in the tmux session.
#[derive(Debug)]
struct ExistingPane {
    pane_index: usize,
    pane_id: String,
    task_id: Option<String>,
}

/// List all panes in the session with their index, id, and parsed task_id.
///
/// Pane start commands follow the pattern:
///   echo '=== LABEL [TASK_ID] ==='; tail -f PATH
/// We extract TASK_ID from the `[...]` part.
async fn list_existing_panes(session: &str) -> Vec<ExistingPane> {
    let args: Vec<&str> = vec![
        "tmux",
        "list-panes",
        "-t",
        session,
        "-F",
        "#{pane_index}\t#{pane_id}\t#{pane_start_command}",
    ];
    let output = match run_cmd(&args, 5).await {
        Some(o) => o,
        None => return Vec::new(),
    };

    output
        .lines()
        .filter_map(|line| {
            let parts: Vec<&str> = line.splitn(3, '\t').collect();
            if parts.len() < 3 {
                return None;
            }
            let pane_index: usize = parts[0].trim().parse().ok()?;
            let pane_id = parts[1].trim().to_string();
            let start_cmd = parts[2];

            // Extract task_id from [TASK_ID] in the start command
            let task_id = extract_task_id_from_pane_cmd(start_cmd);

            Some(ExistingPane {
                pane_index,
                pane_id,
                task_id,
            })
        })
        .collect()
}

/// Extract a task_id from a pane start command.
///
/// Looks for `[TASK_ID]` pattern in strings like:
///   echo '=== Some Label [abc123def] ==='; tail -f /path/to/file
fn extract_task_id_from_pane_cmd(cmd: &str) -> Option<String> {
    // Find the last occurrence of [...] in the command
    // (to avoid matching other brackets that might appear)
    let mut last_open = None;
    let mut last_close = None;
    for (i, c) in cmd.char_indices() {
        if c == '[' {
            last_open = Some(i);
            last_close = None; // reset close for this open
        }
        if c == ']' && last_open.is_some() {
            last_close = Some(i);
        }
    }

    if let (Some(open), Some(close)) = (last_open, last_close) {
        if close > open + 1 {
            let tid = &cmd[open + 1..close];
            // Validate: task IDs are alphanumeric hex-ish strings
            if !tid.is_empty()
                && tid
                    .chars()
                    .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
            {
                return Some(tid.to_string());
            }
        }
    }
    None
}

/// Check if the tmux session exists.
async fn session_exists(session: &str) -> bool {
    run_cmd(&["tmux", "has-session", "-t", session], 5)
        .await
        .is_some()
}

/// Add a tail -f pane for a task. Returns the new pane ID.
async fn add_pane(state: &mut TaskWatchState, task_id: &str, label: &str) -> Option<String> {
    // Already tracked and alive?
    if let Some(tracked) = state.tracked.get(task_id) {
        let alive = get_alive_panes(&state.session).await;
        if alive.contains(&tracked.pane_id) {
            return Some(tracked.pane_id.clone());
        }
    }

    // Check pane count
    let alive = get_alive_panes(&state.session).await;
    if alive.len() >= state.max_panes {
        warn!(
            "Max panes ({}) reached, skipping {}",
            state.max_panes, task_id
        );
        return None;
    }

    let output_file = state.tasks_dir.join(format!("{}.output", task_id));
    let output_path = output_file.to_string_lossy();
    let display_label = label.replace('\'', "'\\''");
    let is_agent = is_agent_output(&state.tasks_dir, task_id);

    // Build the tail command — use JSONL formatter for agent output
    let tail_cmd = if is_agent {
        format!(
            "tail -f {} | task-watch format-jsonl | task-watch timestamp-lines",
            output_path
        )
    } else {
        format!("tail -f {} | task-watch timestamp-lines", output_path)
    };

    let pane_cmd = format!(
        "echo '=== {} [{}] ==='; {}",
        display_label, task_id, tail_cmd
    );

    // Create pane with tail
    let result = run_cmd(
        &[
            "tmux",
            "split-window",
            "-t",
            &state.session,
            "-v",
            "-P",
            "-F",
            "#{pane_id}",
            &pane_cmd,
        ],
        5,
    )
    .await;

    let pane_id = match result {
        Some(id) if !id.is_empty() => id.trim().to_string(),
        _ => return None,
    };

    // Rebalance
    let _ = run_cmd(
        &[
            "tmux",
            "select-layout",
            "-t",
            &state.session,
            "even-vertical",
        ],
        5,
    )
    .await;

    state.tracked.insert(
        task_id.to_string(),
        TrackedTask {
            pane_id: pane_id.clone(),
            label: label.to_string(),
            added_at: Instant::now(),
            is_agent,
        },
    );

    Some(pane_id)
}

/// Remove a task pane.
async fn remove_pane(state: &mut TaskWatchState, task_id: &str) {
    if let Some(tracked) = state.tracked.remove(task_id) {
        let alive = get_alive_panes(&state.session).await;
        if alive.contains(&tracked.pane_id) {
            let _ = run_cmd(&["tmux", "kill-pane", "-t", &tracked.pane_id], 5).await;
        }
        // Rebalance
        let _ = run_cmd(
            &[
                "tmux",
                "select-layout",
                "-t",
                &state.session,
                "even-vertical",
            ],
            5,
        )
        .await;
    }
}

/// Garbage-collect panes that are dead (tmux pane exited but still tracked).
async fn gc_dead_panes(state: &mut TaskWatchState) -> usize {
    let alive = get_alive_panes(&state.session).await;
    let dead: Vec<String> = state
        .tracked
        .iter()
        .filter(|(_, info)| !alive.contains(&info.pane_id))
        .map(|(tid, _)| tid.clone())
        .collect();

    let count = dead.len();
    for tid in &dead {
        if let Some(info) = state.tracked.remove(tid) {
            info!(task_id = %tid, label = %info.label, "GC: pane dead");
        }
    }

    // Also clean pending_removal for dead panes
    for tid in &dead {
        state.pending_removal.remove(tid);
    }

    count
}

/// Main task-watch daemon loop. Runs as a tokio task.
pub async fn run_task_watch_loop(config: TaskWatchConfig, shutdown: Arc<AtomicBool>) {
    let session = config.session.clone();
    let poll_interval = config.poll_interval;
    let done_delay = config.done_delay;
    let agent_done_delay = config.agent_done_delay;
    let show_all = config.show_all;
    let max_panes = config.max_panes;

    let mode = if show_all {
        "all tasks"
    } else {
        "workloads only"
    };
    info!(
        poll_interval,
        done_delay, agent_done_delay, mode, "task-watch loop started"
    );

    // Find initial tasks directory (use override if provided, e.g. for testing)
    let tasks_dir = if let Some(ref override_dir) = config.tasks_dir_override {
        override_dir.clone()
    } else {
        match find_tasks_dir() {
            Some(d) => d,
            None => {
                warn!("No Claude Code tasks directory found, waiting...");
                // Wait and retry
                loop {
                    if shutdown.load(Ordering::Relaxed) {
                        return;
                    }
                    tokio::time::sleep(std::time::Duration::from_secs(poll_interval)).await;
                    if let Some(d) = find_tasks_dir() {
                        break d;
                    }
                }
            }
        }
    };

    info!(tasks_dir = %tasks_dir.display(), "found tasks directory");

    let mut state = TaskWatchState {
        tracked: HashMap::new(),
        pending_removal: HashMap::new(),
        tasks_dir: tasks_dir.clone(),
        session: session.clone(),
        max_panes,
    };

    // Wait for session to exist
    if !session_exists(&session).await {
        info!(session = %session, "waiting for tmux session...");
        loop {
            if shutdown.load(Ordering::Relaxed) {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            if session_exists(&session).await {
                break;
            }
        }
    }

    // Initial scan: find active writers
    info!("scanning for active tasks...");
    let active_tids = tokio::task::spawn_blocking({
        let td = state.tasks_dir.clone();
        move || scan_active_writers(&td, show_all)
    })
    .await
    .unwrap_or_default();

    // Also discover active agents via mtime
    let mut all_active = active_tids;
    {
        let td = state.tasks_dir.clone();
        let extra_agents = tokio::task::spawn_blocking(move || {
            let mut agents = Vec::new();
            if let Ok(entries) = std::fs::read_dir(&td) {
                for entry in entries.flatten() {
                    let fname = entry.file_name().to_string_lossy().to_string();
                    if fname.ends_with(".output") {
                        let tid = fname.strip_suffix(".output").unwrap_or(&fname).to_string();
                        if is_agent_output(&td, &tid) && agent_is_active(&td, &tid) {
                            agents.push(tid);
                        }
                    }
                }
            }
            agents
        })
        .await
        .unwrap_or_default();

        for tid in extra_agents {
            all_active.entry(tid).or_insert(false);
        }
    }

    for tid in all_active.keys() {
        if has_output(&state.tasks_dir, tid) {
            let label = infer_label(&state.tasks_dir, tid);
            if let Some(pane_id) = add_pane(&mut state, tid, &label).await {
                info!(task_id = %tid, label = %label, pane_id = %pane_id, "initial task");
            }
        }
    }

    let active_count = state.tracked.len();
    info!(active_count, "initial sync complete");

    // Orphan pane cleanup: scan existing panes in the session and kill any
    // that aren't tracked (leftover from a previous daemon instance).
    // Pane 0 is the daemon/status pane — never kill it.
    // Workload panes (registered in /var/run/claude/workload-state/state.json by the
    // `workload` CLI) are NOT in state.tracked but must also be preserved —
    // see the 2026-04-30 promote-layl-s01 incident (pane %1832 killed mid-run).
    let existing_panes = list_existing_panes(&session).await;
    let tracked_pane_ids: std::collections::HashSet<String> =
        state.tracked.values().map(|t| t.pane_id.clone()).collect();
    let workload_pane_ids = load_workload_pane_ids();
    let mut orphan_count = 0;
    for pane in &existing_panes {
        // Skip pane 0 (daemon pane)
        if pane.pane_index == 0 {
            continue;
        }
        // Skip panes we just adopted during initial sync
        if tracked_pane_ids.contains(&pane.pane_id) {
            continue;
        }
        // Skip panes registered as active workloads. The workload registry
        // lives in a separate state file from agent task outputs, so these
        // panes will never appear in state.tracked even when fully alive.
        if workload_pane_ids.contains(&pane.pane_id) {
            info!(
                pane_id = %pane.pane_id,
                pane_index = pane.pane_index,
                "preserving workload pane from cleanup"
            );
            continue;
        }
        // This pane is untracked — it's an orphan from a previous daemon instance.
        // Kill it.
        info!(
            pane_id = %pane.pane_id,
            pane_index = pane.pane_index,
            task_id = ?pane.task_id,
            "killing orphan pane from previous daemon"
        );
        let _ = run_cmd(&["tmux", "kill-pane", "-t", &pane.pane_id], 5).await;
        orphan_count += 1;
    }
    if orphan_count > 0 {
        info!(orphan_count, "orphan pane cleanup complete");
        // Rebalance after killing orphans
        let _ = run_cmd(
            &["tmux", "select-layout", "-t", &session, "even-vertical"],
            5,
        )
        .await;
    }

    // Set up file watcher for new .output files
    let (notify_tx, mut notify_rx) = mpsc::unbounded_channel::<String>();

    let mut _watcher = setup_notify_watcher(&state.tasks_dir, notify_tx.clone());

    // Main daemon loop
    loop {
        if shutdown.load(Ordering::Relaxed) {
            break;
        }

        tokio::time::sleep(std::time::Duration::from_secs(poll_interval)).await;

        if shutdown.load(Ordering::Relaxed) {
            break;
        }

        debug!(
            tracked = state.tracked.len(),
            pending = state.pending_removal.len(),
            "task-watch poll cycle"
        );

        // Detect UUID directory change (new Claude Code session)
        if let Some(new_dir) = find_tasks_dir() {
            if new_dir != state.tasks_dir {
                info!(
                    old = %state.tasks_dir.display(),
                    new = %new_dir.display(),
                    "UUID change detected"
                );
                state.tasks_dir = new_dir.clone();
                _watcher = setup_notify_watcher(&new_dir, notify_tx.clone());
            }
        }

        // Process notify events (new files)
        while let Ok(fname) = notify_rx.try_recv() {
            if !fname.ends_with(".output") {
                continue;
            }
            let tid = fname.strip_suffix(".output").unwrap_or(&fname).to_string();
            if state.tracked.contains_key(&tid) {
                continue;
            }

            // Brief delay for file to get initial content
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;

            if is_agent_output(&state.tasks_dir, &tid) {
                if !has_output(&state.tasks_dir, &tid) {
                    continue;
                }
            } else {
                // Check for writer
                let has_writer = {
                    let td = state.tasks_dir.clone();
                    let tid_c = tid.clone();
                    tokio::task::spawn_blocking(move || {
                        file_has_writer(&td.join(format!("{}.output", tid_c)))
                    })
                    .await
                    .unwrap_or(false)
                };

                if !has_writer {
                    continue;
                }

                if !show_all {
                    let is_svc = {
                        let td = state.tasks_dir.clone();
                        let tid_c = tid.clone();
                        tokio::task::spawn_blocking(move || {
                            // Check if any writer pid is a service
                            check_is_service_for_task(&td, &tid_c)
                        })
                        .await
                        .unwrap_or(false)
                    };
                    if is_svc {
                        continue;
                    }
                }

                if !has_output(&state.tasks_dir, &tid) {
                    continue;
                }
            }

            let label = infer_label(&state.tasks_dir, &tid);
            if let Some(pane_id) = add_pane(&mut state, &tid, &label).await {
                info!(task_id = %tid, label = %label, pane_id = %pane_id, "new task");
                state.pending_removal.remove(&tid);
            }
        }

        // GC dead panes
        let gc_count = gc_dead_panes(&mut state).await;
        if gc_count > 0 {
            debug!(gc_count, "GC cleaned dead panes");
        }

        // Full /proc scan for active writers
        let active_tids = {
            let td = state.tasks_dir.clone();
            tokio::task::spawn_blocking(move || scan_active_writers(&td, show_all))
                .await
                .unwrap_or_default()
        };
        debug!(active_count = active_tids.len(), "proc scan results");

        // Detect completed tasks
        let tracked_ids: Vec<String> = state.tracked.keys().cloned().collect();
        for tid in &tracked_ids {
            if state.pending_removal.contains_key(tid) {
                continue;
            }
            if !active_tids.contains_key(tid) {
                let is_agent = is_agent_output(&state.tasks_dir, tid);
                if is_agent {
                    let td = state.tasks_dir.clone();
                    let tid_c = tid.clone();
                    let still_active =
                        tokio::task::spawn_blocking(move || agent_is_active(&td, &tid_c))
                            .await
                            .unwrap_or(false);
                    if still_active {
                        continue;
                    }
                }

                let label = state
                    .tracked
                    .get(tid)
                    .map(|t| t.label.clone())
                    .unwrap_or_default();
                let effective_delay = if is_agent {
                    agent_done_delay
                } else {
                    done_delay
                };

                info!(
                    task_id = %tid,
                    label = %label,
                    delay = effective_delay,
                    "writer gone, scheduling removal"
                );
                state.pending_removal.insert(
                    tid.clone(),
                    Instant::now() + std::time::Duration::from_secs(effective_delay),
                );
            }
        }

        // Pick up untracked active tasks (catch missed inotify events)
        // Also check for new agents via mtime
        {
            let td = state.tasks_dir.clone();
            let extra_agents = tokio::task::spawn_blocking(move || {
                let mut agents = Vec::new();
                if let Ok(entries) = std::fs::read_dir(&td) {
                    for entry in entries.flatten() {
                        let fname = entry.file_name().to_string_lossy().to_string();
                        if fname.ends_with(".output") {
                            let tid = fname.strip_suffix(".output").unwrap_or(&fname).to_string();
                            if is_agent_output(&td, &tid) && agent_is_active(&td, &tid) {
                                agents.push(tid);
                            }
                        }
                    }
                }
                agents
            })
            .await
            .unwrap_or_default();

            let mut all_active_now = active_tids;
            for tid in extra_agents {
                all_active_now.entry(tid).or_insert(false);
            }

            for tid in all_active_now.keys() {
                if !state.tracked.contains_key(tid)
                    && !state.pending_removal.contains_key(tid)
                    && has_output(&state.tasks_dir, tid)
                {
                    let label = infer_label(&state.tasks_dir, tid);
                    if let Some(pane_id) = add_pane(&mut state, tid, &label).await {
                        info!(
                            task_id = %tid,
                            label = %label,
                            pane_id = %pane_id,
                            "discovered task"
                        );
                    }
                }
            }
        }

        // Process due removals
        let now = Instant::now();
        let due: Vec<String> = state
            .pending_removal
            .iter()
            .filter(|(_, removal_time)| now >= **removal_time)
            .map(|(tid, _)| tid.clone())
            .collect();

        for tid in due {
            // Re-check agents before removing
            if is_agent_output(&state.tasks_dir, &tid) {
                let td = state.tasks_dir.clone();
                let tid_c = tid.clone();
                let still_active =
                    tokio::task::spawn_blocking(move || agent_is_active(&td, &tid_c))
                        .await
                        .unwrap_or(false);
                if still_active {
                    state.pending_removal.remove(&tid);
                    info!(task_id = %tid, "agent resumed, cancelling removal");
                    continue;
                }
            }

            state.pending_removal.remove(&tid);
            remove_pane(&mut state, &tid).await;
            info!(task_id = %tid, "removed");
        }

        // Self-heal: if session disappeared, wait for it to come back
        if !session_exists(&session).await {
            warn!("tasks session disappeared, waiting for it to return...");
            // Clear tracked state since panes are gone
            state.tracked.clear();
            state.pending_removal.clear();

            loop {
                if shutdown.load(Ordering::Relaxed) {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                if session_exists(&session).await {
                    info!("tasks session is back, resuming");
                    // Re-scan for active tasks (both /proc writers and agents)
                    let td = state.tasks_dir.clone();
                    let mut active: HashMap<String, bool> =
                        tokio::task::spawn_blocking(move || scan_active_writers(&td, show_all))
                            .await
                            .unwrap_or_default();

                    // Also discover active agents via mtime
                    {
                        let td = state.tasks_dir.clone();
                        let extra_agents = tokio::task::spawn_blocking(move || {
                            let mut agents = Vec::new();
                            if let Ok(entries) = std::fs::read_dir(&td) {
                                for entry in entries.flatten() {
                                    let fname = entry.file_name().to_string_lossy().to_string();
                                    if fname.ends_with(".output") {
                                        let tid = fname
                                            .strip_suffix(".output")
                                            .unwrap_or(&fname)
                                            .to_string();
                                        if is_agent_output(&td, &tid) && agent_is_active(&td, &tid)
                                        {
                                            agents.push(tid);
                                        }
                                    }
                                }
                            }
                            agents
                        })
                        .await
                        .unwrap_or_default();

                        for tid in extra_agents {
                            active.entry(tid).or_insert(false);
                        }
                    }

                    for tid in active.keys() {
                        if has_output(&state.tasks_dir, tid) {
                            let label = infer_label(&state.tasks_dir, tid);
                            if let Some(pane_id) = add_pane(&mut state, tid, &label).await {
                                info!(task_id = %tid, label = %label, pane_id = %pane_id, "reconnected task");
                            }
                        }
                    }
                    break;
                }
            }
        }
    }

    info!("task-watch loop shutting down");
}

/// Check if a specific task's writer process is a service.
fn check_is_service_for_task(tasks_dir: &Path, task_id: &str) -> bool {
    let output_file = tasks_dir.join(format!("{}.output", task_id));
    let real_path = match std::fs::canonicalize(&output_file) {
        Ok(p) => p.to_string_lossy().to_string(),
        Err(_) => output_file.to_string_lossy().to_string(),
    };

    // Find writer PID
    let proc_dir = match std::fs::read_dir("/proc") {
        Ok(d) => d,
        Err(_) => return false,
    };

    for proc_entry in proc_dir.flatten() {
        let pid_name = proc_entry.file_name();
        let pid_str = pid_name.to_string_lossy().to_string();
        if !pid_str.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }

        let fd_dir = format!("/proc/{}/fd", pid_str);
        let fd_entries = match std::fs::read_dir(&fd_dir) {
            Ok(e) => e,
            Err(_) => continue,
        };

        for fd_entry in fd_entries.flatten() {
            let fd_name = fd_entry.file_name();
            let fd_str = fd_name.to_string_lossy().to_string();

            let link_path = format!("/proc/{}/fd/{}", pid_str, fd_str);
            let target = match std::fs::read_link(&link_path) {
                Ok(t) => t.to_string_lossy().to_string(),
                Err(_) => continue,
            };

            if target == real_path && proc_util::fd_is_writable(&pid_str, &fd_str) {
                return proc_util::is_service_process(&pid_str)
                    || proc_util::is_service_output(tasks_dir, task_id);
            }
        }
    }
    false
}

/// Check if a file has an active writer process.
fn file_has_writer(filepath: &Path) -> bool {
    let real_path = match std::fs::canonicalize(filepath) {
        Ok(p) => p.to_string_lossy().to_string(),
        Err(_) => return false,
    };

    let proc_dir = match std::fs::read_dir("/proc") {
        Ok(d) => d,
        Err(_) => return false,
    };

    for proc_entry in proc_dir.flatten() {
        let pid_name = proc_entry.file_name();
        let pid_str = pid_name.to_string_lossy().to_string();
        if !pid_str.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }

        let fd_dir = format!("/proc/{}/fd", pid_str);
        let fd_entries = match std::fs::read_dir(&fd_dir) {
            Ok(e) => e,
            Err(_) => continue,
        };

        for fd_entry in fd_entries.flatten() {
            let fd_name = fd_entry.file_name();
            let fd_str = fd_name.to_string_lossy().to_string();

            let link_path = format!("/proc/{}/fd/{}", pid_str, fd_str);
            let target = match std::fs::read_link(&link_path) {
                Ok(t) => t.to_string_lossy().to_string(),
                Err(_) => continue,
            };

            if target == real_path && proc_util::fd_is_writable(&pid_str, &fd_str) {
                return true;
            }
        }
    }
    false
}

/// Set up a notify file watcher on the tasks directory.
/// Returns the watcher (must be kept alive) or None on failure.
fn setup_notify_watcher(
    tasks_dir: &Path,
    tx: mpsc::UnboundedSender<String>,
) -> Option<RecommendedWatcher> {
    let mut watcher = match RecommendedWatcher::new(
        move |result: Result<Event, notify::Error>| {
            if let Ok(event) = result {
                if matches!(event.kind, EventKind::Create(_)) {
                    for path in &event.paths {
                        if let Some(fname) = path.file_name() {
                            let _ = tx.send(fname.to_string_lossy().to_string());
                        }
                    }
                }
            }
        },
        NotifyConfig::default(),
    ) {
        Ok(w) => w,
        Err(e) => {
            warn!(error = %e, "failed to create file watcher");
            return None;
        }
    };

    if let Err(e) = watcher.watch(tasks_dir, RecursiveMode::NonRecursive) {
        warn!(
            error = %e,
            path = %tasks_dir.display(),
            "failed to watch tasks directory"
        );
        return None;
    }

    info!(path = %tasks_dir.display(), "file watcher active");
    Some(watcher)
}

/// Path to the workload registry state file.
///
/// The `workload` CLI writes a JSON registry keyed by workload label, with
/// each entry containing a `pane_id`. Tests can override the path via the
/// `CLAUDE_WATCH_WORKLOAD_STATE` env var (default:
/// `/var/run/claude/workload-state/state.json`). The default must stay
/// in sync with `workload::WORKLOAD_DIR` — the two are read-only mirrors
/// of the same on-disk file.
fn workload_state_path() -> PathBuf {
    if let Ok(p) = std::env::var("CLAUDE_WATCH_WORKLOAD_STATE") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    PathBuf::from("/var/run/claude/workload-state/state.json")
}

/// Load the set of pane IDs registered as active workloads.
///
/// Default-open semantics: a missing or malformed state file MUST NOT cause
/// the daemon to crash or skip cleanup — it just yields an empty set, so the
/// orphan-pane sweep proceeds as if no workloads exist. This matches the
/// general "broken hook never blackholes the loop" rule for claude-watch.
fn load_workload_pane_ids() -> std::collections::HashSet<String> {
    let path = workload_state_path();
    let content = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) => {
            // Missing file is the common case (no workloads registered yet).
            // Anything else is worth a debug breadcrumb but still default-open.
            if e.kind() != std::io::ErrorKind::NotFound {
                debug!(path = %path.display(), error = %e, "workload state read failed");
            }
            return std::collections::HashSet::new();
        }
    };
    let state: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(e) => {
            warn!(path = %path.display(), error = %e, "workload state malformed; treating as empty");
            return std::collections::HashSet::new();
        }
    };
    let obj = match state.as_object() {
        Some(o) => o,
        None => {
            warn!(path = %path.display(), "workload state is not a JSON object; treating as empty");
            return std::collections::HashSet::new();
        }
    };
    obj.values()
        .filter_map(|info| {
            info.get("pane_id")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
        })
        .collect()
}

/// Get the list of running workloads by reading workload state and intersecting
/// with the currently alive panes in the tasks session.
async fn get_running_workloads(session: &str) -> Vec<String> {
    let path = workload_state_path();
    let content = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let state: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let obj = match state.as_object() {
        Some(o) => o,
        None => return Vec::new(),
    };
    let alive = get_alive_panes(session).await;
    let mut running = Vec::new();
    for (label, info) in obj {
        let pane_id = info
            .get("pane_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if !pane_id.is_empty() && alive.contains(&pane_id) {
            running.push(label.clone());
        }
    }
    running
}

/// Check if claude-watch's task_watch loop is handling the daemon.
fn claude_watch_handles_daemon() -> bool {
    let config = crate::config::load_config();
    config.task_watch.enabled
}

/// CLI handler: create/reinit the tasks tmux session.
///
/// Mirrors the Python `task-watch init` behavior:
/// - If session exists and `--recreate` is not set, respawn daemon pane
///   (preserving workload panes).
/// - If session exists and `--recreate` is set, require `--force` when there
///   are running workloads.
/// - When claude-watch handles the daemon loop, pane 0 gets a stub command
///   (`echo ...; sleep infinity`) instead of `task-watch daemon`.
pub async fn cmd_task_init(
    session: &str,
    show_all: bool,
    detach: bool,
    recreate: bool,
    force: bool,
) {
    let all_flag = if show_all { " --all" } else { "" };

    // Clean orphaned grouped sessions (tasks-N from previous inits)
    if let Some(out) = run_cmd(&["tmux", "list-sessions", "-F", "#{session_name}"], 5).await {
        let prefix = format!("{}-", session);
        for line in out.lines() {
            if line.starts_with(&prefix) {
                let _ = run_cmd(&["tmux", "kill-session", "-t", line], 5).await;
            }
        }
    }

    let cw_handling = claude_watch_handles_daemon();
    let stub_cmd = "echo 'task-watch daemon handled by claude-watch'; sleep infinity".to_string();
    let daemon_cmd = if cw_handling {
        stub_cmd.clone()
    } else {
        format!("task-watch daemon{}", all_flag)
    };

    if session_exists(session).await {
        if recreate {
            // --recreate: destroy and rebuild; --force required if workloads are running
            let running = get_running_workloads(session).await;
            if !running.is_empty() && !force {
                eprintln!(
                    "ERROR: Session '{}' has {} running workload(s): {}.",
                    session,
                    running.len(),
                    running.join(", ")
                );
                eprintln!("Use --recreate --force to destroy and recreate.");
                std::process::exit(1);
            }
            if !running.is_empty() {
                println!(
                    "WARNING: Killing {} running workload(s): {}",
                    running.len(),
                    running.join(", ")
                );
            }
            let _ = run_cmd(&["tmux", "kill-session", "-t", session], 5).await;
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            let _ = run_cmd(
                &[
                    "tmux",
                    "new-session",
                    "-d",
                    "-s",
                    session,
                    "-n",
                    "watch",
                    &daemon_cmd,
                ],
                5,
            )
            .await;
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            println!("Session '{}' recreated (all panes destroyed)", session);
        } else {
            // Safe: respawn daemon pane (pane 0) without killing workload panes
            let _ = run_cmd(
                &[
                    "tmux",
                    "respawn-pane",
                    "-k",
                    "-t",
                    &format!("{}:0.0", session),
                    &daemon_cmd,
                ],
                5,
            )
            .await;
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            if cw_handling {
                println!(
                    "Session '{}' reinited (daemon handled by claude-watch, workload panes preserved)",
                    session
                );
            } else {
                println!(
                    "Session '{}' reinited (daemon restarted, workload panes preserved)",
                    session
                );
            }
        }
        // Ensure window-size is not stuck on 'manual' (iTerm2 -CC sets this
        // and it persists after disconnect, locking the session to a fixed width)
        let _ = run_cmd(
            &["tmux", "set-option", "-t", session, "window-size", "latest"],
            5,
        )
        .await;
    } else {
        // Create new session with daemon pane
        let _ = run_cmd(
            &[
                "tmux",
                "new-session",
                "-d",
                "-s",
                session,
                "-n",
                "watch",
                &daemon_cmd,
            ],
            5,
        )
        .await;
        // Prevent iTerm2 -CC from locking session to a fixed width
        let _ = run_cmd(
            &["tmux", "set-option", "-t", session, "window-size", "latest"],
            5,
        )
        .await;
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        if detach {
            println!(
                "Session '{}' created (detached), daemon running in pane 0",
                session
            );
        } else {
            println!("Session '{}' created, daemon running in pane 0", session);
            println!("Attach with: tmux attach -t {}", session);
        }
    }
}

// ---- Standalone (state-file-backed) CLI subcommands ----
//
// These mirror the Python `task-watch` CLI for `add`, `remove`, `gc`,
// `monitor`, and `attach`. Unlike the in-process daemon loop (which keeps
// state in a HashMap), these CLI commands operate on a JSON state file at
// `~/.config/task-watch/state.json` so that multiple short-lived invocations
// can share pane-tracking state.

fn state_file_path() -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    std::path::PathBuf::from(home).join(".config/task-watch/state.json")
}

fn load_state_file() -> serde_json::Map<String, serde_json::Value> {
    let path = state_file_path();
    let content = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(_) => return serde_json::Map::new(),
    };
    match serde_json::from_str::<serde_json::Value>(&content) {
        Ok(serde_json::Value::Object(m)) => m,
        _ => serde_json::Map::new(),
    }
}

fn save_state_file(state: &serde_json::Map<String, serde_json::Value>) {
    let path = state_file_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(s) = serde_json::to_string_pretty(state) {
        let _ = std::fs::write(&path, s);
    }
}

/// CLI handler: `task add <id> [--label LABEL]`.
pub async fn cmd_task_add(session: &str, task_id: &str, label: Option<&str>) -> i32 {
    if !session_exists(session).await {
        eprintln!("No '{}' session. Run: claude-watch task init", session);
        return 1;
    }
    let tasks_dir = match find_tasks_dir() {
        Some(d) => d,
        None => {
            eprintln!("No Claude Code tasks directory found");
            return 1;
        }
    };
    let output_file = tasks_dir.join(format!("{}.output", task_id));
    if !output_file.exists() {
        eprintln!(
            "No output file for task {}: {}",
            task_id,
            output_file.display()
        );
        return 1;
    }

    let label_str = label.unwrap_or("").to_string();
    let mut state = load_state_file();

    // Already tracked and alive?
    if let Some(existing) = state.get(task_id) {
        if let Some(pane_id) = existing.get("pane_id").and_then(|v| v.as_str()) {
            let alive = get_alive_panes(session).await;
            if alive.contains(pane_id) {
                println!("Already tracked: {} [{}]", label_str, task_id);
                return 0;
            }
        }
    }

    // Pane count check
    let alive = get_alive_panes(session).await;
    if alive.len() >= 20 {
        eprintln!("Max panes (20) reached, skipping {}", task_id);
        return 1;
    }

    let display_label_raw = if label_str.is_empty() {
        task_id.chars().take(12).collect::<String>()
    } else {
        label_str.clone()
    };
    let display_label = display_label_raw.replace('\'', "'\\''");
    let is_agent = is_agent_output(&tasks_dir, task_id);
    let output_path = output_file.to_string_lossy().to_string();
    let tail_cmd = if is_agent {
        format!(
            "tail -f {} | task-watch format-jsonl | task-watch timestamp-lines",
            output_path
        )
    } else {
        format!("tail -f {} | task-watch timestamp-lines", output_path)
    };
    let pane_cmd = format!(
        "echo '=== {} [{}] ==='; {}",
        display_label, task_id, tail_cmd
    );

    let pane_id = match run_cmd(
        &[
            "tmux",
            "split-window",
            "-t",
            session,
            "-v",
            "-P",
            "-F",
            "#{pane_id}",
            &pane_cmd,
        ],
        5,
    )
    .await
    {
        Some(id) if !id.trim().is_empty() => id.trim().to_string(),
        _ => {
            eprintln!("Failed to create pane");
            return 1;
        }
    };

    let _ = run_cmd(
        &["tmux", "select-layout", "-t", session, "even-vertical"],
        5,
    )
    .await;

    let now_ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64();
    let entry = serde_json::json!({
        "pane_id": pane_id,
        "label": label_str,
        "created_ts": now_ts,
    });
    state.insert(task_id.to_string(), entry);
    save_state_file(&state);
    println!(
        "Added pane {} for {} [{}]",
        pane_id, display_label_raw, task_id
    );
    0
}

/// CLI handler: `task remove <id> [-n]`.
pub async fn cmd_task_remove(session: &str, task_id: &str, dry_run: bool) -> i32 {
    let mut state = load_state_file();
    let entry = match state.get(task_id).cloned() {
        Some(e) => e,
        None => {
            eprintln!("Task {} not tracked", task_id);
            return 1;
        }
    };
    let pane_id = entry
        .get("pane_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let label = entry
        .get("label")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let display = if label.is_empty() {
        task_id.to_string()
    } else {
        format!("{} [{}]", label, task_id)
    };

    let alive = get_alive_panes(session).await;
    let is_alive = alive.contains(&pane_id);

    if dry_run {
        println!(
            "Would remove: {} (pane {}, {})",
            display,
            pane_id,
            if is_alive { "alive" } else { "dead" }
        );
        return 0;
    }

    if is_alive {
        let _ = run_cmd(&["tmux", "kill-pane", "-t", &pane_id], 5).await;
    }
    state.remove(task_id);
    save_state_file(&state);
    let _ = run_cmd(
        &["tmux", "select-layout", "-t", session, "even-vertical"],
        5,
    )
    .await;
    println!("Removed: {}", display);
    0
}

/// CLI handler: `task gc` — remove tracked entries whose panes are dead.
pub async fn cmd_task_gc(session: &str) -> i32 {
    let mut state = load_state_file();
    if state.is_empty() {
        println!("No tracked panes");
        return 0;
    }
    let alive = get_alive_panes(session).await;
    let dead: Vec<String> = state
        .iter()
        .filter_map(|(tid, info)| {
            let pane_id = info.get("pane_id").and_then(|v| v.as_str()).unwrap_or("");
            if !pane_id.is_empty() && !alive.contains(pane_id) {
                Some(tid.clone())
            } else {
                None
            }
        })
        .collect();
    let count = dead.len();
    for tid in &dead {
        state.remove(tid);
    }
    if count > 0 {
        save_state_file(&state);
        println!("Cleaned up {} dead pane(s)", count);
    } else {
        println!("No dead panes found");
    }
    0
}

/// CLI handler: `task monitor|attach [--cc]`.
///
/// monitor → read-only attach (`tmux attach -r`)
/// attach  → RW multi-client attach (grouped session)
/// --cc     → use tmux -CC mode (iTerm2 control mode)
pub async fn cmd_task_monitor(session: &str, read_only: bool, cc: bool) -> i32 {
    if !session_exists(session).await {
        eprintln!("No '{}' session. Run: claude-watch task init", session);
        return 1;
    }

    // Kill orphaned grouped sessions from previous attaches
    if let Some(out) = run_cmd(&["tmux", "list-sessions", "-F", "#{session_name}"], 5).await {
        let prefix = format!("{}-", session);
        for line in out.lines() {
            if line.starts_with(&prefix) || line == "tasks-viewer" {
                let _ = run_cmd(&["tmux", "kill-session", "-t", line], 5).await;
            }
        }
    }

    use std::ffi::CString;
    let argv_owned: Vec<CString> = if cc {
        // grouped -CC attach
        vec![
            CString::new("tmux").unwrap(),
            CString::new("-CC").unwrap(),
            CString::new("new-session").unwrap(),
            CString::new("-t").unwrap(),
            CString::new(session).unwrap(),
        ]
    } else if read_only {
        vec![
            CString::new("tmux").unwrap(),
            CString::new("attach").unwrap(),
            CString::new("-t").unwrap(),
            CString::new(session).unwrap(),
            CString::new("-r").unwrap(),
        ]
    } else {
        vec![
            CString::new("tmux").unwrap(),
            CString::new("new-session").unwrap(),
            CString::new("-t").unwrap(),
            CString::new(session).unwrap(),
        ]
    };
    let argv_ptrs: Vec<*const libc::c_char> = argv_owned
        .iter()
        .map(|s| s.as_ptr())
        .chain(std::iter::once(std::ptr::null()))
        .collect();
    // Use execvp via libc
    let program = argv_owned[0].as_c_str();
    unsafe {
        libc::execvp(program.as_ptr(), argv_ptrs.as_ptr());
    }
    // If execvp returned, it failed.
    eprintln!("execvp(tmux) failed");
    1
}

/// CLI handler: list tracked tasks.
pub async fn cmd_task_list(_session: &str, json: bool) {
    // Find tasks dir
    let tasks_dir = match find_tasks_dir() {
        Some(d) => d,
        None => {
            if json {
                println!("[]");
            } else {
                println!("No tasks directory found");
            }
            return;
        }
    };

    // List .output files with mtime
    let mut entries = Vec::new();
    if let Ok(dir) = std::fs::read_dir(&tasks_dir) {
        for entry in dir.flatten() {
            let fname = entry.file_name().to_string_lossy().to_string();
            if !fname.ends_with(".output") {
                continue;
            }
            let tid = fname.strip_suffix(".output").unwrap_or(&fname).to_string();
            let label = infer_label(&tasks_dir, &tid);
            let is_agent = is_agent_output(&tasks_dir, &tid);
            let has_content = has_output(&tasks_dir, &tid);

            entries.push(serde_json::json!({
                "task_id": tid,
                "label": label,
                "is_agent": is_agent,
                "has_output": has_content,
            }));
        }
    }

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&entries).unwrap_or_else(|_| "[]".to_string())
        );
    } else if entries.is_empty() {
        println!("No tracked tasks");
    } else {
        for e in &entries {
            let tid = e["task_id"].as_str().unwrap_or("?");
            let label = e["label"].as_str().unwrap_or("?");
            let agent = if e["is_agent"].as_bool().unwrap_or(false) {
                " [agent]"
            } else {
                ""
            };
            println!("  {:24}  {}{}", label, tid, agent);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_infer_label_from_content_empty() {
        assert_eq!(infer_label_from_content("abcdef123456", ""), "abcdef123456");
    }

    #[test]
    fn test_infer_label_from_content_short_text() {
        assert_eq!(
            infer_label_from_content("tid", "Hello world"),
            "Hello world"
        );
    }

    #[test]
    fn test_infer_label_from_content_long_text() {
        let long = "A".repeat(50);
        let result = infer_label_from_content("tid", &long);
        assert!(result.ends_with("..."));
        assert!(result.len() <= 43); // 37 + "..."
    }

    #[test]
    fn test_infer_label_from_content_agent_slug() {
        let json = r#"{"slug":"my-agent","agentId":"abc123"}"#;
        assert_eq!(infer_label_from_content("tid", json), "agent:my-agent");
    }

    #[test]
    fn test_infer_label_from_content_agent_id_only() {
        let json = r#"{"agentId":"abcdef123456789"}"#;
        assert_eq!(infer_label_from_content("tid", json), "agent:abcdef123456");
    }

    #[test]
    fn test_infer_label_from_content_invalid_json() {
        let content = "{not valid json}";
        // Falls through to text handling
        let result = infer_label_from_content("tid", content);
        assert_eq!(result, "{not valid json}");
    }

    #[test]
    fn test_agent_conversation_complete_from_str_complete() {
        let content = r#"{"message":{"role":"user","content":"do something"}}
{"message":{"role":"assistant","content":[{"type":"text","text":"Done!"}]}}"#;
        assert!(agent_conversation_complete_from_str(content));
    }

    #[test]
    fn test_agent_conversation_complete_from_str_still_working() {
        let content = r#"{"message":{"role":"user","content":"do something"}}
{"message":{"role":"assistant","content":[{"type":"tool_use","name":"Bash","input":{"command":"ls"}}]}}"#;
        assert!(!agent_conversation_complete_from_str(content));
    }

    #[test]
    fn test_agent_conversation_complete_from_str_empty() {
        assert!(!agent_conversation_complete_from_str(""));
    }

    #[test]
    fn test_agent_conversation_complete_from_str_user_only() {
        let content = r#"{"message":{"role":"user","content":"do something"}}"#;
        assert!(!agent_conversation_complete_from_str(content));
    }

    #[test]
    fn test_find_tasks_dir_in_nonexistent() {
        assert!(find_tasks_dir_in("/tmp/nonexistent-claude-watch-test-dir").is_none());
    }

    #[test]
    fn test_is_agent_output_no_file() {
        let dir = Path::new("/tmp/nonexistent-task-watch-test");
        assert!(!is_agent_output(dir, "fake-id"));
    }

    #[test]
    fn test_has_output_no_file() {
        let dir = Path::new("/tmp/nonexistent-task-watch-test");
        assert!(!has_output(dir, "fake-id"));
    }

    #[test]
    fn test_extract_task_id_from_pane_cmd_standard() {
        let cmd = "echo '=== Some Label [abc123def] ==='; tail -f /tmp/tasks/abc123def.output";
        assert_eq!(
            extract_task_id_from_pane_cmd(cmd),
            Some("abc123def".to_string())
        );
    }

    #[test]
    fn test_extract_task_id_from_pane_cmd_agent() {
        let cmd = "echo '=== agent:tracker-search [a644bc543a7a9e8a6] ==='; tail -f /tmp/tasks/a644bc543a7a9e8a6.output | task-watch format-jsonl | task-watch timestamp-lines";
        assert_eq!(
            extract_task_id_from_pane_cmd(cmd),
            Some("a644bc543a7a9e8a6".to_string())
        );
    }

    #[test]
    fn test_extract_task_id_from_pane_cmd_no_brackets() {
        let cmd = "echo 'task-watch daemon handled by claude-watch'; sleep infinity";
        assert_eq!(extract_task_id_from_pane_cmd(cmd), None);
    }

    #[test]
    fn test_extract_task_id_from_pane_cmd_empty_brackets() {
        let cmd = "echo '=== [] ==='; tail -f /dev/null";
        assert_eq!(extract_task_id_from_pane_cmd(cmd), None);
    }

    #[test]
    fn test_extract_task_id_from_pane_cmd_with_hyphens() {
        let cmd = "echo '=== Build Task [my-task-123] ==='; tail -f /tmp/out";
        assert_eq!(
            extract_task_id_from_pane_cmd(cmd),
            Some("my-task-123".to_string())
        );
    }

    // --- load_workload_pane_ids boundary tests ---
    //
    // Each test installs a unique mock state.json path via the
    // `CLAUDE_WATCH_WORKLOAD_STATE` env var, then calls `load_workload_pane_ids`.
    // The env var is process-global, so we use a single mutex to serialize the
    // tests against each other (and against any concurrent unit test that might
    // touch this var). We deliberately do NOT serialize against e2e tests —
    // those run in their own binary, in a separate process.
    use std::sync::Mutex;
    static WORKLOAD_ENV_LOCK: Mutex<()> = Mutex::new(());

    struct EnvGuard;
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            std::env::remove_var("CLAUDE_WATCH_WORKLOAD_STATE");
        }
    }

    #[test]
    fn test_load_workload_pane_ids_missing_file() {
        let _lock = WORKLOAD_ENV_LOCK.lock().unwrap();
        let _guard = EnvGuard;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("does-not-exist.json");
        std::env::set_var("CLAUDE_WATCH_WORKLOAD_STATE", &path);
        // Default-open: missing file → empty set, no panic.
        let ids = load_workload_pane_ids();
        assert!(ids.is_empty(), "missing file should yield empty set");
    }

    #[test]
    fn test_load_workload_pane_ids_malformed_json() {
        let _lock = WORKLOAD_ENV_LOCK.lock().unwrap();
        let _guard = EnvGuard;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.json");
        std::fs::write(&path, "{ this is not valid json").unwrap();
        std::env::set_var("CLAUDE_WATCH_WORKLOAD_STATE", &path);
        // Default-open: malformed JSON → empty set + warn log, no panic.
        let ids = load_workload_pane_ids();
        assert!(ids.is_empty(), "malformed JSON should yield empty set");
    }

    #[test]
    fn test_load_workload_pane_ids_empty_object() {
        let _lock = WORKLOAD_ENV_LOCK.lock().unwrap();
        let _guard = EnvGuard;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.json");
        std::fs::write(&path, "{}").unwrap();
        std::env::set_var("CLAUDE_WATCH_WORKLOAD_STATE", &path);
        let ids = load_workload_pane_ids();
        assert!(ids.is_empty(), "empty object should yield empty set");
    }

    #[test]
    fn test_load_workload_pane_ids_single_workload() {
        let _lock = WORKLOAD_ENV_LOCK.lock().unwrap();
        let _guard = EnvGuard;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        std::fs::write(
            &path,
            r#"{"promote-layl-s01":{"pane_id":"%1832","command":"stv-promote ...","output":"/tmp/x.output","started_at":"2026-04-30T18:00:00"}}"#,
        )
        .unwrap();
        std::env::set_var("CLAUDE_WATCH_WORKLOAD_STATE", &path);
        let ids = load_workload_pane_ids();
        assert_eq!(ids.len(), 1);
        assert!(ids.contains("%1832"));
    }

    #[test]
    fn test_load_workload_pane_ids_multiple_workloads() {
        let _lock = WORKLOAD_ENV_LOCK.lock().unwrap();
        let _guard = EnvGuard;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        std::fs::write(
            &path,
            r#"{
                "wa": {"pane_id": "%100", "command": "x"},
                "wb": {"pane_id": "%200", "command": "y"},
                "wc": {"pane_id": "", "command": "z"}
            }"#,
        )
        .unwrap();
        std::env::set_var("CLAUDE_WATCH_WORKLOAD_STATE", &path);
        let ids = load_workload_pane_ids();
        // Empty pane_id should be filtered out.
        assert_eq!(ids.len(), 2);
        assert!(ids.contains("%100"));
        assert!(ids.contains("%200"));
    }

    #[test]
    fn test_load_workload_pane_ids_root_is_array() {
        // A JSON array at the root is malformed for our schema (we expect an object).
        // Default-open: warn + empty set.
        let _lock = WORKLOAD_ENV_LOCK.lock().unwrap();
        let _guard = EnvGuard;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("array.json");
        std::fs::write(&path, "[1, 2, 3]").unwrap();
        std::env::set_var("CLAUDE_WATCH_WORKLOAD_STATE", &path);
        let ids = load_workload_pane_ids();
        assert!(ids.is_empty(), "non-object root should yield empty set");
    }

    #[test]
    fn test_workload_state_path_default() {
        let _lock = WORKLOAD_ENV_LOCK.lock().unwrap();
        let _guard = EnvGuard;
        std::env::remove_var("CLAUDE_WATCH_WORKLOAD_STATE");
        assert_eq!(
            workload_state_path(),
            PathBuf::from("/var/run/claude/workload-state/state.json")
        );
    }

    #[test]
    fn test_workload_state_path_env_override() {
        let _lock = WORKLOAD_ENV_LOCK.lock().unwrap();
        let _guard = EnvGuard;
        std::env::set_var("CLAUDE_WATCH_WORKLOAD_STATE", "/custom/path/state.json");
        assert_eq!(
            workload_state_path(),
            PathBuf::from("/custom/path/state.json")
        );
    }

    #[test]
    fn test_workload_state_path_empty_env_falls_back_to_default() {
        let _lock = WORKLOAD_ENV_LOCK.lock().unwrap();
        let _guard = EnvGuard;
        std::env::set_var("CLAUDE_WATCH_WORKLOAD_STATE", "");
        // Empty string should NOT redirect to "" — fall back to the default path.
        assert_eq!(
            workload_state_path(),
            PathBuf::from("/var/run/claude/workload-state/state.json")
        );
    }
}
