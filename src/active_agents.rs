//! `claude-watch active-agents` — read-only enumeration of live agents.
//!
//! Emits a fact set about who is running RIGHT NOW:
//!
//!   - `subagents`: live child PIDs of the Claude Code main process that
//!     are NOT watchers and NOT our own introspection commands. Reuses the
//!     same heuristic as `agent::cmd_list` and `respawn::count_active_subagents`.
//!     We emit raw PIDs (not agent IDs) deliberately — agent IDs would
//!     require reading the per-session subagents JSONL directory, which is
//!     a path-leak we explicitly want to avoid in the public-repo source
//!     for the BARE pid set. See `agents` below for the richer mapping.
//!
//!   - `workloads`: labels of currently-running workloads, read from
//!     claude-watch's own internal workload state. `running` here means the
//!     tmux pane is still alive; we exclude completed/dead workloads.
//!
//!   - `agents` (`--with-meta` / via `agent-state`): per-agent records
//!     {agent_id, queue_id, alive, jsonl_age_seconds}. Built by scanning
//!     the active session's `subagents/` JSONL directory and parsing the
//!     first user message of each transcript for a `Queue item: q-XXXX`
//!     marker. Liveness is JSONL-mtime-based (subagents share the parent
//!     Claude Code PID, so per-subagent /proc liveness is impossible —
//!     we infer "still working" from the transcript being actively
//!     appended). The default max-age is 120s, matching the typical
//!     time between agent tool calls.
//!
//! Design intent: claude-watch emits FACTS about live processes + agents.
//! Whoever consumes this output (e.g. work-queue-exporter) decides what is
//! "expected" vs "orphaned" on its own side. claude-watch deliberately
//! does NOT consume the queue.json schema, scope semantics, or any
//! local-path convention. This is the abstraction line the repo failed to
//! hold in the first iteration of #53.

use serde::Serialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::agent::{
    find_claude_pid, get_children, is_own_command, is_watcher, ChildProcess,
};
use crate::workload::{load_state, WorkloadState};

/// Default freshness window for `find_active_subagents_dirs`. Any
/// `subagents/` directory whose newest file mtime is within this many
/// seconds of "now" is considered active and is included in the merge.
///
/// 24h covers the realistic worst case: a long-running agent that
/// outlived ONE parent restart will have a pre-restart transcript dir
/// (with the queue marker) plus a post-restart continuation dir under
/// a new session UUID; both stay within the window.
pub const DEFAULT_SUBAGENTS_DIR_FRESHNESS_SECS: u64 = 24 * 60 * 60;

/// Find ALL recently-active `subagents/` directories under
/// `~/.claude/projects/<project-slug>/<session-uuid>/subagents/`.
///
/// Returns every dir whose newest contained file mtime is within
/// `freshness_secs` of `now`. Sorted newest-first.
///
/// We walk the projects tree directly rather than going through
/// `find_session_dir` (which keys off `/tmp/claude-<uid>/<slug>/<uuid>/tasks/`
/// — those UUIDs DO NOT match the projects/ UUIDs, so the prior
/// path-join-by-session-id was broken on real installs and silently
/// returned nothing).
///
/// Why a list and not the single newest dir: when the main-loop parent
/// process crashes (e.g. OOM/SIGKILL) or self-clears and gets resumed,
/// any subagents that survive write continuation JSONL frames into a
/// new session UUID directory. Those continuation frames start with a
/// `tool_result` (the exit-code reply from the dead parent) and DO NOT
/// re-include the `Queue item: q-XXXX` spawn marker. The marker is
/// still present in the PRE-restart transcript under the OLD session
/// UUID dir. If we only inspect the single most-recent dir we silently
/// drop the marker for the affected agents. Merging across all recent
/// dirs (with a preference for records that DO have a queue id) lets
/// the resolver recover the original marker.
pub fn find_active_subagents_dirs(
    now: SystemTime,
    freshness_secs: u64,
) -> Vec<PathBuf> {
    let home = match std::env::var("HOME") {
        Ok(h) => h,
        Err(_) => return Vec::new(),
    };
    find_active_subagents_dirs_in(
        &PathBuf::from(home).join(".claude/projects"),
        now,
        freshness_secs,
    )
}

/// Same as `find_active_subagents_dirs` but with a caller-supplied
/// projects-root path. Tests use this to point at a tempdir.
pub fn find_active_subagents_dirs_in(
    projects: &Path,
    now: SystemTime,
    freshness_secs: u64,
) -> Vec<PathBuf> {
    let mut hits: Vec<(SystemTime, PathBuf)> = Vec::new();
    let project_dirs = match std::fs::read_dir(projects) {
        Ok(d) => d,
        Err(_) => return Vec::new(),
    };
    for project_entry in project_dirs.flatten() {
        let session_dirs = match std::fs::read_dir(project_entry.path()) {
            Ok(d) => d,
            Err(_) => continue,
        };
        for session_entry in session_dirs.flatten() {
            let subagents = session_entry.path().join("subagents");
            if !subagents.is_dir() {
                continue;
            }
            // Score this subagents dir by the most-recent mtime of any
            // file in it. Empty dirs (no agents yet) get the dir's own
            // mtime so they still register as "this session".
            let mut newest = match std::fs::metadata(&subagents)
                .ok()
                .and_then(|m| m.modified().ok())
            {
                Some(t) => t,
                None => continue,
            };
            if let Ok(children) = std::fs::read_dir(&subagents) {
                for child in children.flatten() {
                    if let Ok(meta) = child.metadata() {
                        if let Ok(mt) = meta.modified() {
                            if mt > newest {
                                newest = mt;
                            }
                        }
                    }
                }
            }
            // Apply the freshness window. Future-dated mtimes (clock
            // skew) always pass. Negative durations from
            // `duration_since` indicate the file is NEWER than `now`,
            // which we treat as "definitely fresh".
            let within_window = match now.duration_since(newest) {
                Ok(d) => d.as_secs() <= freshness_secs,
                Err(_) => true,
            };
            if within_window {
                hits.push((newest, subagents));
            }
        }
    }
    // Newest-first ordering. Callers that care about precedence (e.g.
    // tie-breaking equal-quality records on recency) consume the list
    // front-to-back.
    hits.sort_by(|a, b| b.0.cmp(&a.0));
    hits.into_iter().map(|(_, p)| p).collect()
}

