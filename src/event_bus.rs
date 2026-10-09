//! Claude-event bus emitter.
//!
//! Writes structured JSON events into `~/claude-events/` so that
//! `claude-event-watch` surfaces them to the main loop. This is an
//! ADDITIVE third alert sink alongside the push-notification path
//! (`pingme`) and the tmux-inject prompt — those paths must keep firing
//! whether or not event emission succeeds.
//!
//! Field shape mirrors what `~/bin/claude-event` (Python helper) writes
//! so the consumer (`claude-event-watch`) needs no special-case logic.
//! The full event JSON is:
//!
//! ```json
//! {
//!   "timestamp": <unix float>,
//!   "timestamp_iso": "<RFC 3339 local>",
//!   "hostname": "...",
//!   "source": "claude-watch",
//!   "source_name": "claude-watch",
//!   "tag": "claude-watch-alert",
//!   "priority": "low|normal|high|urgent",
//!   "message": "<full human-readable, same as push-notification body>",
//!   "data": {
//!       "alert_type":        "<heartbeat-stale|prolonged-thinking|...>",
//!       "stuck_reason":      "<short human-readable>",
//!       "stale_minutes":     <int|null>,
//!       "affected_watchers": ["<name>", ...],
//!       "severity":          "<low|medium|high|critical>"
//!   },
//!   "pid":  <int>,
//!   "user": "..."
//! }
//! ```
//!
//! Note: `source` is **`claude-watch`**, which is outside the canonical
//! source enum used by the Python helper (`cron|alertmanager|queue|...`).
//! `claude-event-watch` itself doesn't validate `source` — it dispatches
//! by `tag`. The new tag is `claude-watch-alert`.
//!
//! Writes are atomic (tmp file in same dir + rename). Filename:
//! `<unix_ns>_claude-watch-alert.json` (matches Python helper convention).
//!
//! On any error the function logs and returns — never panics, never
//! propagates failure to the caller. Same default-open principle as the
//! obligations PreToolUse hook: a broken alert sink must not blackhole
//! the push-notification path or tmux-inject.

use serde::Serialize;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

/// Severity levels that map cleanly onto push-notification priority and
/// downstream triage decisions in the routing table.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Low,
    Medium,
    High,
    Critical,
}

impl Severity {
    /// Map severity → claude-event `priority` field. Mirrors what the
    /// Python `claude-event` helper accepts (`low|normal|high|urgent`).
    pub fn as_priority(self) -> &'static str {
        match self {
            Severity::Low => "low",
            Severity::Medium => "normal",
            Severity::High => "high",
            Severity::Critical => "urgent",
        }
    }
}

/// One claude-watch alert. Caller fills the fields it knows; missing
/// optional fields default to None / empty.
#[derive(Debug, Clone)]
pub struct ClaudeWatchAlert<'a> {
    /// Discriminator string matching the codebase alert paths:
    ///   `heartbeat-stale`, `prolonged-thinking`, `watcher-down`,
    ///   `fresh-clear-stuck`, `claude-crashed`, `auto-update-failed`,
    ///   `auto-update-complete`, `reauth-needed`, `credits-exhausted`,
    ///   `wedged-pane`, `permission-prompt`, `permission-prompt-denied`,
    ///   `permission-prompt-deny-failed`.
    ///
    /// NOTE on `credits-exhausted`: the session is authenticated and healthy —
    /// it is out of usage credits for the model it is running, so every turn
    /// fails. It names a session that needs its MODEL changed, and it is also
    /// how the daemon reports that it changed one itself (demote-only; the
    /// promotion back after a credit reset is always a human's).
    ///
    /// NOTE on the `permission-prompt*` family: these name a BLOCKED TOOL
    /// CALL, not an unhealthy session. `permission-prompt` says a tool call is
    /// waiting on an approval nobody has given — the session is alive and must
    /// NOT be respawned (mistaking that state for a dead agent is the incident
    /// the monitor exists for). `permission-prompt-denied` says claude-watch
    /// declined it: the call was REJECTED and whatever depended on it did not
    /// run.
    ///
    /// NOTE on `heartbeat-stale`: the CONDITION it names is now "the main loop
    /// has not acked any event in `[ack] stale_minutes`" — see
    /// `policy::last_ack_timestamp_age`. The string itself is deliberately NOT
    /// renamed: it is a wire value that out-of-tree consumers (alert-gate
    /// hooks, host routing tables) match on, and renaming it would silently
    /// stop those matching with no error anywhere. Read it as the alert's ID,
    /// not as a description of the mechanism.
    pub alert_type: &'a str,
    /// Short human-readable reason. For the liveness path this is the same
    /// `stuck_reason` already threaded through `policy.rs`.
    pub stuck_reason: &'a str,
    /// Liveness staleness in minutes (only meaningful for `heartbeat-stale`;
    /// None elsewhere).
    pub stale_minutes: Option<u64>,
    /// Names of watchers known to be missing (only meaningful for
    /// `watcher-down`; empty elsewhere).
    pub affected_watchers: Vec<String>,
    /// Severity tier driving push-notification priority + dispatch routing.
    pub severity: Severity,
    /// Full human-readable message, byte-for-byte the same string sent
    /// to the push-notification shim so log/event/push all agree.
    pub message: &'a str,
}

