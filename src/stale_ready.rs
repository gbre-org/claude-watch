//! `claude-watch stale-ready-check` — emit a claude-event when one or more
//! session-task queue items have been ready+pending past a threshold.
//!
//! Runs from cron (every 5 minutes by default). Reads queue state via the
//! `session-task` CLI (`queue ready --json` for the qualifying set,
//! `queue list --json` for state-file pruning), keeps a tiny single-emit
//! state file per qid, and emits ONE aggregate claude-event per tick when
//! any items qualify.
//!
//! State file: `<state-dir>/stale-ready-state.json`
//!   { "<queue-id>": "<emit-iso8601>", ... }
//!
//! Single-emit per queue id: an item that has already triggered an event
//! stays in the state file until it leaves the queue entirely (drops out
//! of `queue list`). At that point the entry is pruned. The longer-tail
//! "still stuck after 30 min" case is intentionally NOT covered here —
//! external alerting (Prometheus WorkQueueStuck / WorkQueueBacklog) owns
//! that escalation tier.
//!
//! Aggregation: when N items qualify in one tick, ONE event is emitted
//! with up to TOP_N ids in the message body and the full id list in
//! `data.all_ids`.
//!
//! Design parity with the gomorrah-host cron script
//! (`cron-queue-stale-ready`) so that consumers (main loop event
//! routing, obligation registration) see the same shape regardless of
//! emitter. Tag: `queue-stale-ready`. Source: `claude-watch`.
//! Source-name: `stale-ready-check`. Priority: `low`.
//!
//! Failure mode: default-open. Any error (session-task missing, queue
//! file corrupt, state write failure) is logged to stderr and the
//! command exits non-zero, but does NOT panic or partially-write state.
//! Cron will retry on the next tick.

use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

/// Default threshold: items ready+pending for this many minutes qualify.
pub const DEFAULT_THRESHOLD_MIN: u64 = 6;

/// Max number of ids to list inline in the human-readable `message`
/// field. The full id list always lives in `data.all_ids`.
pub const TOP_N: usize = 3;

/// Tag for the emitted claude-event.
pub const EVENT_TAG: &str = "queue-stale-ready";
/// Source for the emitted claude-event (matches the active-agents writer
/// convention — "this came from claude-watch").
pub const EVENT_SOURCE: &str = "claude-watch";
/// Source-name disambiguator within `source=claude-watch`.
pub const EVENT_SOURCE_NAME: &str = "stale-ready-check";

/// Minimal subset of a queue item we need: id, status, created_at,
/// summary. The session-task `--json` output carries more fields; we
/// ignore the rest with `#[serde(default)]` + deny-unknown-fields-off.
#[derive(Debug, Clone, Deserialize)]
pub struct QueueItem {
    pub id: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub created_at: String,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
}

/// One item that qualifies as stale-ready. Used internally to build the
/// event payload.
#[derive(Debug, Clone)]
pub struct Qualifying {
    pub id: String,
    pub summary: String,
    pub age_min: u64,
}

/// Default state dir. Honors `CLAUDE_WATCH_STATE_DIR` so tests + alt
/// deployments can redirect. Falls back to `/var/lib/claude-watch` —
/// matches the in-container default for the `active-agents.json` writer.
pub fn default_state_dir() -> PathBuf {
    if let Ok(d) = std::env::var("CLAUDE_WATCH_STATE_DIR") {
        if !d.is_empty() {
            return PathBuf::from(d);
        }
    }
    PathBuf::from("/var/lib/claude-watch")
}

/// Parse a queue `created_at` ISO 8601 / RFC 3339 timestamp into epoch
/// seconds. Returns None on failure (the caller then skips the item;
/// don't kill the whole tick over one bad row).
pub fn parse_iso_epoch_secs(ts: &str) -> Option<i64> {
    let ts = ts.trim();
    if ts.is_empty() {
        return None;
    }
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(ts) {
        return Some(dt.timestamp());
    }
    // Accept naive timestamps too (`YYYY-MM-DDTHH:MM:SS[.fff]`) — assume UTC.
    if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(ts, "%Y-%m-%dT%H:%M:%S%.f") {
        return Some(dt.and_utc().timestamp());
    }
    if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(ts, "%Y-%m-%dT%H:%M:%S") {
        return Some(dt.and_utc().timestamp());
    }
    None
}

