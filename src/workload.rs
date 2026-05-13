//! workload — launch long-running tasks in the `tasks` tmux session that
//! survive Claude Code /clear and compaction.
//!
//! Straight Rust port of the Python `workload` script. State lives under
//! `/tmp/claude-workloads/` (state.json, <label>.output, <label>.exit,
//! <label>.sh) for compatibility with the existing layout so in-flight
//! workloads from the old script keep working during the transition.
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
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

const SESSION: &str = "tasks";
const WORKLOAD_DIR: &str = "/tmp/claude-workloads";

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
/// already unique within `/tmp/claude-workloads/state.json` — kill+run
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
    exe_path: &str,
    queue_id: Option<&str>,
) -> String {
    let out_q = shell_quote(&out_path.to_string_lossy());
    let exit_q = shell_quote(&exit_path.to_string_lossy());
    let hb_q = shell_quote(&heartbeat_path.to_string_lossy());
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
         # Reap the heartbeat sidecar on any wrapper exit (normal, signal, or\n\
         # tmux kill-pane). Without this the sidecar leaks and keeps petting\n\
         # the watchdog after the workload has died — exactly the case we\n\
         # want to detect. EXIT pseudo-signal fires unconditionally. Kill the\n\
         # whole process group (negative pid) so any in-flight `sleep` dies\n\
         # alongside the loop subshell.\n\
         trap 'if [ -n \"$HEARTBEAT_PID\" ]; then kill -TERM -\"$HEARTBEAT_PID\" 2>/dev/null || kill \"$HEARTBEAT_PID\" 2>/dev/null || true; fi' EXIT\n\
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
         # Stop heartbeat BEFORE emit-done so the .exit + stop happen tightly.\n\
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

    if let Err(e) = fs::create_dir_all(WORKLOAD_DIR) {
        eprintln!("Failed to create {WORKLOAD_DIR}: {e}");
        return 1;
    }

    let out_path = output_file(label);
    let exit_path = exit_file(label);
    let heartbeat_path = heartbeat_file(label);
    let script_path = script_file(label);

    // Clean up previous run's exit marker + output + heartbeat. The
    // heartbeat MUST be removed up-front so the stale-watchdog detector
    // can't get a false-positive on a stale leftover from a prior run
    // that pet the watchdog and then crashed.
    let _ = fs::remove_file(&exit_path);
    let _ = fs::remove_file(&out_path);
    let _ = fs::remove_file(&heartbeat_path);
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
        &exe_path,
        effective_queue_id.as_deref(),
    );

    if let Err(e) = fs::write(&script_path, script) {
        eprintln!("Failed to write script: {e}");
        return 1;
    }
    let _ = fs::set_permissions(&script_path, fs::Permissions::from_mode(0o700));

    // Create pane running the script
    let out = Command::new("tmux")
        .args([
            "split-window",
            "-t",
            SESSION,
            "-v",
            "-P",
            "-F",
            "#{pane_id}",
            &script_path.to_string_lossy(),
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
        // Existing /tmp/claude-workloads/state.json files predate the
        // queue_id field — must deserialize cleanly with queue_id=None.
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
        let script_full = build_wrapper_script(
            "rt",
            "echo first; echo second",
            &out_path,
            &exit_path,
            &hb_path,
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
            let script_path = tmp.path().join(format!("{label}.sh"));

            let script_full = build_wrapper_script(
                label,
                inner,
                &out_path,
                &exit_path,
                &hb_path,
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
            let script_path = tmp.path().join(format!("{label}.sh"));

            let script_full = build_wrapper_script(
                label,
                inner,
                &out_path,
                &exit_path,
                &hb_path,
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
            "/bin/true",
            None,
        );
        let script = script_full.replace("sleep 30\n", "sleep 0\n");

        std::fs::write(&script_path, &script).expect("write script");
        std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o700))
            .expect("chmod");

        let mut child = Command::new("bash")
            .arg(&script_path)
            // Defeat the heartbeat sidecar — we don't want its writes
            // showing up in the .output sampling window.
            .env("WORKLOAD_HEARTBEAT", "0")
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
        assert!(
            script.contains("trap 'if [ -n \"$HEARTBEAT_PID\" ]; then kill"),
            "wrapper must install EXIT trap reaping sidecar pid:\n{script}"
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

    /// End-to-end: actually run the wrapper with a fast heartbeat interval
    /// and verify (a) the heartbeat file is written immediately on start,
    /// (b) the file mtime is fresh after the user command exits, and
    /// (c) the sidecar is NOT still petting the watchdog after wrapper
    /// teardown (else the EXIT trap is broken). Catches shell-syntax
    /// regressions the contains-tests can't.
    #[test]
    fn wrapper_script_runtime_pets_and_reaps_heartbeat() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let out_path = tmp.path().join("rh.output");
        let exit_path = tmp.path().join("rh.exit");
        let hb_path = tmp.path().join("rh.heartbeat");
        let script_path = tmp.path().join("rh.sh");

        let script_full = build_wrapper_script(
            "rh",
            "echo running; sleep 1",
            &out_path,
            &exit_path,
            &hb_path,
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
        let status = Command::new("bash")
            .env("WORKLOAD_HEARTBEAT_INTERVAL_SECS", "1")
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
}
