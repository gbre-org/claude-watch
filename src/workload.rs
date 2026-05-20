//! workload — launch long-running tasks in the `tasks` tmux session that
//! survive Claude Code /clear and compaction.
//!
//! Straight Rust port of the Python `workload` script. State lives under
//! `/var/run/claude/workload-state/` (state.json, <label>.output,
//! <label>.exit, <label>.sh, <label>.heartbeat, <label>.script.json).
//! `/var/run` is a tmpfs (Debian symlink to `/run`) and
//! `/var/run/claude/` is provisioned at boot by
//! `/etc/tmpfiles.d/claude.conf` (`d /var/run/claude 0755 hndrewaall
//! hndrewaall -`) so the dir is uid-1000 writable and reset on reboot —
//! no systemd-tmpfiles cron sweep can prune live workload artifacts out
//! from under us (the failure mode that pushed us off `/tmp`, Andrew
//! 2026-05-18 02:52 UTC).
//!
//! Note the subdir name — `workload-state/`, NOT `workloads/`. The
//! runtime heartbeat sidecar already owns `/run/claude/workloads/`
//! (`<label>.heartbeat`), and since `/var/run -> /run` is a symlink we
//! can't reuse `workloads/` for the slow 15-min heartbeat file without
//! both sidecars clobbering each other's `<label>.heartbeat` write.
//! Distinct subdirs keep the two heartbeat layers independent.
//!
//! A backward-compat symlink `/tmp/claude-workloads -> /var/run/claude/workload-state`
//! is created lazily by `cmd_run` so legacy consumers (docker bind-mount
//! into queue-minisite, `cron-workload-stale-check`) keep working
//! transparently. The symlink is best-effort: failure to create it does
//! not block workload startup.
//!
//! On workload completion (natural or via `workload kill`), an event of
//! `tag=workload-done`, `source=workload` is emitted into
//! `~/claude-events/` so `claude-event-watch` surfaces the completion to
//! the main loop without needing a separate `workload wait` background
//! task. Idempotency: the wrapper script writes an exit-code marker file
//! BEFORE invoking the emitter; `cmd_kill` consults that marker and
//! skips its own emit if the wrapper already finished naturally.

use crate::event_bus::{emit_workload_done, WorkloadDoneEvent};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

const SESSION: &str = "tasks";

/// Where live workload artifacts (state.json + per-label .output / .exit /
/// .heartbeat / .sh / .script.json) live. Migrated off `/tmp` 2026-05-18
/// after Andrew flagged `/tmp` as sketchy — systemd-tmpfiles can prune
/// `/tmp/claude-workloads` mid-run, and the dir's traversal mode (1777)
/// leaks workload labels to other uids. `/var/run/claude/` is uid-1000
/// owned (provisioned at boot by `/etc/tmpfiles.d/claude.conf`, same dir
/// that already holds the main-loop `/run/claude/heartbeat` and the
/// runtime workload heartbeat under `/run/claude/workloads/`). `/var/run`
/// is a symlink to `/run` on Debian-style systems.
///
/// Subdir is `workload-state/` rather than `workloads/` to keep the
/// slow-cadence (15-min) heartbeat file `<label>.heartbeat` from
/// colliding with the runtime (30s, progress-driven) heartbeat at
/// `/run/claude/workloads/<label>.heartbeat`. Same filename, distinct
/// dirs.
const WORKLOAD_DIR: &str = "/var/run/claude/workload-state";

/// Legacy artifact path. Kept as a symlink target for one cycle so
/// out-of-tree consumers (docker bind-mount into queue-minisite,
/// `cron-workload-stale-check` in server-config) keep working without a
/// coordinated multi-repo deploy.
const LEGACY_WORKLOAD_DIR: &str = "/tmp/claude-workloads";

/// Per-workload runtime heartbeat directory. Used by the daemon's
/// stuck-detection suppression path — see `policy::workload_heartbeat_fresh`.
///
/// **The runtime heartbeat is PROGRESS-driven, not timer-driven.** The
/// wrapper script writes an initial touch on startup (covers warm-up
/// before the wrapped command emits anything), then a sidecar polls the
/// workload's `.output` file size on a fixed interval
/// (`WORKLOAD_RUNTIME_HEARTBEAT_INTERVAL_SECS`, default 30s) and only
/// re-touches the heartbeat when the output grew since the last check.
/// If the wrapped command hangs (no new output), the heartbeat goes
/// stale — the daemon's `policy::workload_heartbeat_fresh` then STOPS
/// suppressing stuck-alerts, so the real stuck state surfaces.
///
/// The original PR #208 design touched the heartbeat unconditionally on
/// a timer, which falsely suppressed alerts whenever the wrapper was
/// alive but its child had hung. Andrew flagged that design flaw
/// 2026-05-16 04:16 UTC. The progress-based variant detects "wrapped
/// command is making progress" (proxied by stdout growth), not "the
/// wrapper's timer is running".
///
/// Distinct from the slow-cadence (`heartbeat_file` above, 15-min interval,
/// `/var/run/claude/workloads/`) which `cron-workload-stale-check`
/// consumes to fire `workload-stale` claude-events at 1h+ stalls. The
/// two heartbeats coexist:
///   * runtime heartbeat (this, `/run/claude/workloads/`): progress-driven,
///     used by claude-watch daemon to suppress prolonged-thinking +
///     heartbeat-stale alerts while a workload is actively making progress.
///   * legacy heartbeat (15-min, `/var/run/claude/workloads/`): cron-side
///     stale-detection. Same parent dir as the slow-cadence sidecar
///     post-migration; they share the workloads dir but write to
///     different per-label files (`<label>.heartbeat` for the 15-min
///     pet vs. `/run/claude/workloads/<label>.heartbeat` for the 30s
///     progress pet — note `/run` vs `/var/run/claude`, two different
///     dirs that happen to live on the same tmpfs).
///
/// `/run/claude/` is a tmpfs (cleared on reboot, same mount as the
/// main-loop heartbeat at `/run/claude/heartbeat`) so leftover files
/// from a crashed wrapper don't outlive the host.
const RUNTIME_HEARTBEAT_DIR: &str = "/run/claude/workloads";

fn state_file() -> PathBuf {
    PathBuf::from(WORKLOAD_DIR).join("state.json")
}

fn output_file(label: &str) -> PathBuf {
    PathBuf::from(WORKLOAD_DIR).join(format!("{label}.output"))
}

fn exit_file(label: &str) -> PathBuf {
    PathBuf::from(WORKLOAD_DIR).join(format!("{label}.exit"))
}

fn script_file(label: &str) -> PathBuf {
    PathBuf::from(WORKLOAD_DIR).join(format!("{label}.sh"))
}

/// Per-workload watchdog heartbeat file. The wrapper script touches this
/// every `WORKLOAD_HEARTBEAT_INTERVAL_SECS` seconds (default 900 = 15 min)
/// while the user command is running. A separate cron-driven detector
/// (`cron-workload-stale-check`) scans these files for stale mtimes and
/// emits a `workload-stale` claude-event when one ages past 1h with no
/// matching `<label>.exit` (i.e. the workload hasn't legitimately
/// finished). Pet-or-fire watchdog pattern — no per-iter health spam,
/// but real stalls page Andrew.
fn heartbeat_file(label: &str) -> PathBuf {
    PathBuf::from(WORKLOAD_DIR).join(format!("{label}.heartbeat"))
}

/// Per-workload runtime heartbeat file under `RUNTIME_HEARTBEAT_DIR`.
/// Initial touch on wrapper startup, then re-touched only when the
/// workload's `.output` file grows (i.e. the wrapped command emitted
/// new bytes since the last poll). Poll interval is
/// `WORKLOAD_RUNTIME_HEARTBEAT_INTERVAL_SECS` (default 30s). The
/// claude-watch daemon scans `RUNTIME_HEARTBEAT_DIR` for fresh-mtime
/// files to suppress prolonged-thinking + heartbeat-stale alerts ONLY
/// while a workload is actively making progress. A hung wrapped command
/// produces no stdout → heartbeat goes stale → suppression lifts →
/// real stuck state surfaces.
fn runtime_heartbeat_file(label: &str) -> PathBuf {
    PathBuf::from(RUNTIME_HEARTBEAT_DIR).join(format!("{label}.heartbeat"))
}

/// Per-workload captured-script sidecar. Written by `cmd_run` at
/// workload start time when the command parses as `<interpreter>
/// <path>` for a known scripting interpreter (bash/sh/python/ruby/
/// node/perl/etc.). The file holds JSON serialised from
/// `ScriptCapture` and is read by queue-minisite's
/// `/api/queue/<id>/meta` endpoint to surface the script's contents in
/// the modal "Script contents" disclosure.
///
/// Capture-at-run-time is robust against `/tmp` cleanup, script
/// edits, or deletes that happen after the workload has started —
/// the modal would otherwise show stale or empty content (a real
/// failure mode observed 2026-05-13 when
/// `/tmp/promote-sweep-batch2.sh` was modified mid-session and the
/// modal had no way to show what had actually run).
fn script_capture_file(label: &str) -> PathBuf {
    PathBuf::from(WORKLOAD_DIR).join(format!("{label}.script.json"))
}

/// Maximum bytes of script content to embed in the capture. Anything
/// larger is truncated; `ScriptCapture::truncated` flips to `true`.
/// 1 MiB is well above any plausible shell/python script size while
/// still bounding the size of the per-workload sidecar.
const SCRIPT_CAPTURE_MAX_BYTES: u64 = 1024 * 1024;

/// Number of bytes from the head of the file to inspect for a NUL
/// byte when detecting binary content. Matches what `file(1)` /
/// `git diff` use for text-vs-binary heuristics.
const SCRIPT_CAPTURE_BINARY_PROBE_BYTES: usize = 512;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ScriptCapture {
    /// Resolved absolute (or PATH-resolved) path of the script.
    pub path: String,
    /// Bare interpreter name (`bash`, `python3`, `node`, ...) as the
    /// front-end sees it. Tracks the FIRST positional arg, not its
    /// resolved absolute path, since that's what the user typed.
    pub interpreter: String,
    /// Total file size in bytes (before truncation).
    pub size_bytes: u64,
    /// True when `content` was clipped to `SCRIPT_CAPTURE_MAX_BYTES`.
    pub truncated: bool,
    /// True when the file was detected as binary (NUL byte in the
    /// first `SCRIPT_CAPTURE_BINARY_PROBE_BYTES`). When true,
    /// `content` is `None`.
    pub binary: bool,
    /// UTF-8 decoded body. `None` when the file is binary. Lossy
    /// decode (invalid bytes → U+FFFD) so we can still surface
    /// near-text content without crashing the renderer.
    pub content: Option<String>,
    /// SHA-256 of the FULL file content (not the truncated body),
    /// so a viewer can confirm the script identity even on a
    /// truncated/binary capture.
    pub sha256: String,
}

/// Recognise interpreter argv0s where the second positional arg is a
/// script path we want to capture.
///
/// Matching is on the bare basename so `/usr/bin/python3.11` and
/// `python3.11` both match. We intentionally keep this list short —
/// each entry is something Andrew or one of his tools actually feeds
/// to `workload run`. Adding more interpreters is a one-line change.
fn interpreter_basename(arg0: &str) -> Option<&'static str> {
    let base = Path::new(arg0)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(arg0);
    match base {
        "bash" => Some("bash"),
        "sh" => Some("sh"),
        "zsh" => Some("zsh"),
        "dash" => Some("dash"),
        "python" | "python3" => Some("python3"),
        // Versioned pythons: python3.11, python3.12, ... Match by prefix.
        b if b.starts_with("python3.") => Some("python3"),
        b if b.starts_with("python2.") => Some("python2"),
        "ruby" => Some("ruby"),
        "node" | "nodejs" => Some("node"),
        "perl" => Some("perl"),
        _ => None,
    }
}

/// Resolve a relative script path against PATH so a command like
/// `bash myscript.sh` (where myscript.sh isn't in cwd but is on
/// PATH) still captures. Absolute paths and `./...` / `../...`
/// short-circuit to the literal value. Returns `None` if the file
/// can't be found anywhere.
fn resolve_script_path(raw: &str) -> Option<PathBuf> {
    let pb = PathBuf::from(raw);
    // Absolute or explicit relative-from-cwd: use as-is.
    if pb.is_absolute() || raw.starts_with("./") || raw.starts_with("../") {
        return if pb.is_file() { Some(pb) } else { None };
    }
    // Try cwd first (common case: `workload run foo -- bash script.sh`
    // launched from the dir containing script.sh).
    if pb.is_file() {
        return Some(pb);
    }
    // Fall back to PATH walk.
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(&pb);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Probe an open file body for a NUL byte in the first
/// `SCRIPT_CAPTURE_BINARY_PROBE_BYTES`. Matches `git`'s binary
/// detection (good-enough for refusing to embed a megabyte of
/// non-text in the modal).
fn looks_binary(bytes: &[u8]) -> bool {
    let probe_len = bytes.len().min(SCRIPT_CAPTURE_BINARY_PROBE_BYTES);
    bytes[..probe_len].contains(&0u8)
}

/// Inspect `cmd_args` and, if it looks like a script invocation
/// (`<interpreter> <single-file-path>`), read the file and return a
/// `ScriptCapture`. Returns `None` when:
///
///   * cmd_args doesn't match the `<interpreter> <path>` shape
///   * the interpreter isn't in the recognised list
///   * the resolved path is a symlink (refused for safety — don't
///     follow `/etc/shadow` etc.)
///   * the resolved path isn't a regular file
///   * any I/O error reading the file
///
/// The function is fail-soft: any error path returns None so the
/// workload itself still runs. Capture is a nice-to-have, not a
/// hard requirement.
///
/// Safety rails:
///   * Symlinks are refused via `symlink_metadata` + `is_symlink()`
///     check. We do NOT use `O_NOFOLLOW` at the syscall level because
///     `fs::read` follows symlinks by default — instead we stat first
///     with `symlink_metadata` and bail before reading.
///   * Files larger than `SCRIPT_CAPTURE_MAX_BYTES` are truncated.
///   * Binary content (NUL in first 512 bytes) skips the body but
///     keeps the metadata (size + sha256) so the user knows the
///     workload ran a binary.
pub fn try_capture_script(cmd_args: &[String]) -> Option<ScriptCapture> {
    if cmd_args.len() != 2 {
        return None;
    }
    let arg0 = cmd_args[0].as_str();
    let arg1 = cmd_args[1].as_str();
    let interpreter = interpreter_basename(arg0)?;

    // The second arg has to look like a path — refuse `-c`-style
    // inline-script invocations (`bash -c 'echo hi'`), and refuse
    // bare option flags.
    if arg1.starts_with('-') {
        return None;
    }

    let resolved = resolve_script_path(arg1)?;

    // Symlink refuse — use symlink_metadata so we don't traverse.
    let lmeta = fs::symlink_metadata(&resolved).ok()?;
    if lmeta.file_type().is_symlink() {
        return None;
    }
    if !lmeta.is_file() {
        return None;
    }

    let size_bytes = lmeta.len();
    let path_str = resolved.to_string_lossy().to_string();

    // Read with a size cap. Read full file to compute sha256, but
    // truncate the embedded body if oversize.
    let full = fs::read(&resolved).ok()?;
    let mut hasher = Sha256::new();
    hasher.update(&full);
    let sha256 = format!("{:x}", hasher.finalize());

    let binary = looks_binary(&full);
    let truncated = (full.len() as u64) > SCRIPT_CAPTURE_MAX_BYTES;

    let content = if binary {
        None
    } else {
        let body: &[u8] = if truncated {
            &full[..SCRIPT_CAPTURE_MAX_BYTES as usize]
        } else {
            &full
        };
        Some(String::from_utf8_lossy(body).into_owned())
    };

    Some(ScriptCapture {
        path: path_str,
        interpreter: interpreter.to_string(),
        size_bytes,
        truncated,
        binary,
        content,
        sha256,
    })
}

/// Persist a capture to the per-label sidecar so queue-minisite can
/// surface it later. Fail-soft: errors are silently swallowed (any
/// failure here means the modal omits the section, which is the
/// existing fall-through behaviour).
fn write_script_capture(label: &str, cap: &ScriptCapture) {
    let path = script_capture_file(label);
    if let Ok(json) = serde_json::to_string(cap) {
        let _ = fs::write(&path, json);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WorkloadEntry {
    #[serde(default)]
    pub pane_id: String,
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub output: String,
    #[serde(default)]
    pub started_at: String,
    /// Queue id this workload is bound to (`workload run --queue-id
    /// q-X`). When set, the wrapper-side `emit_done` carries the qid
    /// into the `workload-done` event AND transitions the queue item
    /// to done/abandoned via `session-task` — first-class workload
    /// model (Andrew DM 2026-05-03 05:23 ET). Backward compatible:
    /// existing state.json entries without the field deserialize as
    /// None.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue_id: Option<String>,
}

pub type WorkloadState = BTreeMap<String, WorkloadEntry>;

pub fn load_state() -> WorkloadState {
    let path = state_file();
    let data = match fs::read_to_string(&path) {
        Ok(d) => d,
        Err(_) => return WorkloadState::new(),
    };
    serde_json::from_str(&data).unwrap_or_default()
}

pub fn save_state(state: &WorkloadState) -> std::io::Result<()> {
    fs::create_dir_all(WORKLOAD_DIR)?;
    let json = serde_json::to_string_pretty(state).unwrap_or_else(|_| "{}".to_string());
    fs::write(state_file(), json)
}

/// Best-effort: ensure `/tmp/claude-workloads -> /var/run/claude/workload-state`
/// exists so out-of-tree consumers (docker bind-mount into queue-minisite,
/// `cron-workload-stale-check` in server-config) keep finding workload
/// artifacts at the legacy path. The symlink is intentionally lazy —
/// created on first `workload run` after a reboot — so a fresh tmpfs
/// always lands us in a known state without depending on a separate
/// boot-time hook.
///
/// Skipped (no error) when:
///   * `legacy` already exists as a directory (real state from a
///     still-running legacy workload — do NOT clobber it, the operator
///     can clean it up manually once everything has migrated). The new
///     path is authoritative regardless.
///   * `legacy` already exists as a symlink (idempotent).
///   * Any I/O error (best-effort; legacy consumers will fail soft).
fn ensure_legacy_compat_symlink() {
    create_compat_symlink(Path::new(LEGACY_WORKLOAD_DIR), Path::new(WORKLOAD_DIR));
}

/// Inner helper extracted for testability. Idempotent + fail-soft.
fn create_compat_symlink(legacy: &Path, target: &Path) {
    // symlink_metadata so we don't traverse if it already points
    // somewhere — we want to know if the link exists, not what it
    // points to.
    match fs::symlink_metadata(legacy) {
        Ok(meta) => {
            if meta.file_type().is_symlink() {
                // Already a symlink — idempotent no-op. We don't
                // re-target even if the existing link points
                // somewhere else; that's the operator's call to fix.
                return;
            }
            // It's a real directory (or file) — leave it alone. The
            // operator can clean it up after the legacy consumers
            // have all migrated.
            return;
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // Fall through to create the symlink.
        }
        Err(_) => {
            // Permission / other I/O error — silently skip.
            return;
        }
    }
    let _ = std::os::unix::fs::symlink(target, legacy);
}

/// POSIX single-quote shell escape.
fn shell_quote(s: &str) -> String {
    let escaped = s.replace('\'', "'\\''");
    format!("'{escaped}'")
}

fn session_exists() -> bool {
    Command::new("tmux")
        .args(["has-session", "-t", SESSION])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn pane_alive(pane_id: &str) -> bool {
    if pane_id.is_empty() {
        return false;
    }
    let out = Command::new("tmux")
        .args(["list-panes", "-t", SESSION, "-F", "#{pane_id}"])
        .output();
    match out {
        Ok(o) if o.status.success() => {
            let stdout = String::from_utf8_lossy(&o.stdout);
            stdout.lines().any(|l| l.trim() == pane_id)
        }
        _ => false,
    }
}

fn rebalance() {
    let _ = Command::new("tmux")
        .args(["select-layout", "-t", SESSION, "even-vertical"])
        .output();
}

/// Best-effort PATH walk for the `session-task` CLI. Used by
/// `transition_queue_item_for_workload` to mark the queue item
/// done/abandoned after a workload-bound (`--queue-id`) workload
/// exits. Honours an explicit override via the `SESSION_TASK_CLI`
/// env var (used by tests to point at a per-test stub).
fn find_session_task_cli() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("SESSION_TASK_CLI") {
        let pb = PathBuf::from(p);
        if pb.exists() {
            return Some(pb);
        }
    }
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join("session-task");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    let home = std::env::var("HOME").ok()?;
    let candidate = PathBuf::from(home).join("bin/session-task");
    if candidate.is_file() {
        Some(candidate)
    } else {
        None
    }
}

/// Auto-create + register a queue item bound to this workload.
///
/// Workloads-are-first-class-queue-items default (Andrew DM 2026-05-04
/// 21:02 ET): if `workload run` wasn't given an explicit `--queue-id`,
/// we synthesise one so the workload appears in `session-task queue
/// list` alongside agent items. Scope is `workload:<label>` (label is
/// already unique within `/var/run/claude/workloads/state.json` — kill+run
/// of the same label is the only collision case, which is the same
/// constraint workloads have always had). `--force-enqueue` bypasses
/// scope-conflict checks: peer workloads with overlapping scope
/// (essentially: the same label re-run, which `cmd_run` itself
/// already kills before reaching this point) shouldn't block queue
/// registration.
///
/// Steps:
///   1. `session-task queue add --scope workload:<label> --summary <60-char snippet> --force-enqueue --json <desc>`
///      → parse `id` from JSON output
///   2. `session-task queue register <id> --silent` to mark running
///   3. return Ok(qid)
///
/// Any failure (CLI missing, non-zero exit, JSON parse) returns Err so
/// the caller can fail-soft and continue without a queue row.
///
/// Both invocations are bounded with a 10s timeout (registration is
/// normally <500ms; a wedged session-task must not stall the workload
/// startup path).
fn auto_create_and_register_queue_item(
    label: &str,
    command: &str,
) -> Result<String, String> {
    let cli = find_session_task_cli()
        .ok_or_else(|| "session-task CLI not found on PATH".to_string())?;

    let scope = format!("workload:{label}");
    // Summary: first ~60 chars of the command for at-a-glance
    // identification in `queue list`. Description gets the full
    // command for forensic detail.
    let summary: String = command.chars().take(60).collect();
    let description = format!("workload:{label} — {command}");

    // Step 1: queue add (returns JSON with id)
    let add_args = vec![
        "queue".to_string(),
        "add".to_string(),
        description,
        "--scope".to_string(),
        scope,
        "--summary".to_string(),
        summary,
        "--force-enqueue".to_string(),
        "--json".to_string(),
        "--created-by".to_string(),
        "workload".to_string(),
    ];
    let add_output = run_session_task_with_timeout(&cli, &add_args, 10)
        .map_err(|e| format!("queue add: {e}"))?;
    if !add_output.status.success() {
        return Err(format!(
            "queue add exited non-zero (rc={:?}): stderr={}",
            add_output.status.code(),
            String::from_utf8_lossy(&add_output.stderr).trim()
        ));
    }
    let stdout = String::from_utf8_lossy(&add_output.stdout);
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim())
        .map_err(|e| format!("queue add JSON parse: {e} (raw={})", stdout.trim()))?;
    let qid = parsed
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| format!("queue add JSON missing `id` field: {}", stdout.trim()))?
        .to_string();

    // Step 2: queue register (atomically claim as running). --silent
    // suppresses the pingme so we don't double-notify (the workload's
    // own start banner is enough). On failure we still return the qid
    // — `cmd_emit_done`'s done/abandon transition still cleans up an
    // unregistered row, and the visibility (the row exists in the list)
    // is the user-facing goal.
    let reg_args = vec![
        "queue".to_string(),
        "register".to_string(),
        qid.clone(),
        "--silent".to_string(),
    ];
    match run_session_task_with_timeout(&cli, &reg_args, 10) {
        Ok(out) if out.status.success() => Ok(qid),
        Ok(out) => {
            tracing::warn!(
                qid = %qid,
                label = %label,
                rc = ?out.status.code(),
                stderr = %String::from_utf8_lossy(&out.stderr).trim(),
                "queue register failed; workload continues with bound qid anyway"
            );
            Ok(qid)
        }
        Err(e) => {
            tracing::warn!(
                qid = %qid,
                label = %label,
                error = %e,
                "queue register errored; workload continues with bound qid anyway"
            );
            Ok(qid)
        }
    }
}

/// Inject the `workload:<label>` scope token onto a caller-supplied
/// queue item via `session-task queue update-scope`.
///
/// Motivation: when the main loop registers a queue item with a
/// non-workload scope (e.g. `resource:promote-4-shows`) and THEN
/// invokes `workload run LABEL -- CMD --queue-id <qid>`, the queue
/// item has no `workload:<label>` token. The work-queue-exporter uses
/// that token to locate the runtime heartbeat file under
/// `/run/claude/workloads/<label>.heartbeat`; without it,
/// `worktask_queue_progress_age_seconds` never gets emitted and the
/// `WorkQueueStuck` / `WorkQueueStuckSoft` alerts false-fire after 1h
/// on healthy long-running workloads.
///
/// This helper is the systemic fix: the workload runner KNOWS its
/// label and its qid at startup, so it can append the token itself.
/// q-2026-05-20-13b9 (scope `resource:promote-4-shows-then-reseed`
/// bound to workload `promote-3-shows`) was the trigger — the
/// progress_age series was never emitted, both stuck alerts false-fired.
///
/// Auto-created queue items already include the `workload:<label>`
/// scope by construction (see `auto_create_and_register_queue_item`),
/// so this helper only runs on the caller-supplied `--queue-id` path.
/// It is idempotent — re-running `workload run LABEL` with the same
/// qid is a no-op on the queue side (the token is added at most once).
///
/// Fail-soft: any CLI / timeout / non-zero exit logs a warning but
/// does NOT block the workload from starting. The exporter false-fire
/// is a monitoring degradation, not a correctness failure — better
/// to keep the workload running.
///
/// Bounded with a 10s timeout (same as the auto-create + register
/// path) so a wedged session-task can't stall workload startup.
fn inject_workload_scope_token(label: &str, qid: &str) {
    let cli = match find_session_task_cli() {
        Some(c) => c,
        None => {
            tracing::warn!(
                label = %label,
                qid = %qid,
                "session-task CLI not found; skipping workload scope-token injection \
                 (worktask_queue_progress_age_seconds will not be emitted)"
            );
            return;
        }
    };
    let token = format!("workload:{label}");
    let args = vec![
        "queue".to_string(),
        "update-scope".to_string(),
        qid.to_string(),
        token.clone(),
    ];
    match run_session_task_with_timeout(&cli, &args, 10) {
        Ok(out) if out.status.success() => {
            tracing::debug!(
                label = %label,
                qid = %qid,
                token = %token,
                "injected workload scope token onto queue item"
            );
        }
        Ok(out) => {
            tracing::warn!(
                label = %label,
                qid = %qid,
                token = %token,
                rc = ?out.status.code(),
                stderr = %String::from_utf8_lossy(&out.stderr).trim(),
                "queue update-scope failed; workload continues without scope-token injection"
            );
        }
        Err(e) => {
            tracing::warn!(
                label = %label,
                qid = %qid,
                token = %token,
                error = %e,
                "queue update-scope errored; workload continues without scope-token injection"
            );
        }
    }
}

/// Run `session-task` with a wall-clock timeout. Returns the full
/// `Output` (status + stdout + stderr). On timeout the child is killed
/// and we return ErrorKind::TimedOut.
fn run_session_task_with_timeout(
    cli: &Path,
    args: &[String],
    timeout_secs: u64,
) -> std::io::Result<std::process::Output> {
    use std::time::{Duration, Instant};
    let mut child = std::process::Command::new(cli)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;

    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    loop {
        match child.try_wait()? {
            Some(_status) => {
                // wait_with_output consumes the child; child.wait_with_output
                // is the canonical way to harvest stdout/stderr after exit.
                return child.wait_with_output();
            }
            None => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        format!("session-task timed out after {timeout_secs}s"),
                    ));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

fn read_exit_code(label: &str) -> Option<i32> {
    let path = exit_file(label);
    let s = fs::read_to_string(path).ok()?;
    s.trim().parse::<i32>().ok()
}

fn print_tail(path: &Path, n: usize) {
    if let Ok(data) = fs::read_to_string(path) {
        let lines: Vec<&str> = data.lines().collect();
        let start = lines.len().saturating_sub(n);
        for line in &lines[start..] {
            println!("{line}");
        }
    }
}

/// Kill only the setsid child process group of a pane — never the wrapper
/// shell's PGID (which may be shared with the tmux session). Mirrors the
/// Python `_kill_pane_tree`.
fn kill_pane_tree(pane_id: &str) {
    // Pane shell PID
    let out = Command::new("tmux")
        .args(["list-panes", "-t", pane_id, "-F", "#{pane_pid}"])
        .output();
    let shell_pid = match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        _ => return,
    };
    if shell_pid.is_empty() {
        return;
    }

    // Shell's own PGID — skip this one
    let shell_pgid = Command::new("ps")
        .args(["-o", "pgid=", "-p", &shell_pid])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();

    // Direct children
    let children: Vec<String> = Command::new("pgrep")
        .args(["-P", &shell_pid])
        .output()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .map(|l| l.trim().to_string())
                .filter(|l| !l.is_empty())
                .collect()
        })
        .unwrap_or_default();

    let mut killed_pgids = std::collections::HashSet::new();
    for pid in &children {
        let pgid = Command::new("ps")
            .args(["-o", "pgid=", "-p", pid])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default();
        if pgid.is_empty() || pgid == "1" || pgid == shell_pgid {
            // Kill the PID directly (not the pgroup)
            let _ = Command::new("kill").args(["-9", pid]).output();
            continue;
        }
        if killed_pgids.insert(pgid.clone()) {
            // setsid group — safe to kill entirely
            let _ = Command::new("kill")
                .args(["-9", "--", &format!("-{pgid}")])
                .output();
        }
    }

    // Kill any remaining descendants by PID
    let remaining = get_descendants(&shell_pid);
    if !remaining.is_empty() {
        let mut args = vec!["-9".to_string()];
        args.extend(remaining);
        let _ = Command::new("kill").args(&args).output();
    }
}

