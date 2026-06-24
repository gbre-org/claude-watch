//! Obligation-arming sink for the alerting hierarchy.
//!
//! This is the OBLIGATION rung between events (mild, non-blocking) and
//! interruptions (forced tmux inject). When the daemon detects a stuck
//! condition it FIRST "arms an obligation" by appending a pending-alert
//! entry here; the companion `pre-tool-claude-watch-alert-gate-hook`
//! (PreToolUse) then DENIES the next tool call until the operator/agent
//! clears it via `claude-watch-ack ack`. Only if the condition persists
//! past the dwell window does the daemon escalate to a tmux interrupt.
//!
//! The on-disk schema MUST match `tools/claude-watch-ack/claude-watch-ack`
//! exactly (the Python CLI is the source of truth and is what the gate hook
//! reads). State file:
//!
//! ```json
//! {
//!   "alerts": [
//!     {
//!       "id": "alert-YYYYMMDD-HHMMSS-NNNN",
//!       "message": "<= 4096 bytes, utf8-boundary truncated>",
//!       "created_at": <unix int>,
//!       "source": "<tag>"
//!     }
//!   ]
//! }
//! ```
//!
//! Path: `${CLAUDE_WATCH_ALERT_STATE_DIR:-~/.config/claude-watch}/pending-alerts.json`.
//! Writes are atomic (sibling `.tmp` + rename); the dir is created 0700 and
//! the file 0600 (best-effort). Every operation is DEFAULT-OPEN: any I/O or
//! parse error is logged and swallowed so a broken sink never blackholes the
//! daemon's other alert paths.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Mirror the Python CLI's `MAX_MESSAGE_BYTES`.
const MAX_MESSAGE_BYTES: usize = 4096;

/// Resolve the state dir, honoring `CLAUDE_WATCH_ALERT_STATE_DIR`
/// (matching the Python CLI), defaulting to `~/.config/claude-watch`.
fn alert_state_dir() -> PathBuf {
    if let Ok(p) = std::env::var("CLAUDE_WATCH_ALERT_STATE_DIR") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home).join(".config").join("claude-watch")
}

fn state_path(dir: &Path) -> PathBuf {
    dir.join("pending-alerts.json")
}

/// Truncate `msg` to at most `MAX_MESSAGE_BYTES` bytes at a UTF-8 boundary.
/// Mirrors the Python CLI's `_truncate_message` (append a `...[truncated]`
/// marker so the gate banner shows the cut explicitly).
fn truncate_message(msg: &str) -> String {
    if msg.len() <= MAX_MESSAGE_BYTES {
        return msg.to_string();
    }
    // Find the largest byte index <= MAX_MESSAGE_BYTES that lands on a
    // UTF-8 char boundary.
    let mut cut = MAX_MESSAGE_BYTES;
    while cut > 0 && !msg.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}...[truncated]", &msg[..cut])
}

/// Generate an `alert-YYYYMMDD-HHMMSS-NNNN` id. The 4-digit suffix is
/// derived from SystemTime nanos (no `rand` dependency) so two alerts in
/// the same second don't collide.
fn new_id() -> String {
    let now = chrono::Utc::now().format("%Y%m%d-%H%M%S");
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let suffix = nanos % 10_000;
    format!("alert-{}-{:04}", now, suffix)
}

/// Read the alerts list tolerantly. Missing / corrupt file => empty list.
fn load_alerts(path: &Path) -> Vec<serde_json::Value> {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let parsed: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    match parsed.get("alerts").and_then(|a| a.as_array()) {
        Some(arr) => arr.clone(),
        None => Vec::new(),
    }
}