/// Build the JSON event body. Public for testability — production
/// callers should use `emit()` which also performs the atomic write.
pub fn build_event_json(alert: &ClaudeWatchAlert<'_>) -> serde_json::Value {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    let now_iso = chrono::Local::now().to_rfc3339();
    let hostname = hostname_string();
    let user = std::env::var("USER").unwrap_or_default();
    let pid = std::process::id();

    let mut event = serde_json::json!({
        "timestamp": now,
        "timestamp_iso": now_iso,
        "hostname": hostname,
        "source": "claude-watch",
        "source_name": "claude-watch",
        "tag": "claude-watch-alert",
        "priority": alert.severity.as_priority(),
        "message": alert.message,
        "data": {
            "alert_type": alert.alert_type,
            "stuck_reason": alert.stuck_reason,
            "stale_minutes": alert.stale_minutes,
            "affected_watchers": alert.affected_watchers,
            "severity": alert.severity,
        },
        "pid": pid,
        "user": user,
    });
    // Producer-stamped routing tier (rung 2 in the classifier precedence).
    // The alert-path watcher-down emission ships tag="claude-watch-alert" with
    // data.alert_type="watcher-down"; without an explicit tier it routes
    // through the claude-watch-alert NON-FATAL ambient row, so a down watcher
    // surfaces only as passive context and a busy main loop never relaunches
    // it (comms watcher down ~4h, incident 2026-08-21). claude-event-watch
    // forwards data.tier to `event-ack ingest --tier`, so stamping it here
    // routes watcher-down ACTIONABLE. Other alert_types keep their existing
    // (conditional-fatal / ambient) classification.
    //
    // `credits-exhausted` is stamped for the same reason. It names a loop that
    // cannot produce a turn, and the demotion flavour of it names a model
    // change only a human can undo after the credit reset — neither is passive
    // context. (Its phone notification does not depend on this: the demotion
    // push is sent directly and verified, precisely so that no routing or
    // suppression decision downstream can swallow it.)
    //
    // `autoresolve-no` is stamped because an auto-denied permission prompt is
    // a decision the main loop must make (retry with a rewritten command, or
    // ask the operator for permission). Left unstamped it classifies ambient
    // and the denial is silently lost.
    if matches!(
        alert.alert_type,
        "watcher-down" | "credits-exhausted" | "autoresolve-no"
    ) {
        event["data"]["tier"] = serde_json::Value::from("actionable");
    }
    event
}

/// Resolve the queue dir. Honors `CLAUDE_EVENT_QUEUE` (preferred) and
/// the legacy `CRON_EVENT_QUEUE`, matching the Python helper. Falls
/// back to `~/claude-events/`.
pub fn queue_dir() -> PathBuf {
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

/// Emit a claude-event JSON file into the queue dir. Default-open: any
/// I/O failure is logged at warn level and swallowed. The caller's
/// push-notification + tmux-inject paths must remain unaffected.
pub fn emit(alert: &ClaudeWatchAlert<'_>) {
    let dir = queue_dir();
    if let Err(e) = std::fs::create_dir_all(&dir) {
        tracing::warn!(error = %e, dir = %dir.display(),
            "claude-event emit: failed to create queue dir, skipping");
        return;
    }

    let event = build_event_json(alert);
    let body = match serde_json::to_string_pretty(&event) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "claude-event emit: failed to serialize event");
            return;
        }
    };

    // Atomic write: tmp file in same dir + rename. Filename matches
    // the Python helper's <ts_ns>_<safe_tag>.json convention so any
    // tooling that parses filenames stays compatible.
    let ts_ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let final_name = format!("{}_claude-watch-alert.json", ts_ns);
    let final_path = dir.join(&final_name);
    let tmp_path = dir.join(format!(".{}.tmp", final_name));

    if let Err(e) = std::fs::write(&tmp_path, body.as_bytes()) {
        tracing::warn!(error = %e, path = %tmp_path.display(),
            "claude-event emit: failed to write tmp file");
        return;
    }
    if let Err(e) = std::fs::rename(&tmp_path, &final_path) {
        tracing::warn!(error = %e, src = %tmp_path.display(), dst = %final_path.display(),
            "claude-event emit: failed to rename tmp into place");
        // best-effort cleanup
        let _ = std::fs::remove_file(&tmp_path);
        return;
    }

    tracing::info!(
        path = %final_path.display(),
        alert_type = %alert.alert_type,
        severity = ?alert.severity,
        "claude-event emitted"
    );
}

/// A generic daemon-emitted cadence event (`keepalive`,
/// `memory-reminder`). Unlike [`ClaudeWatchAlert`], these carry no
/// alert/stuck semantics — they are plain periodic signals the daemon
/// produces on its monotonic clock (see [`crate::cadence`]). The body
/// matches the same JSON shape the Python `claude-event` helper writes so
/// `claude-event-watch` dispatches purely on `tag`.
#[derive(Debug, Clone)]
pub struct CadenceEvent<'a> {
    /// Event tag (also the dispatch key). E.g. `keepalive`.
    pub tag: &'a str,
    /// `source` / `source_name` fields. `claude-watch` for these.
    pub source: &'a str,
    /// Human-readable message — for `keepalive` a short string, for
    /// `memory-reminder` the full action checklist.
    pub message: &'a str,
    /// Priority field (`low|normal|high|urgent`).
    pub priority: &'a str,
    /// Extra `data` fields merged into the event. Pass
    /// `serde_json::json!({})` for none.
    pub data: serde_json::Value,
}

/// Build the JSON body for a cadence event. Public for testability;
/// production callers use [`emit_cadence`].
pub fn build_cadence_json(ev: &CadenceEvent<'_>) -> serde_json::Value {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    let now_iso = chrono::Local::now().to_rfc3339();
    let hostname = hostname_string();
    let user = std::env::var("USER").unwrap_or_default();
    let pid = std::process::id();

    serde_json::json!({
        "timestamp": now,
        "timestamp_iso": now_iso,
        "hostname": hostname,
        "source": ev.source,
        "source_name": ev.source,
        "tag": ev.tag,
        "priority": ev.priority,
        "message": ev.message,
        "data": ev.data,
        "pid": pid,
        "user": user,
    })
}

/// The per-key ack command the main loop runs for EACH pending event after
/// handling a batch — the batch reflex `ack-batch` was disabled 2026-09-11
/// (#773). Each per-key ack stamps the liveness timestamp the
/// daemon reads, so it is both the gate-clear and the proof-of-life. Kept as
/// a const because three producers quote it: the keepalive event body, the
/// ack-stale recovery prompt, and `claude-event-watch`'s per-batch footer.
pub const ACK_COMMAND: &str = "event-ack ack \"<key>\" --action \"<what you did>\"";

/// Build the `data` body for a `keepalive` cadence event.
///
/// Carries the ONE command the main loop must run to clear it —
/// [`ACK_COMMAND`] — plus the quiet window that triggered the emission
/// and how old the last ack was. `claude-event-watch` surfaces every scalar
/// in `data` on the EVENT[...] line, so the loop is TOLD the ritual rather
/// than having to remember it.
///
/// `last_ack_age_secs` is `None` on a host with no ack state yet (fresh boot,
/// or a deployment without event-must-act) — the field is then omitted rather
/// than faked with a 0 that would read as "just acked".
pub fn keepalive_data(interval_secs: u64, last_ack_age_secs: Option<u64>) -> serde_json::Value {
    let mut data = serde_json::json!({
        "ack_command": ACK_COMMAND,
        "quiet_secs": interval_secs,
    });
    if let Some(age) = last_ack_age_secs {
        data["last_ack_age_secs"] = serde_json::json!(age);
    }
    data
}