fn get_descendants(pid: &str) -> Vec<String> {
    let mut out = Vec::new();
    let children: Vec<String> = Command::new("pgrep")
        .args(["-P", pid])
        .output()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .map(|l| l.trim().to_string())
                .filter(|l| !l.is_empty())
                .collect()
        })
        .unwrap_or_default();
    for c in children {
        let sub = get_descendants(&c);
        out.push(c);
        out.extend(sub);
    }
    out
}

/// Build the wrapper bash script that runs the workload command in the
/// `tasks` tmux pane. Pure function (no I/O, no globals) so it can be
/// unit-tested. The wrapper:
///
///   * Traps INT/TERM (fatfinger-proof against accidental Ctrl-C in the
///     pane).
///   * Writes header / footer lines straight to `<label>.output` (no
///     timestamp prefix — the SSE wrapper in queue-minisite stamps each
///     wire frame anyway, and disk timestamps were blocking carriage-
///     return progress; see next bullet).
///   * Runs the user's command under `script -q -f -e -c '...' /dev/null`
///     so it sees a PTY on stdout. WITHOUT this, progress-emitting tools
///     like `rsync --progress`, `curl`, `wget --progress`, `pv` either
///     suppress progress entirely (they detect non-tty) OR emit only
///     `\r`-separated frames that the old `ts | tee` chain buffered
///     until a final `\n` — so all the in-flight progress for one file
///     accumulated and only flushed when the file finished. Users saw
///     "rows updating between files, not during them" in queue.gbre.org.
///     With a PTY, rsync emits `\r`-separated frames continuously AND
///     `script -f` flushes after every write; the bytes flow straight
///     through `tee` (no `\n`-buffered `ts` in front of it) into the
///     .output file, where queue-minisite's `_split_cr_lf_segments`
///     splitter (PR #133) picks them up as transient SSE frames in
///     real time. Opt out with `WORKLOAD_PTY=0` for the rare consumer
///     that genuinely needs a non-tty stdout (CI scripts that test
///     tty-aware branches).
///   * Echoes a header (`=== workload: <label> ===` etc.), runs the
///     command via `setsid --wait script ...`, captures the exit code,
///     writes `<label>.exit`, and emits the `workload-done` claude-event
///     via the embedded `claude-watch workload emit-done` subcommand.
fn build_wrapper_script(
    label: &str,
    command: &str,
    out_path: &Path,
    exit_path: &Path,
    heartbeat_path: &Path,
    runtime_heartbeat_path: &Path,
    exe_path: &str,
    queue_id: Option<&str>,
) -> String {
    let out_q = shell_quote(&out_path.to_string_lossy());
    let exit_q = shell_quote(&exit_path.to_string_lossy());
    let hb_q = shell_quote(&heartbeat_path.to_string_lossy());
    let rt_hb_q = shell_quote(&runtime_heartbeat_path.to_string_lossy());
    let rt_hb_dir_q = shell_quote(
        &runtime_heartbeat_path
            .parent()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default(),
    );
    let cmd_q = shell_quote(command);
    let label_q = shell_quote(label);
    let exe_q = shell_quote(exe_path);
    // Inner command strings passed as a single argument to `script -q
    // -f -c <STR>` (or to `bash -c <STR>` in the no-PTY fallback). The
    // inner string is what the PTY-wrapped shell will parse, so it
    // must itself be a valid bash command line. We compose it as
    // `bash -c '<user-cmd>'` so the original user command runs in its
    // own bash with no re-quoting needed; then we shell-quote the
    // whole thing ONCE more so the wrapper's `INNER_CMD=<...>`
    // assignment lands a clean literal.
    let inner_cmd_lb = format!("PYTHONUNBUFFERED=1 stdbuf -oL -eL bash -c {cmd_q}");
    let inner_cmd_raw = format!("bash -c {cmd_q}");
    let inner_cmd_lb_q = shell_quote(&inner_cmd_lb);
    let inner_cmd_raw_q = shell_quote(&inner_cmd_raw);
    // When the workload is bound to a queue item (auto-created in
    // `cmd_run` OR explicitly supplied via --queue-id), append the qid
    // to the emit-done call so the wrapper-side emit carries the qid
    // into the workload-done event AND triggers the queue done/abandon
    // transition. Bare (--no-queue / auto-create disabled) workloads
    // emit the legacy event with no qid and no queue side effect.
    let queue_id_emit_arg = match queue_id {
        Some(qid) => format!(" --queue-id {}", shell_quote(qid)),
        None => String::new(),
    };
    // Per-line ISO8601 timestamp prefix. Two paths:
    //   1. `ts` from moreutils if installed — fast, native.
    //   2. Pure-bash fallback — a `while IFS= read -r line` loop calling
    //      `date -Is` per line. Slower (one fork per line) but no extra
    //      dependency. `IFS=` + `-r` preserves whitespace and backslashes
    //      verbatim.
    // The detection is in the wrapper itself so the same script works
    // on hosts with or without moreutils installed.
    format!(
        "#!/bin/bash\n\
         # Trap SIGINT/SIGTERM — fatfinger-proof against accidental Ctrl-C.\n\
         # Use `trap :` (no-op handler) NOT `trap ''` (SIG_IGN). SIG_IGN\n\
         # persists across exec into all child processes — including the\n\
         # heartbeat sidecar — so `kill -TERM` to that sidecar would be\n\
         # silently ignored. POSIX: signals inherited as SIG_IGN cannot\n\
         # be reset by the child via `trap`. A `:` handler exec-resets\n\
         # to SIG_DFL in children. Verified during q-2026-05-05-8aae\n\
         # bring-up — without this, the EXIT-trap kill of the heartbeat\n\
         # sidecar fires but the sidecar lives on with PPID=1.\n\
         trap : INT TERM\n\
         # Send all wrapper-side output (headers, heartbeat-related noise,\n\
         # footer) straight to {out_q}. NOTE: we deliberately do NOT pipe\n\
         # through `ts | tee` here — `ts` reads line-by-line and would\n\
         # buffer the user command's `\\r`-separated progress frames\n\
         # (rsync --progress, curl, pv, wget --progress) until a `\\n`\n\
         # arrived, defeating the live-tail SSE path. The user command\n\
         # itself runs under `script -q -f` further down, which both\n\
         # gives it a PTY (so rsync etc. emit progress at all) and\n\
         # flushes after every write. Header / footer lines don't need\n\
         # timestamps — queue-minisite stamps each SSE wire frame\n\
         # server-side, and no downstream consumer parses .output\n\
         # timestamps.\n\
         exec >> {out_q} 2>&1\n\
         echo '=== workload: {label} ==='\n\
         echo 'Started: '$(date -Iseconds)\n\
         echo 'Command: {command_escaped}'\n\
         echo '---'\n\
         # Pet-or-fire watchdog: touch the heartbeat file every\n\
         # ${{WORKLOAD_HEARTBEAT_INTERVAL_SECS:-900}} seconds (default 15 min)\n\
         # while the user command runs. NO claude-event emitted on heartbeat —\n\
         # absence-of-heartbeat is the signal. cron-workload-stale-check\n\
         # detects mtime > 1h + no .exit file and fires workload-stale.\n\
         # Set WORKLOAD_HEARTBEAT=0 to disable (e.g. for tests).\n\
         #\n\
         # Spawn the sidecar via `setsid` so it runs in its OWN process\n\
         # group. We then kill the whole group on teardown (`kill -TERM\n\
         # -<pgid>`), which reliably reaps both the loop subshell AND\n\
         # any in-flight `sleep` child. Without setsid, killing just\n\
         # $! often leaves a dangling `sleep` that wakes up and writes\n\
         # one more heartbeat, giving false-positive freshness to the\n\
         # stale-watchdog detector.\n\
         HEARTBEAT_PID=\n\
         if [ \"${{WORKLOAD_HEARTBEAT:-1}}\" != \"0\" ]; then\n\
             # Touch immediately so a fresh-arrival check has a non-empty file.\n\
             # Use write-tmp + atomic mv so a concurrent reader never sees a\n\
             # post-truncate / pre-write empty file (real prod race; readers\n\
             # like cron-workload-stale-check parse this body as ISO8601).\n\
             date -Iseconds > {hb_q}.tmp 2>/dev/null && mv -f {hb_q}.tmp {hb_q} 2>/dev/null || true\n\
             # Pass the heartbeat path via env var so we don't have to\n\
             # nest single-quoted shell-escape inside the outer\n\
             # `bash -c '...'` (which would close the outer quote and\n\
             # break the loop body — see q-2026-05-05-8aae bring-up).\n\
             WORKLOAD_HB_FILE={hb_q} setsid bash -c 'while true; do\n\
                 sleep \"${{WORKLOAD_HEARTBEAT_INTERVAL_SECS:-900}}\"\n\
                 date -Iseconds > \"$WORKLOAD_HB_FILE.tmp\" 2>/dev/null && mv -f \"$WORKLOAD_HB_FILE.tmp\" \"$WORKLOAD_HB_FILE\" 2>/dev/null || true\n\
               done' </dev/null >/dev/null 2>&1 &\n\
             HEARTBEAT_PID=$!\n\
         fi\n\
         # Runtime heartbeat (progress-driven). Consumed by the\n\
         # claude-watch daemon's stuck-detection suppression path — see\n\
         # `policy::workload_heartbeat_fresh`. While ANY workload's file\n\
         # under {rt_hb_dir_q} has mtime within the daemon's\n\
         # `workload_heartbeat_max_age_secs` window (default 60s), the\n\
         # daemon SUPPRESSES heartbeat-stale + prolonged-thinking alerts\n\
         # on the assumption the main loop is legitimately waiting on an\n\
         # out-of-band long-running workload that is MAKING PROGRESS.\n\
         #\n\
         # PROGRESS = the workload's combined stdout+stderr (everything\n\
         # the wrapper has already redirected to {out_q} via the\n\
         # `exec >> {out_q} 2>&1` above) is GROWING. Polling the file\n\
         # size on a fixed interval (default 30s) is dirt-cheap, picks up\n\
         # both line- and progress-frame (`\\r`) writes the wrapped\n\
         # command makes, and naturally debounces (one heartbeat touch\n\
         # per interval at most, regardless of output rate).\n\
         #\n\
         # If the wrapped command hangs (no new bytes for N intervals)\n\
         # the heartbeat goes stale -> daemon suppression lifts -> the\n\
         # real stuck state surfaces. This is the intended behavior --\n\
         # the original PR #208 design was a dumb timer that touched the\n\
         # heartbeat unconditionally, giving false-confidence whenever\n\
         # the wrapper was alive but its child had hung.\n\
         #\n\
         # The initial touch BEFORE the poll loop covers warm-up: a\n\
         # daemon check that races the very-first second of the workload\n\
         # sees a fresh heartbeat even if the wrapped command hasn't\n\
         # emitted anything yet.\n\
         #\n\
         # Set WORKLOAD_RUNTIME_HEARTBEAT=0 to disable (e.g. for tests).\n\
         # Separate sidecar PID + separate trap so the legacy 15-min\n\
         # heartbeat above is unaffected by changes here.\n\
         #\n\
         # Edge case (intentionally NOT papered over): a workload that\n\
         # legitimately runs silent for long stretches (e.g. a `sleep\n\
         # 600` in a script) will trip stuck-detection. That's a design\n\
         # fact, not a bug -- if the operator wants suppression during\n\
         # silent stretches the workload itself should emit periodic\n\
         # progress lines.\n\
         RUNTIME_HEARTBEAT_PID=\n\
         if [ \"${{WORKLOAD_RUNTIME_HEARTBEAT:-1}}\" != \"0\" ]; then\n\
             # Ensure the runtime heartbeat dir exists. `/run/claude/` is\n\
             # uid-1000 owned tmpfs in prod, but be defensive — if mkdir\n\
             # fails (e.g. running under a different uid in a test rig)\n\
             # silently skip the runtime heartbeat. Fail-soft: the\n\
             # workload still runs, only daemon suppression is degraded.\n\
             if mkdir -p {rt_hb_dir_q} 2>/dev/null; then\n\
                 # Initial touch + atomic mv mirrors the slow heartbeat\n\
                 # — a daemon check on the same tick reads a non-empty\n\
                 # file with a current mtime. Covers the warm-up window\n\
                 # before the wrapped command produces output.\n\
                 date -Iseconds > {rt_hb_q}.tmp 2>/dev/null && mv -f {rt_hb_q}.tmp {rt_hb_q} 2>/dev/null || true\n\
                 # Pass paths via env vars so the inner `bash -c '...'`\n\
                 # body doesn't have to nest single-quoted shell-escape\n\
                 # (same trick as the slow-heartbeat sidecar above).\n\
                 # `WORKLOAD_RT_HB_OUTPUT` is the workload's combined\n\
                 # stdout+stderr file; the sidecar polls its size and\n\
                 # only re-touches the heartbeat when it has grown.\n\
                 # `stat -c %s` is the cheap path; if the output file is\n\
                 # missing (e.g. racy startup) we treat size as 0 and\n\
                 # try again on the next tick.\n\
                 WORKLOAD_RT_HB_FILE={rt_hb_q} WORKLOAD_RT_HB_OUTPUT={out_q} setsid bash -c '\n\
                     prev_size=$(stat -c %s \"$WORKLOAD_RT_HB_OUTPUT\" 2>/dev/null || echo 0)\n\
                     while true; do\n\
                         sleep \"${{WORKLOAD_RUNTIME_HEARTBEAT_INTERVAL_SECS:-30}}\"\n\
                         cur_size=$(stat -c %s \"$WORKLOAD_RT_HB_OUTPUT\" 2>/dev/null || echo 0)\n\
                         if [ \"$cur_size\" != \"$prev_size\" ]; then\n\
                             date -Iseconds > \"$WORKLOAD_RT_HB_FILE.tmp\" 2>/dev/null && mv -f \"$WORKLOAD_RT_HB_FILE.tmp\" \"$WORKLOAD_RT_HB_FILE\" 2>/dev/null || true\n\
                             prev_size=$cur_size\n\
                         fi\n\
                     done\n\
                 ' </dev/null >/dev/null 2>&1 &\n\
                 RUNTIME_HEARTBEAT_PID=$!\n\
             fi\n\
         fi\n\
         # Reap BOTH heartbeat sidecars on any wrapper exit (normal, signal, or\n\
         # tmux kill-pane). Without this the sidecars leak and keep petting\n\
         # the watchdog after the workload has died — exactly the case we\n\
         # want to detect. EXIT pseudo-signal fires unconditionally. Kill the\n\
         # whole process group (negative pid) so any in-flight `sleep` dies\n\
         # alongside the loop subshell. Also delete the runtime heartbeat\n\
         # FILE so the daemon's stuck-detection sees no leftover freshness\n\
         # (a stale mtime would self-correct via the max-age threshold,\n\
         # but explicit cleanup keeps the dir tidy + makes the test\n\
         # assertion deterministic).\n\
         trap '\n\
           if [ -n \"$HEARTBEAT_PID\" ]; then kill -TERM -\"$HEARTBEAT_PID\" 2>/dev/null || kill \"$HEARTBEAT_PID\" 2>/dev/null || true; fi\n\
           if [ -n \"$RUNTIME_HEARTBEAT_PID\" ]; then kill -TERM -\"$RUNTIME_HEARTBEAT_PID\" 2>/dev/null || kill \"$RUNTIME_HEARTBEAT_PID\" 2>/dev/null || true; fi\n\
           rm -f {rt_hb_q} {rt_hb_q}.tmp 2>/dev/null || true\n\
         ' EXIT\n\
         # Force line-buffered stdio for the workload command's stdout+stderr.\n\
         # Without this, programs whose stdout is a pipe (everything here, since\n\
         # we redirect through `>(ts | tee)`) flip to BLOCK buffering by\n\
         # default — `printf`/`puts` accumulate 4-8KB before flushing. The\n\
         # .output file then grows in chunks and the SSE tail through\n\
         # queue-minisite arrives in chunks at the browser.\n\
         #\n\
         # Two layers, since different runtimes use different buffers:\n\
         #   * `stdbuf -oL -eL` flips libc stdio to line-buffered via\n\
         #     `LD_PRELOAD=libstdbuf.so`. Covers C/C++ programs using\n\
         #     libc stdio (printf, fwrite, puts) — Bash builtins, most\n\
         #     coreutils, common CLI tools. LD_PRELOAD propagates to\n\
         #     children so a single wrap at the outer `bash` covers the\n\
         #     whole subtree.\n\
         #   * `PYTHONUNBUFFERED=1` (env, inherited by children). Python\n\
         #     uses its OWN io buffer, not libc stdio, so stdbuf is a\n\
         #     no-op for it; this env var is the Python-specific flip.\n\
         #     `PYTHONUNBUFFERED=1` is equivalent to `python -u`.\n\
         #\n\
         # Neither helps Rust/Go binaries that bypass libc (they use\n\
         # direct write() syscalls + their own buffers) or programs\n\
         # that explicitly call setvbuf() — but it's the right default\n\
         # for the common case (Python, shell, C tools).\n\
         # WORKLOAD_LINE_BUFFER=0 opts out of both.\n\
         #\n\
         # PTY wrap: run the user command inside `script -q -f -e -c '...'\n\
         # /dev/null` so it sees a real PTY on stdout. This is the only\n\
         # way to get `\\r`-separated progress frames (rsync --progress,\n\
         # curl, wget --progress, pv) into the .output file in real\n\
         # time — without a PTY, rsync silently suppresses progress and\n\
         # curl/pv switch to `\\n`-terminated summary output. `-q` quiets\n\
         # script's own start/done banner; `-f` flushes the PTY output\n\
         # after every write so bytes hit fd1 immediately; `-e`/`--return`\n\
         # makes `script` exit with the child's exit code so the\n\
         # wrapper's `EC=$?` captures the user command's real rc instead\n\
         # of always seeing 0 (without `-e`, script's own success masks\n\
         # `false`/`exit 7`/etc., which then mis-routes the queue\n\
         # transition to `done` rather than `abandoned`). The typescript\n\
         # argument is /dev/null — we don't want script's typescript\n\
         # file, we just want its PTY-to-stdout passthrough.\n\
         # WORKLOAD_PTY=0 opts out (e.g. for tests that want a non-tty\n\
         # stdout, or commands that misbehave under a PTY).\n\
         # Build the inner command string. `script -q -f -e -c <STR>` takes\n\
         # ONE string argument that gets passed to /bin/sh -c, so we need\n\
         # to assemble the line-buffer prefix + the user command into a\n\
         # single bash-parseable string. Rust assembles `INNER_CMD_LB`\n\
         # (with stdbuf wrap) and `INNER_CMD_RAW` (without) at template-\n\
         # render time; the wrapper picks one at runtime.\n\
         if [ \"${{WORKLOAD_LINE_BUFFER:-1}}\" != \"0\" ] && command -v stdbuf >/dev/null 2>&1; then\n\
             INNER_CMD={inner_cmd_lb_q}\n\
         else\n\
             INNER_CMD={inner_cmd_raw_q}\n\
         fi\n\
         if [ \"${{WORKLOAD_PTY:-1}}\" != \"0\" ] && command -v script >/dev/null 2>&1; then\n\
             setsid --wait script -q -f -e -c \"$INNER_CMD\" /dev/null\n\
         else\n\
             setsid --wait bash -c \"$INNER_CMD\"\n\
         fi\n\
         EC=$?\n\
         echo ''\n\
         echo \"=== DONE (exit $EC) at $(date -Iseconds) ===\"\n\
         echo $EC > {exit_q}\n\
         # Stop both heartbeats BEFORE emit-done so the .exit + stop happen tightly.\n\
         # The runtime heartbeat goes first so the daemon's next stuck-check\n\
         # immediately sees no fresh proof-of-life (no risk of a one-tick\n\
         # window where the workload is done but suppression still active).\n\
         if [ -n \"$RUNTIME_HEARTBEAT_PID\" ]; then kill -TERM -\"$RUNTIME_HEARTBEAT_PID\" 2>/dev/null || kill \"$RUNTIME_HEARTBEAT_PID\" 2>/dev/null || true; fi\n\
         rm -f {rt_hb_q} {rt_hb_q}.tmp 2>/dev/null || true\n\
         if [ -n \"$HEARTBEAT_PID\" ]; then kill -TERM -\"$HEARTBEAT_PID\" 2>/dev/null || kill \"$HEARTBEAT_PID\" 2>/dev/null || true; fi\n\
         # Emit claude-event for the main loop. Default-open: any failure\n\
         # here is silently swallowed — the exit-file write above is the\n\
         # source of truth for `workload wait`.\n\
         {exe_q} workload emit-done --label {label_q} --exit-code \"$EC\" --log-path {out_q}{queue_id_emit_arg} >/dev/null 2>&1 || true\n\
         sleep 30\n",
        // The "Command: " line gets the unquoted version for readability;
        // escape single quotes for the heredoc context.
        command_escaped = command.replace('\'', "'\\''"),
        inner_cmd_lb_q = inner_cmd_lb_q,
        inner_cmd_raw_q = inner_cmd_raw_q,
    )
}

