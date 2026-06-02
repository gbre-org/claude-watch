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

/// Default PID file directory for watcher liveness tracking.
pub const PID_DIR: &str = "/var/run/claude";

/// Resolve the PID directory. Respects `$CLAUDE_WATCH_PID_DIR` so tests (and
/// any sandboxed environment without write access to `/var/run/claude`) can
/// redirect the watcher PID files. Falls back to [`PID_DIR`] when unset/empty.
pub fn pid_dir() -> String {
    match std::env::var("CLAUDE_WATCH_PID_DIR") {
        Ok(p) if !p.trim().is_empty() => p,
        _ => PID_DIR.to_string(),
    }
}

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

/// Read a watcher PID file and return the recorded PID, if the file exists and
/// contains a parseable integer. Whitespace is trimmed. `None` on missing /
/// unreadable / non-numeric content.
fn read_pid_file(pid_file: &str) -> Option<u32> {
    let content = std::fs::read_to_string(pid_file).ok()?;
    content.trim().parse::<u32>().ok()
}

/// Check whether a PID is currently alive via a `kill(pid, 0)` signal probe.
///
/// Signal 0 performs no delivery but still runs the kernel's
/// permission/existence checks, so `Ok(())` means the process exists (and we
/// may signal it), while `ESRCH` means it's gone. `EPERM` means it exists but
/// we don't own it — still "alive" for our purposes. We treat any other error
/// (or success) conservatively as "alive" only on success/EPERM.
fn pid_is_alive(pid: u32) -> bool {
    use nix::errno::Errno;
    use nix::sys::signal::kill;
    use nix::unistd::Pid;
    // PID 0 is special-cased by kill(2): it targets the caller's entire
    // process group, which always "succeeds". It is never a real watcher PID,
    // so treat it as not-alive to avoid a false positive in the guard.
    if pid == 0 {
        return false;
    }
    match kill(Pid::from_raw(pid as i32), None) {
        Ok(()) => true,
        Err(Errno::EPERM) => true, // exists, just not ours
        Err(_) => false,           // ESRCH (gone) or anything else
    }
}