/// State-file payload: queue-id -> ISO-8601 emit timestamp.
pub type State = std::collections::BTreeMap<String, String>;

/// Load state from `path`. Returns empty state on missing-file or any
/// parse error (logged to stderr but non-fatal — single-emit becomes
/// "emit one extra time after corruption", which is acceptable).
pub fn load_state(path: &Path) -> State {
    let raw = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return State::new(),
        Err(e) => {
            eprintln!(
                "stale-ready-check: state load failed ({}); starting fresh",
                e
            );
            return State::new();
        }
    };
    match serde_json::from_str::<State>(&raw) {
        Ok(s) => s,
        Err(e) => {
            eprintln!(
                "stale-ready-check: state parse failed ({}); starting fresh",
                e
            );
            State::new()
        }
    }
}

/// Atomic save: write to `<path>.tmp` + rename. Creates parent dir on
/// demand. Returns the underlying io::Error on failure so the caller
/// can decide whether to exit non-zero.
pub fn save_state(path: &Path, state: &State) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let body = serde_json::to_string_pretty(state).unwrap_or_else(|_| "{}".to_string());
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, body + "\n")?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Pure: compute the qualifying set given the ready list, the existing
/// state (already-emitted ids), `now` (epoch secs), and the threshold
/// (in seconds).
pub fn compute_qualifying(
    ready: &[QueueItem],
    already_emitted: &State,
    now_epoch_secs: i64,
    threshold_secs: i64,
) -> Vec<Qualifying> {
    let mut out: Vec<Qualifying> = Vec::new();
    for it in ready {
        if it.status != "pending" {
            continue;
        }
        if it.id.is_empty() {
            continue;
        }
        if already_emitted.contains_key(&it.id) {
            // Single-emit rule: skip ids we've already fired for.
            continue;
        }
        let created = match parse_iso_epoch_secs(&it.created_at) {
            Some(t) => t,
            None => continue,
        };
        let age_secs = now_epoch_secs - created;
        if age_secs < threshold_secs {
            continue;
        }
        let age_min = (age_secs / 60).max(0) as u64;
        let summary = it
            .summary
            .clone()
            .or_else(|| it.description.clone())
            .unwrap_or_else(|| "(no summary)".to_string());
        out.push(Qualifying {
            id: it.id.clone(),
            summary,
            age_min,
        });
    }
    // Sort oldest first.
    out.sort_by(|a, b| b.age_min.cmp(&a.age_min));
    out
}