/// Emit a cadence event JSON file into the queue dir. Default-open: any
/// I/O failure is logged at warn level and swallowed (a missed cadence
/// tick is harmless — the next interval re-fires).
pub fn emit_cadence(ev: &CadenceEvent<'_>) {
    let dir = queue_dir();
    if let Err(e) = std::fs::create_dir_all(&dir) {
        tracing::warn!(error = %e, dir = %dir.display(),
            "cadence emit: failed to create queue dir, skipping");
        return;
    }

    let event = build_cadence_json(ev);
    let body = match serde_json::to_string_pretty(&event) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "cadence emit: failed to serialize event");
            return;
        }
    };

    let ts_ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    // Sanitize the tag for the filename (matches the Python helper's
    // <ts_ns>_<safe_tag>.json convention).
    let safe_tag: String = ev
        .tag
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let final_name = format!("{}_{}.json", ts_ns, safe_tag);
    let final_path = dir.join(&final_name);
    let tmp_path = dir.join(format!(".{}.tmp", final_name));

    if let Err(e) = std::fs::write(&tmp_path, body.as_bytes()) {
        tracing::warn!(error = %e, path = %tmp_path.display(),
            "cadence emit: failed to write tmp file");
        return;
    }
    if let Err(e) = std::fs::rename(&tmp_path, &final_path) {
        tracing::warn!(error = %e, src = %tmp_path.display(), dst = %final_path.display(),
            "cadence emit: failed to rename tmp into place");
        let _ = std::fs::remove_file(&tmp_path);
        return;
    }

    tracing::info!(
        path = %final_path.display(),
        tag = %ev.tag,
        "cadence event emitted"
    );
}

/// One workload-done event. Emitted exactly once per workload run when
/// the underlying tmux-pane wrapper script finishes (or `workload kill`
/// terminates it). Surfaced to the main loop via `claude-event-watch`
/// as `EVENT[workload/workload-done] ...`, replacing the "fire a
/// `workload wait` background task and poll" pattern.
///
/// First-class workload model (Andrew DM 2026-05-03 05:23 ET): when
/// the workload was launched with `workload run --queue-id q-X`, the
/// queue id is carried into ``data.queue_id`` AND `cmd_emit_done`
/// transitions the queue item to done/abandoned in the same step.
/// Workload completion IS queue completion; no separate respawn-
/// obligation handshake.
///
/// `Default` is derived so a caller (or a test) can construct the
/// common shape and `..Default::default()` the optional replace
/// markers, which only the `workload run` same-label replace path sets.
#[derive(Debug, Clone, Default)]
pub struct WorkloadDoneEvent<'a> {
    pub label: &'a str,
    /// Exit code as reported by the wrapper script. Negative values
    /// indicate non-natural termination — `-15` for `workload kill`
    /// (SIGTERM marker), other negative for future kill modes.
    pub exit_code: i32,
    /// True iff the wrapper script did not write its own exit code
    /// (i.e. `workload kill` raced ahead and synthesised this event).
    pub killed: bool,
    /// Path to the workload's output log so the main loop can `Read`
    /// the tail without re-deriving paths.
    pub log_path: &'a str,
    /// Optional queue id the workload was tied to (`workload run
    /// --queue-id q-X`). When present, included in the event's
    /// ``data.queue_id`` field so consumers can correlate without
    /// round-tripping through the workload state file.
    ///
    /// Deliberately EMPTY when [`Self::carried_over_queue_id`] is set:
    /// the item is not this run's any more, so presenting it here would
    /// invite a consumer to read a live item as dead.
    pub queue_id: Option<&'a str>,
    /// Why this completion exists, when it is not a plain natural exit
    /// or a plain `workload kill`. Currently only `"replaced"` —
    /// `workload run` was invoked on a label whose previous run was
    /// still live, so that run was torn down to make room.
    ///
    /// The marker is what keeps a replaced run from masquerading as the
    /// completion of the run that replaced it: same label, same log
    /// path, `killed=true`, arriving moments before the new run starts.
    pub reason: Option<&'a str>,
    /// For `reason="replaced"`: the `started_at` of the run that took
    /// this label over (identical to the string in the workload
    /// registry entry the new run writes), so a consumer can line the
    /// two up exactly.
    pub replaced_by: Option<&'a str>,
    /// For `reason="replaced"`: the queue item the dying run was bound
    /// to that the REPLACING run re-binds (it was handed the same
    /// `--queue-id`). It was deliberately NOT transitioned — the item
    /// is still `running`, now owned by the new run.
    pub carried_over_queue_id: Option<&'a str>,
}

/// Build the JSON event body for a workload-done event. Public for
/// testability; production callers should use `emit_workload_done()`.
pub fn build_workload_done_json(ev: &WorkloadDoneEvent<'_>) -> serde_json::Value {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    let now_iso = chrono::Local::now().to_rfc3339();
    let hostname = hostname_string();
    let user = std::env::var("USER").unwrap_or_default();
    let pid = std::process::id();

    // Human-readable message — same string the main loop sees in the
    // `EVENT[workload/workload-done] <preview>` one-liner.
    let message = if let Some(replaced_by) = ev.replaced_by {
        // Say REPLACED, not just "killed": the reader is about to see a
        // new run of the same label, and the two must not blur.
        format!(
            "workload {} replaced (previous run killed rc={}, new run started {}, log={})",
            ev.label, ev.exit_code, replaced_by, ev.log_path
        )
    } else if ev.killed {
        format!(
            "workload {} killed (rc={}, log={})",
            ev.label, ev.exit_code, ev.log_path
        )
    } else {
        format!(
            "workload {} done rc={} log={}",
            ev.label, ev.exit_code, ev.log_path
        )
    };

    // Priority: success = low (informational), failure/kill = normal
    // (still not urgent — the main loop should react but it's not an
    // alert).
    let priority = if ev.exit_code == 0 { "low" } else { "normal" };

    let mut data = serde_json::json!({
        "label": ev.label,
        "exit_code": ev.exit_code,
        "killed": ev.killed,
        "log_path": ev.log_path,
    });
    if let Some(qid) = ev.queue_id {
        data["queue_id"] = serde_json::Value::String(qid.to_string());
    }
    if let Some(reason) = ev.reason {
        data["reason"] = serde_json::Value::String(reason.to_string());
    }
    if let Some(replaced_by) = ev.replaced_by {
        data["replaced_by"] = serde_json::Value::String(replaced_by.to_string());
    }
    if let Some(qid) = ev.carried_over_queue_id {
        data["carried_over_queue_id"] = serde_json::Value::String(qid.to_string());
    }

    serde_json::json!({
        "timestamp": now,
        "timestamp_iso": now_iso,
        "hostname": hostname,
        "source": "workload",
        "source_name": ev.label,
        "tag": "workload-done",
        "priority": priority,
        "message": message,
        "data": data,
        "pid": pid,
        "user": user,
    })
}