/// Atomic write of the alerts list, matching the Python CLI's `_save_state`
/// (indent 2 + trailing newline, dir 0700, file 0600, tmp + rename).
fn save_alerts(dir: &Path, path: &Path, alerts: &[serde_json::Value]) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    // Best-effort perms on the dir (we may not own a bind-mounted dir).
    let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));

    let state = serde_json::json!({ "alerts": alerts });
    let mut body = serde_json::to_string_pretty(&state)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    body.push('\n');

    let tmp_path = dir.join("pending-alerts.json.tmp");
    std::fs::write(&tmp_path, body.as_bytes())?;
    let _ = std::fs::set_permissions(&tmp_path, std::fs::Permissions::from_mode(0o600));
    std::fs::rename(&tmp_path, path)?;
    Ok(())
}

/// Append a pending alert. NO-OP returning `Ok(false)` if an alert with the
/// same `source` is already present (idempotent arm). Returns `Ok(true)`
/// when a new alert was written. Default-open: any I/O error is logged and
/// swallowed, returning `Ok(false)`.
///
/// Honors `CLAUDE_WATCH_ALERT_STATE_DIR`; delegates to
/// [`arm_alert_obligation_in`] for testability (tests pass an explicit dir).
pub fn arm_alert_obligation(message: &str, source: &str) -> std::io::Result<bool> {
    arm_alert_obligation_in(&alert_state_dir(), message, source)
}

/// Dir-explicit variant of [`arm_alert_obligation`] so tests avoid mutating
/// the process-global env.
pub fn arm_alert_obligation_in(
    dir: &Path,
    message: &str,
    source: &str,
) -> std::io::Result<bool> {
    let path = state_path(dir);
    let mut alerts = load_alerts(&path);

    // Idempotent: if an alert with this source already exists, no-op.
    let already = alerts.iter().any(|a| {
        a.get("source").and_then(|s| s.as_str()) == Some(source)
    });
    if already {
        return Ok(false);
    }

    let entry = serde_json::json!({
        "id": new_id(),
        "message": truncate_message(message),
        "created_at": SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        "source": source,
    });
    alerts.push(entry);

    match save_alerts(dir, &path, &alerts) {
        Ok(()) => Ok(true),
        Err(e) => {
            // Default-open: log + swallow. A broken obligation sink must not
            // blackhole the daemon's event / interrupt paths.
            tracing::warn!(error = %e, dir = %dir.display(),
                "arm_alert_obligation: failed to write pending-alerts; default-open");
            Ok(false)
        }
    }
}

/// True iff a pending alert with `source` exists. Missing / corrupt file =>
/// false. Honors `CLAUDE_WATCH_ALERT_STATE_DIR`.
///
/// Part of the public obligation-arm API (used by the gate-precedence logic
/// + tests); `allow(dead_code)` because the daemon's fire sites only need
/// `arm_alert_obligation` directly — the armed-check is exercised via the
/// state-tracked `*_obligation_armed_at` timestamps, not this probe.
#[allow(dead_code)]
pub fn is_obligation_armed(source: &str) -> bool {
    is_obligation_armed_in(&alert_state_dir(), source)
}