/// Pure: drop state entries whose qid is no longer present in the full
/// queue list. Returns a new pruned state.
pub fn prune_state(state: &State, current_ids: &std::collections::HashSet<String>) -> State {
    state
        .iter()
        .filter(|(qid, _)| current_ids.contains(*qid))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

/// Build the human-readable `message` string for the event payload.
pub fn build_message(qualifying: &[Qualifying]) -> String {
    if qualifying.is_empty() {
        return String::new();
    }
    let n = qualifying.len();
    let oldest = qualifying[0].age_min;
    let plural = if n > 1 { "items" } else { "item" };
    let top_ids: Vec<String> = qualifying
        .iter()
        .take(TOP_N)
        .map(|q| q.id.clone())
        .collect();
    format!(
        "{} queue {} stale-ready (oldest {} min): {}",
        n,
        plural,
        oldest,
        top_ids.join(", ")
    )
}

/// Build the full event JSON body. Public for testability — production
/// callers should use `emit()` (writes the file).
pub fn build_event_json(
    qualifying: &[Qualifying],
    threshold_min: u64,
    now_iso: &str,
    hostname: &str,
    user: &str,
    pid: u32,
) -> serde_json::Value {
    let n = qualifying.len();
    let oldest = qualifying.first().map(|q| q.age_min).unwrap_or(0);
    let top: Vec<&Qualifying> = qualifying.iter().take(TOP_N).collect();
    let top_ids: Vec<String> = top.iter().map(|q| q.id.clone()).collect();
    let top_summaries: Vec<String> = top.iter().map(|q| q.summary.clone()).collect();
    let all_ids: Vec<String> = qualifying.iter().map(|q| q.id.clone()).collect();
    let now_epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    serde_json::json!({
        "timestamp": now_epoch,
        "timestamp_iso": now_iso,
        "hostname": hostname,
        "source": EVENT_SOURCE,
        "source_name": EVENT_SOURCE_NAME,
        "tag": EVENT_TAG,
        "priority": "low",
        "message": build_message(qualifying),
        "data": {
            "qualifying_count": n,
            "oldest_age_min": oldest,
            "threshold_min": threshold_min,
            "top_ids": top_ids,
            "top_summaries": top_summaries,
            "all_ids": all_ids,
        },
        "pid": pid,
        "user": user,
    })
}

/// Resolve the claude-events queue dir. Honours `CLAUDE_EVENT_QUEUE`
/// and legacy `CRON_EVENT_QUEUE`, falls back to `~/claude-events/`.
/// Matches the convention used by `event_bus::queue_dir`.
fn event_queue_dir() -> PathBuf {
    if let Ok(p) = std::env::var("CLAUDE_EVENT_QUEUE") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    if let Ok(p) = std::env::var("CRON_EVENT_QUEUE") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home).join("claude-events")
}

/// Write an event file atomically into the queue dir.
fn write_event_file(body: &serde_json::Value) -> std::io::Result<PathBuf> {
    let dir = event_queue_dir();
    std::fs::create_dir_all(&dir)?;
    let ts_ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let final_name = format!("{}_{}.json", ts_ns, EVENT_TAG);
    let final_path = dir.join(&final_name);
    let tmp_path = dir.join(format!(".{}.tmp", final_name));
    let body_str = serde_json::to_string_pretty(body)
        .unwrap_or_else(|_| "{}".to_string());
    std::fs::write(&tmp_path, body_str.as_bytes())?;
    std::fs::rename(&tmp_path, &final_path)?;
    Ok(final_path)
}