/// CLI: `workload run <label> [--queue-id q-X | --no-queue] -- <command...>`
///
/// **Workloads are first-class queue items by default** (Andrew DM
/// 2026-05-04 21:02 ET). When neither `--queue-id` nor `--no-queue` is
/// passed, `cmd_run` auto-creates a queue row via `session-task queue
/// add --force-enqueue` (scope `workload:<label>`, summary derived from
/// the command), atomically `register`s it, and binds the resulting qid
/// to the workload — so the workload appears in `session-task queue
/// list` alongside agent queue items, and on workload exit the queue
/// item transitions to `done` (rc==0) or `abandoned` (rc!=0 / killed).
///
/// Explicit modes:
///   * `--queue-id q-X` — bind to an existing queue item (caller has
///     already added/registered it). Auto-create is skipped; the qid is
///     used as-is.
///   * `--no-queue` — opt out entirely. The workload runs without a
///     queue row (legacy behaviour). Use this when the caller knows the
///     queue layer is unavailable, or for short throwaway workloads
///     that shouldn't pollute the queue history.
///   * Neither — auto-create a queue row tied to the workload.
///
/// Auto-create is fail-soft: if `session-task` is missing or returns
/// non-zero, the workload still runs — only the queue side effect is
/// skipped. Suppression knob: `WORKLOAD_QUEUE_AUTO_CREATE=0` (env)
/// disables auto-create globally without touching CLI args. Used by
/// tests + by environments without `session-task` installed.
pub fn cmd_run(
    label: &str,
    cmd_args: &[String],
    queue_id: Option<&str>,
    no_queue: bool,
) -> i32 {
    if cmd_args.is_empty() {
        eprintln!("No command specified");
        return 1;
    }
    let command: String = cmd_args
        .iter()
        .map(|a| shell_quote(a))
        .collect::<Vec<_>>()
        .join(" ");

    if !session_exists() {
        eprintln!("No '{SESSION}' tmux session. Run: claude-watch task init");
        return 1;
    }

    // Resolve effective queue id. Precedence:
    //   1. Caller-supplied --queue-id wins (existing behaviour).
    //   2. --no-queue opts out entirely (no auto-create).
    //   3. WORKLOAD_QUEUE_AUTO_CREATE=0 env opts out (test escape hatch).
    //   4. Otherwise: auto-create + register a queue row, bind qid.
    let caller_supplied_qid = queue_id.is_some();
    let mut effective_queue_id: Option<String> = queue_id.map(str::to_string);
    let auto_create_disabled = std::env::var("WORKLOAD_QUEUE_AUTO_CREATE")
        .ok()
        .as_deref()
        == Some("0");
    if effective_queue_id.is_none() && !no_queue && !auto_create_disabled {
        match auto_create_and_register_queue_item(label, &command) {
            Ok(qid) => {
                println!("Bound workload '{label}' to queue item {qid}");
                effective_queue_id = Some(qid);
            }
            Err(e) => {
                // Fail-soft: log and continue without a queue row. The
                // workload still runs; only the queue side effect is
                // skipped. This matches the contract that workloads are
                // resilient to queue-layer outages (the queue is a
                // visibility layer, not a hard prerequisite).
                eprintln!(
                    "warning: workload queue auto-register failed (running without queue row): {e}"
                );
            }
        }
    }

    // When the caller supplied --queue-id (i.e. the queue item already
    // existed BEFORE this workload was launched, typically with a
    // non-workload scope like `resource:...`), inject `workload:<label>`
    // onto that item's scope. The work-queue-exporter reads this token
    // to locate the heartbeat file at `/run/claude/workloads/<label>.heartbeat`
    // and emit `worktask_queue_progress_age_seconds`. Without it the
    // exporter never finds the heartbeat → no progress_age series →
    // `WorkQueueStuck` + `WorkQueueStuckSoft` false-fire after 1h on
    // healthy long-running workloads (q-2026-05-20-13b9 trigger).
    //
    // Skipped on the auto-create path because that path's scope
    // already includes the token by construction.
    //
    // Skipped on the test escape hatch (`WORKLOAD_QUEUE_AUTO_CREATE=0`)
    // when the caller also supplied a qid — both opt-outs should
    // collapse into "don't touch session-task" behaviour for the unit-
    // test harness.
    let inject_disabled = std::env::var("WORKLOAD_QUEUE_AUTO_CREATE")
        .ok()
        .as_deref()
        == Some("0");
    if caller_supplied_qid && !inject_disabled {
        if let Some(ref qid) = effective_queue_id {
            inject_workload_scope_token(label, qid);
        }
    }

    if let Err(e) = fs::create_dir_all(WORKLOAD_DIR) {
        eprintln!("Failed to create {WORKLOAD_DIR}: {e}");
        return 1;
    }
    // Best-effort: maintain the legacy `/tmp/claude-workloads` path as
    // a symlink so out-of-tree consumers (docker bind-mount,
    // cron-workload-stale-check) keep working without a coordinated
    // multi-repo deploy. Lazy + idempotent — see helper docs.
    ensure_legacy_compat_symlink();

    let out_path = output_file(label);
    let exit_path = exit_file(label);
    let heartbeat_path = heartbeat_file(label);
    let runtime_heartbeat_path = runtime_heartbeat_file(label);
    let script_path = script_file(label);
    let script_capture_path = script_capture_file(label);

    // Clean up previous run's exit marker + output + heartbeats. The
    // heartbeats MUST be removed up-front so neither the cron-stale
    // detector nor the daemon's stuck-suppression check can get a
    // false-positive on a stale leftover from a prior run that pet the
    // watchdog and then crashed.
    // Also remove any prior script-capture sidecar so a re-run that no
    // longer matches the interpreter pattern doesn't surface a stale
    // capture from the previous invocation.
    let _ = fs::remove_file(&exit_path);
    let _ = fs::remove_file(&out_path);
    let _ = fs::remove_file(&heartbeat_path);
    let _ = fs::remove_file(&runtime_heartbeat_path);
    let _ = fs::remove_file(&script_capture_path);

    // Try to capture the script content NOW (before the workload
    // starts) so a later modify/delete of the script doesn't affect
    // what the modal shows. Fail-soft: no capture means the modal
    // omits the "Script contents" section.
    if let Some(cap) = try_capture_script(cmd_args) {
        write_script_capture(label, &cap);
    }
    // Also clear the cron-workload-stale-check single-emit sentinel so
    // a freshly-started workload that legitimately stalls again will
    // re-fire workload-stale instead of being silently swallowed.
    let alerted_path = PathBuf::from(WORKLOAD_DIR).join(format!("{label}.heartbeat.alerted"));
    let _ = fs::remove_file(&alerted_path);

    // Kill existing workload with same label
    let mut state = load_state();
    if let Some(entry) = state.get(label) {
        if pane_alive(&entry.pane_id) {
            let _ = Command::new("tmux")
                .args(["kill-pane", "-t", &entry.pane_id])
                .output();
        }
        state.remove(label);
        let _ = save_state(&state);
    }

    // Wrapper script — identical layout to Python version, plus a
    // claude-event emit step after the exit-code is written so the main
    // loop's `claude-event-watch` learns about the completion without
    // needing a separate `workload wait` background task.
    //
    // The emit invokes the claude-watch binary itself (this process's
    // current_exe path baked in at run time) via the hidden `workload
    // emit-done` subcommand. We embed the absolute path so the wrapper
    // doesn't depend on PATH discovery inside tmux.
    let exe_path = std::env::current_exe()
        .ok()
        .and_then(|p| p.to_str().map(str::to_string))
        .unwrap_or_else(|| "claude-watch".to_string());
    let script = build_wrapper_script(
        label,
        &command,
        &out_path,
        &exit_path,
        &heartbeat_path,
        &runtime_heartbeat_path,
        &exe_path,
        effective_queue_id.as_deref(),
    );

    if let Err(e) = fs::write(&script_path, script) {
        eprintln!("Failed to write script: {e}");
        return 1;
    }
    let _ = fs::set_permissions(&script_path, fs::Permissions::from_mode(0o700));

    // Create pane running the script. We invoke as `bash <path>`
    // rather than executing the script directly because the script
    // lives under `/var/run/claude/workload-state/` and `/run` is
    // mounted `noexec` on most Linux distros — direct `exec` of a
    // file on a noexec mount fails with EACCES even when the file
    // has the +x bit. Passing the path as a bash arg side-steps
    // that: bash reads-and-interprets the file, which only needs
    // read permission on the underlying inode. (The +x bit set via
    // `set_permissions` above is now belt-and-suspenders for any
    // operator who runs the script directly from a shell.)
    let script_path_str = script_path.to_string_lossy().to_string();
    let out = Command::new("tmux")
        .args([
            "split-window",
            "-t",
            SESSION,
            "-v",
            "-P",
            "-F",
            "#{pane_id}",
            "bash",
            &script_path_str,
        ])
        .output();
    let pane_id = match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        Ok(o) => {
            eprintln!(
                "Failed to create pane: {}",
                String::from_utf8_lossy(&o.stderr)
            );
            return 1;
        }
        Err(e) => {
            eprintln!("Failed to create pane: {e}");
            return 1;
        }
    };

    rebalance();

    let started_at = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string();
    state.insert(
        label.to_string(),
        WorkloadEntry {
            pane_id: pane_id.clone(),
            command: command.clone(),
            output: out_path.to_string_lossy().to_string(),
            started_at,
            queue_id: effective_queue_id.clone(),
        },
    );
    let _ = save_state(&state);

    println!("Started workload '{label}' in pane {pane_id}");
    println!("Output: {}", out_path.display());
    println!(
        "Watch for the `workload-done` claude-event in the next \
         UserPromptSubmit context (fire-and-forget). Do NOT spawn \
         `workload wait` as a background task."
    );
    0
}

/// CLI: `workload list`
pub fn cmd_list() -> i32 {
    let state = load_state();
    if state.is_empty() {
        println!("No workloads");
        return 0;
    }
    for (label, info) in &state {
        let alive = pane_alive(&info.pane_id);
        let exit_code = read_exit_code(label);
        let status = if alive {
            "running".to_string()
        } else if let Some(ec) = exit_code {
            format!("done (exit {ec})")
        } else {
            "dead".to_string()
        };
        println!(
            "  {:24}  {:6}  [{}]  started {}",
            label, info.pane_id, status, info.started_at
        );
        println!("    {}", info.command);
    }
    0
}

/// CLI: `workload wait <label> [--force-i-acknowledge-events-are-better]`
///
/// Disabled by default. Workloads emit a `workload-done` claude-event when
/// they exit; that event arrives in the main loop's next UserPromptSubmit
/// context via the claude-event hook chain, so blocking polling via
/// `workload wait` is fully redundant and ties up a Claude Code background
/// task slot. Returns exit code 2 with an explanatory error unless the
/// user has explicitly opted in via the long flag.
pub fn cmd_wait(label: &str, lines: usize, force_acknowledged: bool) -> i32 {
    if !force_acknowledged {
        eprintln!(
            "ERROR: `workload wait` is disabled by default.\n\
             \n\
             Workloads emit a `workload-done` claude-event when they exit.\n\
             That event surfaces in the main loop's next UserPromptSubmit\n\
             context, so blocking polling via `workload wait` is redundant\n\
             and only clutters the Claude Code background task list.\n\
             \n\
             Recommended pattern: fire-and-forget the workload\n\
             (`workload run <label> -- <cmd>`) and watch for the\n\
             `workload-done` claude-event on the next turn.\n\
             \n\
             If you genuinely need the blocking-poll behavior, opt in:\n\
             \n\
             \tworkload wait {label} --force-i-acknowledge-events-are-better\n\
             \n\
             See feedback_no-explicit-task-watchers.md for the full rule."
        );
        return 2;
    }

    let state = load_state();
    let info = match state.get(label) {
        Some(i) => i.clone(),
        None => {
            eprintln!("No workload '{label}'");
            return 1;
        }
    };

    let exit_path = exit_file(label);
    if exit_path.exists() {
        let ec = read_exit_code(label).unwrap_or(1);
        println!("Workload '{label}' already completed (exit {ec})");
        print_tail(Path::new(&info.output), lines);
        return ec;
    }

    println!("Waiting for workload '{label}' to complete...");

    loop {
        if exit_path.exists() {
            break;
        }
        if !pane_alive(&info.pane_id) {
            // Give a moment for the exit file to appear
            std::thread::sleep(Duration::from_secs(1));
            break;
        }
        std::thread::sleep(Duration::from_secs(5));
    }

    if exit_path.exists() {
        let ec = read_exit_code(label).unwrap_or(1);
        println!("\n=== Workload '{label}' completed (exit {ec}) ===");
        ec
    } else {
        println!("\n=== Workload '{label}' pane died without exit code ===");
        1
    }
}

/// CLI: `workload log <label>`
pub fn cmd_log(label: &str, lines: usize, follow: bool) -> i32 {
    let state = load_state();
    let info = match state.get(label) {
        Some(i) => i.clone(),
        None => {
            eprintln!("No workload '{label}'");
            return 1;
        }
    };
    let path = PathBuf::from(&info.output);
    if !path.exists() {
        eprintln!("No output file: {}", path.display());
        return 1;
    }
    if follow {
        // exec tail -f
        use std::os::unix::process::CommandExt;
        let err = Command::new("tail")
            .args(["-f", "-n", &lines.to_string()])
            .arg(&path)
            .exec();
        eprintln!("exec tail failed: {err}");
        1
    } else {
        print_tail(&path, lines);
        0
    }
}

/// CLI: `workload kill <label>`
pub fn cmd_kill(label: &str) -> i32 {
    let mut state = load_state();
    let info = match state.get(label) {
        Some(i) => i.clone(),
        None => {
            eprintln!("No workload '{label}'");
            return 1;
        }
    };

    // If the wrapper script already wrote its exit file, it also
    // already emitted (or will emit before its 30s sleep ends). Skip
    // our kill-event emit to keep the contract "exactly one event per
    // workload run". Only synthesise a kill event when we're racing
    // ahead of a still-alive wrapper.
    let exit_path = exit_file(label);
    let already_exited = exit_path.exists();

    if pane_alive(&info.pane_id) {
        if !already_exited {
            // Synthesise the exit marker so subsequent `workload wait`
            // calls return cleanly with the kill code, and emit the
            // claude-event before tearing down the pane. We route this
            // through `cmd_emit_done` — NOT a bare `emit_workload_done` —
            // so a queue-bound workload also gets its queue item
            // transitioned to `abandoned` here. The wrapper would
            // normally do that itself on natural exit, but `cmd_kill`
            // SIGKILLs the wrapper before its `emit-done` step runs, so
            // without this the queue item gets stranded in `running`
            // forever (Andrew DM 2026-05-13: rc=-15 event arrived but
            // the queue UI still showed `running`).
            let _ = fs::write(&exit_path, "-15\n");
            cmd_emit_done(
                label,
                -15,
                &info.output,
                true,
                info.queue_id.as_deref(),
            );
        }
        kill_pane_tree(&info.pane_id);
        let _ = Command::new("tmux")
            .args(["kill-pane", "-t", &info.pane_id])
            .output();
        println!("Killed workload '{label}' (pane {})", info.pane_id);
    } else {
        println!("Workload '{label}' already dead");
    }
    state.remove(label);
    let _ = save_state(&state);
    rebalance();
    0
}