/// Default JSONL-mtime "alive" window. An agent transcript that hasn't
/// been touched in this many seconds is considered no-longer-running.
/// Subagents typically write to JSONL on every tool call AND on every
/// model turn — 120s is comfortable headroom for a long thinking pass
/// without false-positive death.
pub const DEFAULT_AGENT_ALIVE_MAX_AGE_SECS: u64 = 120;

/// Output shape for `claude-watch active-agents [--json]`.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct ActiveAgents {
    /// Live PIDs of Claude Code subagent processes (children of the main
    /// Claude PID, minus watchers + own commands). Sorted ascending for a
    /// stable diff. Note: a subagent ONLY shows up here while it has an
    /// active child tool process (Bash, etc.); during pure model thinking
    /// it has no PID. Use `agents` for the full population.
    pub subagents: Vec<u32>,

    /// Labels of currently-running workloads. Sorted ascending.
    pub workloads: Vec<String>,

    /// Per-agent records, one per JSONL in the active session's
    /// `subagents/` dir. Empty when no session is detected. Always
    /// included; consumers join on `queue_id` to map queue items to
    /// agent liveness. Sorted by agent_id for stable diff.
    #[serde(default)]
    pub agents: Vec<AgentRecord>,
}

/// One agent's liveness record.
#[derive(Debug, Serialize, PartialEq, Eq, Clone)]
pub struct AgentRecord {
    /// Agent ID (the `agent-XXXX` filename stem in the subagents dir).
    pub agent_id: String,
    /// Queue item ID parsed from the agent's first user message
    /// (`Queue item: q-XXXX` marker), if present. None for agents
    /// spawned without queue tracking (rare — agents from the spawn
    /// gate always include the marker).
    pub queue_id: Option<String>,
    /// True iff JSONL was modified within the configured max-age window.
    /// Subagents lack stable PIDs (they share the parent Claude PID),
    /// so transcript-mtime is the canonical liveness signal.
    pub alive: bool,
    /// Seconds since JSONL was last modified. None if metadata was
    /// unreadable (treated as `alive=false`).
    pub jsonl_age_seconds: Option<u64>,
}

/// Pure: filter `children` down to subagent PIDs and sort.
pub fn filter_subagent_pids(children: &[ChildProcess]) -> Vec<u32> {
    let mut pids: Vec<u32> = children
        .iter()
        .filter(|c| !is_watcher(&c.cmd) && !is_own_command(&c.cmd))
        .map(|c| c.pid)
        .collect();
    pids.sort_unstable();
    pids
}

/// Pure: extract running workload labels from a `WorkloadState`,
/// using a caller-supplied liveness predicate (so tests can avoid tmux).
///
/// "Running" = tmux pane is still alive. We do NOT consult `<label>.exit`:
/// a workload that has exited but whose pane is in the 30-second sleep
/// tail is conceptually "winding down", not "orphaned"; the consumer
/// shim doesn't need to see it. The pane-alive check is exactly what
/// `workload list` already uses for its "running" status.
pub fn running_workload_labels<F>(state: &WorkloadState, mut is_pane_alive: F) -> Vec<String>
where
    F: FnMut(&str) -> bool,
{
    let mut labels: Vec<String> = state
        .iter()
        .filter(|(_, info)| is_pane_alive(&info.pane_id))
        .map(|(label, _)| label.clone())
        .collect();
    labels.sort();
    labels
}

/// Pure: extract the queue item ID from a JSONL transcript content.
///
/// Looks for the literal marker `Queue item: q-XXXX` anywhere in the
/// content. The first match wins (the marker is always on the first
/// user message — the spawn prompt — but we don't enforce position so
/// the parser stays trivial and robust).
///
/// Queue id format: `q-` followed by alphanumerics or hyphens, terminated
/// by whitespace, end-of-line, or a non-allowed char. Examples:
///   q-2026-05-01-6087   q-bd3a   q-a50a
pub fn extract_queue_id(content: &str) -> Option<String> {
    let needle = "Queue item:";
    let mut start = 0usize;
    while let Some(pos) = content[start..].find(needle) {
        let abs = start + pos + needle.len();
        let rest = &content[abs..];
        // Skip any whitespace after the colon.
        let rest = rest.trim_start();
        // Must start with `q-` to be a queue id.
        if let Some(after_q) = rest.strip_prefix("q-") {
            // Take chars until non-allowed.
            let mut end = 0usize;
            for (i, c) in after_q.char_indices() {
                if c.is_ascii_alphanumeric() || c == '-' {
                    end = i + c.len_utf8();
                } else {
                    break;
                }
            }
            if end > 0 {
                return Some(format!("q-{}", &after_q[..end]));
            }
        }
        // Advance past this needle to look for further occurrences.
        start = abs;
    }
    None
}

