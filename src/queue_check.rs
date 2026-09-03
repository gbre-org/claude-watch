//! `claude-watch queue-check` — emit a `claude-event` when one or more
//! `session-task` queue items are STUCK or ORPHANED.
//!
//! This is the IN-TREE equivalent of the out-of-tree Prometheus alert
//! rules `WorkQueueStuckSoft` / `WorkQueueOrphaned` (which live in the
//! monitoring repo, not here). Shipping detection in-tree means a
//! claude-watch deployment can surface stuck/orphaned queue items to the
//! main loop WITHOUT depending on an external Prometheus + alertmanager.
//!
//! ## Conditions detected
//!
//!   * **orphaned** — a `running` item that is no longer being worked,
//!     detected in this precedence order:
//!       1. **PID fast-path** — an explicitly-claimed owning PID
//!          (`register --pid` / `heartbeat --pid`) that is no longer alive.
//!       2. **active-agents join** — the fix for the historically-dead
//!          orphan branch: `session-task register` sets `pid=None` on every
//!          normal agent-spawn item, so the PID fast-path never fired for
//!          them. We now read the cron-written `active-agents.json` and join
//!          each `running` item on `queue_id` against agent transcript
//!          liveness (the same join the work-queue-exporter already does):
//!            - agent record present + transcript fresh → healthy.
//!            - agent record present + transcript stale → orphaned
//!              (died-after-spawn).
//!            - NO agent record + `registered_at` older than the no-binding
//!              grace window → orphaned (never-spawned). Workload/hostjob-
//!              scoped items are exempt (their liveness is a progress
//!              heartbeat, not an agent transcript). If the state file is
//!              missing/unreadable the join is skipped (fail-open — never
//!              orphan on an absent source; fall back to the stuck check).
//!   * **stuck** — either:
//!       - status `wedged` (an operator or recovery path flagged the item
//!         as system-stuck — context-limit / prolonged-thinking /
//!         heartbeat-stale), OR
//!       - a `running` item whose `last_heartbeat_at` is older than the
//!         stale threshold (default 15 min — well clear of healthy
//!         heartbeat cadences like `workload babysit`'s 60 s default and
//!         the StuckSoft `for:15m` window).
//!
//! ## Default OFF locally
//!
//! Emission is gated behind `[queue_check] emit_events` in `config.toml`,
//! **default `false`**. So the capability ships in every build but stays
//! silent unless explicitly enabled. `--force-emit` overrides the config
//! for one-shot testing; `--dry-run` prints the event JSON without
//! emitting a file or touching the dedup ledger.
//!
//! ## Single-emit dedup
//!
//! State file `<state-dir>/queue-check-state.json` maps a per-(qid,
//! condition) key to the ISO emit timestamp. An item that already fired
//! for a condition won't re-fire for the SAME condition until it leaves
//! the queue (drops out of `queue list --all`). The key includes the
//! condition so an item that transitions orphaned→wedged still surfaces
//! the new condition once.
//!
//! ## Failure mode
//!
//! Default-open. Missing session-task → exit 0 (config choice, not an
//! error). Queue read failure → exit 1 (cron retries). State write
//! failure → exit 1, but the event was already emitted.

use serde::Deserialize;
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

/// Default heartbeat-staleness threshold for the `stuck` condition, in
/// minutes. A `running` item whose `last_heartbeat_at` is older than this
/// is considered stuck. 15 min mirrors the deployed `WorkQueueStuckSoft`
/// `for:15m` window and sits an order of magnitude above healthy
/// heartbeat cadences (e.g. `workload babysit` pats every 60 s).
pub const DEFAULT_STALE_HEARTBEAT_MIN: u64 = 15;

/// Default grace window (seconds) before a `running` item with NO
/// active-agent binding is flagged orphaned (the never-spawned case).
/// 150s ≈ two-and-a-half ticks of the 60s active-agents cron, comfortably
/// past normal agent-spawn latency without false-positiving a
/// just-registered item. Overridable via `[queue_check]
/// no_binding_grace_secs` or `--no-binding-grace-secs`.
pub const DEFAULT_NO_BINDING_GRACE_SECS: u64 = 150;

/// Max number of ids to list inline in the human-readable `message`.
pub const TOP_N: usize = 3;

/// Tag for an emitted orphaned-item event.
pub const EVENT_TAG_ORPHANED: &str = "queue-orphaned";
/// Tag for an emitted stuck-item event.
pub const EVENT_TAG_STUCK: &str = "queue-stuck";
/// `source` field — matches the active-agents / stale-ready writer
/// convention ("this came from claude-watch").
pub const EVENT_SOURCE: &str = "claude-watch";
/// `source_name` disambiguator within `source=claude-watch`.
pub const EVENT_SOURCE_NAME: &str = "queue-check";

/// The condition a queue item qualified under. Used as part of the
/// dedup key and to pick the event tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Condition {
    Orphaned,
    Stuck,
}

impl Condition {
    pub fn tag(self) -> &'static str {
        match self {
            Condition::Orphaned => EVENT_TAG_ORPHANED,
            Condition::Stuck => EVENT_TAG_STUCK,
        }
    }
    /// Short token used in the dedup state key.
    pub fn key_token(self) -> &'static str {
        match self {
            Condition::Orphaned => "orphaned",
            Condition::Stuck => "stuck",
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Condition::Orphaned => "orphaned",
            Condition::Stuck => "stuck",
        }
    }
}

/// Minimal subset of a queue item we need. Extra fields in the
/// session-task `--json` output are ignored.
#[derive(Debug, Clone, Deserialize)]
pub struct QueueItem {
    pub id: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    /// Owning PID. `None` (JSON null / absent) means "no explicit PID
    /// claimed" — trusted-alive, never flagged orphaned via the PID
    /// fast-path (but still subject to the active-agents join below).
    #[serde(default)]
    pub pid: Option<i64>,
    #[serde(default)]
    pub last_heartbeat_at: Option<String>,
    /// When the item transitioned to `running` (`register`). The age
    /// reference for the never-spawned no-binding orphan grace window.
    /// Falls back to `started_at` if absent.
    #[serde(default)]
    pub registered_at: Option<String>,
    #[serde(default)]
    pub started_at: Option<String>,
    /// Scope tokens (e.g. `["repo:claude-watch"]`, `["workload:stv-promote"]`).
    /// Items whose scope carries a `workload:` / `hostjob:` token are
    /// exempt from the no-binding orphan check — their liveness is a
    /// progress heartbeat, not an agent transcript, so a missing agent
    /// record is expected and NOT an orphan signal.
    #[serde(default)]
    pub scope: Vec<String>,
    #[serde(default)]
    pub wedged_reason: Option<String>,
}