/// Locate the `session-task` CLI on PATH (with `~/bin/session-task` as a
/// fallback to match `workload.rs::find_session_task_cli`). Honours the
/// `SESSION_TASK_CLI` env var for test injection.
fn find_session_task_cli() -> Option<PathBuf> {
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

/// Shell out to `session-task <subcommand> [args] --json` and parse the
/// result as a `Vec<QueueItem>`. Returns the parsed list on success or
/// a stringly-typed error.
fn run_session_task_json(
    cli: &Path,
    args: &[&str],
    timeout_secs: u64,
) -> Result<Vec<QueueItem>, String> {
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
        Ok(Err(e)) => return Err(format!("session-task exec failed: {}", e)),
        Err(_) => return Err(format!("session-task timed out after {}s", timeout_secs)),
    };
    if !out.status.success() {
        return Err(format!(
            "session-task exited non-zero (rc={:?}): stderr={}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    serde_json::from_str(trimmed).map_err(|e| {
        format!(
            "session-task JSON parse failed: {} (raw head: {:.200})",
            e, trimmed
        )
    })
}

/// CLI entry point. Returns process exit code.
pub fn cmd_stale_ready_check(
    threshold_min: u64,
    state_dir: Option<&str>,
    dry_run: bool,
) -> i32 {
    let cli = match find_session_task_cli() {
        Some(c) => c,
        None => {
            eprintln!("stale-ready-check: session-task CLI not found on PATH; nothing to do");
            // Exit 0 — running on a host without session-task is a config
            // choice, not an error. Matches the active-agents writer's
            // tolerance for missing inputs.
            return 0;
        }
    };

    // Pull the full queue list (used to prune state). Hard-fail on this
    // one: if we can't read the queue at all, we'd false-positive-emit
    // every minute, which is worse than a silent gap.
    let all_items = match run_session_task_json(&cli, &["queue", "list", "--all", "--json"], 10) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("stale-ready-check: queue list failed: {}", e);
            return 1;
        }
    };
    let current_ids: std::collections::HashSet<String> =
        all_items.iter().map(|it| it.id.clone()).collect();

    // The ready set: pending items with no blockers.
    let ready = match run_session_task_json(&cli, &["queue", "ready", "--json"], 10) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("stale-ready-check: queue ready failed: {}", e);
            return 1;
        }
    };

    let state_dir = state_dir
        .map(PathBuf::from)
        .unwrap_or_else(default_state_dir);
    let state_file = state_dir.join("stale-ready-state.json");
    let state = load_state(&state_file);

    // Prune state to current queue.
    let pruned = prune_state(&state, &current_ids);

    let now_epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let threshold_secs = (threshold_min as i64).saturating_mul(60);

    let qualifying = compute_qualifying(&ready, &pruned, now_epoch, threshold_secs);

    // Always write the pruned state (catches finished-item cleanup even
    // when nothing qualifies).
    let mut next_state = pruned.clone();

    if qualifying.is_empty() {
        if next_state != state {
            if let Err(e) = save_state(&state_file, &next_state) {
                eprintln!(
                    "stale-ready-check: state save failed ({}); continuing",
                    e
                );
                return 1;
            }
        }
        // Silent exit when no qualifying items.
        return 0;
    }

    // Build event body.
    let now_iso = chrono::Local::now().to_rfc3339();
    let hostname = hostname_string();
    let user = std::env::var("USER").unwrap_or_default();
    let pid = std::process::id();
    let event = build_event_json(
        &qualifying,
        threshold_min,
        &now_iso,
        &hostname,
        &user,
        pid,
    );

    if dry_run {
        // Print event body to stdout, leave state untouched.
        println!(
            "{}",
            serde_json::to_string_pretty(&event).unwrap_or_else(|_| "{}".to_string())
        );
        return 0;
    }

    // Emit the event file.
    match write_event_file(&event) {
        Ok(p) => {
            println!(
                "{}: emitted event for {} qualifying item(s) -> {}",
                EVENT_TAG,
                qualifying.len(),
                p.display()
            );
        }
        Err(e) => {
            eprintln!("stale-ready-check: event write failed: {}", e);
            return 1;
        }
    }

    // Record emit timestamps for each qualifying id so we don't re-emit
    // on the next tick.
    let emit_ts = now_iso.clone();
    for q in &qualifying {
        next_state.insert(q.id.clone(), emit_ts.clone());
    }
    if let Err(e) = save_state(&state_file, &next_state) {
        eprintln!(
            "stale-ready-check: state save failed after emit ({}); next tick will re-emit",
            e
        );
        return 1;
    }
    0
}