/// Read a JSONL file and extract the queue id from its FIRST user message.
///
/// We only read the first line for performance — the spawn prompt is
/// always the first user message and contains the marker. Falls back
/// to scanning the whole file if the first line doesn't match (cheap
/// for the small JSONLs; rare path).
fn extract_queue_id_from_jsonl(jsonl_path: &Path) -> Option<String> {
    let content = std::fs::read_to_string(jsonl_path).ok()?;
    // Fast path: just check the first line (the spawn prompt).
    if let Some(first_line) = content.lines().next() {
        if let Some(qid) = extract_queue_id(first_line) {
            return Some(qid);
        }
    }
    // Fallback: scan everything (handles edge cases — e.g. agents
    // continued after a follow-up prompt that re-cites the queue id).
    extract_queue_id(&content)
}

/// Compute alive flag + age from a JSONL mtime and `now`.
pub fn agent_alive_from_mtime(
    jsonl_mtime: Option<SystemTime>,
    now: SystemTime,
    max_age_secs: u64,
) -> (bool, Option<u64>) {
    match jsonl_mtime {
        Some(mt) => match now.duration_since(mt) {
            Ok(age) => {
                let age_s = age.as_secs();
                (age_s <= max_age_secs, Some(age_s))
            }
            // mtime in the future (clock skew, etc.) — treat as just-modified.
            Err(_) => (true, Some(0)),
        },
        None => (false, None),
    }
}

/// Scan a `subagents/` directory and build per-agent records.
///
/// `now` is injected so tests are deterministic.
pub fn collect_agent_records(
    subagents_dir: &Path,
    now: SystemTime,
    max_age_secs: u64,
) -> Vec<AgentRecord> {
    let mut out: Vec<AgentRecord> = Vec::new();
    let entries = match std::fs::read_dir(subagents_dir) {
        Ok(e) => e,
        Err(_) => return out,
    };
    for entry in entries.flatten() {
        let fname = entry.file_name().to_string_lossy().to_string();
        // Match `agent-<id>.jsonl` (NOT meta.json, NOT anything else).
        let agent_id = match fname
            .strip_prefix("agent-")
            .and_then(|s| s.strip_suffix(".jsonl"))
        {
            Some(id) => id.to_string(),
            None => continue,
        };
        let path = entry.path();
        let mtime = std::fs::metadata(&path).ok().and_then(|m| m.modified().ok());
        let (alive, age) = agent_alive_from_mtime(mtime, now, max_age_secs);
        let queue_id = extract_queue_id_from_jsonl(&path);
        out.push(AgentRecord {
            agent_id,
            queue_id,
            alive,
            jsonl_age_seconds: age,
        });
    }
    out.sort_by(|a, b| a.agent_id.cmp(&b.agent_id));
    out
}

/// Collect agent records across MULTIPLE `subagents/` directories and
/// merge by agent id.
///
/// Merge policy (per agent id):
///
///  1. Prefer the record whose `queue_id` is `Some(_)` over one whose
///     `queue_id` is `None`. This is the whole point of the merge: a
///     post-restart continuation JSONL drops the spawn marker, but the
///     pre-restart transcript still has it.
///  2. Among records that BOTH have a queue id (or both have none),
///     prefer `alive=true` over `alive=false`.
///  3. Final tie-break: prefer the more-recent JSONL mtime (i.e. the
///     smaller `jsonl_age_seconds`). `None` ages sort last.
///
/// `dirs` is consumed front-to-back; the iteration order doesn't
/// affect the result because the merge is symmetric on the comparison
/// fields. Returned records are sorted by agent_id for stable diff,
/// matching `collect_agent_records`.
pub fn collect_agent_records_merged(
    dirs: &[PathBuf],
    now: SystemTime,
    max_age_secs: u64,
) -> Vec<AgentRecord> {
    let mut by_id: HashMap<String, AgentRecord> = HashMap::new();
    for dir in dirs {
        for record in collect_agent_records(dir, now, max_age_secs) {
            match by_id.get(&record.agent_id) {
                None => {
                    by_id.insert(record.agent_id.clone(), record);
                }
                Some(existing) => {
                    if should_replace(existing, &record) {
                        by_id.insert(record.agent_id.clone(), record);
                    }
                }
            }
        }
    }
    let mut out: Vec<AgentRecord> = by_id.into_values().collect();
    out.sort_by(|a, b| a.agent_id.cmp(&b.agent_id));
    out
}

/// Pure: decide whether `candidate` should replace `existing` per the
/// merge policy documented on `collect_agent_records_merged`.
fn should_replace(existing: &AgentRecord, candidate: &AgentRecord) -> bool {
    // Rule 1: non-null queue_id wins.
    match (existing.queue_id.is_some(), candidate.queue_id.is_some()) {
        (false, true) => return true,
        (true, false) => return false,
        _ => {}
    }
    // Rule 2: alive wins.
    match (existing.alive, candidate.alive) {
        (false, true) => return true,
        (true, false) => return false,
        _ => {}
    }
    // Rule 3: smaller jsonl_age_seconds (more recent) wins. None ages
    // sort last (i.e. an unreadable-mtime record never wins this
    // tiebreaker against a known one).
    match (existing.jsonl_age_seconds, candidate.jsonl_age_seconds) {
        (Some(_), None) => false,
        (None, Some(_)) => true,
        (Some(a), Some(b)) => b < a,
        (None, None) => false,
    }
}