/// Dir-explicit variant of [`is_obligation_armed`].
#[allow(dead_code)]
pub fn is_obligation_armed_in(dir: &Path, source: &str) -> bool {
    let path = state_path(dir);
    load_alerts(&path).iter().any(|a| {
        a.get("source").and_then(|s| s.as_str()) == Some(source)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_arm_writes_exact_schema() {
        let dir = tempfile::tempdir().unwrap();
        let wrote = arm_alert_obligation_in(dir.path(), "hello world", "src-a").unwrap();
        assert!(wrote, "first arm should write");

        let path = state_path(dir.path());
        let content = std::fs::read_to_string(&path).unwrap();
        let v: serde_json::Value = serde_json::from_str(&content).unwrap();
        let alerts = v["alerts"].as_array().unwrap();
        assert_eq!(alerts.len(), 1);
        let entry = &alerts[0];
        assert!(entry["id"].as_str().unwrap().starts_with("alert-"));
        assert!(entry["created_at"].is_i64() || entry["created_at"].is_u64());
        assert_eq!(entry["source"].as_str().unwrap(), "src-a");
        assert_eq!(entry["message"].as_str().unwrap(), "hello world");
        // Trailing newline, matches Python _save_state.
        assert!(content.ends_with("\n"));
    }

    #[test]
    fn test_message_truncated_at_4096_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let big = "x".repeat(5000);
        arm_alert_obligation_in(dir.path(), &big, "src-trunc").unwrap();
        let path = state_path(dir.path());
        let content = std::fs::read_to_string(&path).unwrap();
        let v: serde_json::Value = serde_json::from_str(&content).unwrap();
        let msg = v["alerts"][0]["message"].as_str().unwrap();
        assert!(msg.starts_with(&"x".repeat(4096)));
        assert!(msg.ends_with("...[truncated]"));
        // The 'x' prefix is exactly MAX_MESSAGE_BYTES bytes.
        assert_eq!(msg.len(), 4096 + "...[truncated]".len());
    }

    #[test]
    fn test_truncate_respects_utf8_boundary() {
        // A multibyte char straddling the 4096 cut must not panic / split.
        let mut s = "a".repeat(4095);
        s.push('é'); // 2 bytes -> straddles byte 4096
        s.push_str(&"b".repeat(100));
        let out = truncate_message(&s);
        // Valid UTF-8 (would panic during slice if not boundary-safe).
        assert!(out.ends_with("...[truncated]"));
        // The 'é' (starting at byte 4095) is dropped because it crosses 4096.
        assert!(out.starts_with(&"a".repeat(4095)));
    }

    #[test]
    fn test_is_armed_true_after_arm_false_other_source() {
        let dir = tempfile::tempdir().unwrap();
        arm_alert_obligation_in(dir.path(), "m", "src-x").unwrap();
        assert!(is_obligation_armed_in(dir.path(), "src-x"));
        assert!(!is_obligation_armed_in(dir.path(), "src-y"));
    }

    #[test]
    fn test_second_arm_same_source_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        assert!(arm_alert_obligation_in(dir.path(), "m1", "dup").unwrap());
        assert!(!arm_alert_obligation_in(dir.path(), "m2", "dup").unwrap());
        let path = state_path(dir.path());
        let content = std::fs::read_to_string(&path).unwrap();
        let v: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(v["alerts"].as_array().unwrap().len(), 1, "no duplicate");
        // Original message preserved (the no-op did not overwrite).
        assert_eq!(v["alerts"][0]["message"].as_str().unwrap(), "m1");
    }

    #[test]
    fn test_distinct_sources_coexist() {
        let dir = tempfile::tempdir().unwrap();
        arm_alert_obligation_in(dir.path(), "a", "s1").unwrap();
        arm_alert_obligation_in(dir.path(), "b", "s2").unwrap();
        assert!(is_obligation_armed_in(dir.path(), "s1"));
        assert!(is_obligation_armed_in(dir.path(), "s2"));
        let path = state_path(dir.path());
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(v["alerts"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn test_file_and_dir_perms() {
        let dir = tempfile::tempdir().unwrap();
        // Nest one level so the created dir's perms are ours (the tempdir
        // root may carry the test runner's umask).
        let sub = dir.path().join("claude-watch");
        arm_alert_obligation_in(&sub, "m", "perm").unwrap();
        let dir_mode = std::fs::metadata(&sub).unwrap().permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o700, "dir mode should be 0700");
        let file_mode = std::fs::metadata(state_path(&sub))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(file_mode, 0o600, "file mode should be 0600");
    }

    #[test]
    fn test_corrupt_file_default_open() {
        let dir = tempfile::tempdir().unwrap();
        let path = state_path(dir.path());
        std::fs::write(&path, "not json at all {{{").unwrap();
        // Corrupt => treated as empty: arm succeeds, is_armed reflects it.
        assert!(!is_obligation_armed_in(dir.path(), "anything"));
        assert!(arm_alert_obligation_in(dir.path(), "m", "fresh").unwrap());
        assert!(is_obligation_armed_in(dir.path(), "fresh"));
    }

    #[test]
    fn test_missing_file_is_not_armed() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!is_obligation_armed_in(dir.path(), "nope"));
    }
}