/// Cheap, no-deps hostname lookup. Mirrors `event_bus::hostname_string`.
fn hostname_string() -> String {
    if let Ok(s) = std::fs::read_to_string("/etc/hostname") {
        let trimmed = s.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    if let Ok(s) = std::env::var("HOSTNAME") {
        if !s.is_empty() {
            return s;
        }
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, Utc};

    fn iso_n_min_ago(min: i64) -> String {
        (Utc::now() - Duration::minutes(min)).to_rfc3339()
    }

    fn item(id: &str, status: &str, created_at: &str, summary: Option<&str>) -> QueueItem {
        QueueItem {
            id: id.to_string(),
            status: status.to_string(),
            created_at: created_at.to_string(),
            summary: summary.map(|s| s.to_string()),
            description: None,
        }
    }

    #[test]
    fn parse_iso_epoch_secs_handles_rfc3339() {
        let ts = "2026-05-19T12:34:56+00:00";
        let got = parse_iso_epoch_secs(ts).unwrap();
        // 2026-05-19T12:34:56Z -> 1779539696
        assert!(got > 1_700_000_000); // sanity bound
    }

    #[test]
    fn parse_iso_epoch_secs_handles_naive() {
        let ts = "2026-05-19T12:34:56";
        let got = parse_iso_epoch_secs(ts);
        assert!(got.is_some());
    }

    #[test]
    fn parse_iso_epoch_secs_rejects_garbage() {
        assert!(parse_iso_epoch_secs("not a date").is_none());
        assert!(parse_iso_epoch_secs("").is_none());
    }

    #[test]
    fn compute_qualifying_skips_non_pending() {
        let ts = iso_n_min_ago(30);
        let ready = vec![item("q-1", "running", &ts, Some("s1"))];
        let now = Utc::now().timestamp();
        let q = compute_qualifying(&ready, &State::new(), now, 360);
        assert!(q.is_empty());
    }

    #[test]
    fn compute_qualifying_skips_under_threshold() {
        // 3 min old, threshold 6 min -> skip.
        let ts = iso_n_min_ago(3);
        let ready = vec![item("q-1", "pending", &ts, Some("s1"))];
        let now = Utc::now().timestamp();
        let q = compute_qualifying(&ready, &State::new(), now, 6 * 60);
        assert!(q.is_empty());
    }

    #[test]
    fn compute_qualifying_picks_over_threshold() {
        let ts = iso_n_min_ago(20);
        let ready = vec![item("q-old", "pending", &ts, Some("summary text"))];
        let now = Utc::now().timestamp();
        let q = compute_qualifying(&ready, &State::new(), now, 6 * 60);
        assert_eq!(q.len(), 1);
        assert_eq!(q[0].id, "q-old");
        assert!(q[0].age_min >= 19);
        assert_eq!(q[0].summary, "summary text");
    }

    #[test]
    fn compute_qualifying_respects_single_emit_state() {
        let ts = iso_n_min_ago(20);
        let ready = vec![item("q-already", "pending", &ts, None)];
        let mut state = State::new();
        state.insert("q-already".to_string(), "2026-05-19T00:00:00Z".to_string());
        let now = Utc::now().timestamp();
        let q = compute_qualifying(&ready, &state, now, 6 * 60);
        assert!(q.is_empty());
    }

    #[test]
    fn compute_qualifying_sorts_oldest_first() {
        let ready = vec![
            item("q-younger", "pending", &iso_n_min_ago(10), Some("y")),
            item("q-older", "pending", &iso_n_min_ago(30), Some("o")),
            item("q-mid", "pending", &iso_n_min_ago(20), Some("m")),
        ];
        let now = Utc::now().timestamp();
        let q = compute_qualifying(&ready, &State::new(), now, 6 * 60);
        assert_eq!(q.len(), 3);
        assert_eq!(q[0].id, "q-older");
        assert_eq!(q[1].id, "q-mid");
        assert_eq!(q[2].id, "q-younger");
    }

    #[test]
    fn compute_qualifying_handles_missing_summary() {
        let ts = iso_n_min_ago(30);
        let ready = vec![item("q-naked", "pending", &ts, None)];
        let now = Utc::now().timestamp();
        let q = compute_qualifying(&ready, &State::new(), now, 6 * 60);
        assert_eq!(q.len(), 1);
        assert_eq!(q[0].summary, "(no summary)");
    }

    #[test]
    fn prune_state_drops_missing_ids() {
        let mut state = State::new();
        state.insert("q-still-here".to_string(), "ts1".to_string());
        state.insert("q-gone".to_string(), "ts2".to_string());
        let mut current = std::collections::HashSet::new();
        current.insert("q-still-here".to_string());
        let pruned = prune_state(&state, &current);
        assert_eq!(pruned.len(), 1);
        assert!(pruned.contains_key("q-still-here"));
        assert!(!pruned.contains_key("q-gone"));
    }

    #[test]
    fn prune_state_keeps_all_when_all_present() {
        let mut state = State::new();
        state.insert("q-a".to_string(), "t1".to_string());
        state.insert("q-b".to_string(), "t2".to_string());
        let current: std::collections::HashSet<String> =
            ["q-a".to_string(), "q-b".to_string()].into_iter().collect();
        let pruned = prune_state(&state, &current);
        assert_eq!(pruned.len(), 2);
    }

    #[test]
    fn build_message_singular() {
        let q = vec![Qualifying {
            id: "q-1".to_string(),
            summary: "s".to_string(),
            age_min: 7,
        }];
        let msg = build_message(&q);
        assert!(msg.contains("1 queue item stale-ready"));
        assert!(msg.contains("oldest 7 min"));
        assert!(msg.contains("q-1"));
    }

    #[test]
    fn build_message_plural_and_top_n() {
        let q: Vec<Qualifying> = (0..5)
            .map(|i| Qualifying {
                id: format!("q-{}", i),
                summary: format!("s{}", i),
                age_min: (10 - i) as u64,
            })
            .collect();
        let msg = build_message(&q);
        assert!(msg.contains("5 queue items"));
        // Only top-N ids in inline message.
        assert!(msg.contains("q-0"));
        assert!(msg.contains("q-1"));
        assert!(msg.contains("q-2"));
        assert!(!msg.contains("q-3"));
        assert!(!msg.contains("q-4"));
    }

    #[test]
    fn build_message_empty_string_on_empty_input() {
        assert_eq!(build_message(&[]), "");
    }

    #[test]
    fn build_event_json_has_required_fields() {
        let q = vec![
            Qualifying {
                id: "q-a".to_string(),
                summary: "summary-a".to_string(),
                age_min: 30,
            },
            Qualifying {
                id: "q-b".to_string(),
                summary: "summary-b".to_string(),
                age_min: 10,
            },
        ];
        let v = build_event_json(&q, 6, "2026-05-19T01:30:00Z", "host", "user", 1234);
        assert_eq!(v["tag"], EVENT_TAG);
        assert_eq!(v["source"], EVENT_SOURCE);
        assert_eq!(v["source_name"], EVENT_SOURCE_NAME);
        assert_eq!(v["priority"], "low");
        assert_eq!(v["hostname"], "host");
        assert_eq!(v["user"], "user");
        assert_eq!(v["pid"], 1234);
        assert_eq!(v["data"]["qualifying_count"], 2);
        assert_eq!(v["data"]["oldest_age_min"], 30);
        assert_eq!(v["data"]["threshold_min"], 6);
        assert_eq!(
            v["data"]["all_ids"],
            serde_json::json!(["q-a", "q-b"])
        );
        assert_eq!(v["data"]["top_ids"], serde_json::json!(["q-a", "q-b"]));
    }

    #[test]
    fn save_and_load_state_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("stale-ready-state.json");
        let mut state = State::new();
        state.insert("q-1".to_string(), "2026-05-19T01:00:00Z".to_string());
        state.insert("q-2".to_string(), "2026-05-19T01:05:00Z".to_string());
        save_state(&path, &state).unwrap();
        let loaded = load_state(&path);
        assert_eq!(loaded, state);
    }

    #[test]
    fn load_state_missing_file_returns_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("does-not-exist.json");
        let loaded = load_state(&path);
        assert!(loaded.is_empty());
    }

    #[test]
    fn load_state_garbage_file_returns_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("garbage.json");
        std::fs::write(&path, "this is not json").unwrap();
        let loaded = load_state(&path);
        assert!(loaded.is_empty());
    }

    #[test]
    fn save_state_writes_atomically() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nested").join("dir").join("state.json");
        let mut state = State::new();
        state.insert("q-x".to_string(), "ts".to_string());
        save_state(&path, &state).unwrap();
        assert!(path.exists());
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("q-x"));
    }
}