/// CLI (hidden): `workload emit-done --label X --exit-code N --log-path P [--killed] [--queue-id q-X]`.
/// Invoked by the wrapper script after the workload exits. Keeps the
/// emit logic in Rust (testable, dep-free) instead of in bash.
///
/// When `queue_id` is set, the workload is treated as a FIRST-CLASS
/// queue item (Andrew DM 2026-05-03 05:23 ET). On exit:
///   * the `workload-done` event carries the qid in `data.queue_id`;
///   * the queue item is transitioned to `done` (rc==0 + not killed)
///     or `abandoned` (non-zero rc OR killed) via `session-task`.
///
/// No respawn-event or mandatory-obligation: workload completion IS
/// queue completion. The main loop sees the canonical `queue-done` /
/// `queue-abandoned` claude-event when `session-task` performs the
/// transition.
///
/// The queue-transition step is best-effort: failure (CLI not on
/// PATH, session-task non-zero) is logged at warn level and
/// swallowed. The `workload-done` event is emitted regardless.
/// Suppression knob: `WORKLOAD_QUEUE_TRANSITION=0` (env) skips the
/// queue call entirely (used by tests).
pub fn cmd_emit_done(
    label: &str,
    exit_code: i32,
    log_path: &str,
    killed: bool,
    queue_id: Option<&str>,
) -> i32 {
    emit_workload_done(&WorkloadDoneEvent {
        label,
        exit_code,
        killed,
        log_path,
        queue_id,
    });
    if let Some(qid) = queue_id {
        transition_queue_item_for_workload(qid, label, exit_code, killed, log_path);
    }
    0
}