/// Result of joining a `running` queue item against the cron-written
/// `active-agents.json` (via the injected `agent_lookup` closure). Modeled
/// as a closure so unit tests can drive every branch without a real state
/// file, mirroring the existing `pid_alive` injected-closure pattern.
///
/// `Dead` carries the bound agent id + last-seen staleness (from the
/// matched `AgentRecord`) so the emitted orphan event can name WHICH
/// agent went dead and HOW stale it is — not just a bare queue id.
/// (Not `Copy`: `Dead` owns a `String`.)
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentLiveness {
    /// State loaded and a matching agent record exists AND its transcript
    /// is fresh (`alive=true`). The item is healthy.
    Alive,
    /// State loaded and a matching agent record exists but its transcript
    /// is stale (`alive=false`) — the agent died AFTER spawning. Orphaned
    /// (died-after-spawn coverage). Carries the bound `agent_id` and the
    /// seconds since its freshest transcript was last modified
    /// (`AgentRecord::jsonl_age_seconds`, `None` if unreadable) for the
    /// enriched event text.
    Dead {
        agent_id: String,
        age_secs: Option<u64>,
    },
    /// State loaded, but NO matching agent record for this qid — the
    /// never-spawned / agent-died-without-a-transcript case. Orphaned only
    /// once past the no-binding grace window (and only for
    /// non-workload/hostjob items).
    NoRecord,
    /// active-agents state UNAVAILABLE (file missing / unreadable). The
    /// join can't be trusted, so it never drives an orphan decision —
    /// detection falls back to the legacy stale-heartbeat `stuck` path.
    Unknown,
}

/// True iff any scope token marks this item as progress-heartbeat-tracked
/// (a `workload:` or `hostjob:` run). Such items are exempt from the
/// no-binding orphan check — they legitimately have no agent transcript.
pub fn is_progress_tracked_scope(scope: &[String]) -> bool {
    scope
        .iter()
        .any(|s| s.starts_with("workload:") || s.starts_with("hostjob:"))
}

/// One qualifying item plus the condition it triggered and a short
/// human-readable detail string for the event body.
#[derive(Debug, Clone)]
pub struct Qualifying {
    pub id: String,
    pub summary: String,
    pub condition: Condition,
    pub detail: String,
    /// The bound agent id, when the condition was decided via the
    /// active-agents transcript join (the `Dead` orphan path). `None` for
    /// pid-fast-path, never-spawned (no binding), and stuck conditions —
    /// there is no single agent to name. Surfaced in the event `data`.
    pub agent_id: Option<String>,
}

impl Qualifying {
    /// Dedup-state key: `<qid>::<condition>` so an item that changes
    /// condition still surfaces the new one once.
    fn state_key(&self) -> String {
        format!("{}::{}", self.id, self.condition.key_token())
    }
}

/// Default state dir. Honours `CLAUDE_WATCH_STATE_DIR`; falls back to
/// `/var/lib/claude-watch` (matches the active-agents writer + stale-ready).
pub fn default_state_dir() -> PathBuf {
    if let Ok(d) = std::env::var("CLAUDE_WATCH_STATE_DIR") {
        if !d.is_empty() {
            return PathBuf::from(d);
        }
    }
    PathBuf::from("/var/lib/claude-watch")
}

/// Parse an ISO 8601 / RFC 3339 timestamp to epoch seconds. None on
/// failure (caller skips the heartbeat-staleness check for that item
/// rather than killing the tick).
pub fn parse_iso_epoch_secs(ts: &str) -> Option<i64> {
    let ts = ts.trim();
    if ts.is_empty() {
        return None;
    }
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(ts) {
        return Some(dt.timestamp());
    }
    if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(ts, "%Y-%m-%dT%H:%M:%S%.f") {
        return Some(dt.and_utc().timestamp());
    }
    if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(ts, "%Y-%m-%dT%H:%M:%S") {
        return Some(dt.and_utc().timestamp());
    }
    None
}

/// Seconds since a `running` item transitioned to `running` (`register`),
/// falling back to `started_at`. `None` if neither timestamp parses — the
/// caller then cannot age a grace window and treats the item as "old
/// enough" (no grace). Never negative (clamped at 0 for clock skew).
pub fn register_age_secs(it: &QueueItem, now_epoch_secs: i64) -> Option<i64> {
    it.registered_at
        .as_deref()
        .or(it.started_at.as_deref())
        .and_then(parse_iso_epoch_secs)
        .map(|reg| (now_epoch_secs - reg).max(0))
}

/// Format a transcript-staleness age for human-readable event text.
/// `None` (unreadable metadata) → "age unknown"; else "<N>s ago" under a
/// minute, "<N>m ago" otherwise. Kept coarse — the reader wants "roughly
/// how stale", not second precision.
pub fn fmt_age(age_secs: Option<u64>) -> String {
    match age_secs {
        None => "age unknown".to_string(),
        Some(s) if s < 60 => format!("{s}s ago"),
        Some(s) => format!("{}m ago", s / 60),
    }
}

/// State-file payload: dedup-key -> ISO emit timestamp.
pub type State = BTreeMap<String, String>;

pub fn load_state(path: &Path) -> State {
    let raw = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return State::new(),
        Err(e) => {
            eprintln!("queue-check: state load failed ({e}); starting fresh");
            return State::new();
        }
    };
    match serde_json::from_str::<State>(&raw) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("queue-check: state parse failed ({e}); starting fresh");
            State::new()
        }
    }
}

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