/// Read `/proc/PID/cmdline` (NUL-separated argv) into a space-joined string.
/// Returns `None` if the process is gone or the file is unreadable.
fn pid_cmdline(pid: u32) -> Option<String> {
    let path = format!("/proc/{}/cmdline", pid);
    let data = std::fs::read(&path).ok()?;
    let s = String::from_utf8_lossy(&data)
        .replace('\0', " ")
        .trim()
        .to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// Identity check: does the live process `pid` actually look like *this*
/// watcher, rather than a recycled PID that the kernel handed to an unrelated
/// process after the watcher died?
///
/// We compare the process's `/proc/PID/cmdline` against the watcher's
/// configured `start_cmd`. A recycled PID running some other program won't
/// share the watcher's argv, so the guard won't wrongly suppress a real
/// restart. The match is intentionally lenient (substring on the first
/// `start_cmd` token, i.e. the watcher binary/script name) because the live
/// process's argv may differ from the literal `start_cmd` — the start command
/// frequently `exec`s a child or wraps the poller (e.g. `uv run X`, or a
/// script that re-execs itself). Requiring the binary token to appear is
/// enough to reject an obviously-unrelated recycled PID while tolerating these
/// wrapper transforms.
///
/// `None` from `pid_cmdline` (process gone, or kernel-thread with empty
/// cmdline) → not a match.
fn pid_matches_watcher(pid: u32, start_cmd: &str) -> bool {
    let token = match start_cmd.split_whitespace().next() {
        Some(t) if !t.is_empty() => t,
        _ => return false,
    };
    // Use the basename of the first token so an absolute path in start_cmd
    // (e.g. `/usr/local/bin/claude-event-watch`) still matches a cmdline that
    // records the bare name, and vice-versa.
    let token_base = token.rsplit('/').next().unwrap_or(token);
    match pid_cmdline(pid) {
        Some(cmdline) => cmdline.contains(token) || cmdline.contains(token_base),
        None => false,
    }
}

/// Pure decision: given what the guard observed, should `watcher_run` no-op
/// (a live instance already holds the slot) instead of starting a second one?
///
/// Inputs (all already probed by the caller — kept pure so it's unit-testable
/// without touching `/proc` or `pgrep`):
/// - `recorded_pid_alive`: the PID file named a process that is alive AND whose
///   cmdline identity matches this watcher (recycled-PID case already filtered
///   out by the caller — a dead/stale/mismatched PID file passes `false`).
/// - `live_poller_count`: number of live processes matching the watcher's
///   `pattern` (the same signal `watcher-status` counts). A value `>= 1` means
///   a poller is already up even if the PID file is stale/missing (e.g. the
///   running instance was started out-of-band).
///
/// Returns `true` (skip / no-op, exit 0 idempotently) when either signal shows
/// a live instance; `false` (proceed to start) otherwise. This covers:
/// - fresh start, no PID file, no poller → start.
/// - stale PID file (process dead), no poller → start.
/// - PID file points at a live matching instance → skip.
/// - PID file stale/missing but a poller is already running → skip.
pub fn run_guard_should_skip(recorded_pid_alive: bool, live_poller_count: u32) -> bool {
    recorded_pid_alive || live_poller_count >= 1
}

/// Atomically claim the PID file via `O_CREAT | O_EXCL`, writing `pid`.
///
/// Returns:
/// - `Ok(true)` — we won the race and the file now records our PID.
/// - `Ok(false)` — the file already existed (someone else holds the slot); the
///   caller should treat this as "lost the race" and no-op.
/// - `Err(_)` — an unexpected I/O error (not `AlreadyExists`).
///
/// This closes the two-near-simultaneous-`run` race: even if both invocations
/// pass the pre-flight liveness check before either has spawned, only one can
/// create the lock file with `O_EXCL`; the loser backs off. The caller must
/// have already removed a *stale* PID file (dead/mismatched) before calling
/// this, so a genuine restart isn't permanently blocked by a leftover file.
fn try_claim_pid_file(pid_file: &str, pid: u32) -> std::io::Result<bool> {
    use std::io::Write as _;
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true) // O_CREAT | O_EXCL
        .open(pid_file)
    {
        Ok(mut f) => {
            f.write_all(pid.to_string().as_bytes())?;
            Ok(true)
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(e) => Err(e),
    }
}

/// RAII exclusive lock over a watcher's spawn slot, backed by `flock(2)` on a
/// dedicated `<name>.lock` file.
///
/// ## Why a `flock` lock and not just the O_EXCL PID file (BUG B fix)
///
/// The PID file alone is a fragile mutex:
///   * It is **overwritten** with the child's PID after spawn (and some
///     watcher scripts, e.g. `memory-remind`, write it themselves as a
///     belt-and-suspenders), so its existence stops meaning "a launch is in
///     progress" the instant the child is up — reopening the window for a
///     second `watcher-ctl run` to slip through.
///   * If `watcher-ctl run` is `SIGKILL`ed, the O_EXCL file **lingers** as a
///     stale lock that the next legitimate run has to detect-and-remove,
///     which itself is a TOCTOU (remove → another run O_EXCL-creates in the
///     gap).
///
/// `flock` fixes both: the lock lives on a SEPARATE file that nothing
/// overwrites, it is held by the running `watcher_run` process for the entire
/// child lifetime, and the kernel **auto-releases it when the holding process
/// dies** (clean or crash) — so there is no stale-lock to garbage-collect and
/// no remove-then-recreate gap. A non-blocking `LOCK_EX | LOCK_NB` acquire
/// means a concurrent run (or a supervisor/daemon-driven respawn that also
/// goes through `watcher_run`) that arrives while the slot is held gets
/// `EWOULDBLOCK` and backs off instead of spawning a duplicate poller.
struct WatcherLock {
    // Held for the lock's lifetime; the kernel releases the flock when this
    // fd is closed (on drop or process exit). We never read/write it.
    _file: std::fs::File,
}

impl WatcherLock {
    /// Try to acquire the exclusive spawn lock for `name` under `pid_dir`.
    ///
    /// Returns:
    /// - `Ok(Some(lock))` — we hold the lock; caller may spawn. Lock is
    ///   released when the returned guard is dropped (or the process exits).
    /// - `Ok(None)`       — another live `watcher_run` already holds it; the
    ///   caller must NOT spawn (idempotent skip).
    /// - `Err(_)`         — could not open the lock file (e.g. the lock dir is
    ///   unwritable). The caller decides how to degrade.
    fn try_acquire(pid_dir: &str, name: &str) -> std::io::Result<Option<WatcherLock>> {
        use std::os::unix::io::AsRawFd;
        let lock_path = format!("{}/{}.lock", pid_dir, name);
        // Open (create if absent) the lock file. We deliberately do NOT
        // O_EXCL here — the lock FILE persisting across runs is fine and
        // desired; mutual exclusion comes from the advisory flock on it, not
        // from the file's existence.
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)?;
        // Non-blocking exclusive advisory lock.
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc == 0 {
            Ok(Some(WatcherLock { _file: file }))
        } else {
            let err = std::io::Error::last_os_error();
            match err.raw_os_error() {
                // EWOULDBLOCK / EAGAIN: someone else holds the lock.
                Some(code) if code == libc::EWOULDBLOCK || code == libc::EAGAIN => Ok(None),
                _ => Err(err),
            }
        }
    }
}