/// Emit a workload-done event into the queue dir. Idempotency is the
/// caller's responsibility — this function unconditionally writes one
/// event file per call. Default-open: I/O failure is logged at warn
/// level and swallowed (the wrapper script's exit-file write already
/// happened; losing the event is recoverable via `workload list`).
pub fn emit_workload_done(ev: &WorkloadDoneEvent<'_>) {
    let dir = queue_dir();
    if let Err(e) = std::fs::create_dir_all(&dir) {
        tracing::warn!(error = %e, dir = %dir.display(),
            "workload-done emit: failed to create queue dir, skipping");
        return;
    }

    let event = build_workload_done_json(ev);
    let body = match serde_json::to_string_pretty(&event) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "workload-done emit: failed to serialize event");
            return;
        }
    };

    let ts_ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let final_name = format!("{}_workload-done.json", ts_ns);
    let final_path = dir.join(&final_name);
    let tmp_path = dir.join(format!(".{}.tmp", final_name));

    if let Err(e) = std::fs::write(&tmp_path, body.as_bytes()) {
        tracing::warn!(error = %e, path = %tmp_path.display(),
            "workload-done emit: failed to write tmp file");
        return;
    }
    if let Err(e) = std::fs::rename(&tmp_path, &final_path) {
        tracing::warn!(error = %e, src = %tmp_path.display(), dst = %final_path.display(),
            "workload-done emit: failed to rename tmp into place");
        let _ = std::fs::remove_file(&tmp_path);
        return;
    }

    tracing::info!(
        path = %final_path.display(),
        label = %ev.label,
        exit_code = ev.exit_code,
        killed = ev.killed,
        "workload-done event emitted"
    );
}

/// Cheap, no-deps hostname lookup. Falls back to `gethostname`'s
/// failure mode (empty string) — the event still emits, the field is
/// just blank.
fn hostname_string() -> String {
    // Try /etc/hostname first (cheap, no syscall), then `uname -n`,
    // then env. nix could supply this but we already pull libc; keep
    // this dep-free.
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

/// One `obligations-bypass` override event drained from the claude-event
/// queue, for daemon-side push notification.
///
/// Background (Andrew #8375): an audited obligations override created
/// IN-CONTAINER cannot Pushover Andrew itself -- the override CLI
/// (`_pingme_override`) shells to the host-only `pingme` binary, which is
/// absent in the container, so it is a silent no-op there. The override DOES
/// emit a loud `obligations-bypass` claude-event, and that event file lands in
/// the bind-mounted queue dir the HOST daemon can read. The daemon runs
/// host-side where Pushover works, so it is the reliable place to fire the
/// notification -- "the backend/daemon should always do it".
#[derive(Debug, Clone)]
pub struct OverrideBypassEvent {
    pub override_id: String,
    pub reason: String,
    pub duration_secs: Option<i64>,
    pub created_by: String,
    /// The event's own `priority` field (`low|normal|high|urgent`).
    pub priority: String,
}

/// Pull one scalar out of an event's `data` object as a String, whether it
/// was written as a JSON string or a JSON number/bool (the Python
/// `claude-event` helper stores `--data k=v` values as strings, but be
/// defensive about a future typed emitter).
fn data_str(data: &serde_json::Value, key: &str) -> Option<String> {
    match data.get(key) {
        Some(serde_json::Value::String(s)) => Some(s.clone()),
        Some(serde_json::Value::Number(n)) => Some(n.to_string()),
        Some(serde_json::Value::Bool(b)) => Some(b.to_string()),
        _ => None,
    }
}

/// Scan the claude-event queue `dir` for `obligations-bypass` events this
/// daemon has not yet notified on, returning the newly-seen ones and recording
/// each override id in `ledger_path` so it fires exactly once.
///
/// Does NOT delete the event files: `claude-event-watch` owns draining the
/// queue so the main loop still surfaces the bypass for its own triage. The
/// daemon therefore re-reads a given file every loop until the watcher deletes
/// it, and the ledger (keyed by override id) is what makes the notification
/// one-shot. The ledger self-prunes entries older than `LEDGER_TTL_SECS`.
///
/// Default-open: any I/O or parse failure on an individual file is skipped, and
/// a missing/corrupt ledger is treated as empty. Best-effort throughout -- a
/// broken scan must never wedge the daemon loop.
pub fn drain_obligations_bypass(
    dir: &std::path::Path,
    ledger_path: &std::path::Path,
) -> Vec<OverrideBypassEvent> {
    const LEDGER_TTL_SECS: u64 = 24 * 60 * 60;

    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        // Queue dir absent (nothing has emitted yet) => nothing to do.
        Err(_) => return Vec::new(),
    };

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    // Ledger shape: { "<override_id>": <processed_at_epoch_secs>, ... }.
    let mut ledger: std::collections::BTreeMap<String, u64> = std::fs::read_to_string(ledger_path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    let mut fresh: Vec<OverrideBypassEvent> = Vec::new();

    for entry in entries.flatten() {
        let path = entry.path();
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n,
            None => continue,
        };
        // Filename convention (Python helper + Rust emitter):
        // `<unix_ns>_<safe_tag>.json`. Fast filter on the tag suffix; skip the
        // atomic-write `.tmp` dotfiles.
        if name.starts_with('.') || !name.ends_with("_obligations-bypass.json") {
            continue;
        }
        let body = match std::fs::read_to_string(&path) {
            Ok(b) => b,
            Err(_) => continue,
        };
        let ev: serde_json::Value = match serde_json::from_str(&body) {
            Ok(v) => v,
            Err(_) => continue,
        };
        // Defensive: confirm the tag actually matches (a stray file named like
        // the convention but carrying a different tag is ignored).
        if ev.get("tag").and_then(|t| t.as_str()) != Some("obligations-bypass") {
            continue;
        }
        let data = ev
            .get("data")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({}));
        // Dedup key: the override id, falling back to the filename so a
        // malformed event (no override_id) still fires at most once.
        let override_id = data_str(&data, "override_id").unwrap_or_else(|| name.to_string());
        if ledger.contains_key(&override_id) {
            continue;
        }
        let reason = data_str(&data, "reason").unwrap_or_default();
        let duration_secs = data_str(&data, "duration_secs").and_then(|s| s.parse::<i64>().ok());
        let created_by = data_str(&data, "created_by").unwrap_or_default();
        let priority = ev
            .get("priority")
            .and_then(|p| p.as_str())
            .unwrap_or("high")
            .to_string();

        ledger.insert(override_id.clone(), now);
        fresh.push(OverrideBypassEvent {
            override_id,
            reason,
            duration_secs,
            created_by,
            priority,
        });
    }

    // Prune stale ledger entries so it can't grow without bound.
    ledger.retain(|_, ts| now.saturating_sub(*ts) < LEDGER_TTL_SECS);

    // Persist only when we recorded something new. A failed write just risks a
    // duplicate notification next loop, never a wedge.
    if !fresh.is_empty() {
        if let Some(parent) = ledger_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(s) = serde_json::to_string_pretty(&ledger) {
            let _ = std::fs::write(ledger_path, s);
        }
    }

    fresh
}