/// Pure: compute the qualifying (stuck/orphaned) set.
///
///   * `items` — the full `queue list --all` set.
///   * `already_emitted` — dedup state (per-(qid,condition) key).
///   * `now_epoch_secs` — current time.
///   * `stale_secs` — heartbeat-staleness threshold for `stuck`.
///   * `no_binding_grace_secs` — grace window before a `running` item with
///     NO agent binding (the never-spawned case) is flagged orphaned.
///   * `pid_alive` — injected PID liveness probe (prod: `/proc/<pid>`
///     check; tests: a fake). Called only for `running` items with an
///     explicit positive PID (the fast path).
///   * `agent_lookup` — injected active-agents join: given a qid, returns
///     `AgentLiveness` (Alive / Dead / NoRecord / Unknown). This is the
///     fix for the dead-code orphan branch: `register` sets `pid=None` on
///     every normal agent-spawn item, so the PID fast-path never fires for
///     them — the transcript-liveness join is what actually detects a
///     died / never-spawned agent.
///
/// ## Orphan detection order (per `running` item)
///
///   1. **PID fast-path** — explicit positive PID no longer alive →
///      orphaned. Cheap; kept as-is for pid-claimed items (workload
///      babysit, explicit `register --pid`).
///   2. **Active-agents join** (`agent_lookup`):
///        - `Alive`    → healthy, no flag.
///        - `Dead`     → orphaned (died-after-spawn).
///        - `NoRecord` → orphaned IFF `registered_at` age >
///          `no_binding_grace_secs` AND the item is not
///          workload/hostjob-scoped (never-spawned case, the q-b09f
///          incident). Within grace → not-yet, stays silent.
///        - `Unknown`  → state unavailable; skip the join (fall through
///          to the stale-heartbeat `stuck` path). Never orphan on an
///          unreadable state file (fail-open).
///   3. **Stuck** — stale heartbeat (unchanged fallback).
///
/// An item can match at most ONE condition per tick. Orphaned takes
/// precedence over stuck (a dead owner is the stronger signal). `wedged`
/// items are always `stuck` (they carry no live PID expectation).
pub fn compute_qualifying<F, G>(
    items: &[QueueItem],
    already_emitted: &State,
    now_epoch_secs: i64,
    stale_secs: i64,
    no_binding_grace_secs: i64,
    mut pid_alive: F,
    mut agent_lookup: G,
) -> Vec<Qualifying>
where
    F: FnMut(i64) -> bool,
    G: FnMut(&str) -> AgentLiveness,
{
    let mut out: Vec<Qualifying> = Vec::new();
    for it in items {
        if it.id.is_empty() {
            continue;
        }
        let summary = it
            .summary
            .clone()
            .or_else(|| it.description.clone())
            .unwrap_or_else(|| "(no summary)".to_string());

        // wedged → stuck (system flagged it; no PID liveness expectation).
        if it.status == "wedged" {
            let detail = it
                .wedged_reason
                .clone()
                .filter(|r| !r.trim().is_empty())
                .map(|r| format!("wedged: {r}"))
                .unwrap_or_else(|| "wedged (no reason given)".to_string());
            push_unique(&mut out, already_emitted, it, &summary, Condition::Stuck, detail, None);
            continue;
        }

        // The rest only applies to running items.
        if it.status != "running" {
            continue;
        }

        // 1. Orphaned fast-path: explicit positive PID that is no longer
        //    alive. (Rare for normal agent items — `register` sets
        //    pid=None — but kept for pid-claimed items like workloads.)
        if let Some(pid) = it.pid {
            if pid > 0 && !pid_alive(pid) {
                push_unique(
                    &mut out,
                    already_emitted,
                    it,
                    &summary,
                    Condition::Orphaned,
                    format!("owning pid {pid} not alive"),
                    None,
                );
                continue;
            }
        }

        // 2. Orphaned via the active-agents transcript-liveness join. This
        //    is the coverage the PID fast-path misses for pid-less items.
        match agent_lookup(&it.id) {
            AgentLiveness::Alive => {
                // Healthy: transcript is fresh. Skip both orphan and stuck
                // (a live agent needn't pat the queue heartbeat — only
                // `workload babysit` does — so the stale-heartbeat stuck
                // path would false-positive here).
                continue;
            }
            AgentLiveness::Dead { agent_id, age_secs } => {
                // Grace for a JUST-registered / just-resumed item: a `Dead`
                // verdict can come from a STALE active-agents snapshot that
                // predates the resume. The q-bad6 incident: an agent was
                // resumed via SendMessage (its pid cleared to None), but the
                // cron snapshot still showed the pre-resume idle transcript,
                // so the join said "stale". Hold until past the grace window
                // — a genuinely dead agent still fires on the next tick, but
                // a fresh resume is no longer a spurious orphan.
                let within_grace = register_age_secs(it, now_epoch_secs)
                    .map(|age| age < no_binding_grace_secs)
                    .unwrap_or(false);
                if !within_grace {
                    push_unique(
                        &mut out,
                        already_emitted,
                        it,
                        &summary,
                        Condition::Orphaned,
                        format!(
                            "agent {agent_id} transcript stale {} (died after spawn)",
                            fmt_age(age_secs)
                        ),
                        Some(agent_id),
                    );
                    continue;
                }
                // Within grace → too fresh to trust a "dead" snapshot; fall
                // through to the stuck check (which needs a stale heartbeat,
                // so a just-registered item stays silent).
            }
            AgentLiveness::NoRecord => {
                // Never-spawned / no-transcript. Only orphan once past the
                // grace window AND for non-progress-tracked items
                // (workload/hostjob items legitimately have no agent).
                if !is_progress_tracked_scope(&it.scope) {
                    if let Some(age) = register_age_secs(it, now_epoch_secs) {
                        if age >= no_binding_grace_secs {
                            let age_min = age / 60;
                            push_unique(
                                &mut out,
                                already_emitted,
                                it,
                                &summary,
                                Condition::Orphaned,
                                format!(
                                    "no agent binding {age_min} min after register (never spawned?)"
                                ),
                                None,
                            );
                            continue;
                        }
                    }
                }
                // Within grace, progress-tracked, or unparseable
                // register-time → fall through to the stuck check.
            }
            AgentLiveness::Unknown => {
                // State unavailable — don't trust the join. Fall through
                // to the stale-heartbeat stuck path (legacy behaviour).
            }
        }

        // 3. Stuck: stale heartbeat.
        if let Some(hb) = it
            .last_heartbeat_at
            .as_deref()
            .and_then(parse_iso_epoch_secs)
        {
            let age = now_epoch_secs - hb;
            if age >= stale_secs {
                let age_min = (age / 60).max(0);
                push_unique(
                    &mut out,
                    already_emitted,
                    it,
                    &summary,
                    Condition::Stuck,
                    format!("heartbeat stale {age_min} min"),
                    None,
                );
            }
        }
    }
    // Orphaned first, then stuck; stable within a condition.
    out.sort_by_key(|q| match q.condition {
        Condition::Orphaned => 0,
        Condition::Stuck => 1,
    });
    out
}