/// Production helper: ask tmux whether a pane id is alive.
///
/// Mirrors the private `pane_alive` in `workload.rs` — duplicating here
/// (instead of exposing the workload helper) keeps `active_agents` tests
/// completely independent of tmux. The function is short and the
/// duplication cost is one tmux invocation's worth of code.
fn pane_alive(pane_id: &str) -> bool {
    if pane_id.is_empty() {
        return false;
    }
    let out = std::process::Command::new("tmux")
        .args(["list-panes", "-t", "tasks", "-F", "#{pane_id}"])
        .output();
    match out {
        Ok(o) if o.status.success() => {
            let stdout = String::from_utf8_lossy(&o.stdout);
            stdout.lines().any(|l| l.trim() == pane_id)
        }
        _ => false,
    }
}

/// Collect the live agents fact-set. Production entry point.
///
/// Fail-open: if no Claude PID is detected (`find_claude_pid` returns None),
/// `subagents` is an empty Vec. Workloads still report normally — they
/// don't depend on the Claude PID. `agents` is also empty when no session
/// dir is detected.
pub fn collect() -> ActiveAgents {
    collect_with_max_age(DEFAULT_AGENT_ALIVE_MAX_AGE_SECS)
}

/// Collect with caller-specified max-age window for agent liveness.
pub fn collect_with_max_age(max_age_secs: u64) -> ActiveAgents {
    let subagents = match find_claude_pid() {
        Some(pid) => filter_subagent_pids(&get_children(pid)),
        None => Vec::new(),
    };
    let workloads = running_workload_labels(&load_state(), pane_alive);
    let now = SystemTime::now();
    let dirs = find_active_subagents_dirs(now, DEFAULT_SUBAGENTS_DIR_FRESHNESS_SECS);
    let agents = if dirs.is_empty() {
        Vec::new()
    } else {
        collect_agent_records_merged(&dirs, now, max_age_secs)
    };
    ActiveAgents {
        subagents,
        workloads,
        agents,
    }
}