/// Test-only support for tests (in ANY module of this crate) that need
/// to redirect event emission away from the user's live queue.
#[cfg(test)]
pub(crate) mod test_support {
    use std::ffi::OsString;
    use std::path::Path;
    use std::sync::{Mutex, MutexGuard, OnceLock};

    static EVENT_QUEUE_ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

    /// RAII guard that points `CLAUDE_EVENT_QUEUE` at a test directory
    /// and restores the previous value on drop (including on panic).
    ///
    /// It holds a process-global mutex for its whole lifetime so
    /// concurrent tests can never interleave their set/restore windows.
    /// Without this, `cargo test`'s threaded runner could restore/remove
    /// the var in test A while test B sits between its own set and emit —
    /// B's emission then falls back to the user's LIVE event queue
    /// (`~/claude-events/`). Not hypothetical: fixture events from this
    /// suite (a "workload translate-book done", a keepalive with a
    /// /custom/run path) were observed landing in the live queue and
    /// churning the real event watcher. (nextest is immune — one process
    /// per test — but plain `cargo test` is a supported runner and must
    /// be safe too.) Every test that mutates `CLAUDE_EVENT_QUEUE` MUST
    /// go through this guard.
    pub(crate) struct EventQueueGuard {
        prev: Option<OsString>,
        _lock: MutexGuard<'static, ()>,
    }

    impl EventQueueGuard {
        pub(crate) fn set(path: &Path) -> Self {
            let lock = EVENT_QUEUE_ENV_LOCK
                .get_or_init(|| Mutex::new(()))
                .lock()
                // A poisoned lock only means another test panicked while
                // holding it; the env var is still consistent because
                // that test's guard restored it during unwind.
                .unwrap_or_else(|e| e.into_inner());
            let prev = std::env::var_os("CLAUDE_EVENT_QUEUE");
            // SAFETY: process-global env mutation, serialized by the
            // lock held for the guard's whole lifetime.
            unsafe {
                std::env::set_var("CLAUDE_EVENT_QUEUE", path);
            }
            EventQueueGuard { prev, _lock: lock }
        }
    }