/// Run a watcher by name. Looks up the entry, rejects if disabled or no
/// start_cmd, then execs the start_cmd and waits for it to complete.
/// Returns the exit code of the child process.
///
/// **Idempotency / PID-guard:** before starting, the function checks whether a
/// live instance already holds the watcher's slot — either via the PID file
/// (PID alive *and* cmdline identity matches this watcher, to reject recycled
/// PIDs) or via the live-poller count (`pgrep` on the watcher's pattern, the
/// same signal `watcher-status` uses). If so it prints a clear message and
/// exits 0 (success — so the main loop's restart cadence doesn't treat the
/// no-op as an error) WITHOUT spawning a second instance. A stale PID file
/// (process dead, or recycled to an unrelated PID) is cleared and the watcher
/// starts normally. The PID file is claimed atomically (`O_EXCL`) so two
/// near-simultaneous `run` invocations can't both win.
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
    let pid_dir = pid_dir();
    let _ = std::fs::create_dir_all(&pid_dir);

    let pid_file = format!("{}/{}.pid", pid_dir, name);
    let pid_file_exists = std::path::Path::new(&pid_file).exists();

    // --- Spawn-slot lock (BUG B fix) ---------------------------------------
    // Acquire an exclusive `flock` over `<name>.lock` for the WHOLE duration
    // of this run. This is the atomic, crash-safe mutex that guarantees only
    // ONE poller can be spawned at a time, no matter how many concurrent
    // `watcher-ctl run <name>` invocations (or supervisor/daemon-driven
    // respawns that route through here) race. Unlike the PID file, the lock
    // file is never overwritten and is auto-released by the kernel when this
    // process exits — so there is no stale-lock cleanup and no remove-then-
    // recreate TOCTOU. We bind it to `_slot_lock` (NOT `_`) so it lives until
    // `watcher_run` returns; `let _ = ...` would drop it immediately.
    let _slot_lock = match WatcherLock::try_acquire(&pid_dir, name) {
        Ok(Some(lock)) => Some(lock),
        Ok(None) => {
            // Another live run holds the slot. Idempotent skip (success so the
            // main loop's restart cadence doesn't treat this as an error).
            println!(
                "{} launch already in progress (spawn lock held by a concurrent run); \
                 not starting a second instance",
                name
            );
            return Ok(0);
        }
        Err(e) => {
            // Could not even open the lock file (e.g. unwritable lock dir).
            // Degrade to the PID-file/pgrep guards below rather than wedging
            // the watcher entirely — but warn loudly so the broken lock dir
            // gets noticed.
            eprintln!(
                "warning: could not acquire spawn lock for '{}': {} — falling back to PID-file guard",
                name, e
            );
            None
        }
    };

    // --- PID-guard (idempotency) -------------------------------------------
    // Determine whether a live instance already holds this watcher's slot.
    //
    // Two independent signals:
    //   1. PID file: alive AND cmdline identity matches this watcher. A
    //      recycled PID running something unrelated does NOT count (so we
    //      don't wrongly suppress a real restart). A stale PID file (process
    //      dead, or recycled to a non-matching process) is removed below so
    //      the atomic O_EXCL claim can succeed.
    //   2. Live poller count: `pgrep` on the watcher's pattern — the same
    //      signal `watcher-status` uses. Catches an instance started
    //      out-of-band whose PID isn't (or no longer is) in the file.
    let recorded_pid = read_pid_file(&pid_file);
    let recorded_pid_alive = match recorded_pid {
        Some(pid) => pid_is_alive(pid) && pid_matches_watcher(pid, start_cmd),
        None => false,
    };
    let live_poller_count = process_pids(&entry.pattern).await.len() as u32;

    if run_guard_should_skip(recorded_pid_alive, live_poller_count) {
        let where_ = if recorded_pid_alive {
            format!("pid {}", recorded_pid.unwrap())
        } else {
            format!(
                "{} live poller(s) matching '{}'",
                live_poller_count, entry.pattern
            )
        };
        println!(
            "{} already running ({}); not starting a second instance",
            name, where_
        );
        return Ok(0);
    }

    // No live instance. If a PID file lingers it is stale (dead/recycled PID)
    // — remove it so the atomic O_EXCL claim below can succeed.
    if recorded_pid.is_some() {
        let _ = std::fs::remove_file(&pid_file);
    }

    // Print history on restart (PID file existed from a previous run).
    if pid_file_exists {
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

    // Atomically claim the PID slot BEFORE spawning, with our own PID as a
    // placeholder. If another `run` invocation raced us here and already
    // created the file, back off and no-op (idempotent success) — this closes
    // the window where both invocations pass the liveness check above before
    // either has spawned. We rewrite the file with the child PID once spawned.
    match try_claim_pid_file(&pid_file, std::process::id()) {
        Ok(true) => {}
        Ok(false) => {
            println!(
                "{} launch already in progress (PID file held by a concurrent run); \
                 not starting a second instance",
                name
            );
            return Ok(0);
        }
        Err(e) => {
            // Couldn't create the lock file for an unexpected reason. Fall
            // back to a best-effort start rather than wedging the watcher.
            eprintln!("warning: could not claim PID file for '{}': {}", name, e);
        }
    }

    // Spawn child process
    let mut child = tokio::process::Command::new(args[0])
        .args(&args[1..])
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .map_err(|e| {
            // Spawn failed — release the slot we claimed so a retry isn't
            // blocked by our orphaned lock file.
            let _ = std::fs::remove_file(&pid_file);
            format!("failed to start '{}': {}", start_cmd, e)
        })?;

    // Record the real child PID (overwrite the placeholder claim).
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
    if let Ok(dir) = std::fs::read_dir(pid_dir()) {
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

    // --- PID-guard tests ---------------------------------------------------

    #[test]
    fn test_run_guard_skip_when_recorded_pid_alive() {
        // A live, identity-matched PID file → skip (no second instance),
        // regardless of poller count.
        assert!(run_guard_should_skip(true, 0));
        assert!(run_guard_should_skip(true, 1));
    }

    #[test]
    fn test_run_guard_skip_when_poller_already_running() {
        // PID file stale/missing (recorded_pid_alive=false) but a live poller
        // is already matched by pgrep → still skip.
        assert!(run_guard_should_skip(false, 1));
        assert!(run_guard_should_skip(false, 3));
    }

    #[test]
    fn test_run_guard_start_when_nothing_alive() {
        // No live PID, no poller → proceed (fresh start OR stale PID file).
        assert!(!run_guard_should_skip(false, 0));
    }

    #[test]
    fn test_read_pid_file_valid() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("w.pid");
        std::fs::write(&p, "  4242\n").unwrap();
        assert_eq!(read_pid_file(p.to_str().unwrap()), Some(4242));
    }

    #[test]
    fn test_read_pid_file_missing_or_garbage() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope.pid");
        assert_eq!(read_pid_file(missing.to_str().unwrap()), None);

        let garbage = dir.path().join("bad.pid");
        std::fs::write(&garbage, "not-a-pid").unwrap();
        assert_eq!(read_pid_file(garbage.to_str().unwrap()), None);

        let empty = dir.path().join("empty.pid");
        std::fs::write(&empty, "").unwrap();
        assert_eq!(read_pid_file(empty.to_str().unwrap()), None);
    }

    #[test]
    fn test_pid_is_alive_self_true() {
        // The test process itself is, definitionally, alive.
        assert!(pid_is_alive(std::process::id()));
    }

    #[test]
    fn test_pid_is_alive_bogus_false() {
        // PID 0 is not a real process; a very high PID is essentially
        // guaranteed not to exist on a normal system. Either way → not alive.
        assert!(!pid_is_alive(0));
        assert!(!pid_is_alive(u32::MAX - 1));
    }

    #[test]
    fn test_pid_matches_watcher_self() {
        // Our own cmdline contains the test binary path. Use the actual first
        // argv token as the start_cmd so the identity check matches.
        let argv0 = std::env::args().next().unwrap_or_default();
        assert!(
            pid_matches_watcher(std::process::id(), &argv0),
            "self cmdline should match its own argv0"
        );
    }

    #[test]
    fn test_pid_matches_watcher_mismatch_rejects_recycled_pid() {
        // A start_cmd for some unrelated binary must NOT match our process's
        // cmdline — this is the recycled-PID guard.
        assert!(!pid_matches_watcher(
            std::process::id(),
            "definitely-not-a-real-watcher-binary-xyz"
        ));
    }

    #[test]
    fn test_pid_matches_watcher_dead_pid_is_false() {
        // No cmdline for a dead PID → not a match (can't claim identity).
        assert!(!pid_matches_watcher(u32::MAX - 1, "anything"));
    }

    #[test]
    fn test_pid_matches_watcher_empty_start_cmd_is_false() {
        assert!(!pid_matches_watcher(std::process::id(), ""));
        assert!(!pid_matches_watcher(std::process::id(), "   "));
    }

    #[test]
    fn test_try_claim_pid_file_first_wins_second_loses() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("claim.pid");
        let path = p.to_str().unwrap();

        // First claim creates the file and wins.
        assert_eq!(try_claim_pid_file(path, 111).unwrap(), true);
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "111");

        // Second claim on the existing file loses (no overwrite, no error).
        assert_eq!(try_claim_pid_file(path, 222).unwrap(), false);
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "111");
    }

    #[test]
    fn test_try_claim_pid_file_after_removal_succeeds() {
        // Mirrors the stale-PID-file recovery path: remove the stale file,
        // then the claim must succeed for a genuine restart.
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("stale.pid");
        let path = p.to_str().unwrap();

        std::fs::write(&p, "999").unwrap(); // stale leftover
        std::fs::remove_file(&p).unwrap(); // caller clears it
        assert_eq!(try_claim_pid_file(path, 333).unwrap(), true);
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "333");
    }

    // --- WatcherLock (BUG B) tests -----------------------------------------
    //
    // The flock-backed spawn lock is the atomic mutex that guarantees only one
    // poller survives concurrent `watcher-ctl run <name>` invocations (or any
    // supervisor/daemon-driven respawn routed through `watcher_run`). These
    // pin the contract: a second acquire while the first is held must fail
    // (so the caller skips its spawn), and the slot must free up once the
    // holder is dropped (so a genuine later restart isn't permanently blocked).

    #[test]
    fn test_watcher_lock_excludes_concurrent_holder() {
        // BUG B regression: while one run holds the spawn lock, a second
        // concurrent acquire on the SAME watcher name must return None (lost
        // the race → must NOT spawn a duplicate poller). Modelling the
        // window where both invocations passed the pre-flight pgrep guard
        // (saw 0 live pollers) before either spawned.
        let dir = tempfile::tempdir().unwrap();
        let pid_dir = dir.path().to_str().unwrap();

        let first = WatcherLock::try_acquire(pid_dir, "memory-remind")
            .expect("first acquire should not error")
            .expect("first acquire should win the lock");

        // Second acquire while `first` is still held → None (back off).
        let second = WatcherLock::try_acquire(pid_dir, "memory-remind")
            .expect("second acquire should not error");
        assert!(
            second.is_none(),
            "a second concurrent acquire must NOT obtain the lock — \
             exactly one poller may be spawned"
        );

        // Keep `first` alive across the assertion.
        drop(first);
    }

    #[test]
    fn test_watcher_lock_released_on_drop_allows_reacquire() {
        // After the holder drops (run finished / watcher exited), the slot is
        // free again — a genuine later restart must be able to claim it.
        let dir = tempfile::tempdir().unwrap();
        let pid_dir = dir.path().to_str().unwrap();

        {
            let _first = WatcherLock::try_acquire(pid_dir, "claude-event-watch")
                .unwrap()
                .expect("first acquire wins");
            // While held, a concurrent acquire fails.
            assert!(WatcherLock::try_acquire(pid_dir, "claude-event-watch")
                .unwrap()
                .is_none());
        } // _first dropped here → kernel releases the flock.

        // Now the slot is free; re-acquire must succeed.
        let reacquired = WatcherLock::try_acquire(pid_dir, "claude-event-watch")
            .unwrap();
        assert!(
            reacquired.is_some(),
            "after the holder drops, the spawn lock must be re-acquirable so a \
             real restart isn't permanently blocked"
        );
    }

    #[test]
    fn test_watcher_lock_distinct_names_dont_collide() {
        // Two DIFFERENT watchers must lock independently — holding one must
        // not block spawning another.
        let dir = tempfile::tempdir().unwrap();
        let pid_dir = dir.path().to_str().unwrap();

        let _a = WatcherLock::try_acquire(pid_dir, "watcher-a")
            .unwrap()
            .expect("watcher-a lock");
        let b = WatcherLock::try_acquire(pid_dir, "watcher-b").unwrap();
        assert!(
            b.is_some(),
            "distinct watcher names use distinct lock files and must not \
             contend with each other"
        );
    }

    // --- PID-guard end-to-end (`watcher_run`) tests ------------------------
    //
    // These set process-global env vars (CLAUDE_WATCH_PID_DIR, WATCHERS_CONFIG)
    // so they must not run concurrently with each other. A shared mutex
    // serializes them. Each test points the PID dir + config at a unique
    // tempdir so they don't collide with the live system or each other.

    use std::sync::Mutex;
    static RUN_ENV_LOCK: Mutex<()> = Mutex::new(());

    /// RAII guard that sets the watcher env vars on construction and restores
    /// the prior values on drop, holding the serialization lock for its
    /// lifetime.
    struct RunEnv<'a> {
        _lock: std::sync::MutexGuard<'a, ()>,
        prev_pid_dir: Option<String>,
        prev_cfg: Option<String>,
        prev_cfg_extra: Option<String>,
    }
    impl<'a> RunEnv<'a> {
        fn new(pid_dir: &str, cfg: &str) -> Self {
            let lock = RUN_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let prev_pid_dir = std::env::var("CLAUDE_WATCH_PID_DIR").ok();
            let prev_cfg = std::env::var("WATCHERS_CONFIG").ok();
            let prev_cfg_extra = std::env::var("WATCHERS_CONFIG_EXTRA").ok();
            std::env::set_var("CLAUDE_WATCH_PID_DIR", pid_dir);
            std::env::set_var("WATCHERS_CONFIG", cfg);
            std::env::remove_var("WATCHERS_CONFIG_EXTRA");
            RunEnv {
                _lock: lock,
                prev_pid_dir,
                prev_cfg,
                prev_cfg_extra,
            }
        }
    }
    impl<'a> Drop for RunEnv<'a> {
        fn drop(&mut self) {
            restore("CLAUDE_WATCH_PID_DIR", &self.prev_pid_dir);
            restore("WATCHERS_CONFIG", &self.prev_cfg);
            restore("WATCHERS_CONFIG_EXTRA", &self.prev_cfg_extra);
        }
    }
    fn restore(key: &str, prev: &Option<String>) {
        match prev {
            Some(v) => std::env::set_var(key, v),
            None => std::env::remove_var(key),
        }
    }

    /// `watcher_run` for a watcher with a stale PID file (recorded PID dead)
    /// and no live poller → must start normally (spawn the start_cmd).
    #[tokio::test]
    async fn test_watcher_run_stale_pid_file_starts() {
        let dir = tempfile::tempdir().unwrap();
        let pid_dir = dir.path().join("pids");
        std::fs::create_dir_all(&pid_dir).unwrap();
        let cfg = dir.path().join("watchers.conf");

        // A unique sentinel as the pattern so pgrep only matches our poller.
        // We materialize a tiny executable script *named* with the sentinel,
        // so the marker lives in argv[0] (matchable by `pgrep -f`) without
        // needing whitespace in the (whitespace-split) start_cmd.
        let sentinel = format!("cw-runtest-stale-{}", unique_token("w"));
        let script = make_poller_script(dir.path(), &sentinel, "0.3");
        std::fs::write(&cfg, format!("runtest|{}|1|true|{}\n", sentinel, script)).unwrap();

        // Plant a stale PID file pointing at a definitely-dead PID.
        let pid_file = pid_dir.join("runtest.pid");
        std::fs::write(&pid_file, (u32::MAX - 1).to_string()).unwrap();

        let _env = RunEnv::new(pid_dir.to_str().unwrap(), cfg.to_str().unwrap());

        let code = watcher_run(&config_path(), config_path_extra().as_deref(), "runtest")
            .await
            .expect("run should succeed");
        // The sleep exits 0; a no-op guard would also return 0, so to prove we
        // actually STARTED we check the PID file was rewritten to a live (now
        // exited) child PID that is NOT the stale sentinel.
        assert_eq!(code, 0);
        let recorded = std::fs::read_to_string(&pid_file).unwrap();
        assert_ne!(
            recorded.trim(),
            (u32::MAX - 1).to_string(),
            "stale PID file should have been overwritten by a real start"
        );
    }

    /// `watcher_run` for a watcher with no PID file and no poller → starts.
    #[tokio::test]
    async fn test_watcher_run_no_pid_file_starts() {
        let dir = tempfile::tempdir().unwrap();
        let pid_dir = dir.path().join("pids");
        std::fs::create_dir_all(&pid_dir).unwrap();
        let cfg = dir.path().join("watchers.conf");
        let sentinel = format!("cw-runtest-fresh-{}", unique_token("w"));
        let script = make_poller_script(dir.path(), &sentinel, "0.3");
        std::fs::write(&cfg, format!("runtest|{}|1|true|{}\n", sentinel, script)).unwrap();

        let pid_file = pid_dir.join("runtest.pid");
        assert!(!pid_file.exists());

        let _env = RunEnv::new(pid_dir.to_str().unwrap(), cfg.to_str().unwrap());
        let code = watcher_run(&config_path(), config_path_extra().as_deref(), "runtest")
            .await
            .expect("run should succeed");
        assert_eq!(code, 0);
        // A real start wrote a PID file with the child PID.
        assert!(pid_file.exists(), "a real start should write the PID file");
    }

    /// Two sequential `watcher_run` invocations for the same watcher while the
    /// first instance is still alive → the second must NO-OP (PID-guard),
    /// returning 0 without starting a second poller.
    #[tokio::test]
    async fn test_watcher_run_second_invocation_noops_while_alive() {
        let dir = tempfile::tempdir().unwrap();
        let pid_dir = dir.path().join("pids");
        std::fs::create_dir_all(&pid_dir).unwrap();
        let cfg = dir.path().join("watchers.conf");
        let sentinel = format!("cw-runtest-dup-{}", unique_token("w"));
        // Long-lived poller so it's still alive when we fire the second run.
        let script = make_poller_script(dir.path(), &sentinel, "30");
        std::fs::write(&cfg, format!("runtest|{}|1|true|{}\n", sentinel, script)).unwrap();
        let pid_file = pid_dir.join("runtest.pid");

        let _env = RunEnv::new(pid_dir.to_str().unwrap(), cfg.to_str().unwrap());

        // Spawn the first instance directly (don't await — it sleeps 30s) so
        // it's alive for the guard check. We run the SAME script watcher_run
        // would, then write the PID file as watcher_run does.
        let mut first = tokio::process::Command::new(&script)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn first poller");
        let first_pid = first.id().expect("first pid");
        std::fs::write(&pid_file, first_pid.to_string()).unwrap();

        // Now fire watcher_run — it should observe the live poller (pgrep on
        // the sentinel pattern) and/or the live PID file and NO-OP.
        let code = watcher_run(&config_path(), config_path_extra().as_deref(), "runtest")
            .await
            .expect("guarded run should return Ok");
        assert_eq!(code, 0, "guarded no-op must exit 0 (idempotent)");

        // The PID file must still point at the FIRST instance — proof no
        // second instance was started and recorded.
        let recorded = std::fs::read_to_string(&pid_file).unwrap();
        assert_eq!(
            recorded.trim(),
            first_pid.to_string(),
            "second run must not have replaced the live instance's PID file"
        );

        // Exactly one live poller for the sentinel.
        let pollers = process_pids(&sentinel).await;
        assert_eq!(
            pollers.len(),
            1,
            "only the first instance should be alive, got pids {:?}",
            pollers
        );

        // Cleanup.
        let _ = first.start_kill();
        let _ = first.wait().await;
    }

    fn unique_token(prefix: &str) -> String {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        format!(
            "{}-{}-{}",
            prefix,
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        )
    }

    /// Materialize an executable shell script whose *filename* embeds
    /// `sentinel`, so the running process's argv[0] carries the sentinel and is
    /// matchable by `pgrep -f -- <sentinel>`. The script sleeps for `secs`
    /// (NOT via `exec`, so the sentinel-bearing argv[0] survives for the
    /// lifetime of the poller). Returns the absolute path (used directly as the
    /// watcher's `start_cmd`, no whitespace).
    fn make_poller_script(dir: &std::path::Path, sentinel: &str, secs: &str) -> String {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(sentinel);
        std::fs::write(&path, format!("#!/bin/sh\nsleep {}\n", secs)).unwrap();
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).unwrap();
        path.to_string_lossy().into_owned()
    }
}