/// Push a qualifying item unless the dedup ledger already has this
/// (qid, condition) key.
fn push_unique(
    out: &mut Vec<Qualifying>,
    already_emitted: &State,
    it: &QueueItem,
    summary: &str,
    condition: Condition,
    detail: String,
    agent_id: Option<String>,
) {
    let q = Qualifying {
        id: it.id.clone(),
        summary: summary.to_string(),
        condition,
        detail,
        agent_id,
    };
    if already_emitted.contains_key(&q.state_key()) {
        return;
    }
    out.push(q);
}

/// Pure: drop dedup entries whose qid is no longer present in the queue.
/// The state key is `<qid>::<condition>`, so we match on the qid prefix.
pub fn prune_state(state: &State, current_ids: &HashSet<String>) -> State {
    state
        .iter()
        .filter(|(key, _)| {
            let qid = key.split("::").next().unwrap_or("");
            current_ids.contains(qid)
        })
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

/// Human-readable `message` for a single-condition batch.
///
/// This is the ONE line the main loop sees as `EVENT[claude-watch/…]
/// <message>`, so it must be self-contained: WHICH item, and WHY. A bare
/// list of queue ids ("1 queue item orphaned: q-bad6") sent the reader to
/// the wrong item in the q-bad6 incident. Each shown item is rendered as
/// `<qid> [<summary>] (<detail>)` where `<detail>` already carries the
/// bound agent id + staleness + reason, e.g. `q-0a54 [Fix the widget]
/// (agent agent-a7f0 transcript stale 4m ago (died after spawn))`. Capped
/// at `TOP_N`; a `(+N more)` suffix notes the remainder (the full set is
/// in `data.items`).
pub fn build_message(condition: Condition, qualifying: &[Qualifying]) -> String {
    if qualifying.is_empty() {
        return String::new();
    }
    let n = qualifying.len();
    let plural = if n > 1 { "items" } else { "item" };
    let shown: Vec<String> = qualifying
        .iter()
        .take(TOP_N)
        .map(|q| format!("{} [{}] ({})", q.id, q.summary, q.detail))
        .collect();
    let mut msg = format!("{} queue {} {}: {}", n, plural, condition.label(), shown.join("; "));
    if n > TOP_N {
        msg.push_str(&format!(" (+{} more)", n - TOP_N));
    }
    msg
}

/// Build the full event JSON body for one condition's batch.
pub fn build_event_json(
    condition: Condition,
    qualifying: &[Qualifying],
    now_iso: &str,
    hostname: &str,
    user: &str,
    pid: u32,
) -> serde_json::Value {
    let n = qualifying.len();
    let top: Vec<&Qualifying> = qualifying.iter().take(TOP_N).collect();
    let top_ids: Vec<String> = top.iter().map(|q| q.id.clone()).collect();
    let top_summaries: Vec<String> = top.iter().map(|q| q.summary.clone()).collect();
    let all_ids: Vec<String> = qualifying.iter().map(|q| q.id.clone()).collect();
    let details: Vec<serde_json::Value> = qualifying
        .iter()
        .map(|q| {
            serde_json::json!({
                "id": q.id,
                "summary": q.summary,
                "detail": q.detail,
                "agent_id": q.agent_id,
            })
        })
        .collect();
    let now_epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    // urgent for orphaned (a dead owner means the work stalled silently),
    // high for stuck (attention needed but the item may still recover).
    let priority = match condition {
        Condition::Orphaned => "urgent",
        Condition::Stuck => "high",
    };
    serde_json::json!({
        "timestamp": now_epoch,
        "timestamp_iso": now_iso,
        "hostname": hostname,
        "source": EVENT_SOURCE,
        "source_name": EVENT_SOURCE_NAME,
        "tag": condition.tag(),
        "priority": priority,
        "message": build_message(condition, qualifying),
        "data": {
            "condition": condition.label(),
            "qualifying_count": n,
            "top_ids": top_ids,
            "top_summaries": top_summaries,
            "all_ids": all_ids,
            "items": details,
        },
        "pid": pid,
        "user": user,
    })
}

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

fn write_event_file(body: &serde_json::Value, tag: &str) -> std::io::Result<PathBuf> {
    let dir = event_queue_dir();
    std::fs::create_dir_all(&dir)?;
    let ts_ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let final_name = format!("{ts_ns}_{tag}.json");
    let final_path = dir.join(&final_name);
    let tmp_path = dir.join(format!(".{final_name}.tmp"));
    let body_str = serde_json::to_string_pretty(body).unwrap_or_else(|_| "{}".to_string());
    std::fs::write(&tmp_path, body_str.as_bytes())?;
    std::fs::rename(&tmp_path, &final_path)?;
    Ok(final_path)
}

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
        Ok(Err(e)) => return Err(format!("session-task exec failed: {e}")),
        Err(_) => return Err(format!("session-task timed out after {timeout_secs}s")),
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
    serde_json::from_str(trimmed)
        .map_err(|e| format!("session-task JSON parse failed: {e} (raw head: {trimmed:.200})"))
}

/// Prod PID-liveness probe: a `/proc/<pid>` directory exists. Matches the
/// container's Linux runtime (the rest of `proc_util` reads `/proc` the
/// same way).
fn pid_is_alive(pid: i64) -> bool {
    if pid <= 0 {
        return false;
    }
    Path::new(&format!("/proc/{pid}")).exists()
}

/// Build the prod `agent_lookup` from the cron-written
/// `active-agents.json` at `<state-dir>/active-agents.json`.
///
/// Returns a `(map, state_present)` pair. `map` is `queue_id ->
/// AgentRecord`; `state_present` is false when the state file was missing
/// or unparseable, in which case the returned closure yields
/// `AgentLiveness::Unknown` for EVERY qid (fail-open — never orphan on an
/// absent join source). When the state IS present, a qid with no record
/// yields `NoRecord`, a fresh record `Alive`, a stale record `Dead`.
fn build_agent_liveness(
    state_dir: &Path,
) -> (std::collections::HashMap<String, crate::active_agents::AgentRecord>, bool) {
    let path = state_dir.join("active-agents.json");
    match crate::active_agents::load_agent_state(&path) {
        Some(state) => (crate::active_agents::agents_by_queue_id(&state), true),
        None => (std::collections::HashMap::new(), false),
    }
}

/// Map a `(map, present)` join result into an `AgentLiveness` for one qid.
fn agent_liveness_for(
    map: &std::collections::HashMap<String, crate::active_agents::AgentRecord>,
    present: bool,
    qid: &str,
) -> AgentLiveness {
    if !present {
        return AgentLiveness::Unknown;
    }
    match map.get(qid) {
        Some(rec) if rec.alive => AgentLiveness::Alive,
        Some(rec) => AgentLiveness::Dead {
            agent_id: rec.agent_id.clone(),
            age_secs: rec.jsonl_age_seconds,
        },
        None => AgentLiveness::NoRecord,
    }
}