    impl Drop for EventQueueGuard {
        fn drop(&mut self) {
            // SAFETY: the lock field drops after this body, so we still
            // hold it while restoring.
            unsafe {
                match self.prev.take() {
                    Some(v) => std::env::set_var("CLAUDE_EVENT_QUEUE", v),
                    None => std::env::remove_var("CLAUDE_EVENT_QUEUE"),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn severity_maps_to_priority() {
        assert_eq!(Severity::Low.as_priority(), "low");
        assert_eq!(Severity::Medium.as_priority(), "normal");
        assert_eq!(Severity::High.as_priority(), "high");
        assert_eq!(Severity::Critical.as_priority(), "urgent");
    }

    #[test]
    fn build_event_json_has_required_fields() {
        let alert = ClaudeWatchAlert {
            alert_type: "heartbeat-stale",
            stuck_reason: "heartbeat stale (574min, threshold=10min, watchmen=8)",
            stale_minutes: Some(574),
            affected_watchers: vec![],
            severity: Severity::High,
            message: "Claude stuck: heartbeat stale (574min, threshold=10min, watchmen=8). 2 consecutive checks failed.",
        };

        let v = build_event_json(&alert);

        assert_eq!(v["tag"], "claude-watch-alert");
        assert_eq!(v["source"], "claude-watch");
        assert_eq!(v["source_name"], "claude-watch");
        assert_eq!(v["priority"], "high");
        assert_eq!(v["message"], alert.message);
        assert!(v["timestamp"].is_number());
        assert!(v["timestamp_iso"].is_string());
        assert!(v["pid"].is_number());

        let data = &v["data"];
        assert_eq!(data["alert_type"], "heartbeat-stale");
        assert_eq!(data["stuck_reason"], alert.stuck_reason);
        assert_eq!(data["stale_minutes"], 574);
        assert_eq!(data["severity"], "high");
        assert!(data["affected_watchers"].is_array());
        assert_eq!(data["affected_watchers"].as_array().unwrap().len(), 0);
        // Only watcher-down is producer-stamped actionable; other alert
        // types (here heartbeat-stale) carry no data.tier and keep their
        // existing conditional/ambient classification.
        assert!(data["tier"].is_null());
    }

    #[test]
    fn build_event_json_handles_watcher_down() {
        let alert = ClaudeWatchAlert {
            alert_type: "watcher-down",
            stuck_reason: "2 watcher(s) missing: alerts-watcher, torrent-wait",
            stale_minutes: None,
            affected_watchers: vec!["alerts-watcher".to_string(), "torrent-wait".to_string()],
            severity: Severity::Medium,
            message: "watchers down: alerts-watcher, torrent-wait",
        };

        let v = build_event_json(&alert);

        assert_eq!(v["data"]["alert_type"], "watcher-down");
        assert!(v["data"]["stale_minutes"].is_null());
        let watchers = v["data"]["affected_watchers"].as_array().unwrap();
        assert_eq!(watchers.len(), 2);
        assert_eq!(watchers[0], "alerts-watcher");
        assert_eq!(watchers[1], "torrent-wait");
        assert_eq!(v["priority"], "normal");
        // Producer-stamped routing tier: watcher-down must ship
        // data.tier=actionable so it routes to the actionable pending
        // list, not the claude-watch-alert ambient row (incident
        // 2026-08-21).
        assert_eq!(v["data"]["tier"], "actionable");
    }

    /// A loop that cannot produce a turn, and a model change only a human can
    /// undo, are not passive context either.
    #[test]
    fn build_event_json_stamps_credits_exhausted_actionable() {
        let alert = ClaudeWatchAlert {
            alert_type: "credits-exhausted",
            stuck_reason: "claude code out of usage credits, model demoted",
            stale_minutes: None,
            affected_watchers: vec![],
            severity: Severity::High,
            message: "demoted the main loop; promote it back after the credit reset",
        };
        let v = build_event_json(&alert);
        assert_eq!(v["data"]["alert_type"], "credits-exhausted");
        assert_eq!(v["data"]["tier"], "actionable");
        assert_eq!(v["priority"], "high");
    }

    /// An auto-denied permission prompt must reach the main loop.
    #[test]
    fn build_event_json_stamps_autoresolve_no_actionable() {
        let alert = ClaudeWatchAlert {
            alert_type: "autoresolve-no",
            stuck_reason: "permission prompt auto-denied",
            stale_minutes: None,
            affected_watchers: vec![],
            severity: Severity::High,
            message: "auto-resolved a permission prompt with No",
        };
        let v = build_event_json(&alert);
        assert_eq!(v["data"]["alert_type"], "autoresolve-no");
        assert_eq!(v["data"]["tier"], "actionable");
    }

    #[test]
    fn build_event_json_handles_minimal_alert() {
        // Push-notification-only paths (e.g. auto-update-complete) carry no
        // structured stale/watcher data — verify the optional fields
        // serialise cleanly as null/empty.
        let alert = ClaudeWatchAlert {
            alert_type: "auto-update-complete",
            stuck_reason: "claude-watch: auto-update complete (1.0.0 → 1.0.1)",
            stale_minutes: None,
            affected_watchers: vec![],
            severity: Severity::Low,
            message: "claude-watch: auto-update complete (1.0.0 → 1.0.1)",
        };

        let v = build_event_json(&alert);
        assert_eq!(v["data"]["alert_type"], "auto-update-complete");
        assert!(v["data"]["stale_minutes"].is_null());
        assert!(v["data"]["affected_watchers"]
            .as_array()
            .unwrap()
            .is_empty());
        assert_eq!(v["priority"], "low");
    }

    #[test]
    fn emit_writes_a_file_in_temp_queue_dir() {
        // Point CLAUDE_EVENT_QUEUE at a tempdir, emit, verify file lands
        // with valid JSON. Doesn't touch ~/claude-events/.
        let tmp = tempfile::tempdir().expect("tempdir");
        let _env = super::test_support::EventQueueGuard::set(tmp.path());

        let alert = ClaudeWatchAlert {
            alert_type: "prolonged-thinking",
            stuck_reason: "prolonged thinking (>300s)",
            stale_minutes: None,
            affected_watchers: vec![],
            severity: Severity::Medium,
            message: "prolonged thinking interrupt #1 fired",
        };
        emit(&alert);

        // Verify exactly one file was created with the right tag and
        // that it parses as valid JSON with our expected fields.
        let entries: Vec<_> = std::fs::read_dir(tmp.path())
            .expect("read tempdir")
            .filter_map(Result::ok)
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .ends_with("_claude-watch-alert.json")
            })
            .collect();
        assert_eq!(entries.len(), 1, "expected exactly one event file");

        let content = std::fs::read_to_string(entries[0].path()).expect("read event");
        let parsed: serde_json::Value =
            serde_json::from_str(&content).expect("event is valid JSON");
        assert_eq!(parsed["tag"], "claude-watch-alert");
        assert_eq!(parsed["data"]["alert_type"], "prolonged-thinking");
    }

    #[test]
    fn build_workload_done_natural_exit() {
        let ev = WorkloadDoneEvent {
            label: "ebook-twilight",
            exit_code: 0,
            killed: false,
            log_path: "/tmp/claude-workloads/ebook-twilight.output",
            queue_id: None,
            ..Default::default()
        };
        let v = build_workload_done_json(&ev);

        assert_eq!(v["tag"], "workload-done");
        assert_eq!(v["source"], "workload");
        assert_eq!(v["source_name"], "ebook-twilight");
        assert_eq!(v["priority"], "low"); // exit 0 → low
        assert!(v["message"]
            .as_str()
            .unwrap()
            .contains("workload ebook-twilight done rc=0"));
        let data = &v["data"];
        assert_eq!(data["label"], "ebook-twilight");
        assert_eq!(data["exit_code"], 0);
        assert_eq!(data["killed"], false);
        assert_eq!(
            data["log_path"],
            "/tmp/claude-workloads/ebook-twilight.output"
        );
        // Without --queue-id, no queue_id key appears in data.
        assert!(
            data.get("queue_id").is_none(),
            "queue_id must be absent when not bound"
        );
    }

    #[test]
    fn build_workload_done_failure_exit() {
        let ev = WorkloadDoneEvent {
            label: "broken-task",
            exit_code: 2,
            killed: false,
            log_path: "/tmp/claude-workloads/broken-task.output",
            queue_id: None,
            ..Default::default()
        };
        let v = build_workload_done_json(&ev);
        assert_eq!(v["priority"], "normal"); // non-zero exit → normal
        assert_eq!(v["data"]["exit_code"], 2);
        assert_eq!(v["data"]["killed"], false);
    }

    #[test]
    fn build_workload_done_killed() {
        let ev = WorkloadDoneEvent {
            label: "dead-task",
            exit_code: -15,
            killed: true,
            log_path: "/tmp/claude-workloads/dead-task.output",
            queue_id: None,
            ..Default::default()
        };
        let v = build_workload_done_json(&ev);
        assert_eq!(v["priority"], "normal");
        assert_eq!(v["data"]["killed"], true);
        assert_eq!(v["data"]["exit_code"], -15);
        assert!(v["message"]
            .as_str()
            .unwrap()
            .contains("workload dead-task killed"));
    }

    #[test]
    fn build_workload_done_with_queue_id_includes_field() {
        // First-class workload model: when --queue-id was passed at
        // `workload run`, the event's data.queue_id mirrors it so the
        // main loop can correlate the workload exit with the queue
        // item without round-tripping state.json.
        let ev = WorkloadDoneEvent {
            label: "stv-promote-Akudama",
            exit_code: 0,
            killed: false,
            log_path: "/tmp/claude-workloads/stv-promote-Akudama.output",
            queue_id: Some("q-2026-05-03-test"),
            ..Default::default()
        };
        let v = build_workload_done_json(&ev);
        assert_eq!(v["tag"], "workload-done");
        assert_eq!(v["data"]["queue_id"], "q-2026-05-03-test");
    }

    /// A run torn down because `workload run` re-used its label must be
    /// distinguishable from the run that replaced it. Same label, same
    /// log path, `killed=true`, arriving a moment before the new run
    /// starts — without the markers the main loop would have no way to
    /// tell it is not the NEW run's completion.
    #[test]
    fn build_workload_done_replaced_carries_the_replace_markers() {
        let ev = WorkloadDoneEvent {
            label: "media-promote",
            exit_code: -15,
            killed: true,
            log_path: "/tmp/claude-workloads/media-promote.output",
            queue_id: Some("q-2026-08-22-old0"),
            reason: Some("replaced"),
            replaced_by: Some("2026-08-22T11:04:07"),
            carried_over_queue_id: None,
        };
        let v = build_workload_done_json(&ev);

        assert_eq!(v["data"]["killed"], true);
        assert_eq!(v["data"]["reason"], "replaced");
        assert_eq!(v["data"]["replaced_by"], "2026-08-22T11:04:07");
        // The dying run's own item IS terminal here, so it is named.
        assert_eq!(v["data"]["queue_id"], "q-2026-08-22-old0");
        assert!(v["data"].get("carried_over_queue_id").is_none());
        let msg = v["message"].as_str().unwrap();
        assert!(
            msg.contains("replaced") && msg.contains("2026-08-22T11:04:07"),
            "the one-liner must say REPLACED and name the new run: {msg}"
        );
    }

    /// When the replacing run was handed the SAME `--queue-id`, the item
    /// belongs to the new run: it must NOT be presented as this (dead)
    /// run's queue item, or a consumer correlating on `data.queue_id`
    /// would read a live item as abandoned.
    #[test]
    fn build_workload_done_replaced_moves_a_carried_over_qid_out_of_queue_id() {
        let ev = WorkloadDoneEvent {
            label: "media-promote",
            exit_code: -15,
            killed: true,
            log_path: "/tmp/claude-workloads/media-promote.output",
            queue_id: None,
            reason: Some("replaced"),
            replaced_by: Some("2026-08-22T11:04:07"),
            carried_over_queue_id: Some("q-2026-08-22-same"),
        };
        let v = build_workload_done_json(&ev);

        assert!(
            v["data"].get("queue_id").is_none(),
            "a carried-over item must never appear as the dead run's queue_id"
        );
        assert_eq!(v["data"]["carried_over_queue_id"], "q-2026-08-22-same");
    }

    /// Nothing changes for the paths that are not a replace: no stray
    /// keys in `data`, no reworded message.
    #[test]
    fn build_workload_done_plain_paths_carry_no_replace_keys() {
        for (killed, rc) in [(false, 0), (true, -15)] {
            let ev = WorkloadDoneEvent {
                label: "plain",
                exit_code: rc,
                killed,
                log_path: "/tmp/claude-workloads/plain.output",
                ..Default::default()
            };
            let v = build_workload_done_json(&ev);
            for key in ["reason", "replaced_by", "carried_over_queue_id"] {
                assert!(
                    v["data"].get(key).is_none(),
                    "{key} must be absent on a non-replace completion"
                );
            }
        }
    }

    #[test]
    fn emit_workload_done_writes_file_with_correct_shape() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let _env = super::test_support::EventQueueGuard::set(tmp.path());

        let ev = WorkloadDoneEvent {
            label: "translate-book",
            exit_code: 0,
            killed: false,
            log_path: "/tmp/claude-workloads/translate-book.output",
            queue_id: None,
            ..Default::default()
        };
        emit_workload_done(&ev);

        let entries: Vec<_> = std::fs::read_dir(tmp.path())
            .expect("read tempdir")
            .filter_map(Result::ok)
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .ends_with("_workload-done.json")
            })
            .collect();
        assert_eq!(
            entries.len(),
            1,
            "expected exactly one workload-done event file"
        );