/// Mark the queue item bound to this workload as `done` (clean exit)
/// or `abandoned` (non-zero rc / killed). Best-effort; never fails the
/// caller. Suppression knob: `WORKLOAD_QUEUE_TRANSITION=0`. CLI
/// override: `SESSION_TASK_CLI` (used by tests to point at a stub).
///
/// Mapping rationale:
///   * rc==0 && !killed → `session-task queue done <qid>` (success)
///   * killed           → `session-task queue abandon <qid> --reason ...`
///   * other rc != 0    → `session-task queue abandon <qid> --reason ...`
///
/// `session-task queue done` already emits the `queue-done` claude-
/// event; `queue abandon` emits `queue-abandoned`. Either way the main
/// loop sees the canonical lifecycle event without us inventing a new
/// tag. First-class workload model — Andrew DM 2026-05-03 05:23 ET.
fn transition_queue_item_for_workload(
    queue_id: &str,
    label: &str,
    exit_code: i32,
    killed: bool,
    log_path: &str,
) {
    if std::env::var("WORKLOAD_QUEUE_TRANSITION")
        .ok()
        .as_deref()
        == Some("0")
    {
        return;
    }
    let cli = match find_session_task_cli() {
        Some(p) => p,
        None => {
            tracing::warn!(
                queue_id = %queue_id,
                label = %label,
                "workload queue transition: session-task CLI not found, skipping"
            );
            return;
        }
    };

    let args: Vec<String> = if exit_code == 0 && !killed {
        vec![
            "queue".to_string(),
            "done".to_string(),
            queue_id.to_string(),
            "--silent".to_string(),
        ]
    } else {
        let reason = if killed {
            format!("workload {label} killed (rc={exit_code}, log={log_path})")
        } else {
            format!(
                "workload {label} exited non-zero rc={exit_code} (log={log_path})"
            )
        };
        vec![
            "queue".to_string(),
            "abandon".to_string(),
            queue_id.to_string(),
            "--reason".to_string(),
            reason,
            "--silent".to_string(),
        ]
    };

    let result = std::process::Command::new(&cli)
        .args(&args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::time::{Duration, Instant};
            // 15s timeout — session-task queue done/abandon is
            // normally <500ms but file-locking under load can stretch.
            // A wedged CLI must not stall the wrapper.
            let deadline = Instant::now() + Duration::from_secs(15);
            loop {
                match child.try_wait() {
                    Ok(Some(status)) => return Ok(status),
                    Ok(None) => {
                        if Instant::now() >= deadline {
                            let _ = child.kill();
                            return Err(std::io::Error::new(
                                std::io::ErrorKind::TimedOut,
                                "session-task queue transition timed out (15s)",
                            ));
                        }
                        std::thread::sleep(Duration::from_millis(50));
                    }
                    Err(e) => return Err(e),
                }
            }
        });

    match result {
        Ok(status) if status.success() => {
            tracing::info!(
                queue_id = %queue_id,
                label = %label,
                exit_code = exit_code,
                killed = killed,
                "workload queue transition succeeded"
            );
        }
        Ok(status) => {
            tracing::warn!(
                queue_id = %queue_id,
                label = %label,
                rc = ?status.code(),
                "workload queue transition exited non-zero"
            );
        }
        Err(e) => {
            tracing::warn!(
                queue_id = %queue_id,
                label = %label,
                error = %e,
                "workload queue transition failed"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Serializes tests that mutate process-global env vars (CLAUDE_EVENT_QUEUE,
    // SESSION_TASK_CLI, WORKLOAD_QUEUE_TRANSITION). Without it, parallel test
    // execution interleaves env-var sets and the wrong tempdir / stub path is
    // observed by the function under test. Same pattern as `task_watch::tests`'
    // WORKLOAD_ENV_LOCK. Acquire BEFORE setting any env var; hold for the
    // entire body so the restore-on-drop window is exclusive.
    use std::sync::Mutex;
    static WORKLOAD_TEST_ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn shell_quote_plain() {
        assert_eq!(shell_quote("hello"), "'hello'");
    }

    #[test]
    fn shell_quote_with_apostrophe() {
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
    }

    #[test]
    fn state_roundtrip() {
        let mut s = WorkloadState::new();
        s.insert(
            "foo".to_string(),
            WorkloadEntry {
                pane_id: "%3".to_string(),
                command: "sleep 10".to_string(),
                output: "/tmp/claude-workloads/foo.output".to_string(),
                started_at: "2026-01-01T00:00:00".to_string(),
                queue_id: None,
            },
        );
        let j = serde_json::to_string(&s).unwrap();
        let parsed: WorkloadState = serde_json::from_str(&j).unwrap();
        assert_eq!(parsed["foo"].pane_id, "%3");
        assert_eq!(parsed["foo"].command, "sleep 10");
        assert_eq!(parsed["foo"].queue_id, None);
    }

    #[test]
    fn state_roundtrip_with_queue_id() {
        // First-class workload model: when `workload run --queue-id`
        // is used, the qid is persisted in state.json so `cmd_kill`'s
        // synthesised event also carries it.
        let mut s = WorkloadState::new();
        s.insert(
            "scoped".to_string(),
            WorkloadEntry {
                pane_id: "%4".to_string(),
                command: "sleep 99".to_string(),
                output: "/tmp/claude-workloads/scoped.output".to_string(),
                started_at: "2026-05-03T05:00:00".to_string(),
                queue_id: Some("q-2026-05-03-test".to_string()),
            },
        );
        let j = serde_json::to_string(&s).unwrap();
        let parsed: WorkloadState = serde_json::from_str(&j).unwrap();
        assert_eq!(
            parsed["scoped"].queue_id.as_deref(),
            Some("q-2026-05-03-test")
        );
    }

    #[test]
    fn state_loads_legacy_entry_without_queue_id_field() {
        // Existing state.json files predate the queue_id field — must
        // deserialize cleanly with queue_id=None. (Path doesn't matter
        // here; we're testing JSON shape, not on-disk location.)
        let raw = r#"{"foo":{"pane_id":"%5","command":"x","output":"/tmp/x","started_at":"2026"}}"#;
        let parsed: WorkloadState = serde_json::from_str(raw).expect("legacy parse");
        assert_eq!(parsed["foo"].queue_id, None);
    }

    #[test]
    fn state_loads_missing_file_as_empty() {
        // load_state uses WORKLOAD_DIR which may not exist in CI — should return empty.
        let s = load_state();
        // Just verify no panic and is a BTreeMap
        let _ = s.len();
    }

    #[test]
    fn cmd_wait_without_force_flag_exits_with_code_2() {
        // Bare `workload wait <label>` must short-circuit BEFORE touching
        // any state — Andrew's rule (2026-05-01): the `workload-done`
        // claude-event is the canonical completion signal, polling is
        // redundant. The flag has to be hard to type accidentally.
        let rc = cmd_wait("nonexistent-label", 20, false);
        assert_eq!(
            rc, 2,
            "bare `workload wait` must exit 2 (opt-in required), got {rc}"
        );
    }

    #[test]
    fn cmd_wait_with_force_flag_proceeds_to_state_lookup() {
        // With the opt-in flag set, the gate is bypassed and we fall
        // through to the existing state-lookup code path. For a missing
        // label that yields exit code 1 ("No workload 'X'"), proving the
        // flag actually unblocked the function (versus the gate's exit 2).
        let rc = cmd_wait("definitely-not-a-real-workload-xyz", 20, true);
        assert_eq!(
            rc, 1,
            "opt-in `workload wait` should reach state lookup and exit 1 \
             for missing label, got {rc}"
        );
    }

    #[test]
    fn cmd_emit_done_writes_event_file() {
        // Point CLAUDE_EVENT_QUEUE at a tempdir; cmd_emit_done should
        // produce exactly one workload-done event with the right shape.
        let _lock = WORKLOAD_TEST_ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().expect("tempdir");
        let prev = std::env::var("CLAUDE_EVENT_QUEUE").ok();
        // SAFETY: lock above serializes against peer tests touching
        // the same process-global env vars.
        unsafe {
            std::env::set_var("CLAUDE_EVENT_QUEUE", tmp.path());
        }

        let rc = cmd_emit_done("test-task", 0, "/tmp/foo.output", false, None);
        assert_eq!(rc, 0);

        // Restore env first so any panic below doesn't leak.
        unsafe {
            match prev {
                Some(v) => std::env::set_var("CLAUDE_EVENT_QUEUE", v),
                None => std::env::remove_var("CLAUDE_EVENT_QUEUE"),
            }
        }

        let entries: Vec<_> = std::fs::read_dir(tmp.path())
            .expect("read tempdir")
            .filter_map(Result::ok)
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .ends_with("_workload-done.json")
            })
            .collect();
        assert_eq!(entries.len(), 1, "expected exactly one event");

        let body = std::fs::read_to_string(entries[0].path()).expect("read");
        let v: serde_json::Value = serde_json::from_str(&body).expect("json");
        assert_eq!(v["tag"], "workload-done");
        assert_eq!(v["data"]["label"], "test-task");
        assert_eq!(v["data"]["exit_code"], 0);
        assert_eq!(v["data"]["killed"], false);
        assert_eq!(v["data"]["log_path"], "/tmp/foo.output");
        // Without --queue-id, no queue_id field in event data.
        assert!(v["data"].get("queue_id").is_none());
    }

    #[test]
    fn cmd_emit_done_killed_marker() {
        let _lock = WORKLOAD_TEST_ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().expect("tempdir");
        let prev = std::env::var("CLAUDE_EVENT_QUEUE").ok();
        unsafe {
            std::env::set_var("CLAUDE_EVENT_QUEUE", tmp.path());
        }
        let rc = cmd_emit_done("killed-task", -15, "/tmp/k.output", true, None);
        assert_eq!(rc, 0);
        unsafe {
            match prev {
                Some(v) => std::env::set_var("CLAUDE_EVENT_QUEUE", v),
                None => std::env::remove_var("CLAUDE_EVENT_QUEUE"),
            }
        }

        let entries: Vec<_> = std::fs::read_dir(tmp.path())
            .expect("read tempdir")
            .filter_map(Result::ok)
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .ends_with("_workload-done.json")
            })
            .collect();
        assert_eq!(entries.len(), 1);
        let body = std::fs::read_to_string(entries[0].path()).expect("read");
        let v: serde_json::Value = serde_json::from_str(&body).expect("json");
        assert_eq!(v["data"]["killed"], true);
        assert_eq!(v["data"]["exit_code"], -15);
        assert!(v["message"]
            .as_str()
            .unwrap()
            .contains("workload killed-task killed"));
    }

    #[test]
    fn cmd_emit_done_with_queue_id_carries_qid_in_event() {
        // First-class workload model: --queue-id puts data.queue_id in
        // the event. Suppress the queue transition (no session-task on
        // PATH in CI) — that path is exercised in the stub-CLI test.
        let _lock = WORKLOAD_TEST_ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().expect("tempdir");
        let prev_q = std::env::var("CLAUDE_EVENT_QUEUE").ok();
        let prev_t = std::env::var("WORKLOAD_QUEUE_TRANSITION").ok();
        unsafe {
            std::env::set_var("CLAUDE_EVENT_QUEUE", tmp.path());
            std::env::set_var("WORKLOAD_QUEUE_TRANSITION", "0");
        }

        let rc = cmd_emit_done(
            "qa-task",
            0,
            "/tmp/qa.output",
            false,
            Some("q-2026-05-03-test"),
        );
        assert_eq!(rc, 0);

        unsafe {
            match prev_q {
                Some(v) => std::env::set_var("CLAUDE_EVENT_QUEUE", v),
                None => std::env::remove_var("CLAUDE_EVENT_QUEUE"),
            }
            match prev_t {
                Some(v) => std::env::set_var("WORKLOAD_QUEUE_TRANSITION", v),
                None => std::env::remove_var("WORKLOAD_QUEUE_TRANSITION"),
            }
        }

        let entries: Vec<_> = std::fs::read_dir(tmp.path())
            .expect("read tempdir")
            .filter_map(Result::ok)
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .ends_with("_workload-done.json")
            })
            .collect();
        assert_eq!(entries.len(), 1, "expected one workload-done event");
        let body = std::fs::read_to_string(entries[0].path()).expect("read");
        let v: serde_json::Value = serde_json::from_str(&body).expect("json");
        assert_eq!(v["tag"], "workload-done");
        assert_eq!(v["data"]["queue_id"], "q-2026-05-03-test");
    }

    #[test]
    fn cmd_emit_done_calls_session_task_queue_done_on_clean_exit() {
        // Stub session-task as a recording bash script. Verify it was
        // invoked with `queue done <qid>` when exit_code=0 and
        // killed=false. SESSION_TASK_CLI overrides PATH lookup.
        let _lock = WORKLOAD_TEST_ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().expect("tempdir");
        let prev_q = std::env::var("CLAUDE_EVENT_QUEUE").ok();
        let prev_cli = std::env::var("SESSION_TASK_CLI").ok();
        let prev_t = std::env::var("WORKLOAD_QUEUE_TRANSITION").ok();

        let recording = tmp.path().join("session-task.recording");
        let stub_path = tmp.path().join("session-task-stub");
        let stub = format!(
            "#!/bin/bash\nprintf '%s\\n' \"$@\" > {rec}\nexit 0\n",
            rec = shell_quote(&recording.to_string_lossy()),
        );
        std::fs::write(&stub_path, stub).expect("write stub");
        let _ = std::fs::set_permissions(
            &stub_path,
            std::fs::Permissions::from_mode(0o755),
        );

        unsafe {
            std::env::set_var("CLAUDE_EVENT_QUEUE", tmp.path());
            std::env::set_var("SESSION_TASK_CLI", &stub_path);
            std::env::remove_var("WORKLOAD_QUEUE_TRANSITION");
        }

        let rc = cmd_emit_done(
            "stub-task",
            0,
            "/tmp/stub.output",
            false,
            Some("q-2026-05-03-stub"),
        );

        unsafe {
            match prev_q {
                Some(v) => std::env::set_var("CLAUDE_EVENT_QUEUE", v),
                None => std::env::remove_var("CLAUDE_EVENT_QUEUE"),
            }
            match prev_cli {
                Some(v) => std::env::set_var("SESSION_TASK_CLI", v),
                None => std::env::remove_var("SESSION_TASK_CLI"),
            }
            match prev_t {
                Some(v) => std::env::set_var("WORKLOAD_QUEUE_TRANSITION", v),
                None => std::env::remove_var("WORKLOAD_QUEUE_TRANSITION"),
            }
        }

        assert_eq!(rc, 0);
        assert!(recording.exists(), "stub session-task should have been invoked");
        let recorded = std::fs::read_to_string(&recording).expect("read recording");
        assert!(
            recorded.contains("queue\ndone\nq-2026-05-03-stub"),
            "expected `queue done <qid>` invocation in {recorded}"
        );
        assert!(
            recorded.contains("--silent"),
            "expected --silent flag in {recorded}"
        );
    }

    #[test]
    fn cmd_emit_done_calls_queue_abandon_on_failure() {
        // Stub session-task. Verify `queue abandon <qid> --reason ...`
        // when the workload exits non-zero. Reason should mention the
        // exit code so post-mortem inspection is straightforward.
        let _lock = WORKLOAD_TEST_ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().expect("tempdir");
        let prev_q = std::env::var("CLAUDE_EVENT_QUEUE").ok();
        let prev_cli = std::env::var("SESSION_TASK_CLI").ok();
        let prev_t = std::env::var("WORKLOAD_QUEUE_TRANSITION").ok();

        let recording = tmp.path().join("session-task.recording");
        let stub_path = tmp.path().join("session-task-stub");
        let stub = format!(
            "#!/bin/bash\nprintf '%s\\n' \"$@\" > {rec}\nexit 0\n",
            rec = shell_quote(&recording.to_string_lossy()),
        );
        std::fs::write(&stub_path, stub).expect("write stub");
        let _ = std::fs::set_permissions(
            &stub_path,
            std::fs::Permissions::from_mode(0o755),
        );

        unsafe {
            std::env::set_var("CLAUDE_EVENT_QUEUE", tmp.path());
            std::env::set_var("SESSION_TASK_CLI", &stub_path);
            std::env::remove_var("WORKLOAD_QUEUE_TRANSITION");
        }

        let rc = cmd_emit_done(
            "fail-task",
            7,
            "/tmp/fail.output",
            false,
            Some("q-2026-05-03-fail"),
        );

        unsafe {
            match prev_q {
                Some(v) => std::env::set_var("CLAUDE_EVENT_QUEUE", v),
                None => std::env::remove_var("CLAUDE_EVENT_QUEUE"),
            }
            match prev_cli {
                Some(v) => std::env::set_var("SESSION_TASK_CLI", v),
                None => std::env::remove_var("SESSION_TASK_CLI"),
            }
            match prev_t {
                Some(v) => std::env::set_var("WORKLOAD_QUEUE_TRANSITION", v),
                None => std::env::remove_var("WORKLOAD_QUEUE_TRANSITION"),
            }
        }

        assert_eq!(rc, 0);
        assert!(recording.exists(), "stub should have been invoked");
        let recorded = std::fs::read_to_string(&recording).expect("read recording");
        assert!(
            recorded.contains("queue\nabandon\nq-2026-05-03-fail"),
            "expected `queue abandon <qid>` in {recorded}"
        );
        assert!(recorded.contains("--reason"));
        assert!(
            recorded.contains("rc=7"),
            "abandon reason must mention exit code: {recorded}"
        );
    }

    #[test]
    fn cmd_emit_done_calls_queue_abandon_on_kill() {
        // Killed workloads transition to abandoned with a reason
        // mentioning the kill — symmetric with non-zero exit handling.
        let _lock = WORKLOAD_TEST_ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().expect("tempdir");
        let prev_q = std::env::var("CLAUDE_EVENT_QUEUE").ok();
        let prev_cli = std::env::var("SESSION_TASK_CLI").ok();
        let prev_t = std::env::var("WORKLOAD_QUEUE_TRANSITION").ok();

        let recording = tmp.path().join("session-task.recording");
        let stub_path = tmp.path().join("session-task-stub");
        let stub = format!(
            "#!/bin/bash\nprintf '%s\\n' \"$@\" > {rec}\nexit 0\n",
            rec = shell_quote(&recording.to_string_lossy()),
        );
        std::fs::write(&stub_path, stub).expect("write stub");
        let _ = std::fs::set_permissions(
            &stub_path,
            std::fs::Permissions::from_mode(0o755),
        );

        unsafe {
            std::env::set_var("CLAUDE_EVENT_QUEUE", tmp.path());
            std::env::set_var("SESSION_TASK_CLI", &stub_path);
            std::env::remove_var("WORKLOAD_QUEUE_TRANSITION");
        }

        let rc = cmd_emit_done(
            "killed-task",
            -15,
            "/tmp/killed.output",
            true,
            Some("q-2026-05-03-killed"),
        );

        unsafe {
            match prev_q {
                Some(v) => std::env::set_var("CLAUDE_EVENT_QUEUE", v),
                None => std::env::remove_var("CLAUDE_EVENT_QUEUE"),
            }
            match prev_cli {
                Some(v) => std::env::set_var("SESSION_TASK_CLI", v),
                None => std::env::remove_var("SESSION_TASK_CLI"),
            }
            match prev_t {
                Some(v) => std::env::set_var("WORKLOAD_QUEUE_TRANSITION", v),
                None => std::env::remove_var("WORKLOAD_QUEUE_TRANSITION"),
            }
        }

        assert_eq!(rc, 0);
        let recorded = std::fs::read_to_string(&recording).expect("read recording");
        assert!(
            recorded.contains("queue\nabandon\nq-2026-05-03-killed"),
            "killed → abandon expected: {recorded}"
        );
        assert!(
            recorded.contains("killed"),
            "reason must mention kill: {recorded}"
        );
    }

    #[test]
    fn cmd_emit_done_queue_transition_skipped_by_env() {
        // WORKLOAD_QUEUE_TRANSITION=0 must skip the session-task call
        // entirely (regression safety + test-harness escape hatch).
        let _lock = WORKLOAD_TEST_ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().expect("tempdir");
        let prev_q = std::env::var("CLAUDE_EVENT_QUEUE").ok();
        let prev_cli = std::env::var("SESSION_TASK_CLI").ok();
        let prev_t = std::env::var("WORKLOAD_QUEUE_TRANSITION").ok();

        let recording = tmp.path().join("session-task.recording");
        let stub_path = tmp.path().join("session-task-stub");
        let stub = format!(
            "#!/bin/bash\nprintf '%s\\n' \"$@\" > {rec}\nexit 0\n",
            rec = shell_quote(&recording.to_string_lossy()),
        );
        std::fs::write(&stub_path, stub).expect("write stub");
        let _ = std::fs::set_permissions(
            &stub_path,
            std::fs::Permissions::from_mode(0o755),
        );

        unsafe {
            std::env::set_var("CLAUDE_EVENT_QUEUE", tmp.path());
            std::env::set_var("SESSION_TASK_CLI", &stub_path);
            std::env::set_var("WORKLOAD_QUEUE_TRANSITION", "0");
        }

        let rc = cmd_emit_done(
            "skip-task",
            0,
            "/tmp/skip.output",
            false,
            Some("q-2026-05-03-skip"),
        );

        unsafe {
            match prev_q {
                Some(v) => std::env::set_var("CLAUDE_EVENT_QUEUE", v),
                None => std::env::remove_var("CLAUDE_EVENT_QUEUE"),
            }
            match prev_cli {
                Some(v) => std::env::set_var("SESSION_TASK_CLI", v),
                None => std::env::remove_var("SESSION_TASK_CLI"),
            }
            match prev_t {
                Some(v) => std::env::set_var("WORKLOAD_QUEUE_TRANSITION", v),
                None => std::env::remove_var("WORKLOAD_QUEUE_TRANSITION"),
            }
        }

        assert_eq!(rc, 0);
        assert!(
            !recording.exists(),
            "stub must NOT be invoked when WORKLOAD_QUEUE_TRANSITION=0"
        );
    }

    // ---------------------------------------------------------------
    // Auto-register tests (workloads-as-first-class-queue-items by
    // default — Andrew DM 2026-05-04 21:02 ET).
    //
    // These exercise `auto_create_and_register_queue_item` directly
    // (cmd_run is wedged behind the live `tasks` tmux session, which
    // we don't want to spin up in unit tests). The helper is the
    // single source of truth for the queue-add + register semantics
    // — testing it covers the auto-register path comprehensively.
    // ---------------------------------------------------------------

    /// Build a session-task stub that records argv to `recording` and
    /// emits a fake `queue add --json` payload on stdout when invoked
    /// with `queue add`. `register` invocations succeed silently.
    /// Returns the path to the stub.
    fn write_session_task_stub(
        tmp: &std::path::Path,
        recording: &std::path::Path,
        synth_qid: &str,
    ) -> PathBuf {
        let stub_path = tmp.join("session-task-stub");
        // Append all invocations to recording (one block per call,
        // separated by `---`) so multi-call tests can inspect both
        // the add AND the register call.
        let stub = format!(
            "#!/bin/bash\n\
             {{\n\
               printf '=== invocation ===\\n'\n\
               printf '%s\\n' \"$@\"\n\
             }} >> {rec}\n\
             # When invoked with `queue add`, emit JSON containing the\n\
             # synthesised qid on stdout. Otherwise (`queue register`,\n\
             # etc.) just exit cleanly.\n\
             if [[ \"$1\" == \"queue\" && \"$2\" == \"add\" ]]; then\n\
               printf '{{\"id\":\"%s\",\"group_id\":\"g-test\",\"position\":1,\"ready_now\":true,\"serialized_after\":[],\"depends_on\":[],\"dep_blockers\":[],\"scope\":[\"workload:stub\"],\"running_scope_conflicts\":[],\"spawn_instruction\":\"READY\"}}\\n' '{qid}'\n\
             fi\n\
             exit 0\n",
            rec = shell_quote(&recording.to_string_lossy()),
            qid = synth_qid,
        );
        std::fs::write(&stub_path, stub).expect("write stub");
        let _ = std::fs::set_permissions(
            &stub_path,
            std::fs::Permissions::from_mode(0o755),
        );
        stub_path
    }

    #[test]
    fn auto_create_calls_queue_add_with_workload_scope() {
        // Default-on auto-register: cmd_run with no --queue-id should
        // call `session-task queue add --scope workload:<label> --force-enqueue`.
        let _lock = WORKLOAD_TEST_ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().expect("tempdir");
        let prev_cli = std::env::var("SESSION_TASK_CLI").ok();

        let recording = tmp.path().join("session-task.recording");
        let stub_path =
            write_session_task_stub(tmp.path(), &recording, "q-test-auto-1");

        unsafe {
            std::env::set_var("SESSION_TASK_CLI", &stub_path);
        }

        let result = auto_create_and_register_queue_item(
            "auto-test-1",
            "echo hello world",
        );

        unsafe {
            match prev_cli {
                Some(v) => std::env::set_var("SESSION_TASK_CLI", v),
                None => std::env::remove_var("SESSION_TASK_CLI"),
            }
        }

        let qid = result.expect("auto-create should succeed");
        assert_eq!(qid, "q-test-auto-1");

        assert!(recording.exists(), "stub should have been invoked");
        let recorded = std::fs::read_to_string(&recording).expect("read");
        // First invocation: queue add with workload:<label> scope.
        assert!(
            recorded.contains("queue\nadd"),
            "expected queue add invocation: {recorded}"
        );
        assert!(
            recorded.contains("workload:auto-test-1"),
            "scope must be workload:<label>: {recorded}"
        );
        assert!(
            recorded.contains("--force-enqueue"),
            "must pass --force-enqueue to bypass scope conflicts: {recorded}"
        );
        assert!(
            recorded.contains("--json"),
            "queue add must request JSON output to parse qid: {recorded}"
        );
        // Summary should be derived from the command (first ~60 chars).
        assert!(
            recorded.contains("echo hello world"),
            "summary should contain command snippet: {recorded}"
        );
        // Created-by stamp identifies the source.
        assert!(
            recorded.contains("workload"),
            "created-by stamp expected: {recorded}"
        );
    }

    #[test]
    fn auto_create_calls_register_after_add() {
        // After `queue add` returns the qid, we MUST call `queue
        // register <qid> --silent` so the row is in `running` state
        // (matches the agent-spawn pattern). Without --silent the
        // pingme would double-fire alongside the workload's own start
        // banner.
        let _lock = WORKLOAD_TEST_ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().expect("tempdir");
        let prev_cli = std::env::var("SESSION_TASK_CLI").ok();

        let recording = tmp.path().join("session-task.recording");
        let stub_path =
            write_session_task_stub(tmp.path(), &recording, "q-test-reg-1");

        unsafe {
            std::env::set_var("SESSION_TASK_CLI", &stub_path);
        }

        let _ = auto_create_and_register_queue_item("reg-test", "true");

        unsafe {
            match prev_cli {
                Some(v) => std::env::set_var("SESSION_TASK_CLI", v),
                None => std::env::remove_var("SESSION_TASK_CLI"),
            }
        }

        let recorded = std::fs::read_to_string(&recording).expect("read");
        // Must contain a register invocation with the qid + --silent.
        assert!(
            recorded.contains("queue\nregister\nq-test-reg-1"),
            "expected `queue register <qid>`: {recorded}"
        );
        assert!(
            recorded.contains("--silent"),
            "register must use --silent: {recorded}"
        );
        // And the order: add comes BEFORE register (substring index check).
        let add_idx = recorded.find("queue\nadd").expect("add invocation");
        let reg_idx = recorded
            .find("queue\nregister")
            .expect("register invocation");
        assert!(
            add_idx < reg_idx,
            "add must precede register: add={add_idx}, register={reg_idx}"
        );
    }

    #[test]
    fn auto_create_returns_err_when_session_task_missing() {
        // CLI not on PATH → fail-soft contract: caller logs warning
        // and continues without a queue row. The helper itself returns
        // Err so the caller can decide what to do.
        let _lock = WORKLOAD_TEST_ENV_LOCK.lock().unwrap();
        let prev_cli = std::env::var("SESSION_TASK_CLI").ok();
        let prev_path = std::env::var("PATH").ok();
        let prev_home = std::env::var("HOME").ok();

        // Point env vars at an empty tempdir so the find_session_task_cli
        // walk (PATH first, $HOME/bin/session-task fallback) finds nothing.
        let tmp = tempfile::tempdir().expect("tempdir");
        unsafe {
            std::env::remove_var("SESSION_TASK_CLI");
            std::env::set_var("PATH", tmp.path()); // empty dir
            std::env::set_var("HOME", tmp.path()); // no $HOME/bin/session-task
        }

        let result =
            auto_create_and_register_queue_item("missing-cli-test", "true");

        unsafe {
            match prev_cli {
                Some(v) => std::env::set_var("SESSION_TASK_CLI", v),
                None => std::env::remove_var("SESSION_TASK_CLI"),
            }
            match prev_path {
                Some(v) => std::env::set_var("PATH", v),
                None => std::env::remove_var("PATH"),
            }
            match prev_home {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
        }

        assert!(
            result.is_err(),
            "helper must return Err when CLI missing: {:?}",
            result
        );
        let err = result.unwrap_err();
        assert!(
            err.contains("not found"),
            "error message should mention CLI not found: {err}"
        );
    }

    #[test]
    fn auto_create_returns_err_on_queue_add_failure() {
        // session-task queue add exits non-zero (e.g. malformed args,
        // DB lock contention) → helper returns Err. Important: we do
        // NOT proceed to `register` against an unknown qid.
        let _lock = WORKLOAD_TEST_ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().expect("tempdir");
        let prev_cli = std::env::var("SESSION_TASK_CLI").ok();

        let recording = tmp.path().join("session-task.recording");
        // Stub that fails on queue add.
        let stub_path = tmp.path().join("session-task-stub");
        let stub = format!(
            "#!/bin/bash\n\
             printf '%s\\n' \"$@\" >> {rec}\n\
             if [[ \"$1\" == \"queue\" && \"$2\" == \"add\" ]]; then\n\
               printf 'simulated queue add failure\\n' >&2\n\
               exit 7\n\
             fi\n\
             exit 0\n",
            rec = shell_quote(&recording.to_string_lossy()),
        );
        std::fs::write(&stub_path, stub).expect("write stub");
        let _ = std::fs::set_permissions(
            &stub_path,
            std::fs::Permissions::from_mode(0o755),
        );

        unsafe {
            std::env::set_var("SESSION_TASK_CLI", &stub_path);
        }

        let result =
            auto_create_and_register_queue_item("fail-add-test", "true");

        unsafe {
            match prev_cli {
                Some(v) => std::env::set_var("SESSION_TASK_CLI", v),
                None => std::env::remove_var("SESSION_TASK_CLI"),
            }
        }

        assert!(
            result.is_err(),
            "helper must return Err when queue add fails: {:?}",
            result
        );
        let err = result.unwrap_err();
        assert!(
            err.contains("queue add"),
            "error must mention queue add: {err}"
        );
        // Crucially, register should NOT have been called.
        let recorded = std::fs::read_to_string(&recording).expect("read");
        assert!(
            !recorded.contains("queue\nregister"),
            "register must NOT run after add failure: {recorded}"
        );
    }

    #[test]
    fn auto_create_returns_err_on_malformed_json() {
        // Stub returns success but emits non-JSON on stdout. Helper
        // must surface the parse error so the caller can fail-soft.
        let _lock = WORKLOAD_TEST_ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().expect("tempdir");
        let prev_cli = std::env::var("SESSION_TASK_CLI").ok();

        let stub_path = tmp.path().join("session-task-stub");
        let stub = "#!/bin/bash\n\
                    if [[ \"$1\" == \"queue\" && \"$2\" == \"add\" ]]; then\n\
                      printf 'this is not json\\n'\n\
                    fi\n\
                    exit 0\n";
        std::fs::write(&stub_path, stub).expect("write stub");
        let _ = std::fs::set_permissions(
            &stub_path,
            std::fs::Permissions::from_mode(0o755),
        );

        unsafe {
            std::env::set_var("SESSION_TASK_CLI", &stub_path);
        }

        let result = auto_create_and_register_queue_item(
            "malformed-json-test",
            "true",
        );

        unsafe {
            match prev_cli {
                Some(v) => std::env::set_var("SESSION_TASK_CLI", v),
                None => std::env::remove_var("SESSION_TASK_CLI"),
            }
        }

        assert!(result.is_err(), "malformed JSON must surface as Err");
        let err = result.unwrap_err();
        assert!(
            err.to_lowercase().contains("json"),
            "error should mention JSON: {err}"
        );
    }

    #[test]
    fn auto_create_register_failure_still_returns_qid() {
        // queue add succeeds (qid extracted); queue register fails.
        // The helper still returns Ok(qid) because:
        //   * The row exists in the queue (visible in `queue list`)
        //   * The done/abandon transition path can still clean it up
        //   * The user-facing visibility goal is met
        // Better to have a row in `pending` than no row at all.
        let _lock = WORKLOAD_TEST_ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().expect("tempdir");
        let prev_cli = std::env::var("SESSION_TASK_CLI").ok();

        let stub_path = tmp.path().join("session-task-stub");
        let stub = "#!/bin/bash\n\
                    if [[ \"$1\" == \"queue\" && \"$2\" == \"add\" ]]; then\n\
                      printf '{\"id\":\"q-soft-fail\",\"ready_now\":true}\\n'\n\
                      exit 0\n\
                    fi\n\
                    if [[ \"$1\" == \"queue\" && \"$2\" == \"register\" ]]; then\n\
                      printf 'simulated register failure\\n' >&2\n\
                      exit 5\n\
                    fi\n\
                    exit 0\n";
        std::fs::write(&stub_path, stub).expect("write stub");
        let _ = std::fs::set_permissions(
            &stub_path,
            std::fs::Permissions::from_mode(0o755),
        );

        unsafe {
            std::env::set_var("SESSION_TASK_CLI", &stub_path);
        }

        let result =
            auto_create_and_register_queue_item("register-fail-test", "true");

        unsafe {
            match prev_cli {
                Some(v) => std::env::set_var("SESSION_TASK_CLI", v),
                None => std::env::remove_var("SESSION_TASK_CLI"),
            }
        }

        let qid = result.expect("register failure should be soft (Ok-soft)");
        assert_eq!(qid, "q-soft-fail");
    }

    // ---------------------------------------------------------------
    // inject_workload_scope_token tests (q-2026-05-20-7482).
    //
    // The exporter at exporters/work-queue-exporter/work_queue_exporter.py
    // (~line 299) finds the heartbeat file via a `workload:<label>`
    // scope token on the queue item. When the queue item was created
    // BEFORE the workload started (the common main-loop pattern:
    // queue.add → queue.register → workload.run --queue-id), the
    // scope is whatever the main loop chose (typically `resource:` or
    // a repo: scope) and has no workload: token. `inject_workload_scope_token`
    // is the systemic fix — the workload runner knows label+qid at
    // startup and appends the token itself.
    // ---------------------------------------------------------------

    /// Build a recording session-task stub that:
    ///   * appends argv to `recording` (one block per call, separator `=== invocation ===`)
    ///   * exits 0 on every invocation
    fn write_inject_recording_stub(
        tmp: &std::path::Path,
        recording: &std::path::Path,
    ) -> PathBuf {
        let stub_path = tmp.join("session-task-inject-stub");
        let stub = format!(
            "#!/bin/bash\n\
             {{\n\
               printf '=== invocation ===\\n'\n\
               printf '%s\\n' \"$@\"\n\
             }} >> {rec}\n\
             exit 0\n",
            rec = shell_quote(&recording.to_string_lossy()),
        );
        std::fs::write(&stub_path, stub).expect("write stub");
        let _ = std::fs::set_permissions(
            &stub_path,
            std::fs::Permissions::from_mode(0o755),
        );
        stub_path
    }

    #[test]
    fn inject_workload_scope_token_calls_update_scope() {
        // Happy path: caller supplies qid → helper invokes
        // `session-task queue update-scope <qid> workload:<label>`.
        let _lock = WORKLOAD_TEST_ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().expect("tempdir");
        let prev_cli = std::env::var("SESSION_TASK_CLI").ok();

        let recording = tmp.path().join("session-task.recording");
        let stub_path = write_inject_recording_stub(tmp.path(), &recording);

        unsafe {
            std::env::set_var("SESSION_TASK_CLI", &stub_path);
        }

        inject_workload_scope_token("promote-3-shows", "q-2026-05-20-13b9");

        unsafe {
            match prev_cli {
                Some(v) => std::env::set_var("SESSION_TASK_CLI", v),
                None => std::env::remove_var("SESSION_TASK_CLI"),
            }
        }

        assert!(recording.exists(), "stub should have been invoked");
        let recorded = std::fs::read_to_string(&recording).expect("read");
        assert!(
            recorded.contains("queue\nupdate-scope\nq-2026-05-20-13b9"),
            "expected `queue update-scope <qid>` invocation: {recorded}"
        );
        assert!(
            recorded.contains("workload:promote-3-shows"),
            "must pass workload:<label> token: {recorded}"
        );
    }

    #[test]
    fn inject_workload_scope_token_is_fail_soft_on_missing_cli() {
        // CLI not on PATH → helper logs a warning and RETURNS (does
        // not panic). The workload must keep running even when the
        // queue layer is unreachable.
        let _lock = WORKLOAD_TEST_ENV_LOCK.lock().unwrap();
        let prev_cli = std::env::var("SESSION_TASK_CLI").ok();
        let prev_path = std::env::var("PATH").ok();
        let prev_home = std::env::var("HOME").ok();

        let tmp = tempfile::tempdir().expect("tempdir");
        unsafe {
            std::env::remove_var("SESSION_TASK_CLI");
            std::env::set_var("PATH", tmp.path()); // empty
            std::env::set_var("HOME", tmp.path());
        }

        // Should not panic, should not stall.
        inject_workload_scope_token("missing-cli", "q-x");

        unsafe {
            match prev_cli {
                Some(v) => std::env::set_var("SESSION_TASK_CLI", v),
                None => std::env::remove_var("SESSION_TASK_CLI"),
            }
            match prev_path {
                Some(v) => std::env::set_var("PATH", v),
                None => std::env::remove_var("PATH"),
            }
            match prev_home {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
        }
    }

    #[test]
    fn inject_workload_scope_token_is_fail_soft_on_nonzero_exit() {
        // session-task update-scope exits non-zero (e.g. item not
        // found) → helper logs a warning and RETURNS. Workload
        // startup must not block on queue-layer errors.
        let _lock = WORKLOAD_TEST_ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().expect("tempdir");
        let prev_cli = std::env::var("SESSION_TASK_CLI").ok();

        let stub_path = tmp.path().join("inject-fail-stub");
        let stub = "#!/bin/bash\nprintf 'simulated failure\\n' >&2\nexit 1\n";
        std::fs::write(&stub_path, stub).expect("write stub");
        let _ = std::fs::set_permissions(
            &stub_path,
            std::fs::Permissions::from_mode(0o755),
        );

        unsafe {
            std::env::set_var("SESSION_TASK_CLI", &stub_path);
        }

        // Should not panic, should return after the stub's exit 1.
        inject_workload_scope_token("fail-stub", "q-fail");

        unsafe {
            match prev_cli {
                Some(v) => std::env::set_var("SESSION_TASK_CLI", v),
                None => std::env::remove_var("SESSION_TASK_CLI"),
            }
        }
    }

    #[test]
    fn inject_workload_scope_token_is_idempotent() {
        // Two consecutive calls with the same qid+label must both
        // invoke `queue update-scope`; the subcommand itself is
        // idempotent (tested separately in session-task pytest
        // suite). The point of this test is that the Rust helper
        // doesn't dedupe or short-circuit on a re-run (re-running
        // `workload run LABEL --queue-id X` is the operator's
        // signal that they want the token to exist; we delegate
        // idempotency to update-scope rather than caching state
        // here).
        let _lock = WORKLOAD_TEST_ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().expect("tempdir");
        let prev_cli = std::env::var("SESSION_TASK_CLI").ok();

        let recording = tmp.path().join("session-task.recording");
        let stub_path = write_inject_recording_stub(tmp.path(), &recording);

        unsafe {
            std::env::set_var("SESSION_TASK_CLI", &stub_path);
        }

        inject_workload_scope_token("idem", "q-idem");
        inject_workload_scope_token("idem", "q-idem");

        unsafe {
            match prev_cli {
                Some(v) => std::env::set_var("SESSION_TASK_CLI", v),
                None => std::env::remove_var("SESSION_TASK_CLI"),
            }
        }

        let recorded = std::fs::read_to_string(&recording).expect("read");
        let invocations = recorded.matches("=== invocation ===").count();
        assert_eq!(
            invocations, 2,
            "both calls must invoke session-task (delegated idempotency): {recorded}"
        );
        // Both invocations should target the same qid + token.
        let qid_count = recorded.matches("q-idem").count();
        assert_eq!(
            qid_count, 2,
            "qid must appear in both invocations: {recorded}"
        );
        let token_count = recorded.matches("workload:idem").count();
        assert_eq!(
            token_count, 2,
            "token must appear in both invocations: {recorded}"
        );
    }

    #[test]
    fn run_session_task_with_timeout_kills_runaway_child() {
        // A wedged session-task must not stall the workload startup
        // path. The bounded timeout (here 1s) must fire and we must
        // get TimedOut back.
        let _lock = WORKLOAD_TEST_ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().expect("tempdir");
        let stub_path = tmp.path().join("hang-stub");
        let stub = "#!/bin/bash\nsleep 30\n";
        std::fs::write(&stub_path, stub).expect("write stub");
        let _ = std::fs::set_permissions(
            &stub_path,
            std::fs::Permissions::from_mode(0o755),
        );

        let start = std::time::Instant::now();
        let res = run_session_task_with_timeout(
            &stub_path,
            &["queue".to_string(), "add".to_string()],
            1, // 1 second
        );
        let elapsed = start.elapsed();

        assert!(res.is_err(), "hang-stub must trip the timeout");
        let err = res.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
        // Loose upper bound: should be ~1s (50ms poll interval). 5s
        // gives us plenty of slack on a loaded test runner without
        // letting a regression that disables the timeout slip through.
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "timeout fired too late: {elapsed:?}"
        );
    }

    // ----- build_wrapper_script: PTY-wrap + raw-output tests ----------------

    #[test]
    fn wrapper_script_writes_output_without_ts_prefix() {
        // The new wrapper redirects all wrapper-side output straight to
        // .output (`exec >> OUT 2>&1`) without piping through `ts | tee`.
        // The `ts | tee` chain block-buffered on `\n`, swallowing
        // `\r`-separated progress frames from rsync/curl/pv until a
        // final newline arrived — the bug behind q-2026-05-13-e6ab.
        // Hard guards: both the `ts` prefixer AND the pure-bash
        // `date -Is` per-line fallback must be GONE.
        let script = build_wrapper_script(
            "demo",
            "echo hi",
            Path::new("/tmp/claude-workloads/demo.output"),
            Path::new("/tmp/claude-workloads/demo.exit"),
            Path::new("/tmp/claude-workloads/demo.heartbeat"),
            Path::new("/tmp/claude-wl-rt/demo.heartbeat"),
            "/usr/local/bin/claude-watch",
            None,
        );
        assert!(
            !script.contains("ts '%Y-%m-%dT%H:%M:%S%z '"),
            "wrapper must NOT pipe through `ts` (it `\\n`-buffers, killing \\r progress):\n{script}"
        );
        assert!(
            !script.contains("while IFS= read -r line"),
            "wrapper must NOT use the per-line bash fallback (same `\\n`-buffer problem):\n{script}"
        );
        assert!(
            script.contains("exec >> '/tmp/claude-workloads/demo.output' 2>&1"),
            "wrapper must redirect headers/footers straight into the .output path:\n{script}"
        );
    }

    #[test]
    fn wrapper_script_wraps_user_command_in_pty() {
        // The user command runs under `script -q -f -e -c <STR> /dev/null`
        // so progress-emitting tools (rsync --progress, curl,
        // wget --progress, pv) see a TTY and emit `\r`-separated
        // progress frames continuously instead of suppressing them
        // entirely. `-q` quiets script's banner; `-f` flushes after
        // every write so bytes hit fd1 immediately; `-e` propagates
        // the child's exit code so the wrapper sees the real rc.
        let script = build_wrapper_script(
            "pty",
            "rsync --progress src dst",
            Path::new("/tmp/pty.output"),
            Path::new("/tmp/pty.exit"),
            Path::new("/tmp/pty.heartbeat"),
            Path::new("/tmp/claude-wl-rt/pty.heartbeat"),
            "/usr/bin/claude-watch",
            None,
        );
        assert!(
            script.contains("setsid --wait script -q -f -e -c"),
            "wrapper must wrap user command in `script -q -f -e -c` for PTY allocation \
             with exit-code propagation:\n{script}"
        );
        assert!(
            script.contains("WORKLOAD_PTY"),
            "wrapper must honor WORKLOAD_PTY=0 opt-out:\n{script}"
        );
        // The no-PTY fallback path (when `script` is missing, or
        // WORKLOAD_PTY=0) still runs the user command via setsid bash.
        assert!(
            script.contains("setsid --wait bash -c "),
            "wrapper must retain a non-PTY `setsid --wait bash -c` fallback:\n{script}"
        );
    }

    #[test]
    fn wrapper_script_no_unprefixed_tee_exec() {
        // Regression guard: the old wrapper had `exec > >(tee -a OUT) 2>&1`
        // (or `exec > >(ts | tee -a OUT) 2>&1`). The new wrapper writes
        // straight to the file via `exec >> OUT 2>&1` — no `tee`, no
        // `>( ... )` process substitution that would re-introduce
        // pipe-side buffering on the path between the user command
        // and the disk file.
        let script = build_wrapper_script(
            "guard",
            "true",
            Path::new("/tmp/g.output"),
            Path::new("/tmp/g.exit"),
            Path::new("/tmp/g.heartbeat"),
            Path::new("/tmp/claude-wl-rt/guard.heartbeat"),
            "/bin/claude-watch",
            None,
        );
        assert!(
            !script.contains("exec > >(tee -a"),
            "regressed to unprefixed tee:\n{script}"
        );
        assert!(
            !script.contains(">(tee -a"),
            "found `>(tee -a` — every wrapper-side write must go straight to the file (no process-sub pipe):\n{script}"
        );
        assert!(
            !script.contains("| tee -a"),
            "found a `| tee -a` chain — re-introduces pipe buffering between user cmd and .output:\n{script}"
        );
    }

    #[test]
    fn wrapper_script_preserves_headers_and_emit_done() {
        // The PTY change must NOT break the existing wrapper
        // structure: header lines, setsid invocation, exit-file write,
        // and the emit-done CLI call all stay.
        let script = build_wrapper_script(
            "wp",
            "true",
            Path::new("/tmp/wp.output"),
            Path::new("/tmp/wp.exit"),
            Path::new("/tmp/wp.heartbeat"),
            Path::new("/tmp/claude-wl-rt/wp.heartbeat"),
            "/usr/bin/claude-watch",
            Some("q-2026-05-05-test"),
        );
        assert!(script.contains("=== workload: wp ==="));
        assert!(script.contains("setsid --wait"));
        assert!(script.contains("echo $EC > '/tmp/wp.exit'"));
        assert!(script.contains("workload emit-done --label 'wp'"));
        assert!(
            script.contains("--queue-id 'q-2026-05-05-test'"),
            "queue id must be plumbed into emit-done:\n{script}"
        );
        assert!(script.contains("=== DONE (exit $EC)"));
    }

    #[test]
    fn wrapper_script_line_buffers_via_stdbuf() {
        // Workload output reaching the .output file (and from there the
        // queue-minisite SSE tail and the browser) must arrive per-line,
        // not in 4-8KB stdio chunks. The wrapper must prepend `stdbuf
        // -oL -eL` to the inner `bash -c` so libc stdio in the workload's
        // child process tree line-buffers stdout/stderr.
        let script = build_wrapper_script(
            "lb",
            "echo hi",
            Path::new("/tmp/lb.output"),
            Path::new("/tmp/lb.exit"),
            Path::new("/tmp/lb.heartbeat"),
            Path::new("/tmp/claude-wl-rt/lb.heartbeat"),
            "/usr/bin/claude-watch",
            None,
        );
        assert!(
            script.contains("stdbuf -oL -eL bash -c"),
            "wrapper must wrap the inner bash with `stdbuf -oL -eL` for libc-stdio programs:\n{script}"
        );
        assert!(
            script.contains("PYTHONUNBUFFERED=1"),
            "wrapper must set PYTHONUNBUFFERED=1 so Python children flush per-line:\n{script}"
        );
        assert!(
            script.contains("WORKLOAD_LINE_BUFFER"),
            "wrapper must honor the WORKLOAD_LINE_BUFFER opt-out env:\n{script}"
        );
        // The opt-out branch must still run the bare `setsid --wait bash -c`
        // (no `stdbuf` prefix) so tests can disable the wrapper.
        assert!(
            script.contains("setsid --wait bash -c "),
            "wrapper must retain a bare `setsid --wait bash -c` fallback:\n{script}"
        );
    }

    #[test]
    fn wrapper_script_no_queue_id_omits_arg() {
        // When the workload has no queue binding, the emit-done call
        // must NOT include `--queue-id` (legacy event shape).
        let script = build_wrapper_script(
            "wnq",
            "true",
            Path::new("/tmp/wnq.output"),
            Path::new("/tmp/wnq.exit"),
            Path::new("/tmp/wnq.heartbeat"),
            Path::new("/tmp/claude-wl-rt/wnq.heartbeat"),
            "/usr/bin/claude-watch",
            None,
        );
        assert!(
            !script.contains("--queue-id"),
            "no-queue workload must omit --queue-id from emit-done:\n{script}"
        );
    }

    /// End-to-end verification: actually execute the wrapper script and
    /// check that the .output file contains the user command's stdout
    /// AND the wrapper headers/footer. The old version of this test
    /// asserted ISO8601 prefixes on every line; the new wrapper writes
    /// raw output (no `ts` prefix) so we instead check that the body
    /// is non-empty, headers are present, and the user's echo output
    /// made it through.
    ///
    /// We run the wrapper directly (not through tmux). The wrapper
    /// includes a trailing `sleep 30` (so tmux pane stays alive a bit
    /// after exit) — we patch it to `sleep 0` before executing so the
    /// test runs in <1s.
    #[test]
    fn wrapper_script_runtime_emits_body_and_headers() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let out_path = tmp.path().join("rt.output");
        let exit_path = tmp.path().join("rt.exit");
        let script_path = tmp.path().join("rt.sh");

        // Use /bin/true as the "claude-watch" binary; the wrapper's
        // emit-done call becomes `/bin/true workload emit-done ...`
        // which exits 0 (true ignores its args) and the wrapper's
        // `|| true` swallows any anomaly.
        let hb_path = tmp.path().join("rt.heartbeat");
        let rt_hb_path = tmp.path().join("rt.runtime.heartbeat");
        let script_full = build_wrapper_script(
            "rt",
            "echo first; echo second",
            &out_path,
            &exit_path,
            &hb_path,
            &rt_hb_path,
            "/bin/true",
            None,
        );
        // Patch out the `sleep 30` keep-alive so the wrapper exits
        // promptly. The wrapper has exactly one `sleep 30\n` line.
        let script = script_full.replace("sleep 30\n", "sleep 0\n");
        assert_ne!(
            script, script_full,
            "expected `sleep 30` keep-alive in generated script"
        );

        std::fs::write(&script_path, &script).expect("write script");
        std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o700))
            .expect("chmod");

        let status = Command::new("bash")
            .arg(&script_path)
            .status()
            .expect("run wrapper");
        assert!(
            status.success(),
            "wrapper exited non-zero: {status:?}\nscript:\n{script}"
        );

        // The user command runs under `script -q -f`, which flushes
        // after every write — no buffering settle-time needed in
        // practice, but a small slack covers PTY teardown.
        std::thread::sleep(Duration::from_millis(200));

        let body = std::fs::read_to_string(&out_path)
            .unwrap_or_else(|e| panic!("read {out_path:?}: {e}"));
        assert!(
            !body.is_empty(),
            "output file is empty; script:\n{script}"
        );

        // No more ISO8601 prefix — verify the OPPOSITE: lines must NOT
        // be prefixed with a `YYYY-MM-DDTHH:MM:SS±HHMM ` timestamp.
        // (One header line `Started: <iso>` contains an ISO8601 but
        // not as a leading prefix — it's preceded by `Started: `.)
        let leading_ts_re = regex_lite::Regex::new(
            r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}[+-]\d{2}:?\d{2} ",
        )
        .expect("compile regex");
        for (i, line) in body.lines().enumerate() {
            if line.is_empty() {
                continue;
            }
            assert!(
                !leading_ts_re.is_match(line),
                "line {i} unexpectedly carries leading ISO8601 prefix: {line:?}\nfull body:\n{body}"
            );
        }

        assert!(
            body.contains("first") && body.contains("second"),
            "expected echoed text in output:\n{body}"
        );
        assert!(
            body.contains("=== workload: rt ==="),
            "expected workload header in output:\n{body}"
        );
        assert!(
            body.contains("=== DONE (exit 0)"),
            "expected DONE line in output:\n{body}"
        );
    }

    /// End-to-end verification of the PTY exit-code-propagation fix:
    /// when the inner command fails (`false`, `exit 7`, etc.), the
    /// wrapper's `EC=$?` must capture the real rc — NOT 0 from
    /// `script`'s own success. Without `script -e`, `script` swallows
    /// the child rc and the wrapper writes `=== DONE (exit 0) ===`
    /// even for a genuinely failed inner command, which then mis-
    /// routes the queue-bound workload to `done` instead of `abandon`
    /// (PR #138 contract).
    ///
    /// Three cases per failure mode:
    ///   * `false`          → `exit 1`
    ///   * `exit 7`         → `exit 7`
    ///   * `bash -c "exit 7"` → `exit 7` (extra layer; same rc)
    ///
    /// Plus the clean-exit baseline (`true` → `exit 0`) as a
    /// regression cover.
    #[test]
    fn wrapper_script_runtime_propagates_inner_exit_code() {
        // Requires `script` (util-linux) on PATH; the PTY branch is
        // where the bug lives. Without `script` the wrapper falls back
        // to bare `setsid --wait bash -c` which already propagates rc.
        if Command::new("sh")
            .args(["-c", "command -v script >/dev/null"])
            .status()
            .map(|s| !s.success())
            .unwrap_or(true)
        {
            eprintln!("script(1) not on PATH; skipping exit-code propagation test");
            return;
        }

        // (label, inner command, expected exit code)
        let cases: &[(&str, &str, i32)] = &[
            ("ecok", "true", 0),
            ("ecfalse", "false", 1),
            ("ecexit7", "exit 7", 7),
            ("ecbashexit7", "bash -c \"exit 7\"", 7),
        ];

        for (label, inner, expected_rc) in cases {
            let tmp = tempfile::tempdir().expect("tempdir");
            let out_path = tmp.path().join(format!("{label}.output"));
            let exit_path = tmp.path().join(format!("{label}.exit"));
            let hb_path = tmp.path().join(format!("{label}.heartbeat"));
            let rt_hb_path = tmp.path().join(format!("{label}.runtime.heartbeat"));
            let script_path = tmp.path().join(format!("{label}.sh"));

            let script_full = build_wrapper_script(
                label,
                inner,
                &out_path,
                &exit_path,
                &hb_path,
                &rt_hb_path,
                "/bin/true",
                None,
            );
            let script = script_full.replace("sleep 30\n", "sleep 0\n");

            std::fs::write(&script_path, &script).expect("write script");
            std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o700))
                .expect("chmod");

            let status = Command::new("bash")
                .arg(&script_path)
                .env("WORKLOAD_HEARTBEAT", "0")
                .env("WORKLOAD_RUNTIME_HEARTBEAT", "0")
                .status()
                .expect("run wrapper");
            assert!(
                status.success(),
                "wrapper itself must exit 0 (bug isn't about the wrapper's own rc): \
                 case={label} inner={inner:?} status={status:?}"
            );

            // settle the .exit write
            std::thread::sleep(Duration::from_millis(200));

            let exit_body = std::fs::read_to_string(&exit_path)
                .unwrap_or_else(|e| panic!("read .exit for {label}: {e}"));
            let recorded_rc: i32 = exit_body
                .trim()
                .parse()
                .unwrap_or_else(|e| panic!("parse rc for {label}: {e} (body={exit_body:?})"));
            assert_eq!(
                recorded_rc, *expected_rc,
                "case={label} inner={inner:?}: .exit recorded {recorded_rc}, expected {expected_rc} \
                 (script(1) is masking the child rc — needs `-e/--return`)"
            );

            // The DONE line in the .output must also carry the real rc.
            let out_body = std::fs::read_to_string(&out_path).unwrap_or_default();
            let expected_done = format!("=== DONE (exit {expected_rc})");
            assert!(
                out_body.contains(&expected_done),
                "case={label} inner={inner:?}: expected {expected_done:?} in .output:\n{out_body}"
            );
        }
    }

    /// End-to-end verification that the rc propagated through the PTY
    /// flows into `cmd_emit_done`'s queue transition: a failed inner
    /// command must drive `queue abandon`, a clean exit must drive
    /// `queue done`. Stubs `session-task` and asserts the stub was
    /// invoked with the right subcommand.
    ///
    /// We can't drive the wrapper's `emit-done` shellout to invoke
    /// `cmd_emit_done` directly (the wrapper calls the claude-watch
    /// binary, which we don't have on PATH inside the test). Instead
    /// we read `EC` from the wrapper's `.exit` file and feed it into
    /// `cmd_emit_done` ourselves — which mirrors exactly what the
    /// real wrapper does (`{exe_q} workload emit-done --exit-code "$EC"
    /// ...`). The contract under test is: rc captured in `.exit`
    /// → matched queue transition.
    #[test]
    fn wrapper_script_runtime_drives_queue_abandon_on_failure() {
        if Command::new("sh")
            .args(["-c", "command -v script >/dev/null"])
            .status()
            .map(|s| !s.success())
            .unwrap_or(true)
        {
            eprintln!("script(1) not on PATH; skipping rc→queue transition test");
            return;
        }

        let _lock = WORKLOAD_TEST_ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().expect("tempdir");
        let prev_q = std::env::var("CLAUDE_EVENT_QUEUE").ok();
        let prev_cli = std::env::var("SESSION_TASK_CLI").ok();
        let prev_t = std::env::var("WORKLOAD_QUEUE_TRANSITION").ok();

        // Stub session-task: record args, exit 0.
        let recording = tmp.path().join("session-task.recording");
        let stub_path = tmp.path().join("session-task-stub");
        let stub = format!(
            "#!/bin/bash\nprintf '%s\\n' \"$@\" >> {rec}\nprintf -- '---\\n' >> {rec}\nexit 0\n",
            rec = shell_quote(&recording.to_string_lossy()),
        );
        std::fs::write(&stub_path, stub).expect("write stub");
        let _ = std::fs::set_permissions(
            &stub_path,
            std::fs::Permissions::from_mode(0o755),
        );

        unsafe {
            std::env::set_var("CLAUDE_EVENT_QUEUE", tmp.path());
            std::env::set_var("SESSION_TASK_CLI", &stub_path);
            std::env::remove_var("WORKLOAD_QUEUE_TRANSITION");
        }

        let cases: &[(&str, &str, i32, &str, &str)] = &[
            // (label, inner, expected_rc, qid, expected_subcommand)
            ("rqfalse", "false", 1, "q-rqfalse-test", "abandon"),
            ("rqexit7", "bash -c \"exit 7\"", 7, "q-rqexit7-test", "abandon"),
            ("rqok", "true", 0, "q-rqok-test", "done"),
        ];

        for (label, inner, expected_rc, qid, _expected_sub) in cases {
            let out_path = tmp.path().join(format!("{label}.output"));
            let exit_path = tmp.path().join(format!("{label}.exit"));
            let hb_path = tmp.path().join(format!("{label}.heartbeat"));
            let rt_hb_path = tmp.path().join(format!("{label}.runtime.heartbeat"));
            let script_path = tmp.path().join(format!("{label}.sh"));

            let script_full = build_wrapper_script(
                label,
                inner,
                &out_path,
                &exit_path,
                &hb_path,
                &rt_hb_path,
                "/bin/true",
                None,
            );
            let script = script_full.replace("sleep 30\n", "sleep 0\n");
            std::fs::write(&script_path, &script).expect("write script");
            std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o700))
                .expect("chmod");

            let status = Command::new("bash")
                .arg(&script_path)
                .env("WORKLOAD_HEARTBEAT", "0")
                .env("WORKLOAD_RUNTIME_HEARTBEAT", "0")
                .status()
                .expect("run wrapper");
            assert!(status.success(), "wrapper rc != 0 for case {label}: {status:?}");

            std::thread::sleep(Duration::from_millis(150));

            // Read rc from .exit (this is the wrapper's source of truth)
            // and feed it into cmd_emit_done with the test qid — mirrors
            // the real emit-done invocation in the wrapper template.
            let exit_body = std::fs::read_to_string(&exit_path)
                .unwrap_or_else(|e| panic!("read .exit for {label}: {e}"));
            let captured_rc: i32 = exit_body
                .trim()
                .parse()
                .unwrap_or_else(|e| panic!("parse rc for {label}: {e}"));
            assert_eq!(
                captured_rc, *expected_rc,
                "wrapper .exit rc mismatch for {label}: got {captured_rc}, expected {expected_rc}"
            );

            let _ = cmd_emit_done(
                label,
                captured_rc,
                &out_path.to_string_lossy(),
                false,
                Some(qid),
            );
        }

        unsafe {
            match prev_q {
                Some(v) => std::env::set_var("CLAUDE_EVENT_QUEUE", v),
                None => std::env::remove_var("CLAUDE_EVENT_QUEUE"),
            }
            match prev_cli {
                Some(v) => std::env::set_var("SESSION_TASK_CLI", v),
                None => std::env::remove_var("SESSION_TASK_CLI"),
            }
            match prev_t {
                Some(v) => std::env::set_var("WORKLOAD_QUEUE_TRANSITION", v),
                None => std::env::remove_var("WORKLOAD_QUEUE_TRANSITION"),
            }
        }

        let recorded = std::fs::read_to_string(&recording).expect("read recording");

        // Each case's queue transition is recorded in the stub. Split
        // on the `---\n` separator we wrote between invocations.
        for (_label, _inner, _expected_rc, qid, expected_sub) in cases {
            let needle_sub = format!("\n{expected_sub}\n{qid}\n");
            // Also accept the case where the subcommand is the first arg:
            // `queue\nabandon\n<qid>\n` — printf "%s\n" expands each arg
            // on its own line.
            let alt_needle = format!("queue\n{expected_sub}\n{qid}\n");
            assert!(
                recorded.contains(&needle_sub) || recorded.contains(&alt_needle),
                "expected `queue {expected_sub} {qid}` in stub recording:\n{recorded}"
            );
        }
    }

    /// End-to-end verification of the central bug fix: `\r`-separated
    /// progress frames must reach the .output file in real time, NOT
    /// be buffered until a `\n` arrives. The old `ts | tee` chain
    /// failed this; the new PTY-wrapped `script -q -f` path passes it.
    ///
    /// Producer: a bash loop that emits 5 `\rprog:N%` frames over ~1s
    /// with NO `\n` until the final `done\n`. We sample the .output
    /// file size midway through; with the bug, the file would still
    /// be empty (or contain only the wrapper header) because the user
    /// command's progress bytes are buffered behind ts/tee. With the
    /// fix, all 5 frames are already on disk when we sample.
    #[test]
    fn wrapper_script_runtime_streams_carriage_return_progress() {
        // Skip if `script` (util-linux) is unavailable — required for
        // the PTY wrap. Without it the wrapper falls back to non-PTY
        // mode and the test would race on platform default buffering.
        if Command::new("sh")
            .args(["-c", "command -v script >/dev/null"])
            .status()
            .map(|s| !s.success())
            .unwrap_or(true)
        {
            eprintln!("script(1) not on PATH; skipping \\r-progress runtime test");
            return;
        }

        let tmp = tempfile::tempdir().expect("tempdir");
        let out_path = tmp.path().join("cr.output");
        let exit_path = tmp.path().join("cr.exit");
        let hb_path = tmp.path().join("cr.heartbeat");
        let rt_hb_path = tmp.path().join("cr.runtime.heartbeat");
        let script_path = tmp.path().join("cr.sh");

        // 5 frames, 200ms apart — total ~1s. Each frame is `\rprog:N%`
        // with NO `\n`. Final newline + done line graduates the row.
        let inner = "for i in 1 2 3 4 5; do printf '\\rprog: %d%%' $((i*20)); sleep 0.2; done; printf '\\ndone\\n'";

        let script_full = build_wrapper_script(
            "cr",
            inner,
            &out_path,
            &exit_path,
            &hb_path,
            &rt_hb_path,
            "/bin/true",
            None,
        );
        let script = script_full.replace("sleep 30\n", "sleep 0\n");

        std::fs::write(&script_path, &script).expect("write script");
        std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o700))
            .expect("chmod");

        let mut child = Command::new("bash")
            .arg(&script_path)
            .env("WORKLOAD_HEARTBEAT", "0")
            .env("WORKLOAD_RUNTIME_HEARTBEAT", "0")
            .spawn()
            .expect("spawn wrapper");

        // Sample at ~600ms in. By now the producer has emitted 3
        // of 5 frames (200ms, 400ms, 600ms). The .output file must
        // already contain at least the first 2 `\rprog:` frames if
        // the PTY+raw-tee chain is unbuffered. With the old bug
        // (ts | tee), it would contain ZERO progress bytes here —
        // they'd all flush at the final `\n` ~1s later.
        std::thread::sleep(Duration::from_millis(600));
        let mid_body = std::fs::read_to_string(&out_path).unwrap_or_default();

        let _ = child.wait();
        std::thread::sleep(Duration::from_millis(200));
        let final_body = std::fs::read_to_string(&out_path).unwrap_or_default();

        // Sanity: the run produced the expected final output.
        assert!(
            final_body.contains("done"),
            "expected final 'done' marker in .output:\n{final_body}"
        );

        // Count `\r` bytes in the mid-sample. The producer emits
        // exactly one `\r` per frame. With unbuffered streaming we
        // expect ≥2 `\r` by 600ms in.
        let mid_cr_count = mid_body.matches('\r').count();
        assert!(
            mid_cr_count >= 2,
            "mid-run .output contained {mid_cr_count} `\\r` bytes — expected ≥2 \
             (PTY+raw-tee streaming broken; old `ts | tee` chain would show 0). \
             full mid_body bytes (with `\\r` rendered):\n{mid_body:?}\n\
             final_body:\n{final_body:?}"
        );
    }

    /// End-to-end: a Python child process inside the wrapper must
    /// flush its stdout per-line (not in 4-8KB block-buffer chunks).
    /// Verifies the `stdbuf -oL -eL bash -c ...` wrap actually
    /// propagates through libc stdio to the workload's children.
    ///
    /// Strategy: emit 10 short lines from a Python child with `sleep 0.05`
    /// between each (total ~0.5s). With libc block-buffering the file
    /// would stay empty until Python exits and the buffer flushes; with
    /// line-buffering the file should reach its full size mid-flight.
    /// We sample the file size at half the runtime and assert it's
    /// already grown substantially.
    #[test]
    fn wrapper_script_runtime_line_buffers_python_child() {
        // Skip if python3 isn't on PATH — keeps the test friendly to
        // minimal CI images.
        if Command::new("sh")
            .args(["-c", "command -v python3 >/dev/null"])
            .status()
            .map(|s| !s.success())
            .unwrap_or(true)
        {
            eprintln!("python3 not on PATH; skipping line-buffer runtime test");
            return;
        }
        // Likewise stdbuf — without it the wrapper's opt-in branch
        // silently falls back to bare `bash -c` and the test would
        // race on the platform default (almost always block-buffered).
        if Command::new("sh")
            .args(["-c", "command -v stdbuf >/dev/null"])
            .status()
            .map(|s| !s.success())
            .unwrap_or(true)
        {
            eprintln!("stdbuf not on PATH; skipping line-buffer runtime test");
            return;
        }

        let tmp = tempfile::tempdir().expect("tempdir");
        let out_path = tmp.path().join("lb.output");
        let exit_path = tmp.path().join("lb.exit");
        let hb_path = tmp.path().join("lb.heartbeat");
        let rt_hb_path = tmp.path().join("lb.runtime.heartbeat");
        let script_path = tmp.path().join("lb.sh");

        // 10 short lines, 100ms apart — total ~1s. Without stdbuf the
        // file stays empty until the python interpreter exits; with
        // stdbuf -oL we should see the file grow line-by-line.
        // Use a semicolon-joined one-liner so we don't have to fight
        // Python indentation through shell quoting.
        let inner = "python3 -c 'import time\n\
for i in range(10): print(\"py-line-\" + str(i)); time.sleep(0.1)\n'";

        let script_full = build_wrapper_script(
            "lb",
            inner,
            &out_path,
            &exit_path,
            &hb_path,
            &rt_hb_path,
            "/bin/true",
            None,
        );
        let script = script_full.replace("sleep 30\n", "sleep 0\n");

        std::fs::write(&script_path, &script).expect("write script");
        std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o700))
            .expect("chmod");

        let mut child = Command::new("bash")
            .arg(&script_path)
            // Defeat the heartbeat sidecars — we don't want their writes
            // showing up in the .output sampling window.
            .env("WORKLOAD_HEARTBEAT", "0")
            .env("WORKLOAD_RUNTIME_HEARTBEAT", "0")
            .spawn()
            .expect("spawn wrapper");

        // Sample the file size halfway through the python run (~500ms
        // in). If line-buffering is working, the file should already
        // have several lines worth of bytes by now.
        std::thread::sleep(Duration::from_millis(500));
        let mid_size = std::fs::metadata(&out_path).map(|m| m.len()).unwrap_or(0);

        let _ = child.wait();
        std::thread::sleep(Duration::from_millis(200));
        let final_size = std::fs::metadata(&out_path).map(|m| m.len()).unwrap_or(0);
        let body = std::fs::read_to_string(&out_path).unwrap_or_default();

        // Sanity: the run produced output.
        assert!(
            body.contains("py-line-9"),
            "expected python output in .output file:\n{body}"
        );
        assert!(
            final_size > 0,
            "final size should be > 0; body:\n{body}"
        );

        // Core assertion: at the mid-point, the file should already
        // contain a substantial fraction of the final output. We allow
        // generous slack (≥25%) because timing on CI is noisy, but a
        // block-buffered run would have ~0 bytes (just the wrapper's
        // header lines, written before python starts).
        let mid_fraction = mid_size as f64 / final_size as f64;
        assert!(
            mid_fraction > 0.25,
            "mid-run file size was {mid_size}/{final_size} bytes \
             ({:.0}% of final) — expected >25% if line-buffering works; \
             a block-buffered python child would dump everything at once. \
             body:\n{body}",
            mid_fraction * 100.0,
        );
    }

    // ----- build_wrapper_script: heartbeat sidecar tests --------------------

    #[test]
    fn wrapper_script_contains_heartbeat_sidecar() {
        // The wrapper must spawn a backgrounded `while true; touch HB; sleep N`
        // sidecar so the watchdog file gets pet every interval, AND must
        // install an EXIT trap that reaps the sidecar PID. Both halves are
        // load-bearing — without the trap the sidecar leaks past wrapper
        // death (the exact case the watchdog should detect).
        let script = build_wrapper_script(
            "hb",
            "true",
            Path::new("/tmp/claude-workloads/hb.output"),
            Path::new("/tmp/claude-workloads/hb.exit"),
            Path::new("/tmp/claude-workloads/hb.heartbeat"),
            Path::new("/run/claude/workloads/hb.heartbeat"),
            "/usr/local/bin/claude-watch",
            None,
        );
        assert!(
            script.contains("WORKLOAD_HEARTBEAT:-1"),
            "wrapper must default WORKLOAD_HEARTBEAT to 1 when unset:\n{script}"
        );
        assert!(
            script.contains("WORKLOAD_HEARTBEAT_INTERVAL_SECS:-900"),
            "wrapper must default heartbeat interval to 900s (15 min):\n{script}"
        );
        assert!(
            script.contains("'/tmp/claude-workloads/hb.heartbeat'"),
            "wrapper must write to the per-label heartbeat path:\n{script}"
        );
        assert!(
            script.contains("HEARTBEAT_PID=$!"),
            "wrapper must capture sidecar pid:\n{script}"
        );
        // EXIT trap must reap the heartbeat sidecar (new multi-line trap
        // also reaps the runtime heartbeat sidecar — assert on the
        // load-bearing kill substring rather than the literal trap line
        // so the assertion stays robust to formatting tweaks).
        assert!(
            script.contains("if [ -n \"$HEARTBEAT_PID\" ]; then kill -TERM -\"$HEARTBEAT_PID\""),
            "wrapper must reap HEARTBEAT_PID in EXIT trap:\n{script}"
        );
        assert!(
            script.contains("kill -TERM -\"$HEARTBEAT_PID\""),
            "wrapper must kill the sidecar's whole process group (kill -- -pgid):\n{script}"
        );
        assert!(
            script.contains("setsid bash -c 'while true"),
            "sidecar must run via setsid so it owns its own pgid:\n{script}"
        );
        assert!(
            script.contains("WORKLOAD_HB_FILE="),
            "heartbeat path must be passed via env var (not inline quoted) to avoid breaking the outer single-quote of bash -c:\n{script}"
        );
        // After setsid returns, before emit-done, the wrapper kills the
        // sidecar so the heartbeat does NOT keep getting pet during the
        // 30s tmux keepalive sleep at the end (otherwise a workload that
        // exited an hour ago could still appear "alive" to the watchdog).
        let post_exit_idx = script.find("=== DONE (exit $EC)")
            .expect("DONE line present");
        let post_exit = &script[post_exit_idx..];
        assert!(
            post_exit.contains("kill") && post_exit.contains("HEARTBEAT_PID"),
            "wrapper must kill sidecar after the user command exits, not just on EXIT trap:\n{post_exit}"
        );
    }

    #[test]
    fn wrapper_script_heartbeat_disabled_via_env() {
        // The opt-out path: WORKLOAD_HEARTBEAT=0 in the environment skips
        // the sidecar entirely. The script still must compile (the
        // shell-script-side guard does the work).
        let script = build_wrapper_script(
            "hbo",
            "true",
            Path::new("/tmp/hbo.output"),
            Path::new("/tmp/hbo.exit"),
            Path::new("/tmp/hbo.heartbeat"),
            Path::new("/run/claude/workloads/hbo.heartbeat"),
            "/bin/claude-watch",
            None,
        );
        // Must reference the env var (gating logic exists).
        assert!(
            script.contains("WORKLOAD_HEARTBEAT:-1"),
            "wrapper must reference WORKLOAD_HEARTBEAT env var:\n{script}"
        );
        // The condition checks for "!= 0" (default-on, opt-out):
        assert!(
            script.contains("\"${WORKLOAD_HEARTBEAT:-1}\" != \"0\""),
            "wrapper heartbeat must be default-on (opt-out via =0):\n{script}"
        );
    }

    /// Asserts the runtime heartbeat sidecar is wired into the wrapper as
    /// a PROGRESS-driven re-touch loop (not a dumb timer). The sidecar
    /// must:
    ///   * be gated by WORKLOAD_RUNTIME_HEARTBEAT (default-on, opt-out)
    ///   * poll on `WORKLOAD_RUNTIME_HEARTBEAT_INTERVAL_SECS` (default 30s)
    ///   * write to the per-label heartbeat path under
    ///     `/run/claude/workloads/`
    ///   * `mkdir -p` the parent dir (fresh tmpfs boot)
    ///   * `stat -c %s "$WORKLOAD_RT_HB_OUTPUT"` to read the workload's
    ///     output file size each tick and only re-touch the heartbeat
    ///     when the size has grown since the last poll
    ///   * receive the workload's `.output` file path via
    ///     `WORKLOAD_RT_HB_OUTPUT=<out_q>` (so the touch loop can stat
    ///     it without nested single-quoting)
    ///   * still touch the heartbeat ONCE on startup (warm-up coverage)
    ///   * be spawned via `setsid` for clean process-group kill on EXIT
    ///   * be reaped both in the EXIT trap and BEFORE `emit-done`
    ///   * have its heartbeat file removed on EXIT (no leftover freshness)
    #[test]
    fn wrapper_script_contains_runtime_heartbeat_sidecar() {
        let script = build_wrapper_script(
            "rt",
            "true",
            Path::new("/tmp/rt.output"),
            Path::new("/tmp/rt.exit"),
            Path::new("/tmp/rt.heartbeat"),
            Path::new("/run/claude/workloads/rt.heartbeat"),
            "/usr/local/bin/claude-watch",
            None,
        );
        // Master env switch — default-on, opt-out via =0.
        assert!(
            script.contains("WORKLOAD_RUNTIME_HEARTBEAT:-1"),
            "wrapper must default WORKLOAD_RUNTIME_HEARTBEAT to 1:\n{script}"
        );
        // Default poll interval = 30s.
        assert!(
            script.contains("WORKLOAD_RUNTIME_HEARTBEAT_INTERVAL_SECS:-30"),
            "wrapper must default runtime heartbeat poll interval to 30s:\n{script}"
        );
        // Must reference the runtime heartbeat path (not just the
        // legacy 15-min one).
        assert!(
            script.contains("'/run/claude/workloads/rt.heartbeat'"),
            "wrapper must write to the per-label runtime heartbeat path:\n{script}"
        );
        // Must `mkdir -p` the parent dir so a fresh tmpfs boot works.
        assert!(
            script.contains("mkdir -p '/run/claude/workloads'"),
            "wrapper must mkdir -p the runtime heartbeat dir:\n{script}"
        );
        // Sidecar PID captured + reaped on EXIT.
        assert!(
            script.contains("RUNTIME_HEARTBEAT_PID=$!"),
            "wrapper must capture runtime sidecar pid:\n{script}"
        );
        assert!(
            script.contains("kill -TERM -\"$RUNTIME_HEARTBEAT_PID\""),
            "wrapper must kill the runtime sidecar's whole process group on EXIT trap:\n{script}"
        );
        // PROGRESS-DRIVEN design (the load-bearing change vs PR #208):
        //   1. The workload's combined-output file path is passed via
        //      WORKLOAD_RT_HB_OUTPUT so the touch loop can stat it.
        //   2. Each tick the loop stats the output size; only when it
        //      has grown does it re-touch the heartbeat file. A hung
        //      wrapped command produces no new bytes → no re-touch →
        //      the daemon's stuck-detection suppression lifts.
        //   3. There must be an initial touch BEFORE the loop, to cover
        //      the warm-up window before any output is produced.
        //   4. There must NOT be an unconditional per-tick re-touch —
        //      that's the bug we're fixing.
        let rt_sidecar_idx = script
            .find("WORKLOAD_RT_HB_FILE=")
            .expect("runtime sidecar spawn present");
        // Slice from sidecar start to the trailing `&` so we test only
        // the loop body, not the EXIT-trap touch logic later in the
        // script.
        let rt_sidecar_end = rt_sidecar_idx + script[rt_sidecar_idx..]
            .find("RUNTIME_HEARTBEAT_PID=$!")
            .expect("sidecar end marker present");
        let rt_sidecar_block = &script[rt_sidecar_idx..rt_sidecar_end];

        assert!(
            rt_sidecar_block.contains("WORKLOAD_RT_HB_OUTPUT="),
            "runtime sidecar must receive the workload output path via WORKLOAD_RT_HB_OUTPUT:\n{rt_sidecar_block}"
        );
        assert!(
            rt_sidecar_block.contains("setsid bash -c"),
            "runtime sidecar must run via setsid for clean process-group kill:\n{rt_sidecar_block}"
        );
        assert!(
            rt_sidecar_block.contains("stat -c %s \"$WORKLOAD_RT_HB_OUTPUT\""),
            "runtime sidecar must stat the workload's .output file size each tick (progress detection):\n{rt_sidecar_block}"
        );
        assert!(
            rt_sidecar_block.contains("prev_size") && rt_sidecar_block.contains("cur_size"),
            "runtime sidecar must compare previous + current output size to detect progress:\n{rt_sidecar_block}"
        );
        assert!(
            rt_sidecar_block.contains("if [ \"$cur_size\" != \"$prev_size\" ]"),
            "runtime sidecar must only re-touch heartbeat when output size changed:\n{rt_sidecar_block}"
        );
        // Initial touch BEFORE the sidecar spawn — covers warm-up
        // before the wrapped command produces any output. Search the
        // whole pre-sidecar region (don't tight-window, since the
        // template's comments can grow without breaking semantics).
        let pre_sidecar = &script[..rt_sidecar_idx];
        assert!(
            pre_sidecar.contains("date -Iseconds > '/run/claude/workloads/rt.heartbeat'.tmp"),
            "wrapper must do an initial heartbeat touch BEFORE the progress-poll sidecar starts"
        );
        // Runtime heartbeat FILE removed on EXIT so daemon sees no
        // leftover freshness from a crashed wrapper.
        assert!(
            script.contains("rm -f '/run/claude/workloads/rt.heartbeat'"),
            "wrapper EXIT trap must remove the runtime heartbeat file:\n{script}"
        );
        // The runtime heartbeat sidecar is killed BEFORE emit-done so
        // the daemon's next stuck-check sees no fresh proof-of-life.
        let post_exit_idx = script
            .find("=== DONE (exit $EC)")
            .expect("DONE line present");
        let post_exit = &script[post_exit_idx..];
        assert!(
            post_exit.contains("RUNTIME_HEARTBEAT_PID"),
            "wrapper must kill runtime sidecar after user command exits (not just EXIT trap):\n{post_exit}"
        );
    }

    /// Anti-regression: the runtime sidecar loop body must NOT contain
    /// an unconditional "touch the heartbeat on every tick" pattern.
    /// That was the PR #208 design flaw — wrapper alive + child hung =
    /// heartbeat stays fresh = real stuck state hidden.
    #[test]
    fn wrapper_script_runtime_sidecar_does_not_touch_unconditionally() {
        let script = build_wrapper_script(
            "rt2",
            "true",
            Path::new("/tmp/rt2.output"),
            Path::new("/tmp/rt2.exit"),
            Path::new("/tmp/rt2.heartbeat"),
            Path::new("/run/claude/workloads/rt2.heartbeat"),
            "/usr/local/bin/claude-watch",
            None,
        );
        let rt_idx = script
            .find("WORKLOAD_RT_HB_FILE=")
            .expect("runtime sidecar present");
        // The loop body starts after the `setsid bash -c '` opening and
        // ends at the closing `'`. Find the end of the loop body so we
        // don't false-positive on the initial touch above.
        let loop_start = rt_idx + script[rt_idx..]
            .find("while true; do")
            .expect("while true present");
        let loop_end = loop_start + script[loop_start..]
            .find("done\n")
            .expect("done marker present");
        let loop_body = &script[loop_start..loop_end];
        // The dumb-timer pattern was:
        //     sleep N
        //     date -Iseconds > $HB.tmp && mv -f $HB.tmp $HB
        // i.e. NO conditional / NO size-comparison between the sleep
        // and the touch. The fixed loop wraps the touch in
        // `if [ "$cur_size" != "$prev_size" ]; then ... fi`. Assert
        // the conditional is present.
        assert!(
            loop_body.contains("if [ \"$cur_size\" != \"$prev_size\" ]"),
            "runtime sidecar loop body must guard the heartbeat touch with a progress check; \
             unconditional per-tick touch is the regression we're guarding against:\n{loop_body}"
        );
    }

    /// End-to-end: run the wrapper with the runtime heartbeat ENABLED
    /// and a fast interval, verify the runtime heartbeat file is
    /// created (initial touch on startup), AND that the file is
    /// REMOVED on wrapper exit (cleanup contract — see the EXIT trap
    /// in `build_wrapper_script`). This catches shell-syntax bugs the
    /// contains-tests can't.
    #[test]
    fn wrapper_script_runtime_pets_and_cleans_runtime_heartbeat() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let out_path = tmp.path().join("rh2.output");
        let exit_path = tmp.path().join("rh2.exit");
        let hb_path = tmp.path().join("rh2.heartbeat");
        // Runtime heartbeat under a SUBDIR so the wrapper's
        // `mkdir -p` path is exercised end-to-end (the real prod
        // path is `/run/claude/workloads/`, but the wrapper must
        // create it if missing).
        let rt_hb_path = tmp.path().join("runtime").join("rh2.heartbeat");
        let script_path = tmp.path().join("rh2.sh");

        let script_full = build_wrapper_script(
            "rh2",
            "echo running; sleep 1",
            &out_path,
            &exit_path,
            &hb_path,
            &rt_hb_path,
            "/bin/true",
            None,
        );
        // Patch out the trailing tmux-keepalive sleep so the test runs
        // in ~1s (the user command sleeps 1s by design — covers an
        // interval boundary).
        let script = script_full.replace("sleep 30\n", "sleep 0\n");
        std::fs::write(&script_path, &script).expect("write script");
        std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o700))
            .expect("chmod");

        // Run with the slow heartbeat DISABLED (separate concern, has
        // its own test) and a 1-second runtime heartbeat interval; the
        // sidecar should pet at least once during the 1-second user
        // command.
        let status = Command::new("bash")
            .env("WORKLOAD_HEARTBEAT", "0")
            .env("WORKLOAD_RUNTIME_HEARTBEAT_INTERVAL_SECS", "1")
            .arg(&script_path)
            .status()
            .expect("run wrapper");
        assert!(
            status.success(),
            "wrapper exited non-zero: {status:?}\nscript:\n{script}"
        );

        // Wrapper exit removes the runtime heartbeat file via the EXIT
        // trap. Cleanup IS the contract — a leftover file would falsely
        // suppress stuck-alerts after the workload finished.
        std::thread::sleep(Duration::from_millis(200));
        assert!(
            !rt_hb_path.exists(),
            "runtime heartbeat file must be removed on wrapper exit; \
             found leftover at {rt_hb_path:?}"
        );

        // The parent directory must exist (mkdir -p ran) so a daemon
        // scan from the same path won't ENOENT-fail.
        assert!(
            rt_hb_path.parent().expect("has parent").exists(),
            "wrapper must mkdir -p the runtime heartbeat dir"
        );

        // Exit file must also exist with the user-command rc.
        let ec = std::fs::read_to_string(&exit_path).expect("read exit");
        assert_eq!(ec.trim(), "0", "expected exit 0; got {ec:?}");
    }

    /// End-to-end: a workload that emits MANY progress lines triggers
    /// many heartbeat re-touches. The happy-path proof that the
    /// progress-driven sidecar fires when the wrapped command IS
    /// making progress.
    ///
    /// Strategy: run a wrapped command that emits one line per second
    /// for 5 seconds. After it exits, capture the heartbeat mtime
    /// AND compare it to the wrapper-start time. The heartbeat must
    /// have advanced by at least 3 seconds past wrapper-start (one
    /// touch per output line, debounced to one touch per poll
    /// interval). We snapshot via the wrapped command itself
    /// (writing `stat -c %Y` to a side file BEFORE the wrapper's EXIT
    /// trap can remove the heartbeat).
    #[test]
    fn wrapper_script_runtime_heartbeat_refreshes_when_output_grows() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let out_path = tmp.path().join("prog.output");
        let exit_path = tmp.path().join("prog.exit");
        let hb_path = tmp.path().join("prog.heartbeat");
        let rt_hb_path = tmp.path().join("runtime").join("prog.heartbeat");
        let script_path = tmp.path().join("prog.sh");
        let start_mtime_path = tmp.path().join("start.mtime");
        let final_mtime_path = tmp.path().join("final.mtime");
        // The wrapped command snapshots the heartbeat mtime up-front
        // (a baseline from the wrapper's initial touch), then emits 5
        // lines on a 1s cadence (5x the poll interval), then captures
        // the heartbeat mtime again. The progress-driven sidecar must
        // have re-touched at least once -> final_mtime > start_mtime.
        let user_cmd = format!(
            "stat -c %Y {hb_s} > {start}; \
             echo line1; sleep 1; \
             echo line2; sleep 1; \
             echo line3; sleep 1; \
             echo line4; sleep 1; \
             echo line5; sleep 1; \
             stat -c %Y {hb_f} > {final}",
            hb_s = rt_hb_path.display(),
            hb_f = rt_hb_path.display(),
            start = start_mtime_path.display(),
            final = final_mtime_path.display(),
        );

        let script_full = build_wrapper_script(
            "prog",
            &user_cmd,
            &out_path,
            &exit_path,
            &hb_path,
            &rt_hb_path,
            "/bin/true",
            None,
        );
        let script = script_full.replace("sleep 30\n", "sleep 0\n");
        std::fs::write(&script_path, &script).expect("write script");
        std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o700))
            .expect("chmod");

        // Slow heartbeat OFF; runtime heartbeat poll = 1s. PTY OFF
        // for hermetic behavior on CI (no PTY echo / EOL conversion
        // noise from `script`).
        let status = Command::new("bash")
            .env("WORKLOAD_HEARTBEAT", "0")
            .env("WORKLOAD_RUNTIME_HEARTBEAT_INTERVAL_SECS", "1")
            .env("WORKLOAD_PTY", "0")
            .arg(&script_path)
            .status()
            .expect("run wrapper");
        assert!(
            status.success(),
            "wrapper exited non-zero: {status:?}\nscript:\n{script}"
        );

        let start = std::fs::read_to_string(&start_mtime_path)
            .expect("read start.mtime")
            .trim()
            .parse::<i64>()
            .expect("start mtime int");
        let final_mt = std::fs::read_to_string(&final_mtime_path)
            .expect("read final.mtime")
            .trim()
            .parse::<i64>()
            .expect("final mtime int");
        let delta = final_mt - start;
        assert!(
            delta >= 2,
            "runtime heartbeat mtime must advance by >=2s across 5x 1s output lines: \
             start={start} final={final_mt} delta={delta}s\n\
             (delta < 2 means the sidecar is not re-touching on progress)"
        );
    }

    /// End-to-end: a workload that emits NOTHING after startup leaves
    /// the heartbeat mtime stuck. Load-bearing proof that the
    /// progress-driven sidecar does NOT give false-confidence when
    /// the wrapped command hangs (the PR #208 regression case).
    ///
    /// Strategy: the wrapped command captures the heartbeat mtime
    /// immediately on entry (baseline), then `sleep 5` silently (5x
    /// the 1s poll interval, no output writes at all), then captures
    /// again. The progress-driven sidecar sees zero growth across all
    /// polls -> the second mtime equals the first.
    ///
    /// This test is hermetic in a way the earlier `exec >/dev/null;
    /// sleep` variant was not: there's no inner bash output, no
    /// flushable libstdbuf buffer, no race between snap_a and the
    /// sidecar's first poll. The only growth in `.output` after
    /// snap_a is the wrapper's `=== DONE ===` footer, which lands
    /// AFTER snap_b has already captured the heartbeat mtime.
    #[test]
    fn wrapper_script_runtime_heartbeat_does_not_refresh_when_silent() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let out_path = tmp.path().join("hung.output");
        let exit_path = tmp.path().join("hung.exit");
        let hb_path = tmp.path().join("hung.heartbeat");
        let rt_hb_path = tmp.path().join("runtime").join("hung.heartbeat");
        let script_path = tmp.path().join("hung.sh");
        let start_mtime_path = tmp.path().join("start.mtime");
        let final_mtime_path = tmp.path().join("final.mtime");
        // Capture heartbeat mtime on entry, sleep 5s silently,
        // capture again. `stat -c %Y > file` writes to a SIDE file,
        // not stdout, so the .output file genuinely doesn't grow
        // between the two snapshots.
        let user_cmd = format!(
            "stat -c %Y {hb_s} > {start}; \
             sleep 5; \
             stat -c %Y {hb_f} > {final}",
            hb_s = rt_hb_path.display(),
            hb_f = rt_hb_path.display(),
            start = start_mtime_path.display(),
            final = final_mtime_path.display(),
        );

        let script_full = build_wrapper_script(
            "hung",
            &user_cmd,
            &out_path,
            &exit_path,
            &hb_path,
            &rt_hb_path,
            "/bin/true",
            None,
        );
        let script = script_full.replace("sleep 30\n", "sleep 0\n");
        std::fs::write(&script_path, &script).expect("write script");
        std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o700))
            .expect("chmod");

        // Slow heartbeat OFF; runtime heartbeat poll = 1s. PTY OFF
        // for hermetic behavior.
        let status = Command::new("bash")
            .env("WORKLOAD_HEARTBEAT", "0")
            .env("WORKLOAD_RUNTIME_HEARTBEAT_INTERVAL_SECS", "1")
            .env("WORKLOAD_PTY", "0")
            .arg(&script_path)
            .status()
            .expect("run wrapper");
        assert!(
            status.success(),
            "wrapper exited non-zero: {status:?}\nscript:\n{script}"
        );

        let start = std::fs::read_to_string(&start_mtime_path)
            .expect("read start.mtime")
            .trim()
            .parse::<i64>()
            .expect("start mtime int");
        let final_mt = std::fs::read_to_string(&final_mtime_path)
            .expect("read final.mtime")
            .trim()
            .parse::<i64>()
            .expect("final mtime int");
        let out_body = std::fs::read_to_string(&out_path).unwrap_or_default();
        assert_eq!(
            start, final_mt,
            "runtime heartbeat mtime must NOT advance during a 5s silent stretch: \
             start={start} final={final_mt} delta={delta}s\n\
             .output body:\n{out_body}\n\
             (if final > start: sidecar is still touching on a timer — the PR #208 regression)",
            delta = final_mt - start,
        );
    }

    #[test]
    fn wrapper_script_runtime_pets_and_reaps_heartbeat() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let out_path = tmp.path().join("rh.output");
        let exit_path = tmp.path().join("rh.exit");
        let hb_path = tmp.path().join("rh.heartbeat");
        let rt_hb_path = tmp.path().join("rh.runtime.heartbeat");
        let script_path = tmp.path().join("rh.sh");

        let script_full = build_wrapper_script(
            "rh",
            "echo running; sleep 1",
            &out_path,
            &exit_path,
            &hb_path,
            &rt_hb_path,
            "/bin/true",
            None,
        );
        // Patch out the trailing tmux-keepalive sleep so the test runs in
        // ~1s (the user command sleeps 1s by design — covers an interval
        // boundary).
        let script = script_full.replace("sleep 30\n", "sleep 0\n");
        std::fs::write(&script_path, &script).expect("write script");
        std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o700))
            .expect("chmod");

        // Run with a 1-second heartbeat interval; that way during the
        // 1-second user command the sidecar should pet at least once.
        // Disable the runtime heartbeat (separate sidecar) — this test
        // only exercises the legacy 15-min heartbeat sidecar.
        let status = Command::new("bash")
            .env("WORKLOAD_HEARTBEAT_INTERVAL_SECS", "1")
            .env("WORKLOAD_RUNTIME_HEARTBEAT", "0")
            .arg(&script_path)
            .status()
            .expect("run wrapper");
        assert!(
            status.success(),
            "wrapper exited non-zero: {status:?}\nscript:\n{script}"
        );

        // (a) Heartbeat file must exist (touched on startup).
        assert!(
            hb_path.exists(),
            "heartbeat file was never created at {hb_path:?}\nscript:\n{script}"
        );
        let body = std::fs::read_to_string(&hb_path).expect("read heartbeat");
        // Body should be an ISO8601 timestamp from `date -Iseconds`.
        let ts_re = regex_lite::Regex::new(
            r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}[+-]\d{2}:?\d{2}",
        )
        .expect("compile regex");
        assert!(
            ts_re.is_match(body.trim()),
            "heartbeat body is not ISO8601: {body:?}"
        );

        // (b) Capture mtime, sleep > 2× interval, verify it didn't move
        // (sidecar reaped). If the EXIT trap is broken the sidecar
        // would still be petting the file after wrapper exit.
        let mtime_a = std::fs::metadata(&hb_path)
            .expect("stat hb")
            .modified()
            .expect("mtime");
        std::thread::sleep(Duration::from_millis(2500));
        let mtime_b = std::fs::metadata(&hb_path)
            .expect("stat hb")
            .modified()
            .expect("mtime");
        assert_eq!(
            mtime_a, mtime_b,
            "heartbeat file kept getting touched after wrapper exit — sidecar leaked past EXIT trap"
        );

        // (c) Exit file must also exist with the user-command rc.
        let ec = std::fs::read_to_string(&exit_path).expect("read exit");
        assert_eq!(ec.trim(), "0", "expected exit 0; got {ec:?}");
    }

    // ----- script-capture tests -----

    #[test]
    fn capture_bash_script_happy_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let script = dir.path().join("hello.sh");
        std::fs::write(&script, "#!/bin/bash\necho hi\necho there\n").expect("write");

        let args = vec!["bash".to_string(), script.to_string_lossy().to_string()];
        let cap = try_capture_script(&args).expect("capture");
        assert_eq!(cap.interpreter, "bash");
        assert_eq!(cap.path, script.to_string_lossy());
        assert!(!cap.truncated);
        assert!(!cap.binary);
        assert_eq!(
            cap.content.as_deref(),
            Some("#!/bin/bash\necho hi\necho there\n")
        );
        assert_eq!(cap.size_bytes, 31);
        // sha256 of the file content
        let expected = {
            let mut h = Sha256::new();
            h.update(b"#!/bin/bash\necho hi\necho there\n");
            format!("{:x}", h.finalize())
        };
        assert_eq!(cap.sha256, expected);
    }

    #[test]
    fn capture_python_versioned_interpreter() {
        let dir = tempfile::tempdir().expect("tempdir");
        let script = dir.path().join("script.py");
        std::fs::write(&script, "print('hi')\n").expect("write");

        let args = vec![
            "python3.11".to_string(),
            script.to_string_lossy().to_string(),
        ];
        let cap = try_capture_script(&args).expect("capture");
        assert_eq!(cap.interpreter, "python3");
        assert_eq!(cap.content.as_deref(), Some("print('hi')\n"));
    }

    #[test]
    fn capture_absolute_interpreter_path_basename_match() {
        let dir = tempfile::tempdir().expect("tempdir");
        let script = dir.path().join("foo.sh");
        std::fs::write(&script, "exit 0\n").expect("write");

        let args = vec![
            "/usr/bin/bash".to_string(),
            script.to_string_lossy().to_string(),
        ];
        let cap = try_capture_script(&args).expect("capture");
        assert_eq!(cap.interpreter, "bash");
    }

    #[test]
    fn capture_refuses_symlink() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("real.sh");
        std::fs::write(&target, "echo hi\n").expect("write target");
        let symlink = dir.path().join("link.sh");
        std::os::unix::fs::symlink(&target, &symlink).expect("symlink");

        let args = vec!["bash".to_string(), symlink.to_string_lossy().to_string()];
        let cap = try_capture_script(&args);
        assert!(
            cap.is_none(),
            "symlink must be refused (don't follow into /etc/shadow etc.)"
        );
    }

    #[test]
    fn capture_refuses_nonexistent_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("does-not-exist.sh");
        let args = vec!["bash".to_string(), missing.to_string_lossy().to_string()];
        assert!(try_capture_script(&args).is_none());
    }

    #[test]
    fn capture_refuses_dash_c_inline_script() {
        // `bash -c 'echo hi'` is NOT a script invocation — refuse.
        let args = vec![
            "bash".to_string(),
            "-c".to_string(),
            "echo hi".to_string(),
        ];
        assert!(try_capture_script(&args).is_none());
    }

    #[test]
    fn capture_refuses_unknown_interpreter() {
        let dir = tempfile::tempdir().expect("tempdir");
        let script = dir.path().join("thing");
        std::fs::write(&script, "hello\n").expect("write");
        let args = vec!["ls".to_string(), script.to_string_lossy().to_string()];
        assert!(try_capture_script(&args).is_none());
    }

    #[test]
    fn capture_refuses_too_many_args() {
        let dir = tempfile::tempdir().expect("tempdir");
        let script = dir.path().join("foo.sh");
        std::fs::write(&script, "echo $1\n").expect("write");
        // `bash foo.sh arg1` — three positional args, not a single-file
        // invocation we're confident about. Could enable later but for
        // now the explicit test guards the "exactly 2 args" rule.
        let args = vec![
            "bash".to_string(),
            script.to_string_lossy().to_string(),
            "arg1".to_string(),
        ];
        assert!(try_capture_script(&args).is_none());
    }

    #[test]
    fn capture_truncates_oversized_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let script = dir.path().join("big.sh");
        // Build a file just over the cap.
        let body = "a".repeat(SCRIPT_CAPTURE_MAX_BYTES as usize + 100);
        std::fs::write(&script, &body).expect("write");

        let args = vec!["bash".to_string(), script.to_string_lossy().to_string()];
        let cap = try_capture_script(&args).expect("capture");
        assert!(cap.truncated);
        assert_eq!(cap.size_bytes, body.len() as u64);
        let content = cap.content.as_deref().expect("content");
        assert_eq!(content.len(), SCRIPT_CAPTURE_MAX_BYTES as usize);
        // sha256 is over the FULL body, not the truncated slice.
        let expected = {
            let mut h = Sha256::new();
            h.update(body.as_bytes());
            format!("{:x}", h.finalize())
        };
        assert_eq!(cap.sha256, expected);
    }

    #[test]
    fn capture_detects_binary_content() {
        let dir = tempfile::tempdir().expect("tempdir");
        let script = dir.path().join("blob");
        // NUL byte inside the first 512 bytes → binary.
        let mut body: Vec<u8> = b"#!/bin/bash\n".to_vec();
        body.push(0);
        body.extend_from_slice(b"echo hi\n");
        std::fs::write(&script, &body).expect("write");

        let args = vec!["bash".to_string(), script.to_string_lossy().to_string()];
        let cap = try_capture_script(&args).expect("capture");
        assert!(cap.binary, "NUL byte in head should flag binary");
        assert!(
            cap.content.is_none(),
            "binary content must omit body, keep metadata"
        );
        assert_eq!(cap.size_bytes, body.len() as u64);
        assert!(!cap.sha256.is_empty());
    }

    #[test]
    fn capture_does_not_crash_on_missing_dir() {
        // Resolving a relative-with-cwd path that isn't on PATH must
        // simply return None, not panic.
        let args = vec![
            "bash".to_string(),
            "definitely-not-on-path-xyz.sh".to_string(),
        ];
        assert!(try_capture_script(&args).is_none());
    }

    #[test]
    fn capture_resolves_path_lookup() {
        // Place a script in a tmpdir, prepend to PATH, invoke with bare
        // basename. We want resolve_script_path to find it via PATH.
        let _guard = WORKLOAD_TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().expect("tempdir");
        let script = dir.path().join("only-on-path.sh");
        std::fs::write(&script, "echo hi\n").expect("write");

        // Save and override PATH so the test is hermetic.
        let prev_path = std::env::var_os("PATH");
        let new_path = match &prev_path {
            Some(p) => {
                let mut v = std::ffi::OsString::new();
                v.push(dir.path());
                v.push(":");
                v.push(p);
                v
            }
            None => dir.path().as_os_str().to_os_string(),
        };
        // SAFETY: WORKLOAD_TEST_ENV_LOCK held across the read/write.
        unsafe {
            std::env::set_var("PATH", &new_path);
        }

        let args = vec![
            "bash".to_string(),
            "only-on-path.sh".to_string(),
        ];
        let cap = try_capture_script(&args);

        // Restore PATH before any assert (so a failing assert doesn't
        // leave the test process with a polluted PATH).
        unsafe {
            match prev_path {
                Some(p) => std::env::set_var("PATH", p),
                None => std::env::remove_var("PATH"),
            }
        }

        let cap = cap.expect("PATH lookup should resolve");
        assert_eq!(cap.interpreter, "bash");
        assert_eq!(cap.content.as_deref(), Some("echo hi\n"));
    }

    #[test]
    fn capture_serde_roundtrip() {
        let cap = ScriptCapture {
            path: "/tmp/foo.sh".to_string(),
            interpreter: "bash".to_string(),
            size_bytes: 42,
            truncated: false,
            binary: false,
            content: Some("echo hi\n".to_string()),
            sha256: "abc123".to_string(),
        };
        let j = serde_json::to_string(&cap).expect("serialize");
        let back: ScriptCapture = serde_json::from_str(&j).expect("deserialize");
        assert_eq!(cap, back);
    }

    // ----- legacy compat symlink helper -------------------------------------

    /// Fresh-tempdir case: the legacy path doesn't exist yet, so we
    /// create the symlink pointing at the real WORKLOAD_DIR.
    #[test]
    fn create_compat_symlink_links_when_missing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let target = tmp.path().join("workload-state");
        std::fs::create_dir(&target).expect("mkdir target");
        let legacy = tmp.path().join("legacy");
        // Pre-condition: legacy doesn't exist.
        assert!(
            !legacy.exists(),
            "legacy path must not exist before the test"
        );
        create_compat_symlink(&legacy, &target);
        // Post-condition: legacy IS a symlink to target.
        let meta = std::fs::symlink_metadata(&legacy)
            .expect("symlink metadata after create");
        assert!(meta.file_type().is_symlink(), "legacy must be a symlink");
        let resolved = std::fs::read_link(&legacy).expect("read_link");
        assert_eq!(resolved, target, "symlink target mismatch");
    }

    /// Idempotency case: helper runs twice in a row, no error, no panic.
    /// The pre-existing symlink stays as-is.
    #[test]
    fn create_compat_symlink_is_idempotent() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let target = tmp.path().join("workload-state");
        std::fs::create_dir(&target).expect("mkdir target");
        let legacy = tmp.path().join("legacy");
        create_compat_symlink(&legacy, &target);
        // Second invocation should be a no-op.
        create_compat_symlink(&legacy, &target);
        let meta = std::fs::symlink_metadata(&legacy)
            .expect("symlink metadata after second create");
        assert!(
            meta.file_type().is_symlink(),
            "legacy must still be a symlink"
        );
    }

    /// Real-directory case: legacy already exists as a directory (a
    /// still-running legacy workload pre-migration). Helper must NOT
    /// touch it — the operator owns cleanup.
    #[test]
    fn create_compat_symlink_skips_existing_dir() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let target = tmp.path().join("workload-state");
        std::fs::create_dir(&target).expect("mkdir target");
        let legacy = tmp.path().join("legacy");
        std::fs::create_dir(&legacy).expect("mkdir legacy as real dir");
        // Drop a file inside so we can verify nothing got clobbered.
        let canary = legacy.join("canary");
        std::fs::write(&canary, b"untouched").expect("write canary");
        create_compat_symlink(&legacy, &target);
        // Post-condition: legacy is still a dir (NOT a symlink), canary
        // still exists.
        let meta = std::fs::symlink_metadata(&legacy)
            .expect("symlink metadata after create");
        assert!(
            !meta.file_type().is_symlink(),
            "real legacy dir must NOT be replaced with a symlink"
        );
        assert!(meta.is_dir(), "legacy must still be a directory");
        assert!(canary.exists(), "canary inside legacy must survive");
    }
}