/// Atomic write a string to `path` via `<path>.tmp` + rename.
fn atomic_write(path: &Path, content: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, content)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// CLI entry point. Returns exit code.
///
/// Prints to stdout (always) and OPTIONALLY also writes to
/// `--write-state <path>` (atomic). The writeable mode is meant for cron
/// (e.g. `* * * * * claude-watch active-agents --json --write-state
/// /var/lib/claude-watch/active-agents.json`) so other consumers
/// (work-queue-exporter container) can read the JSON via a bind-mount
/// without shelling out to claude-watch.
pub fn cmd_active_agents(json: bool, max_age_secs: u64, write_state: Option<&str>) -> i32 {
    let agents = collect_with_max_age(max_age_secs);

    // Always render JSON for the state file (machine-readable contract).
    let json_str =
        serde_json::to_string_pretty(&agents).unwrap_or_else(|_| "{}".to_string());

    if let Some(path) = write_state {
        if let Err(e) = atomic_write(Path::new(path), &(json_str.clone() + "\n")) {
            eprintln!("error: failed to write state file {}: {}", path, e);
            return 2;
        }
    }

    if json {
        println!("{}", json_str);
    } else {
        // Human-readable, scannable in a terminal.
        if agents.subagents.is_empty() {
            println!("Subagents: (none)");
        } else {
            let pids: Vec<String> = agents.subagents.iter().map(|p| p.to_string()).collect();
            println!("Subagents: {}", pids.join(", "));
        }
        if agents.workloads.is_empty() {
            println!("Workloads: (none)");
        } else {
            println!("Workloads: {}", agents.workloads.join(", "));
        }
        if agents.agents.is_empty() {
            println!("Agents:    (none)");
        } else {
            println!("Agents:");
            for a in &agents.agents {
                let qid = a.queue_id.as_deref().unwrap_or("-");
                let age = a
                    .jsonl_age_seconds
                    .map(|s| format!("{}s", s))
                    .unwrap_or_else(|| "?".to_string());
                let alive = if a.alive { "alive" } else { "stale" };
                println!(
                    "  {}  queue={}  {}  age={}",
                    a.agent_id, qid, alive, age
                );
            }
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workload::WorkloadEntry;
    use std::time::Duration;

    fn cp(pid: u32, cmd: &str) -> ChildProcess {
        ChildProcess {
            pid,
            cmd: cmd.to_string(),
        }
    }

    #[test]
    fn filter_subagent_pids_excludes_watchers() {
        let children = vec![
            cp(100, "zsh -c eval 'watcher-ctl run alerts-watcher' < /dev/null"),
            cp(200, "python3 /home/user/.claude/sidechain-agent.py"),
            cp(300, "zsh -c eval 'watcher-ctl run torrent-wait' < /dev/null"),
        ];
        // 100 + 300 are watchers; only 200 should remain.
        assert_eq!(filter_subagent_pids(&children), vec![200]);
    }

    #[test]
    fn filter_subagent_pids_excludes_own_commands() {
        let children = vec![
            cp(100, "agent-ctl list"),
            cp(200, "claude-watch agent list"),
            cp(300, "ps --ppid 1 -o pid=,cmd="),
            cp(400, "python3 /some/agent-script.py"),
        ];
        assert_eq!(filter_subagent_pids(&children), vec![400]);
    }

    #[test]
    fn filter_subagent_pids_sorts_ascending() {
        let children = vec![
            cp(500, "python3 /tmp/a.py"),
            cp(100, "ruby /tmp/b.rb"),
            cp(300, "node /tmp/c.js"),
        ];
        assert_eq!(filter_subagent_pids(&children), vec![100, 300, 500]);
    }

    #[test]
    fn filter_subagent_pids_empty_input() {
        assert!(filter_subagent_pids(&[]).is_empty());
    }

    #[test]
    fn filter_subagent_pids_all_watchers() {
        let children = vec![
            cp(1, "zsh -c eval 'watcher-ctl run alerts-watcher' < /dev/null"),
            cp(2, "zsh -c eval 'memory-remind' < /dev/null"),
            cp(3, "zsh -c eval 'context-watch' < /dev/null"),
        ];
        assert!(filter_subagent_pids(&children).is_empty());
    }

    #[test]
    fn running_workload_labels_filters_dead_panes() {
        let mut state = WorkloadState::new();
        state.insert(
            "alive-1".to_string(),
            WorkloadEntry {
                pane_id: "%5".to_string(),
                command: "sleep 100".to_string(),
                output: "/tmp/a.output".to_string(),
                started_at: "2026-05-01T00:00:00".to_string(),
                queue_id: None,
            },
        );
        state.insert(
            "dead-1".to_string(),
            WorkloadEntry {
                pane_id: "%6".to_string(),
                command: "echo hi".to_string(),
                output: "/tmp/b.output".to_string(),
                started_at: "2026-05-01T00:00:01".to_string(),
                queue_id: None,
            },
        );

        // Predicate: only %5 is alive.
        let alive_only = |pid: &str| pid == "%5";
        let labels = running_workload_labels(&state, alive_only);
        assert_eq!(labels, vec!["alive-1".to_string()]);
    }

    #[test]
    fn running_workload_labels_sorts_alphabetically() {
        let mut state = WorkloadState::new();
        for label in ["zeta", "alpha", "mu"] {
            state.insert(
                label.to_string(),
                WorkloadEntry {
                    pane_id: format!("%{label}"),
                    command: String::new(),
                    output: String::new(),
                    started_at: String::new(),
                    queue_id: None,
                },
            );
        }
        let labels = running_workload_labels(&state, |_| true);
        assert_eq!(labels, vec!["alpha", "mu", "zeta"]);
    }

    #[test]
    fn running_workload_labels_empty_state() {
        let state = WorkloadState::new();
        assert!(running_workload_labels(&state, |_| true).is_empty());
    }

    #[test]
    fn running_workload_labels_all_dead() {
        let mut state = WorkloadState::new();
        state.insert(
            "x".to_string(),
            WorkloadEntry {
                pane_id: "%1".to_string(),
                command: String::new(),
                output: String::new(),
                started_at: String::new(),
                queue_id: None,
            },
        );
        assert!(running_workload_labels(&state, |_| false).is_empty());
    }

    #[test]
    fn active_agents_serializes_to_expected_json_shape() {
        let agents = ActiveAgents {
            subagents: vec![1234, 5678],
            workloads: vec!["promote-foo".to_string(), "scan-bar".to_string()],
            agents: vec![AgentRecord {
                agent_id: "abc123".to_string(),
                queue_id: Some("q-2026-05-01-6087".to_string()),
                alive: true,
                jsonl_age_seconds: Some(15),
            }],
        };
        let json = serde_json::to_value(&agents).expect("serialize");
        assert_eq!(json["subagents"], serde_json::json!([1234, 5678]));
        assert_eq!(
            json["workloads"],
            serde_json::json!(["promote-foo", "scan-bar"])
        );
        assert_eq!(
            json["agents"],
            serde_json::json!([{
                "agent_id": "abc123",
                "queue_id": "q-2026-05-01-6087",
                "alive": true,
                "jsonl_age_seconds": 15,
            }])
        );
        // No extra keys leak through.
        let obj = json.as_object().expect("object");
        assert_eq!(obj.len(), 3);
    }

    #[test]
    fn active_agents_empty_serializes_to_empty_arrays() {
        let agents = ActiveAgents {
            subagents: vec![],
            workloads: vec![],
            agents: vec![],
        };
        let json = serde_json::to_string(&agents).expect("serialize");
        assert_eq!(
            json,
            r#"{"subagents":[],"workloads":[],"agents":[]}"#
        );
    }

    // --- queue id extraction ---

    #[test]
    fn extract_queue_id_basic() {
        let s = "Queue item: q-2026-05-01-6087\n\nAndrew DM 15:57";
        assert_eq!(
            extract_queue_id(s).as_deref(),
            Some("q-2026-05-01-6087")
        );
    }

    #[test]
    fn extract_queue_id_short_form() {
        let s = "blah blah Queue item: q-bd3a stuff";
        assert_eq!(extract_queue_id(s).as_deref(), Some("q-bd3a"));
    }

    #[test]
    fn extract_queue_id_no_marker() {
        let s = "totally normal prompt with no marker";
        assert_eq!(extract_queue_id(s), None);
    }

    #[test]
    fn extract_queue_id_marker_no_qid() {
        // Marker present but next token isn't a queue id — should NOT match.
        let s = "Queue item: TBD";
        assert_eq!(extract_queue_id(s), None);
    }

    #[test]
    fn extract_queue_id_extra_whitespace() {
        let s = "Queue item:    q-abc-123  more text";
        assert_eq!(extract_queue_id(s).as_deref(), Some("q-abc-123"));
    }

    #[test]
    fn extract_queue_id_terminator_punctuation() {
        // Trailing comma should NOT be part of the id.
        let s = "Queue item: q-2026-05-01-a50a, please do X";
        assert_eq!(
            extract_queue_id(s).as_deref(),
            Some("q-2026-05-01-a50a")
        );
    }

    #[test]
    fn extract_queue_id_first_match_wins() {
        let s = "Queue item: q-first ... Queue item: q-second";
        assert_eq!(extract_queue_id(s).as_deref(), Some("q-first"));
    }

    // --- liveness ---

    #[test]
    fn agent_alive_from_mtime_fresh() {
        let now = SystemTime::now();
        let mt = now - Duration::from_secs(10);
        let (alive, age) = agent_alive_from_mtime(Some(mt), now, 120);
        assert!(alive);
        assert_eq!(age, Some(10));
    }

    #[test]
    fn agent_alive_from_mtime_stale() {
        let now = SystemTime::now();
        let mt = now - Duration::from_secs(300);
        let (alive, age) = agent_alive_from_mtime(Some(mt), now, 120);
        assert!(!alive);
        assert_eq!(age, Some(300));
    }

    #[test]
    fn agent_alive_from_mtime_at_threshold() {
        let now = SystemTime::now();
        let mt = now - Duration::from_secs(120);
        let (alive, age) = agent_alive_from_mtime(Some(mt), now, 120);
        // Exactly at threshold = still alive (<=).
        assert!(alive);
        assert_eq!(age, Some(120));
    }

    #[test]
    fn agent_alive_from_mtime_none() {
        let now = SystemTime::now();
        let (alive, age) = agent_alive_from_mtime(None, now, 120);
        assert!(!alive);
        assert_eq!(age, None);
    }

    #[test]
    fn agent_alive_from_mtime_future() {
        // Clock skew: mtime in the future — be lenient, treat as just-modified.
        let now = SystemTime::now();
        let mt = now + Duration::from_secs(5);
        let (alive, age) = agent_alive_from_mtime(Some(mt), now, 120);
        assert!(alive);
        assert_eq!(age, Some(0));
    }

    // --- collect_agent_records integration ---

    #[test]
    fn collect_agent_records_parses_queue_id_and_alive() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Two agents:
        //  agent-aaaa1.jsonl: marker present, very recent mtime → alive
        //  agent-bbbb2.jsonl: no marker, old mtime → stale, queue_id=None
        let p1 = dir.path().join("agent-aaaa1.jsonl");
        std::fs::write(
            &p1,
            r#"{"message":{"content":"Queue item: q-2026-05-01-6087\n\nDo the thing"}}
{"message":{"content":[{"type":"text","text":"working..."}]}}
"#,
        )
        .unwrap();
        let p2 = dir.path().join("agent-bbbb2.jsonl");
        std::fs::write(
            &p2,
            r#"{"message":{"content":"This prompt has no queue marker."}}
"#,
        )
        .unwrap();

        // Backdate agent-bbbb2 by 10 minutes via filetime so it's clearly stale.
        // Use SystemTime::now() - 600s.
        let stale = SystemTime::now() - Duration::from_secs(600);
        let stale_ft = filetime::FileTime::from_system_time(stale);
        filetime::set_file_mtime(&p2, stale_ft).unwrap();

        // Also include a meta.json + a non-agent file — should be ignored.
        std::fs::write(
            dir.path().join("agent-aaaa1.meta.json"),
            r#"{"description":"x","agentType":"general-purpose"}"#,
        )
        .unwrap();
        std::fs::write(dir.path().join("README.txt"), "not an agent").unwrap();

        let now = SystemTime::now();
        let records = collect_agent_records(dir.path(), now, 120);
        assert_eq!(records.len(), 2, "{:?}", records);

        // Sorted by agent_id; aaaa1 < bbbb2 lexicographically.
        let a = &records[0];
        assert_eq!(a.agent_id, "aaaa1");
        assert_eq!(a.queue_id.as_deref(), Some("q-2026-05-01-6087"));
        assert!(a.alive);
        assert!(a.jsonl_age_seconds.unwrap_or(999) < 120);

        let b = &records[1];
        assert_eq!(b.agent_id, "bbbb2");
        assert!(b.queue_id.is_none());
        assert!(!b.alive);
        assert!(b.jsonl_age_seconds.unwrap_or(0) >= 600);
    }

    #[test]
    fn collect_agent_records_empty_dir() {
        let dir = tempfile::tempdir().expect("tempdir");
        let records = collect_agent_records(dir.path(), SystemTime::now(), 120);
        assert!(records.is_empty());
    }

    #[test]
    fn collect_agent_records_missing_dir() {
        let records = collect_agent_records(
            Path::new("/nonexistent/path/that/does/not/exist"),
            SystemTime::now(),
            120,
        );
        assert!(records.is_empty());
    }

    // --- atomic_write ---

    #[test]
    fn atomic_write_writes_and_replaces() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("state.json");
        atomic_write(&path, "first\n").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "first\n");
        atomic_write(&path, "second\n").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "second\n");
        // No leftover .tmp.
        assert!(!path.with_extension("json.tmp").exists());
    }

    #[test]
    fn atomic_write_creates_parent_dir() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nested").join("deep").join("state.json");
        atomic_write(&path, "hi\n").unwrap();
        assert!(path.exists());
    }

    // --- multi-dir merge: post-restart marker recovery ---

    /// Build a fake projects tree at `root` with two `subagents/`
    /// directories under different session UUIDs of the same project
    /// slug. Returns (older_dir, newer_dir).
    fn build_two_session_dirs(root: &Path) -> (PathBuf, PathBuf) {
        let project = root.join("-home-fake-project");
        let session_old = project.join("11111111-1111-1111-1111-111111111111");
        let session_new = project.join("22222222-2222-2222-2222-222222222222");
        let older = session_old.join("subagents");
        let newer = session_new.join("subagents");
        std::fs::create_dir_all(&older).unwrap();
        std::fs::create_dir_all(&newer).unwrap();
        (older, newer)
    }

    /// Write a JSONL whose first line carries a `Queue item:` marker.
    fn write_marker_jsonl(path: &Path, qid: &str) {
        let body = format!(
            "{{\"message\":{{\"content\":\"Queue item: {}\\n\\nDo the thing\"}}}}\n",
            qid
        );
        std::fs::write(path, body).unwrap();
    }

    /// Write a JSONL simulating a post-restart continuation transcript:
    /// the first frame is a `tool_result` reporting the prior parent's
    /// exit, with no spawn marker anywhere.
    fn write_continuation_jsonl(path: &Path) {
        let body = "{\"type\":\"user\",\"message\":{\"content\":[{\"type\":\"tool_result\",\"content\":\"Exit code 137\"}]}}\n";
        std::fs::write(path, body).unwrap();
    }

    /// Set the mtime of `path` to `secs` seconds before `SystemTime::now()`.
    fn backdate(path: &Path, secs: u64) {
        let mt = SystemTime::now() - Duration::from_secs(secs);
        let ft = filetime::FileTime::from_system_time(mt);
        filetime::set_file_mtime(path, ft).unwrap();
    }

    #[test]
    fn merge_prefers_record_with_queue_id_over_one_without() {
        // The bug: agent survived a parent restart. The OLD session dir
        // has a JSONL with the spawn marker; the NEW session dir has a
        // continuation JSONL with no marker. The merged result must
        // surface the marker, NOT the post-restart None.
        let tmp = tempfile::tempdir().expect("tempdir");
        let (older, newer) = build_two_session_dirs(tmp.path());

        let id = "abc123";
        let older_jsonl = older.join(format!("agent-{}.jsonl", id));
        let newer_jsonl = newer.join(format!("agent-{}.jsonl", id));

        write_marker_jsonl(&older_jsonl, "q-test-001");
        write_continuation_jsonl(&newer_jsonl);

        // Backdate the older transcript so it's clearly the elder.
        // Leave the newer one at "now" so it sorts first in the dirs
        // list. The merge must still prefer the older one's queue id.
        backdate(&older_jsonl, 600);

        let dirs = vec![newer.clone(), older.clone()];
        let records = collect_agent_records_merged(&dirs, SystemTime::now(), 120);

        assert_eq!(records.len(), 1, "{:?}", records);
        let r = &records[0];
        assert_eq!(r.agent_id, id);
        assert_eq!(
            r.queue_id.as_deref(),
            Some("q-test-001"),
            "marker from older dir must survive merge",
        );
    }

    #[test]
    fn merge_returns_none_when_no_dir_has_marker() {
        // Sanity: the merge does not fabricate a queue id. An agent
        // whose only transcript is a continuation frame (no marker
        // anywhere) ends up with queue_id=None.
        let tmp = tempfile::tempdir().expect("tempdir");
        let (older, newer) = build_two_session_dirs(tmp.path());

        let id = "lonely1";
        // Only present in the NEW session dir, no marker.
        write_continuation_jsonl(&newer.join(format!("agent-{}.jsonl", id)));

        let dirs = vec![newer.clone(), older.clone()];
        let records = collect_agent_records_merged(&dirs, SystemTime::now(), 120);

        assert_eq!(records.len(), 1);
        let r = &records[0];
        assert_eq!(r.agent_id, id);
        assert!(
            r.queue_id.is_none(),
            "merge must not fabricate a queue id",
        );
    }

    #[test]
    fn merge_dedupes_and_sorts_by_agent_id() {
        // Two distinct agents appearing across two dirs — each should
        // appear once in the output, sorted lexicographically.
        let tmp = tempfile::tempdir().expect("tempdir");
        let (older, newer) = build_two_session_dirs(tmp.path());

        write_marker_jsonl(&older.join("agent-zzz.jsonl"), "q-z");
        write_marker_jsonl(&newer.join("agent-aaa.jsonl"), "q-a");
        // Same agent in both dirs (older has marker, newer doesn't).
        write_marker_jsonl(&older.join("agent-mmm.jsonl"), "q-m");
        write_continuation_jsonl(&newer.join("agent-mmm.jsonl"));

        let dirs = vec![newer, older];
        let records = collect_agent_records_merged(&dirs, SystemTime::now(), 120);
        let ids: Vec<&str> = records.iter().map(|r| r.agent_id.as_str()).collect();
        assert_eq!(ids, vec!["aaa", "mmm", "zzz"]);
        assert_eq!(
            records[1].queue_id.as_deref(),
            Some("q-m"),
            "marker must propagate through merge for shared agent_id",
        );
    }

    #[test]
    fn merge_breaks_alive_tie_by_freshest_mtime() {
        // Both records have queue ids (so rule 1 ties) and both are
        // alive within the window (so rule 2 ties). The fresher mtime
        // should win.
        let tmp = tempfile::tempdir().expect("tempdir");
        let (older, newer) = build_two_session_dirs(tmp.path());

        let id = "tied";
        let older_jsonl = older.join(format!("agent-{}.jsonl", id));
        let newer_jsonl = newer.join(format!("agent-{}.jsonl", id));
        write_marker_jsonl(&older_jsonl, "q-old");
        write_marker_jsonl(&newer_jsonl, "q-new");
        // Backdate older but keep within max_age so both stay alive.
        backdate(&older_jsonl, 30);

        let dirs = vec![older.clone(), newer.clone()];
        let records = collect_agent_records_merged(&dirs, SystemTime::now(), 120);
        assert_eq!(records.len(), 1);
        assert_eq!(
            records[0].queue_id.as_deref(),
            Some("q-new"),
            "freshest record wins when queue_id + alive tie",
        );
    }

    #[test]
    fn merge_prefers_alive_record_over_stale_when_queue_ids_tie() {
        // Both have queue ids; one is alive, the other is stale. The
        // alive one wins regardless of the (un)freshness comparison.
        let tmp = tempfile::tempdir().expect("tempdir");
        let (older, newer) = build_two_session_dirs(tmp.path());

        let id = "mixed";
        let older_jsonl = older.join(format!("agent-{}.jsonl", id));
        let newer_jsonl = newer.join(format!("agent-{}.jsonl", id));
        write_marker_jsonl(&older_jsonl, "q-stale-but-marker");
        write_marker_jsonl(&newer_jsonl, "q-alive");
        // Push older clearly past the max_age window.
        backdate(&older_jsonl, 600);

        let dirs = vec![older.clone(), newer.clone()];
        let records = collect_agent_records_merged(&dirs, SystemTime::now(), 120);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].queue_id.as_deref(), Some("q-alive"));
        assert!(records[0].alive);
    }

    // --- find_active_subagents_dirs_in: freshness window + ordering ---

    #[test]
    fn find_dirs_returns_only_dirs_within_window() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (older, newer) = build_two_session_dirs(tmp.path());
        // Put one fresh file in each so they both pass the
        // "directory has content" test. Then backdate older WAY past
        // the freshness window.
        write_continuation_jsonl(&older.join("agent-a.jsonl"));
        write_continuation_jsonl(&newer.join("agent-b.jsonl"));
        backdate(&older.join("agent-a.jsonl"), 48 * 60 * 60);
        // The dir mtime is what find_active_subagents_dirs_in actually
        // compares — make sure that's also stale.
        backdate(&older, 48 * 60 * 60);

        let dirs = find_active_subagents_dirs_in(
            tmp.path(),
            SystemTime::now(),
            24 * 60 * 60,
        );
        // Only the newer dir should appear.
        assert_eq!(dirs.len(), 1, "{:?}", dirs);
        assert_eq!(dirs[0], newer);
    }

    #[test]
    fn find_dirs_returns_all_when_all_fresh() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (older, newer) = build_two_session_dirs(tmp.path());
        write_continuation_jsonl(&older.join("agent-a.jsonl"));
        write_continuation_jsonl(&newer.join("agent-b.jsonl"));
        // Both within the window.
        let dirs = find_active_subagents_dirs_in(
            tmp.path(),
            SystemTime::now(),
            24 * 60 * 60,
        );
        assert_eq!(dirs.len(), 2);
        // Sorted newest-first: `newer` was written second, so its
        // newest-file mtime should be greater than `older`'s. (We rely
        // on the test runner not stalling between writes — if both
        // mtimes happen to be equal at filesystem resolution, the
        // ordering is unspecified but the SET equality below still
        // holds.)
        let set: std::collections::HashSet<&PathBuf> = dirs.iter().collect();
        assert!(set.contains(&older));
        assert!(set.contains(&newer));
    }

    #[test]
    fn find_dirs_missing_projects_root_is_empty() {
        let dirs = find_active_subagents_dirs_in(
            Path::new("/nonexistent/projects/root"),
            SystemTime::now(),
            DEFAULT_SUBAGENTS_DIR_FRESHNESS_SECS,
        );
        assert!(dirs.is_empty());
    }

    // --- should_replace ---

    fn rec(id: &str, qid: Option<&str>, alive: bool, age: Option<u64>) -> AgentRecord {
        AgentRecord {
            agent_id: id.to_string(),
            queue_id: qid.map(|s| s.to_string()),
            alive,
            jsonl_age_seconds: age,
        }
    }

    #[test]
    fn should_replace_prefers_some_queue_id() {
        let existing = rec("x", None, true, Some(1));
        let candidate = rec("x", Some("q-1"), false, Some(999));
        assert!(should_replace(&existing, &candidate));
    }

    #[test]
    fn should_replace_keeps_some_queue_id() {
        let existing = rec("x", Some("q-1"), false, Some(999));
        let candidate = rec("x", None, true, Some(1));
        assert!(!should_replace(&existing, &candidate));
    }

    #[test]
    fn should_replace_prefers_alive_when_qid_ties() {
        let existing = rec("x", Some("q-1"), false, Some(1));
        let candidate = rec("x", Some("q-1"), true, Some(999));
        assert!(should_replace(&existing, &candidate));
    }

    #[test]
    fn should_replace_prefers_fresher_when_qid_and_alive_tie() {
        let existing = rec("x", Some("q-1"), true, Some(50));
        let candidate = rec("x", Some("q-1"), true, Some(10));
        assert!(should_replace(&existing, &candidate));
        // And NOT the other way around.
        assert!(!should_replace(&candidate, &existing));
    }

    #[test]
    fn should_replace_does_not_replace_on_equal_fields() {
        let existing = rec("x", Some("q-1"), true, Some(10));
        let candidate = rec("x", Some("q-1"), true, Some(10));
        assert!(!should_replace(&existing, &candidate));
    }
}