        let content = std::fs::read_to_string(entries[0].path()).expect("read event");
        let parsed: serde_json::Value =
            serde_json::from_str(&content).expect("event is valid JSON");
        assert_eq!(parsed["tag"], "workload-done");
        assert_eq!(parsed["source"], "workload");
        assert_eq!(parsed["source_name"], "translate-book");
        assert_eq!(parsed["data"]["label"], "translate-book");
        assert_eq!(parsed["data"]["exit_code"], 0);
        assert_eq!(parsed["data"]["killed"], false);
    }

    #[test]
    fn build_cadence_json_has_required_fields() {
        let ev = CadenceEvent {
            tag: "keepalive",
            source: "claude-watch",
            message: "keepalive tick",
            priority: "low",
            data: serde_json::json!({"interval_secs": 60}),
        };
        let v = build_cadence_json(&ev);
        assert_eq!(v["tag"], "keepalive");
        assert_eq!(v["source"], "claude-watch");
        assert_eq!(v["source_name"], "claude-watch");
        assert_eq!(v["priority"], "low");
        assert_eq!(v["message"], "keepalive tick");
        assert_eq!(v["data"]["interval_secs"], 60);
        assert!(v["timestamp"].is_number());
        assert!(v["timestamp_iso"].is_string());
        assert!(v["pid"].is_number());
    }

    #[test]
    fn emit_cadence_writes_file_with_sanitized_name() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let _env = super::test_support::EventQueueGuard::set(tmp.path());

        let ev = CadenceEvent {
            tag: "memory-reminder",
            source: "claude-watch",
            message: "=== MEMORY REMINDER ===",
            priority: "high",
            data: serde_json::json!({"interval_secs": 900}),
        };
        emit_cadence(&ev);

        let entries: Vec<_> = std::fs::read_dir(tmp.path())
            .expect("read tempdir")
            .filter_map(Result::ok)
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .ends_with("_memory-reminder.json")
            })
            .collect();
        assert_eq!(entries.len(), 1, "expected exactly one cadence event file");

        let content = std::fs::read_to_string(entries[0].path()).expect("read event");
        let parsed: serde_json::Value =
            serde_json::from_str(&content).expect("event is valid JSON");
        assert_eq!(parsed["tag"], "memory-reminder");
        assert_eq!(parsed["priority"], "high");
        assert_eq!(parsed["data"]["interval_secs"], 900);
    }

    /// Regression guard: keepalive must reach the event queue.
    ///
    /// An earlier change made the daemon's cadence tick a no-op (logged only,
    /// never delivered), so the main loop stopped getting its quiet-period
    /// poke → liveness went stale and the daemon fired spurious stale alerts.
    /// This test builds the keepalive cadence event exactly as `run_daemon`
    /// does (using the `cadence` constants) and asserts an event file lands in
    /// the queue with the right tag/source/priority.
    #[test]
    fn emit_cadence_keepalive_writes_event() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let _env = super::test_support::EventQueueGuard::set(tmp.path());

        // Mirror the production call site in `main::run_daemon`.
        let ev = CadenceEvent {
            tag: crate::cadence::KEEPALIVE_TAG,
            source: crate::cadence::CADENCE_SOURCE,
            message: "keepalive: no event acked recently",
            priority: "low",
            data: keepalive_data(300, Some(612)),
        };
        emit_cadence(&ev);

        let entries: Vec<_> = std::fs::read_dir(tmp.path())
            .expect("read tempdir")
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().ends_with("_keepalive.json"))
            .collect();
        assert_eq!(
            entries.len(),
            1,
            "keepalive must produce exactly one event-queue file"
        );

        let content = std::fs::read_to_string(entries[0].path()).expect("read event");
        let parsed: serde_json::Value =
            serde_json::from_str(&content).expect("event is valid JSON");
        assert_eq!(parsed["tag"], "keepalive");
        assert_eq!(parsed["source"], "claude-watch");
        assert_eq!(parsed["priority"], "low");
        assert_eq!(parsed["data"]["quiet_secs"], 300);
        assert_eq!(parsed["data"]["last_ack_age_secs"], 612);
        // The body must TELL the loop the ritual, not assume it remembers.
        // claude-event-watch renders every scalar in `data` on the EVENT line,
        // so this is what makes the instruction visible.
        assert_eq!(
            parsed["data"]["ack_command"],
            "event-ack ack \"<key>\" --action \"<what you did>\""
        );
    }

    #[test]
    fn keepalive_data_carries_the_ack_command_and_quiet_window() {
        let data = keepalive_data(300, Some(900));
        assert_eq!(data["ack_command"], ACK_COMMAND);
        assert_eq!(data["quiet_secs"], 300);
        assert_eq!(data["last_ack_age_secs"], 900);

        // A configured override surfaces verbatim — the window is NOT
        // hardcoded in the body.
        let data = keepalive_data(60, Some(61));
        assert_eq!(data["quiet_secs"], 60);
    }

    #[test]
    fn keepalive_data_omits_age_when_no_ack_recorded() {
        // No ack state at all (fresh boot / no event-must-act). Omitting the
        // field is deliberate: a 0 would render on the EVENT line as
        // `last_ack_age_secs=0`, i.e. "just acked" — the exact opposite of
        // what triggered the emission.
        let data = keepalive_data(300, None);
        assert!(
            data.get("last_ack_age_secs").is_none(),
            "unknown ack age must be omitted, never faked as 0; got: {data}"
        );
        assert_eq!(data["ack_command"], ACK_COMMAND);
    }
}