/// Resolve `[queue_check] emit_events` from config (default false). Any
/// config-load failure → false (default-OFF, fail-closed for emission).
fn config_emit_events() -> bool {
    match crate::config::try_load_config() {
        Ok(cfg) => cfg.queue_check.emit_events,
        Err(_) => false,
    }
}

/// CLI entry point. Returns the process exit code.
///
/// `no_binding_grace_secs` — grace window (seconds) before a `running`
/// item with no active-agent binding is flagged orphaned (never-spawned
/// case). Resolved by the caller: `--no-binding-grace-secs` CLI flag wins,
/// else `[queue_check] no_binding_grace_secs`, else the built-in default.
pub fn cmd_queue_check(
    stale_heartbeat_min: u64,
    no_binding_grace_secs: u64,
    state_dir: Option<&str>,
    force_emit: bool,
    dry_run: bool,
) -> i32 {
    let cli = match find_session_task_cli() {
        Some(c) => c,
        None => {
            eprintln!("queue-check: session-task CLI not found on PATH; nothing to do");
            return 0;
        }
    };

    let all_items = match run_session_task_json(&cli, &["queue", "list", "--all", "--json"], 10) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("queue-check: queue list failed: {e}");
            return 1;
        }
    };
    let current_ids: HashSet<String> = all_items.iter().map(|it| it.id.clone()).collect();

    let state_dir = state_dir.map(PathBuf::from).unwrap_or_else(default_state_dir);
    let state_file = state_dir.join("queue-check-state.json");
    let state = load_state(&state_file);
    let pruned = prune_state(&state, &current_ids);

    let now_epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let stale_secs = (stale_heartbeat_min as i64).saturating_mul(60);
    let grace_secs = no_binding_grace_secs as i64;

    // Build the active-agents join once (reads <state-dir>/active-agents.json).
    let (agent_map, state_present) = build_agent_liveness(&state_dir);

    let qualifying = compute_qualifying(
        &all_items,
        &pruned,
        now_epoch,
        stale_secs,
        grace_secs,
        pid_is_alive,
        |qid| agent_liveness_for(&agent_map, state_present, qid),
    );

    // Always persist the pruned state (cleans up finished items).
    let mut next_state = pruned.clone();

    if qualifying.is_empty() {
        if next_state != state {
            if let Err(e) = save_state(&state_file, &next_state) {
                eprintln!("queue-check: state save failed ({e}); continuing");
                return 1;
            }
        }
        return 0;
    }

    // Honour the emit toggle UNLESS --force-emit / --dry-run.
    let emit_enabled = force_emit || config_emit_events();
    if !emit_enabled && !dry_run {
        // Detection ran (and may have found items) but emission is OFF.
        // Persist pruned state only — do NOT record dedup entries (so the
        // very first tick after the toggle is flipped on still fires).
        if next_state != state {
            if let Err(e) = save_state(&state_file, &next_state) {
                eprintln!("queue-check: state save failed ({e}); continuing");
                return 1;
            }
        }
        eprintln!(
            "queue-check: {} qualifying item(s) but emit_events is OFF (set [queue_check] emit_events = true or pass --force-emit)",
            qualifying.len()
        );
        return 0;
    }

    // Split by condition; emit ONE event per non-empty condition bucket.
    let orphaned: Vec<Qualifying> = qualifying
        .iter()
        .filter(|q| q.condition == Condition::Orphaned)
        .cloned()
        .collect();
    let stuck: Vec<Qualifying> = qualifying
        .iter()
        .filter(|q| q.condition == Condition::Stuck)
        .cloned()
        .collect();

    let now_iso = chrono::Local::now().to_rfc3339();
    let hostname = hostname_string();
    let user = std::env::var("USER").unwrap_or_default();
    let pid = std::process::id();

    for (condition, batch) in [
        (Condition::Orphaned, &orphaned),
        (Condition::Stuck, &stuck),
    ] {
        if batch.is_empty() {
            continue;
        }
        let event = build_event_json(condition, batch, &now_iso, &hostname, &user, pid);
        if dry_run {
            println!(
                "{}",
                serde_json::to_string_pretty(&event).unwrap_or_else(|_| "{}".to_string())
            );
            continue;
        }
        match write_event_file(&event, condition.tag()) {
            Ok(p) => {
                println!(
                    "{}: emitted event for {} item(s) -> {}",
                    condition.tag(),
                    batch.len(),
                    p.display()
                );
            }
            Err(e) => {
                eprintln!("queue-check: event write failed: {e}");
                return 1;
            }
        }
    }

    if dry_run {
        // Leave the dedup ledger untouched on dry-run.
        return 0;
    }

    // Record dedup entries for everything we emitted.
    let emit_ts = now_iso.clone();
    for q in &qualifying {
        next_state.insert(q.state_key(), emit_ts.clone());
    }
    if let Err(e) = save_state(&state_file, &next_state) {
        eprintln!("queue-check: state save failed after emit ({e}); next tick will re-emit");
        return 1;
    }
    0
}

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

    fn item(
        id: &str,
        status: &str,
        pid: Option<i64>,
        last_heartbeat_at: Option<&str>,
    ) -> QueueItem {
        QueueItem {
            id: id.to_string(),
            status: status.to_string(),
            summary: Some(format!("summary for {id}")),
            description: None,
            pid,
            last_heartbeat_at: last_heartbeat_at.map(|s| s.to_string()),
            registered_at: None,
            started_at: None,
            scope: Vec::new(),
            wedged_reason: None,
        }
    }

    // PID liveness probes for tests.
    fn all_dead(_pid: i64) -> bool {
        false
    }
    fn all_alive(_pid: i64) -> bool {
        true
    }

    // Agent-liveness lookups for tests.
    /// Join source unavailable — inert (existing tests use this so the
    /// active-agents branch is skipped and legacy pid/stuck logic runs).
    fn no_agents(_qid: &str) -> AgentLiveness {
        AgentLiveness::Unknown
    }
    fn all_agents_alive(_qid: &str) -> AgentLiveness {
        AgentLiveness::Alive
    }
    fn all_agents_dead(_qid: &str) -> AgentLiveness {
        AgentLiveness::Dead {
            agent_id: "agent-dead01".to_string(),
            age_secs: Some(240),
        }
    }
    fn all_agents_no_record(_qid: &str) -> AgentLiveness {
        AgentLiveness::NoRecord
    }

    // Default grace window used by legacy tests (matches the const).
    const GRACE: i64 = 150;

    #[test]
    fn orphaned_when_pid_dead() {
        let items = vec![item("q-1", "running", Some(4242), None)];
        let now = Utc::now().timestamp();
        let q = compute_qualifying(&items, &State::new(), now, 15 * 60, GRACE, all_dead, no_agents);
        assert_eq!(q.len(), 1);
        assert_eq!(q[0].id, "q-1");
        assert_eq!(q[0].condition, Condition::Orphaned);
        assert!(q[0].detail.contains("4242"));
    }

    #[test]
    fn not_orphaned_when_pid_alive() {
        let items = vec![item("q-1", "running", Some(4242), None)];
        let now = Utc::now().timestamp();
        let q = compute_qualifying(&items, &State::new(), now, 15 * 60, GRACE, all_alive, no_agents);
        assert!(q.is_empty());
    }

    #[test]
    fn pid_none_never_orphaned() {
        // pid=None is trusted-alive; with a fresh heartbeat it's clean.
        let hb = iso_n_min_ago(0);
        let items = vec![item("q-1", "running", None, Some(&hb))];
        let now = Utc::now().timestamp();
        let q = compute_qualifying(&items, &State::new(), now, 15 * 60, GRACE, all_dead, no_agents);
        assert!(q.is_empty());
    }

    // --- active-agents join (orphan detection via transcript liveness) ---

    /// A `running`, pid-less item registered `reg_min_ago` minutes ago
    /// with an optional scope. Models the normal agent-spawn flow where
    /// `register` sets pid=None.
    fn running_reg(id: &str, reg_min_ago: i64, scope: &[&str]) -> QueueItem {
        let mut it = item(id, "running", None, None);
        it.registered_at = Some(iso_n_min_ago(reg_min_ago));
        it.scope = scope.iter().map(|s| s.to_string()).collect();
        it
    }

    #[test]
    fn agent_alive_is_healthy() {
        // Live transcript → neither orphaned nor stuck, even with a stale
        // queue heartbeat (a live agent needn't pat the queue heartbeat).
        let mut it = running_reg("q-live", 30, &[]);
        it.last_heartbeat_at = Some(iso_n_min_ago(30));
        let now = Utc::now().timestamp();
        let q = compute_qualifying(
            &[it], &State::new(), now, 15 * 60, GRACE, all_alive, all_agents_alive,
        );
        assert!(q.is_empty(), "{:?}", q);
    }

    #[test]
    fn agent_dead_transcript_is_orphaned() {
        // Record exists but transcript is stale → died-after-spawn orphan.
        let it = running_reg("q-died", 30, &[]);
        let now = Utc::now().timestamp();
        let q = compute_qualifying(
            &[it], &State::new(), now, 15 * 60, GRACE, all_alive, all_agents_dead,
        );
        assert_eq!(q.len(), 1);
        assert_eq!(q[0].id, "q-died");
        assert_eq!(q[0].condition, Condition::Orphaned);
        assert!(q[0].detail.contains("died after spawn"), "{}", q[0].detail);
    }

    #[test]
    fn dead_detail_names_agent_and_staleness() {
        // #6543: a Dead orphan must name WHICH agent + HOW stale, both in
        // the per-item detail AND on Qualifying.agent_id (for the event
        // data), so the reader isn't handed a bare queue id.
        let it = running_reg("q-died", 30, &[]);
        let now = Utc::now().timestamp();
        let q = compute_qualifying(
            &[it], &State::new(), now, 15 * 60, GRACE, all_alive, all_agents_dead,
        );
        assert_eq!(q.len(), 1);
        assert!(q[0].detail.contains("agent-dead01"), "{}", q[0].detail);
        assert!(q[0].detail.contains("4m ago"), "{}", q[0].detail);
        assert_eq!(q[0].agent_id.as_deref(), Some("agent-dead01"));
    }

    #[test]
    fn dead_within_grace_not_orphaned() {
        // #6543 core fix: a Dead verdict on a JUST-registered / just-resumed
        // item (register within grace) is held — the active-agents snapshot
        // may predate a resume (the q-bad6 incident: pid cleared to None by
        // the resume, snapshot still showed the pre-resume idle transcript).
        // Fresh heartbeat too → stays completely silent this tick.
        let mut it = running_reg("q-resumed", 1, &[]); // 1 min < 150s grace
        it.last_heartbeat_at = Some(iso_n_min_ago(0));
        let now = Utc::now().timestamp();
        let q = compute_qualifying(
            &[it], &State::new(), now, 15 * 60, GRACE, all_alive, all_agents_dead,
        );
        assert!(q.is_empty(), "{:?}", q);
    }

    #[test]
    fn dead_past_grace_still_orphaned() {
        // The grace is a fresh-item guard only: a Dead item registered well
        // past the window is a genuine died-after-spawn orphan and fires.
        let it = running_reg("q-old-dead", 10, &[]); // 10 min > 150s grace
        let now = Utc::now().timestamp();
        let q = compute_qualifying(
            &[it], &State::new(), now, 15 * 60, GRACE, all_alive, all_agents_dead,
        );
        assert_eq!(q.len(), 1);
        assert_eq!(q[0].condition, Condition::Orphaned);
    }

    #[test]
    fn no_binding_past_grace_is_orphaned() {
        // No agent record + registered well past the grace window →
        // never-spawned orphan (the q-b09f incident).
        let it = running_reg("q-never", 10, &[]); // 10 min > 150s grace
        let now = Utc::now().timestamp();
        let q = compute_qualifying(
            &[it], &State::new(), now, 15 * 60, GRACE, all_alive, all_agents_no_record,
        );
        assert_eq!(q.len(), 1);
        assert_eq!(q[0].id, "q-never");
        assert_eq!(q[0].condition, Condition::Orphaned);
        assert!(q[0].detail.contains("no agent binding"), "{}", q[0].detail);
    }

    #[test]
    fn no_binding_within_grace_not_yet() {
        // No record but only just registered (within grace) → stay silent.
        // registered 1 min ago, grace 150s (2.5 min).
        let it = running_reg("q-fresh", 1, &[]);
        let now = Utc::now().timestamp();
        let q = compute_qualifying(
            &[it], &State::new(), now, 15 * 60, GRACE, all_alive, all_agents_no_record,
        );
        assert!(q.is_empty(), "{:?}", q);
    }

    #[test]
    fn no_binding_workload_scope_exempt() {
        // A workload-scoped item legitimately has no agent transcript —
        // must NOT be flagged orphaned even long past the grace window.
        let it = running_reg("q-wl", 60, &["workload:stv-promote"]);
        let now = Utc::now().timestamp();
        let q = compute_qualifying(
            &[it], &State::new(), now, 15 * 60, GRACE, all_alive, all_agents_no_record,
        );
        assert!(q.is_empty(), "{:?}", q);
    }

    #[test]
    fn no_binding_hostjob_scope_exempt() {
        let it = running_reg("q-hj", 60, &["hostjob:qc-build"]);
        let now = Utc::now().timestamp();
        let q = compute_qualifying(
            &[it], &State::new(), now, 15 * 60, GRACE, all_alive, all_agents_no_record,
        );
        assert!(q.is_empty(), "{:?}", q);
    }

    #[test]
    fn agent_state_unknown_falls_back_to_stuck() {
        // State unavailable (Unknown) → the join is skipped; a pid-less
        // running item with a stale heartbeat still surfaces as STUCK via
        // the legacy fallback (fail-open, no orphan on missing state).
        let mut it = running_reg("q-fb", 30, &[]);
        it.last_heartbeat_at = Some(iso_n_min_ago(30));
        let now = Utc::now().timestamp();
        let q = compute_qualifying(
            &[it], &State::new(), now, 15 * 60, GRACE, all_alive, no_agents,
        );
        assert_eq!(q.len(), 1);
        assert_eq!(q[0].condition, Condition::Stuck);
    }

    #[test]
    fn agent_state_unknown_no_false_orphan() {
        // Unknown join + fresh heartbeat → nothing (never orphan on an
        // unreadable state file).
        let mut it = running_reg("q-fb2", 30, &[]);
        it.last_heartbeat_at = Some(iso_n_min_ago(1));
        let now = Utc::now().timestamp();
        let q = compute_qualifying(
            &[it], &State::new(), now, 15 * 60, GRACE, all_alive, no_agents,
        );
        assert!(q.is_empty(), "{:?}", q);
    }

    #[test]
    fn pid_dead_fast_path_wins_over_agent_join() {
        // Explicit dead PID short-circuits to Orphaned before the agent
        // join is even consulted (the join here would say Alive).
        let mut it = item("q-pid", "running", Some(4242), None);
        it.registered_at = Some(iso_n_min_ago(30));
        let now = Utc::now().timestamp();
        let q = compute_qualifying(
            &[it], &State::new(), now, 15 * 60, GRACE, all_dead, all_agents_alive,
        );
        assert_eq!(q.len(), 1);
        assert_eq!(q[0].condition, Condition::Orphaned);
        assert!(q[0].detail.contains("4242"));
    }

    #[test]
    fn no_binding_no_register_time_falls_through() {
        // NoRecord but registered_at/started_at unparseable → can't age
        // the grace window, so fall through (no orphan). Fresh-ish
        // heartbeat keeps it silent.
        let mut it = item("q-nots", "running", None, Some(&iso_n_min_ago(1)));
        it.registered_at = None;
        it.started_at = None;
        let now = Utc::now().timestamp();
        let q = compute_qualifying(
            &[it], &State::new(), now, 15 * 60, GRACE, all_alive, all_agents_no_record,
        );
        assert!(q.is_empty(), "{:?}", q);
    }

    #[test]
    fn wedged_is_stuck() {
        let mut it = item("q-w", "wedged", None, None);
        it.wedged_reason = Some("context-limit".to_string());
        let now = Utc::now().timestamp();
        let q = compute_qualifying(&[it], &State::new(), now, 15 * 60, GRACE, all_alive, no_agents);
        assert_eq!(q.len(), 1);
        assert_eq!(q[0].condition, Condition::Stuck);
        assert!(q[0].detail.contains("context-limit"));
    }

    #[test]
    fn stuck_on_stale_heartbeat() {
        let hb = iso_n_min_ago(30); // 30 min old, threshold 15
        let items = vec![item("q-s", "running", None, Some(&hb))];
        let now = Utc::now().timestamp();
        let q = compute_qualifying(&items, &State::new(), now, 15 * 60, GRACE, all_alive, no_agents);
        assert_eq!(q.len(), 1);
        assert_eq!(q[0].condition, Condition::Stuck);
        assert!(q[0].detail.contains("min"));
    }

    #[test]
    fn fresh_heartbeat_not_stuck() {
        let hb = iso_n_min_ago(2);
        let items = vec![item("q-s", "running", None, Some(&hb))];
        let now = Utc::now().timestamp();
        let q = compute_qualifying(&items, &State::new(), now, 15 * 60, GRACE, all_alive, no_agents);
        assert!(q.is_empty());
    }

    #[test]
    fn orphaned_takes_precedence_over_stale_heartbeat() {
        // running, dead pid, AND stale heartbeat → orphaned (one entry).
        let hb = iso_n_min_ago(30);
        let items = vec![item("q-1", "running", Some(9999), Some(&hb))];
        let now = Utc::now().timestamp();
        let q = compute_qualifying(&items, &State::new(), now, 15 * 60, GRACE, all_dead, no_agents);
        assert_eq!(q.len(), 1);
        assert_eq!(q[0].condition, Condition::Orphaned);
    }

    #[test]
    fn pending_items_ignored() {
        let items = vec![item("q-p", "pending", Some(1), None)];
        let now = Utc::now().timestamp();
        let q = compute_qualifying(&items, &State::new(), now, 15 * 60, GRACE, all_dead, no_agents);
        assert!(q.is_empty());
    }

    #[test]
    fn completed_items_ignored() {
        let items = vec![item("q-c", "completed", Some(1), None)];
        let now = Utc::now().timestamp();
        let q = compute_qualifying(&items, &State::new(), now, 15 * 60, GRACE, all_dead, no_agents);
        assert!(q.is_empty());
    }

    #[test]
    fn dedup_skips_already_emitted_same_condition() {
        let items = vec![item("q-1", "running", Some(4242), None)];
        let mut state = State::new();
        state.insert("q-1::orphaned".to_string(), "2026-06-03T00:00:00Z".to_string());
        let now = Utc::now().timestamp();
        let q = compute_qualifying(&items, &state, now, 15 * 60, GRACE, all_dead, no_agents);
        assert!(q.is_empty());
    }

    #[test]
    fn dedup_allows_new_condition_for_same_id() {
        // Already emitted "stuck" for q-1; now it's orphaned → fires.
        let items = vec![item("q-1", "running", Some(4242), None)];
        let mut state = State::new();
        state.insert("q-1::stuck".to_string(), "2026-06-03T00:00:00Z".to_string());
        let now = Utc::now().timestamp();
        let q = compute_qualifying(&items, &state, now, 15 * 60, GRACE, all_dead, no_agents);
        assert_eq!(q.len(), 1);
        assert_eq!(q[0].condition, Condition::Orphaned);
    }

    #[test]
    fn orphaned_sorted_before_stuck() {
        let stale = iso_n_min_ago(30);
        let items = vec![
            item("q-stuck", "running", None, Some(&stale)),
            item("q-orphan", "running", Some(8888), None),
        ];
        let now = Utc::now().timestamp();
        let q = compute_qualifying(&items, &State::new(), now, 15 * 60, GRACE, all_dead, no_agents);
        assert_eq!(q.len(), 2);
        assert_eq!(q[0].condition, Condition::Orphaned);
        assert_eq!(q[1].condition, Condition::Stuck);
    }

    #[test]
    fn prune_drops_missing_qids() {
        let mut state = State::new();
        state.insert("q-here::stuck".to_string(), "t".to_string());
        state.insert("q-gone::orphaned".to_string(), "t".to_string());
        let mut current = HashSet::new();
        current.insert("q-here".to_string());
        let pruned = prune_state(&state, &current);
        assert_eq!(pruned.len(), 1);
        assert!(pruned.contains_key("q-here::stuck"));
    }

    #[test]
    fn build_message_orphaned_singular() {
        let q = vec![Qualifying {
            id: "q-1".to_string(),
            summary: "s".to_string(),
            condition: Condition::Orphaned,
            detail: "d".to_string(),
            agent_id: None,
        }];
        let msg = build_message(Condition::Orphaned, &q);
        assert!(msg.contains("1 queue item orphaned"));
        assert!(msg.contains("q-1"));
        // Enriched: summary + detail are inline, not just the bare id.
        assert!(msg.contains("[s]"), "{msg}");
        assert!(msg.contains("(d)"), "{msg}");
    }

    #[test]
    fn build_message_enriched_carries_agent_and_reason() {
        // #6543: the one-line message the main loop reads must name the
        // item, its summary, and the full detail (agent id + staleness +
        // reason) — the bare-id message sent the reader to the wrong item.
        let q = vec![Qualifying {
            id: "q-0a54".to_string(),
            summary: "Fix the widget".to_string(),
            condition: Condition::Orphaned,
            detail: "agent agent-a7f0 transcript stale 4m ago (died after spawn)".to_string(),
            agent_id: Some("agent-a7f0".to_string()),
        }];
        let msg = build_message(Condition::Orphaned, &q);
        assert!(msg.contains("q-0a54"), "{msg}");
        assert!(msg.contains("Fix the widget"), "{msg}");
        assert!(msg.contains("agent-a7f0"), "{msg}");
        assert!(msg.contains("stale 4m ago"), "{msg}");
    }

    #[test]
    fn build_message_caps_and_counts_remainder() {
        // More than TOP_N items → show TOP_N inline + a (+N more) suffix.
        let q: Vec<Qualifying> = (0..TOP_N + 2)
            .map(|i| Qualifying {
                id: format!("q-{i}"),
                summary: format!("s{i}"),
                condition: Condition::Orphaned,
                detail: format!("d{i}"),
                agent_id: None,
            })
            .collect();
        let msg = build_message(Condition::Orphaned, &q);
        assert!(msg.contains(&format!("{} queue items orphaned", TOP_N + 2)), "{msg}");
        assert!(msg.contains("(+2 more)"), "{msg}");
    }

    #[test]
    fn fmt_age_buckets() {
        assert_eq!(fmt_age(None), "age unknown");
        assert_eq!(fmt_age(Some(45)), "45s ago");
        assert_eq!(fmt_age(Some(240)), "4m ago");
    }

    #[test]
    fn build_event_json_orphaned_shape() {
        let q = vec![Qualifying {
            id: "q-a".to_string(),
            summary: "summary-a".to_string(),
            condition: Condition::Orphaned,
            detail: "agent agent-xyz transcript stale 3m ago (died after spawn)".to_string(),
            agent_id: Some("agent-xyz".to_string()),
        }];
        let v = build_event_json(Condition::Orphaned, &q, "2026-06-03T01:30:00Z", "host", "user", 1234);
        assert_eq!(v["tag"], EVENT_TAG_ORPHANED);
        assert_eq!(v["source"], EVENT_SOURCE);
        assert_eq!(v["source_name"], EVENT_SOURCE_NAME);
        assert_eq!(v["priority"], "urgent");
        assert_eq!(v["data"]["condition"], "orphaned");
        assert_eq!(v["data"]["qualifying_count"], 1);
        assert_eq!(v["data"]["all_ids"], serde_json::json!(["q-a"]));
        assert_eq!(
            v["data"]["items"][0]["detail"],
            "agent agent-xyz transcript stale 3m ago (died after spawn)"
        );
        // #6543: the bound agent id is surfaced structurally too.
        assert_eq!(v["data"]["items"][0]["agent_id"], "agent-xyz");
    }

    #[test]
    fn build_event_json_stuck_priority() {
        let q = vec![Qualifying {
            id: "q-b".to_string(),
            summary: "s".to_string(),
            condition: Condition::Stuck,
            detail: "wedged: x".to_string(),
            agent_id: None,
        }];
        let v = build_event_json(Condition::Stuck, &q, "2026-06-03T01:30:00Z", "h", "u", 1);
        assert_eq!(v["tag"], EVENT_TAG_STUCK);
        assert_eq!(v["priority"], "high");
        assert_eq!(v["data"]["condition"], "stuck");
    }

    #[test]
    fn state_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("queue-check-state.json");
        let mut state = State::new();
        state.insert("q-1::stuck".to_string(), "2026-06-03T01:00:00Z".to_string());
        save_state(&path, &state).unwrap();
        assert_eq!(load_state(&path), state);
    }

    #[test]
    fn load_state_missing_returns_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nope.json");
        assert!(load_state(&path).is_empty());
    }

    #[test]
    fn parse_iso_handles_rfc3339_and_naive() {
        assert!(parse_iso_epoch_secs("2026-06-03T12:00:00+00:00").is_some());
        assert!(parse_iso_epoch_secs("2026-06-03T12:00:00").is_some());
        assert!(parse_iso_epoch_secs("garbage").is_none());
    }
}