#[cfg(test)]
mod obligations_bypass_tests {
    use super::*;

    fn write_event(dir: &std::path::Path, ns: &str, tag: &str, json: serde_json::Value) {
        let p = dir.join(format!("{}_{}.json", ns, tag));
        std::fs::write(p, serde_json::to_string_pretty(&json).unwrap()).unwrap();
    }

    #[test]
    fn drains_new_bypass_events_and_dedups_via_ledger() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("events");
        std::fs::create_dir_all(&dir).unwrap();
        let ledger = tmp.path().join("ledger.json");

        write_event(
            &dir,
            "1000",
            "obligations-bypass",
            serde_json::json!({
                "tag": "obligations-bypass",
                "priority": "high",
                "message": "obligations override created (ov-1, duration=300s): urgent hotfix",
                "data": {
                    "override_id": "ov-1",
                    "reason": "urgent hotfix",
                    "duration_secs": "300",
                    "created_by": "cli"
                }
            }),
        );
        // A non-bypass event in the same dir must be ignored.
        write_event(
            &dir,
            "1001",
            "keepalive",
            serde_json::json!({"tag": "keepalive", "data": {}}),
        );

        let first = drain_obligations_bypass(&dir, &ledger);
        assert_eq!(first.len(), 1, "one bypass event expected");
        let ov = &first[0];
        assert_eq!(ov.override_id, "ov-1");
        assert_eq!(ov.reason, "urgent hotfix");
        assert_eq!(ov.duration_secs, Some(300));
        assert_eq!(ov.created_by, "cli");
        assert_eq!(ov.priority, "high");

        // File still on disk (we do NOT delete -- claude-event-watch owns that),
        // but the ledger makes a second scan a no-op.
        let second = drain_obligations_bypass(&dir, &ledger);
        assert!(
            second.is_empty(),
            "already-notified override must not re-fire"
        );
    }

    #[test]
    fn missing_queue_dir_is_empty_not_error() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let out = drain_obligations_bypass(
            &tmp.path().join("nonexistent"),
            &tmp.path().join("ledger.json"),
        );
        assert!(out.is_empty());
    }
}
