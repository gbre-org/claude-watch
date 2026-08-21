//! Watcher supervision: list, status, run, enable/disable, restart.
//!
//! Replaces the shell scripts `watcher-ctl`, `watcher-status`, and
//! `watcher-restart` with native Rust implementations.

use crate::cmd::run_cmd_any;
use crate::status::{WatcherEntry, WatcherMode};
use serde::Serialize;
use std::io::Write;
use std::os::unix::process::ExitStatusExt;

/// Default BASE config path for watchers, relative to `$XDG_CONFIG_HOME`
/// (which itself defaults to `$HOME/.config`).
const DEFAULT_CONFIG: &str = "watchmen/watchers.conf";

/// Default OVERRIDE config path (the user-dir layer), relative to
/// `$XDG_CONFIG_HOME`. Entries here override same-named entries in the base
/// file field-by-field; see `status::load_watchers_config`. The file may be a
/// symlink into a dotfiles/config repo. Inside a container the effective path
/// is whatever `$WATCHERS_CONFIG_EXTRA` names (the entrypoint points it at the
/// bind-mounted operator config dir) — never a host-absolute path.
const DEFAULT_OVERRIDE_CONFIG: &str = "watchmen/watchers.override.conf";

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

/// `$XDG_CONFIG_HOME`, defaulting to `$HOME/.config`. Both layers of the
/// watcher config resolve relative to this, so the same binary finds the
/// right files on a host (`~/.config/...`) and inside a container whose
/// `$HOME`/`$XDG_CONFIG_HOME` point at a bind-mounted user config tree.
pub fn xdg_config_home() -> String {
    if let Ok(p) = std::env::var("XDG_CONFIG_HOME") {
        if !p.trim().is_empty() {
            return p;
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/home/user".to_string());
    format!("{}/.config", home)
}

/// Resolve the BASE watchers.conf path (respects $WATCHERS_CONFIG for tests
/// and for the container, which bakes the committed default's location).
pub fn config_path() -> String {
    if let Ok(p) = std::env::var("WATCHERS_CONFIG") {
        return p;
    }
    format!("{}/{}", xdg_config_home(), DEFAULT_CONFIG)
}

/// Resolve the OVERRIDE (user-dir) watchers.conf path.
///
/// * `$WATCHERS_CONFIG_EXTRA` set and non-empty → that path (the container
///   entrypoint points it at the bind-mounted operator dir);
/// * `$WATCHERS_CONFIG_EXTRA` set but EMPTY → `None` (explicitly "no override
///   layer" — what the test suites use to stay isolated from a real user file);
/// * unset → `$XDG_CONFIG_HOME/watchmen/watchers.override.conf`.
///
/// The file is optional: a missing override is a silent no-op and the base
/// config loads on its own.
pub fn config_path_extra() -> Option<String> {
    match std::env::var("WATCHERS_CONFIG_EXTRA") {
        Ok(p) if p.trim().is_empty() => None,
        Ok(p) => Some(p),
        Err(_) => Some(format!("{}/{}", xdg_config_home(), DEFAULT_OVERRIDE_CONFIG)),
    }
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
/// - `"ARMING"` — monitor mode only: no live pid, but `watcher-ctl run <name>`
///   recorded a `<name>.monitor-intent` younger than the arming grace
///   (`[watcher_monitor].monitor_arming_grace_secs`, default 120s) that no
///   runtime file has superseded. The main loop is between "printed the
///   Monitor command" and "the Monitor is live". Healthy-pending: NOT
///   unhealthy for `--unhealthy-only` (so the `watchers_healthy` gate does not
///   trip) and NOT a miss for the daemon. Flips to `ok` once the pidfile shows
///   a live pid; past the grace with no pid it is `DOWN` again.
/// - `"off"` — disabled in watchers.conf
///
/// `dup_supervisors` and `dup_pollers` are populated (non-empty) only when the
/// corresponding duplicate condition is detected. The lists carry the PIDs so
/// the human can `kill` them by hand. We deliberately do NOT auto-kill — the
/// wrong choice could take out the canonical poller.
#[derive(Debug, Serialize)]
pub struct WatcherStatus {
    pub name: String,
    /// "ok", "DOWN", "DUPLICATE", "off", or — monitor mode only — "ARMING"
    /// (no live pid yet, but a fresh unconsumed `<name>.monitor-intent`
    /// from `watcher-ctl run` is within the arming grace; healthy-pending,
    /// NOT counted as unhealthy by `--unhealthy-only`).
    pub status: String,
    pub count: u32,
    pub required: u32,
    pub pids: String,
    pub enabled: bool,
    /// Delivery mode from the (layered) config: `"oneshot"` or `"monitor"`.
    /// Informational — liveness is decided the same way for both — but it
    /// changes the recovery hint (a monitor-mode watcher is re-ARMED via the
    /// main loop's Monitor tool; `watcher-ctl run <name>` prints the command).
    pub mode: String,
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

/// Linux caps `/proc/PID/comm` at 15 characters plus the NUL terminator, so a
/// longer program name is silently truncated there (a watcher named
/// `claude-event-watch` reports `claude-event-wa`). Any comparison against a
/// configured/derived program name has to allow for that truncation or it will
/// never match a real watcher.
const COMM_MAX_LEN: usize = 15;

/// Interpreter / wrapper program names that legitimately appear in
/// `/proc/PID/comm` for a watcher whose real identity is an argument rather
/// than the executable — e.g. a shell script invoked as
/// `bash /path/to/watcher` gets `comm == "bash"`. These are the ONLY comms for
/// which we fall back to inspecting argv (see [`pattern_matches_argv_tokens`]),
/// because on their own they say nothing about what the process is.
const INTERPRETER_COMMS: &[&str] = &[
    "bash", "sh", "dash", "zsh", "ksh", "python", "python3", "perl", "ruby", "env", "stdbuf",
];

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

/// Program names a genuine poller for this watcher may report in
/// `/proc/PID/comm`.
///
/// Derived from the entry itself, so no watcher name is hard-coded:
///   * the basename of `start_cmd`'s first token (`signal-wait --tag dm` ->
///     `signal-wait`), plus that basename with a launcher-script suffix
///     stripped (`claude-event-watch.sh` -> `claude-event-watch`), because the
///     launcher `exec`s the bare binary;
///   * the same two derivations from the last path segment of `pattern`, but
///     ONLY when the pattern looks path-like (`bin/claude-event-watch` ->
///     `claude-event-watch`). A pattern that is a bare flag fragment
///     (`--tag dm`) yields nothing here.
///
/// Returns an empty vec when nothing plausible can be derived — callers treat
/// that as "cannot filter" and fall back to the unfiltered list, so a config
/// shape we do not understand can never cause a false DOWN.
pub(crate) fn expected_poller_comms(pattern: &str, start_cmd: Option<&str>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |s: &str| {
        let s = s.trim();
        // A name with whitespace is not a program name, and a name that is
        // pure punctuation (`--tag`) is a flag, not an executable.
        if s.is_empty()
            || s.contains(char::is_whitespace)
            || s.starts_with('-')
            || !s.chars().any(|c| c.is_alphanumeric())
        {
            return;
        }
        if !out.iter().any(|e| e == s) {
            out.push(s.to_string());
        }
    };

    if let Some(cmd) = start_cmd {
        if let Some(tok) = cmd.split_whitespace().next() {
            let base = tok.rsplit('/').next().unwrap_or(tok);
            push(base);
            push(crate::status::strip_script_suffix(base));
        }
    }

    // Only treat the pattern as a path when it actually contains a separator;
    // otherwise `--tag dm` would contribute the nonsense name `--tag dm`.
    if pattern.contains('/') {
        if let Some(seg) = pattern.rsplit('/').next() {
            push(seg);
            push(crate::status::strip_script_suffix(seg));
        }
    }

    out
}

/// Does `comm` name one of `expected`, allowing for the kernel's 15-character
/// truncation of `/proc/PID/comm`?
pub(crate) fn comm_matches_expected(comm: &str, expected: &[String]) -> bool {
    let comm = comm.trim();
    if comm.is_empty() {
        return false;
    }
    expected.iter().any(|e| {
        e == comm
            // Truncated form: the kernel kept only the first 15 bytes.
            || (comm.len() >= COMM_MAX_LEN && e.len() > comm.len() && e.starts_with(comm))
    })
}

/// Does `pattern` occur in `argv` as a run of WHOLE ARGUMENTS (allowing the
/// first to match at a `/` path boundary), rather than as a bare substring
/// anywhere in the command line?
///
/// This is the discriminator that a plain `pgrep -f` lacks. `pgrep -f` matches
/// the pattern anywhere in the SPACE-JOINED command line, so a process that
/// merely *quotes* the pattern inside one of its arguments — a scratch test
/// script, a message being drafted that names the watcher — counts as a live
/// poller. Requiring whole-argument (or path-suffix) alignment keeps the
/// genuine `bash /home/u/bin/claude-event-watch --quiet 10` and rejects
/// `some-tool --message "restart bin/claude-event-watch please"`.
///
/// `argv` MUST be the real NUL-separated argument vector, not a space-joined
/// rendering of it: joining destroys the argument boundaries that make this
/// check meaningful, and a quoted mention would then look like its own token.
pub(crate) fn pattern_matches_argv_tokens(argv: &[String], pattern: &str) -> bool {
    let pat: Vec<&str> = pattern.split_whitespace().collect();
    if pat.is_empty() {
        return false;
    }
    if argv.len() < pat.len() {
        return false;
    }
    for start in 0..=(argv.len() - pat.len()) {
        // The first pattern token may be the whole argument, or a suffix of it
        // at a `/` boundary (`bin/claude-event-watch` inside
        // `/home/u/bin/claude-event-watch`). The boundary requirement is what
        // rejects the same text appearing mid-argument.
        let head_ok =
            argv[start] == pat[0] || argv[start].ends_with(&format!("/{}", pat[0]));
        if !head_ok {
            continue;
        }
        if pat[1..]
            .iter()
            .enumerate()
            .all(|(i, p)| argv[start + 1 + i] == *p)
        {
            return true;
        }
    }
    false
}

/// Read `/proc/PID/cmdline` as the real NUL-separated argument vector.
/// `None` when the process is gone or the file is unreadable; empty arguments
/// (and the trailing NUL) are dropped.
fn pid_argv(pid: u32) -> Option<Vec<String>> {
    let data = std::fs::read(format!("/proc/{}/cmdline", pid)).ok()?;
    let argv: Vec<String> = data
        .split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).to_string())
        .collect();
    if argv.is_empty() {
        None
    } else {
        Some(argv)
    }
}

/// Pure decision: given a candidate's `/proc` facts, is it really a poller for
/// this watcher, or a `pgrep -f` false positive?
///
/// `comm`/`cmdline` are `None` when the corresponding `/proc` file could not be
/// read (process already gone, or a non-Linux host). A missing `comm` is
/// treated as "cannot filter" and accepted, so the check can only ever remove
/// processes we can positively identify as something else.
pub(crate) fn is_poller_candidate(
    comm: Option<&str>,
    argv: Option<&[String]>,
    pattern: &str,
    expected: &[String],
) -> bool {
    // Nothing derivable from the config -> no filtering (preserve old
    // behaviour rather than risk a false DOWN).
    if expected.is_empty() {
        return true;
    }
    let comm = match comm {
        Some(c) if !c.trim().is_empty() => c.trim(),
        // Unreadable comm: keep the candidate.
        _ => return true,
    };
    if comm_matches_expected(comm, expected) {
        return true;
    }
    // The watcher may legitimately be running under an interpreter, in which
    // case comm names the interpreter and argv names the watcher. Accept only
    // when the pattern lines up with whole argv tokens.
    if INTERPRETER_COMMS.contains(&comm) {
        return match argv {
            Some(a) => pattern_matches_argv_tokens(a, pattern),
            None => true,
        };
    }
    false
}

/// Get PIDs of live POLLER processes for a watcher entry.
///
/// This is [`process_pids`] (a raw `pgrep -f`) plus a `/proc/PID/comm` filter,
/// the same shape [`supervisor_pids`] already applied to supervisors and which
/// the poller side was missing. Without it, `pgrep -f <pattern>` counts every
/// process whose command line merely CONTAINS the pattern text — and the
/// resulting phantom "duplicate poller" is not cosmetic:
///   * `watcher_run` refuses to start a watcher when it sees >= 1 live poller,
///     so a phantom match blocks a legitimate start;
///   * `watcher_restart` and `watcher_toggle` SIGTERM everything on this list,
///     so a phantom match means killing an unrelated process.
pub async fn poller_pids(pattern: &str, start_cmd: Option<&str>) -> Vec<u32> {
    let candidates = process_pids(pattern).await;
    if candidates.is_empty() {
        return candidates;
    }
    let expected = expected_poller_comms(pattern, start_cmd);
    if expected.is_empty() {
        return candidates;
    }
    candidates
        .into_iter()
        .filter(|pid| {
            let comm = read_proc_comm(*pid);
            let argv = pid_argv(*pid);
            is_poller_candidate(comm.as_deref(), argv.as_deref(), pattern, &expected)
        })
        .collect()
}

/// Read `/proc/PID/comm`, trimmed. `None` on any I/O error.
fn read_proc_comm(pid: u32) -> Option<String> {
    std::fs::read_to_string(format!("/proc/{}/comm", pid))
        .ok()
        .map(|s| s.trim().to_string())
}

/// Load the LAYERED watcher config: base file + optional override file. The
/// override changes same-named entries field-by-field (blank = inherit) and
/// appends unknown names; a missing override is silently a no-op. Shared with
/// the daemon (`status::load_watchers_config`) so CLI and daemon agree.
fn load_entries(config_path: &str, extra_config_path: Option<&str>) -> Vec<WatcherEntry> {
    crate::status::load_watchers_config(config_path, extra_config_path)
}

/// List all watcher entries from config.
pub fn watcher_list(config_path: &str, extra_config_path: Option<&str>) -> Vec<WatcherEntry> {
    load_entries(config_path, extra_config_path)
}

/// Get status for all watchers.
///
/// **Liveness is decided by the SAME pidfile model the daemon's
/// watcher_monitor uses** (`crate::status::watcher_pidfile_liveness`), NOT by
/// `pgrep`. This is the fix for the exec-argv false-DOWN bug: the watcher
/// launcher (`<name>.sh`) does `exec /usr/local/bin/<name>`, which REPLACES the
/// process argv with the exec'd binary's — so the `.sh` path is gone from argv
/// and a `pgrep -f <.sh path>` (which is the watcher's configured `pattern`)
/// can NEVER match a healthy watcher. The daemon was migrated to pidfile
/// liveness in PR #339; this CLI path was left on the broken `pgrep` approach
/// and reported a live watcher as DOWN (`0/1`). We now read the PID the watcher
/// itself records (its `<name>.lock` flock file, or the `<name>.pid` written by
/// `watcher_run`), probe it for genuine (non-zombie) liveness, and verify
/// cmdline identity — all of which survive the exec-to-binary transform.
///
/// The supervisor `pgrep` fan is RETAINED, but ONLY for DUPLICATE detection:
/// nested `watcher-ctl run <name>` parents accumulating because each redundant
/// invocation spawns a fresh wrapper that doesn't clean up its predecessors.
/// The poller `pgrep` fan is also retained, again ONLY to surface duplicate
/// pollers (and a PID list for the human) — it never decides UP/DOWN.
///
/// Both fans run as `tokio::spawn` tasks so the wall-clock per status call
/// stays near one pgrep round-trip even with many watchers configured.
pub async fn watcher_status(config_path: &str, extra_config_path: Option<&str>) -> Vec<WatcherStatus> {
    let arming_grace = crate::status::resolve_monitor_arming_grace_secs();
    watcher_status_with(config_path, extra_config_path, arming_grace).await
}

/// [`watcher_status`] with an explicit monitor-mode ARMING grace (seconds)
/// instead of the env/config-resolved one. The daemon passes its own
/// `[watcher_monitor].monitor_arming_grace_secs`; tests pin a value.
pub async fn watcher_status_with(
    config_path: &str,
    extra_config_path: Option<&str>,
    arming_grace_secs: f64,
) -> Vec<WatcherStatus> {
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
        let start_cmd = entry.start_cmd.clone();
        let poller_h =
            tokio::spawn(async move { poller_pids(&pattern, start_cmd.as_deref()).await });
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

    // Scan ALL candidate pid dirs (not one env-resolved dir): watcher-ctl
    // status runs in an interactive shell where `$XDG_RUNTIME_DIR` resolves the
    // single-dir helper to `/run/user/<uid>`, which holds the flock-guard
    // watchers' `.lock` files but NOT the `signal-wait-*` `.pid` files that
    // `watcher_run` writes to `/var/run/claude` — a single-dir read reported
    // those live watchers as DOWN. See `status::watcher_pidfile_liveness_multi`.
    let pid_dirs = crate::status::watcher_pid_dirs();

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
                mode: entry.mode.as_str().to_string(),
                dup_supervisors: Vec::new(),
                dup_pollers: Vec::new(),
            });
            continue;
        }

        let (pollers, supervisors) = joined_opt.unwrap_or_default();

        // --- UP/DOWN: pidfile liveness (NOT pgrep). Same model as the daemon.
        // min_count == 0 means "never DOWN" — preserve that opt-out so a
        // watcher explicitly opting out of liveness checks can't trip DOWN.
        let (recorded_pid, pidfile_down) = crate::status::watcher_pidfile_liveness_multi(
            &pid_dirs,
            &entry.name,
            entry.start_cmd.as_deref(),
        );
        let is_down = entry.min_count != 0 && pidfile_down;

        // ARMING (monitor mode only): `watcher-ctl run <name>` recorded an
        // arm intent (`<name>.monitor-intent`) that is younger than the
        // arming grace and has not been consumed by a runtime file written
        // since. The watcher has no process YET because the main loop is
        // between "printed the Monitor command" and "the Monitor is live" —
        // healthy-pending, not DOWN. Same helper + same intent file the
        // daemon's watcher_monitor consults, so CLI and daemon agree.
        let is_arming = is_down
            && entry.mode == WatcherMode::Monitor
            && crate::status::watcher_is_arming(
                crate::status::watcher_monitor_intent_age_secs_multi(&pid_dirs, &entry.name),
                crate::status::watcher_runtime_file_age_secs_multi(&pid_dirs, &entry.name),
                arming_grace_secs,
            );

        // `count` reflects the single-instance pidfile model: 1 when the
        // pidfile names a live matching watcher, else 0. (The poller pgrep
        // count is unreliable post-exec, so it must NOT drive this.)
        let count: u32 = if is_down { 0 } else { 1 };

        // PID display: prefer the recorded watcher PID (authoritative). Fall
        // back to the pgrep poller PIDs only for the human-readable column.
        let pid_str = match recorded_pid {
            Some(pid) if !is_down => pid.to_string(),
            _ => pollers
                .iter()
                .map(|p| p.to_string())
                .collect::<Vec<_>>()
                .join(" "),
        };

        // Duplicate detection is orthogonal to UP/DOWN and still uses pgrep.
        // Multiple live pollers matching the pattern, or multiple supervisor
        // wrappers, indicate a state-cleanliness problem the human should fix.
        let dup_pollers = if pollers.len() > 1 {
            pollers.clone()
        } else {
            Vec::new()
        };
        let dup_supervisors = if supervisors.len() > 1 {
            supervisors
        } else {
            Vec::new()
        };

        // Status precedence: ARMING > DOWN > DUPLICATE > ok. A dead poller is
        // the more urgent failure; duplicates are a state-cleanliness issue.
        // If both apply the dup vecs are still populated so the human sees
        // both. ARMING is the monitor-mode "no process yet, arm pending"
        // state — it outranks DOWN only because it IS the DOWN case with a
        // fresh, unconsumed arm intent (see `is_arming` above).
        let status = if is_arming {
            "ARMING".to_string()
        } else if is_down {
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
            mode: entry.mode.as_str().to_string(),
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
    // ALSO strip a trailing launcher-script extension (`.sh`/`.bash`/`.py`):
    // the launcher `<name>.sh` does `exec /usr/local/bin/<name>`, so the live
    // cmdline carries the bare stem (no `.sh`). Without this, a `.sh` start_cmd
    // would never match the exec'd binary — the same exec-defeats-match bug the
    // daemon's `cmdline_matches_watcher` already handles. Kept consistent with
    // that helper (see `crate::status::cmdline_matches_watcher`).
    let stem = crate::status::strip_script_suffix(token_base);
    match pid_cmdline(pid) {
        Some(cmdline) => {
            cmdline.contains(token)
                || cmdline.contains(token_base)
                || (!stem.is_empty() && cmdline.contains(stem))
        }
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
/// dedicated `<name>.runlock` file.
///
/// ## Why `.runlock`, NOT `.lock` (self-deadlock fix)
///
/// The watcher scripts (e.g. `claude-event-watch`) take their OWN `flock`
/// singleton guard on `<pid_dir>/<name>.lock`. This parent lock therefore
/// MUST live on a different path — it is held across the child's whole
/// spawn+wait lifetime, so if it shared `<name>.lock` the spawned child
/// could never acquire its own guard and would refuse with
/// `already running (pid unknown)` + exit 3 every time (the parent starving
/// the child). `.runlock` keeps the two concerns on separate inodes:
/// `.runlock` serializes concurrent `watcher_run` callers; `.lock` is the
/// child's own duplicate-poller guard.
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
        // Parent spawn-serialization lock. MUST use a path DISTINCT from
        // the watcher script's own `<name>.lock` singleton-guard lockfile
        // (the bash watchers `flock` `<pid_dir>/<name>.lock`). If we locked
        // the SAME path here and held it across the child's spawn+wait
        // below, the child could NEVER acquire its own guard -> it would
        // print "already running (pid unknown)" and exit 3 forever
        // (self-deadlock: parent starves the child of the child's lock).
        // `.runlock` serializes concurrent `watcher_run` callers without
        // colliding with the child's `.lock`.
        let lock_path = format!("{}/{}.runlock", pid_dir, name);
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

    // mode=monitor: this watcher is armed ONCE from the main loop through a
    // line-streaming launcher (the Monitor tool) and stays alive across
    // batches. Exec'ing the one-shot here would be wrong on two counts — the
    // watcher would see a block-print-exit supervisor above it and decline
    // monitor mode, and its stdout would be captured-until-exit. So print the
    // exact command to arm, record the intent, and return.
    if entry.mode == WatcherMode::Monitor {
        return monitor_arm(entry).await;
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
    // Comm-filtered: a raw `pgrep -f` here counted any process that merely
    // mentioned the pattern and refused a legitimate start.
    let live_poller_count = poller_pids(&entry.pattern, entry.start_cmd.as_deref())
        .await
        .len() as u32;

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

/// `watcher-ctl run <name>` for a `mode=monitor` watcher.
///
/// Does NOT spawn anything. If a live instance is already recorded (same
/// pidfile model `watcher-ctl status` uses) it says so and exits 0 — the
/// idempotent no-op the main loop's restart cadence expects. Otherwise it
/// writes `<pid_dir>/<name>.monitor-intent` (epoch + command, so "was arming
/// ever requested, and with what?" is answerable after the fact) and prints
/// the Monitor-tool invocation for the MAIN LOOP to arm.
async fn monitor_arm(entry: &WatcherEntry) -> Result<i32, String> {
    let cmd = entry.effective_monitor_cmd().ok_or_else(|| {
        format!(
            "watcher '{}' is mode=monitor but has neither start_cmd nor monitor_cmd configured",
            entry.name
        )
    })?;

    let pid_dirs = crate::status::watcher_pid_dirs();
    let (recorded_pid, is_down) = crate::status::watcher_pidfile_liveness_multi(
        &pid_dirs,
        &entry.name,
        entry.start_cmd.as_deref(),
    );
    if !is_down {
        // The pidfile model proves a live instance, not WHICH mode it runs in
        // (a one-shot started before the flip holds the same `.lock`). Say
        // exactly that, and how to get from here to an armed monitor.
        println!(
            "{} already running (pid {}; a live instance holds its pidfile) — nothing to arm. \
             If that is the one-shot instance and you want the monitor: `watcher-restart` \
             (stops it), then `watcher-ctl run {}` again to get the Monitor command.",
            entry.name,
            recorded_pid.map(|p| p.to_string()).unwrap_or_else(|| "?".to_string()),
            entry.name
        );
        return Ok(0);
    }

    // Record intent (best-effort: an unwritable pid dir must not block the
    // instructions from printing).
    let pid_dir = pid_dir();
    let _ = std::fs::create_dir_all(&pid_dir);
    let intent_path = format!("{}/{}.monitor-intent", pid_dir, entry.name);
    let epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let intent_written = std::fs::write(
        &intent_path,
        format!("epoch={}\ncommand={}\n", epoch, cmd),
    )
    .is_ok();

    print!(
        "{}",
        format_monitor_arm_instructions(
            entry,
            &cmd,
            if intent_written {
                Some(intent_path.as_str())
            } else {
                None
            }
        )
    );
    Ok(0)
}

/// The shell command string to hand to the line-streaming launcher: the
/// configured monitor command with stderr merged into stdout (only stdout is
/// the event stream there; a warning on stderr would be invisible). Idempotent
/// when the command already merges.
pub fn monitor_launch_command(cmd: &str) -> String {
    let trimmed = cmd.trim();
    if trimmed.ends_with("2>&1") {
        trimmed.to_string()
    } else {
        format!("{} 2>&1", trimmed)
    }
}

/// Pure: the text `watcher-ctl run <name>` prints for a monitor-mode watcher.
pub fn format_monitor_arm_instructions(
    entry: &WatcherEntry,
    cmd: &str,
    intent_path: Option<&str>,
) -> String {
    let layer_note = if entry.overridden.iter().any(|k| k == "mode") {
        "mode set by the override layer".to_string()
    } else if entry.layer == crate::status::WATCHER_LAYER_OVERRIDE {
        "entry defined in the override layer".to_string()
    } else {
        "mode set in the base watchers.conf".to_string()
    };
    let mut out = String::new();
    out.push_str(&format!(
        "[monitor-mode] {} is configured mode=monitor ({}) — NOT exec'ing the one-shot watcher.\n",
        entry.name, layer_note
    ));
    out.push_str(
        "ARM IT NOW from the main loop with the Monitor tool (not a background Bash task, not `&`):\n",
    );
    out.push_str("  Monitor\n");
    out.push_str(&format!("    command:     {}\n", monitor_launch_command(cmd)));
    out.push_str(&format!(
        "    description: {} (monitor-mode watcher)\n",
        entry.name
    ));
    out.push_str("    persistent:  true\n");
    out.push_str(
        "Reminder: every stdout line is a notification — read each EVENT[...] line and ACT on it; \
         the watcher never acks on your behalf. Lines tagged [monitor-mode] are watcher status \
         (ACTIVE / ALIVE / STOPPED), not events.\n",
    );
    out.push_str(&format!(
        "Stop: TaskStop the monitor, or `watcher-restart` (kills it like any other watcher). \
         Flip back: set `{}|mode=oneshot` in the override watchers.conf, then \
         `watcher-ctl run {}` as a background task.\n",
        entry.name, entry.name
    ));
    match intent_path {
        Some(p) => out.push_str(&format!("intent recorded: {}\n", p)),
        None => out.push_str("intent NOT recorded (pid dir unwritable)\n"),
    }
    out
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
/// flips the config bit and stops there — the next `watcher-ctl run <name>` /
/// session-resume run *by the main loop* is what actually spawns the
/// watcher. We do NOT `nohup` (or any other supervisor mechanism) the
/// start_cmd from this process: a daemon-spawned watcher would live in the
/// wrong process tree and become invisible to the main loop's obligation
/// gate. See the watcher-architecture cardinal rule (operator notes).
///
/// On disable, kills matching processes (this side is fine — the main loop
/// owns the watcher, killing it cleanly is not the same as spawning).
///
/// Watchers that must never be disabled (guardrails).
const PROTECTED_WATCHERS: &[&str] = &["memory-remind"];

pub async fn watcher_toggle(
    config_path: &str,
    override_path: Option<&str>,
    name: &str,
    enable: bool,
) -> Result<String, String> {
    if !enable && PROTECTED_WATCHERS.contains(&name) {
        return Err(format!(
            "watcher '{}' is protected and cannot be disabled. \
             Edit ~/.config/watchmen/watchers.conf manually if you really mean it.",
            name
        ));
    }

    // The override layer wins over the base file, so flipping `enabled` in
    // the base while the override pins it would be a silent no-op. Refuse
    // with a pointer instead of writing a flag that has no effect.
    if let Some(ov) = override_path {
        if let Ok(ov_content) = std::fs::read_to_string(ov) {
            let pins = crate::status::parse_watcher_lines(&ov_content)
                .iter()
                .any(|raw| raw.name == name && raw.fields[2].is_some());
            if pins {
                return Err(format!(
                    "watcher '{}': `enabled` is pinned by the override layer ({}) — \
                     edit it there (e.g. `{}|enabled={}`), not in the base file",
                    name,
                    ov,
                    name,
                    if enable { "true" } else { "false" }
                ));
            }
        }
    }

    let content = std::fs::read_to_string(config_path)
        .map_err(|e| format!("failed to read config: {}", e))?;

    // Resolve pattern/start_cmd from the merged view (so a pattern the
    // override layer changed is the one we kill on disable).
    let merged = load_entries(config_path, override_path);
    let (target_pattern, target_start_cmd) = match merged.iter().find(|e| e.name == name) {
        Some(e) => (e.pattern.clone(), e.start_cmd.clone().unwrap_or_default()),
        None => (String::new(), String::new()),
    };

    let new_content = rewrite_config_toggle(&content, name, enable)
        .ok_or_else(|| format!("watcher '{}' not found in config", name))?;

    // Write updated config
    let mut file =
        std::fs::File::create(config_path).map_err(|e| format!("failed to write config: {}", e))?;
    file.write_all(new_content.as_bytes())
        .map_err(|e| format!("failed to write config: {}", e))?;

    if enable {
        // Config-only flip. The main loop is responsible for spawning the
        // watcher (via a fresh `watcher-ctl run <name>` background task;
        // `watcher-restart` only STOPS watchers). We deliberately do not
        // spawn it here — see the doc comment above.
        Ok(format!(
            "{}: enabled (config flipped — main loop must spawn via \
             `watcher-ctl run {}`)",
            name, name
        ))
    } else {
        // Kill matching processes. Comm-filtered — an unfiltered `pgrep -f`
        // here would SIGTERM any process that merely mentions the pattern.
        let start_cmd_opt = if target_start_cmd.is_empty() {
            None
        } else {
            Some(target_start_cmd.as_str())
        };
        let pids = poller_pids(&target_pattern, start_cmd_opt).await;
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
// the watcher-architecture cardinal rule (operator notes).
//
// What replaces it: nothing in this file. The daemon's only emergency
// recovery action is now the existing tmux-inject path in `policy.rs`,
// which types `watcher-ctl run <name>` into the Claude Code pane so the
// MAIN LOOP spawns the watcher in its own process tree. claude-watch
// (the daemon) never touches the watcher process directly.
// ---------------------------------------------------------------------------

/// Read every live PID's parent PID from `/proc/PID/stat`.
///
/// `/proc/PID/stat` is `pid (comm) state ppid ...`, and `comm` may itself
/// contain spaces and parentheses, so we split after the LAST `)`.
fn read_ppid_map() -> Vec<(u32, u32)> {
    let dir = match std::fs::read_dir("/proc") {
        Ok(d) => d,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    for entry in dir.flatten() {
        let name = entry.file_name();
        let name = match name.to_str() {
            Some(n) => n,
            None => continue,
        };
        let pid: u32 = match name.parse() {
            Ok(p) => p,
            Err(_) => continue,
        };
        let stat = match std::fs::read_to_string(format!("/proc/{}/stat", pid)) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let close = match stat.rfind(')') {
            Some(i) => i,
            None => continue,
        };
        let mut fields = stat[close + 1..].split_whitespace();
        let _state = fields.next();
        if let Some(ppid) = fields.next().and_then(|p| p.parse::<u32>().ok()) {
            out.push((pid, ppid));
        }
    }
    out
}

/// Pure helper: every transitive descendant of `roots`, given a `(pid, ppid)`
/// edge list. Roots themselves are NOT included. Cycle-safe (a pid is only
/// ever expanded once) and does not descend from pid 0/1.
pub(crate) fn descendants_of(roots: &[u32], ppid_map: &[(u32, u32)]) -> Vec<u32> {
    let mut found: Vec<u32> = Vec::new();
    let mut frontier: Vec<u32> = roots.to_vec();
    while let Some(parent) = frontier.pop() {
        for (pid, ppid) in ppid_map {
            if *ppid != parent || *pid <= 1 {
                continue;
            }
            if roots.contains(pid) || found.contains(pid) {
                continue;
            }
            found.push(*pid);
            frontier.push(*pid);
        }
    }
    found
}

/// Kill all enabled watcher processes and clean PID files.
///
/// Also kills each watcher's DESCENDANTS. A watcher's blocking child (for the
/// event watcher, `inotifywait`) does not carry the watcher's own argv, so it
/// never matched the configured `pattern` and survived a restart as an orphan
/// — running on its own timeout, holding whatever file descriptors it
/// inherited. That is why "stop, then immediately start" could be refused by a
/// singleton lock with no live watcher behind it. Descendants are enumerated
/// BEFORE anything is signalled: once the parent dies its children are
/// reparented to init and are no longer reachable from the watcher's PID.
pub async fn watcher_restart(config_path: &str, extra_config_path: Option<&str>) -> String {
    let entries = load_entries(config_path, extra_config_path);
    let mut total = 0u32;
    let mut messages = Vec::new();

    for entry in &entries {
        if !entry.enabled {
            continue;
        }
        // Comm-filtered so a process that merely quotes the pattern in an
        // argument is not signalled.
        let pids = poller_pids(&entry.pattern, entry.start_cmd.as_deref()).await;
        if !pids.is_empty() {
            // Snapshot the tree first — see the note on this function.
            let children = descendants_of(&pids, &read_ppid_map());
            let count = pids.len() as u32;
            for pid in &pids {
                let _ = run_cmd_any(&["kill", &pid.to_string()], 5).await;
            }
            for pid in &children {
                let _ = run_cmd_any(&["kill", &pid.to_string()], 5).await;
            }
            if children.is_empty() {
                messages.push(format!("Killed {} {} process(es)", count, entry.name));
            } else {
                messages.push(format!(
                    "Killed {} {} process(es) + {} child process(es)",
                    count,
                    entry.name,
                    children.len()
                ));
            }
            total += count + children.len() as u32;
        }
    }

    // Clean PID files — and monitor-mode arm intents: a restart voids any
    // pending arm (the monitor it was for is being stopped), so a leftover
    // `<name>.monitor-intent` must not keep the watcher reading ARMING for
    // the rest of its grace window with nothing arming it.
    if let Ok(dir) = std::fs::read_dir(pid_dir()) {
        for entry in dir.flatten() {
            let path = entry.path();
            let is_pid = path.extension().is_some_and(|ext| ext == "pid");
            let is_intent = path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with(".monitor-intent"));
            if is_pid || is_intent {
                let _ = std::fs::remove_file(path);
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
                    "mode": e.mode.as_str(),
                    "monitor_cmd": e.effective_monitor_cmd(),
                    "layer": e.layer,
                    "overridden": e.overridden,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&items).unwrap());
    } else {
        print!("{}", format_list(&entries, config_path, extra_config_path));
    }
}

/// `claude-watch watcher status [--json] [--unhealthy-only]`
///
/// `unhealthy_only`: when set, the command emits NOTHING and returns exit 0
/// if every enabled watcher is `ok`. If any enabled watcher is `DOWN` *or*
/// `DUPLICATE` the full status output is printed (same format as the default
/// case) so the caller can see what's wrong. Designed for the PostToolUse
/// hook that surfaces watcher health on every tool call.
pub async fn cmd_status(
    config_path: &str,
    extra_config_path: Option<&str>,
    json: bool,
    unhealthy_only: bool,
    all: bool,
) {
    let statuses = watcher_status(config_path, extra_config_path).await;

    if unhealthy_only && !any_unhealthy(&statuses) {
        // Stay silent when everything is healthy. JSON mode gets the same
        // silence treatment so the hook stays non-spammy in either case.
        return;
    }

    if json {
        // JSON always carries the full set (including disabled watchers) so
        // machine consumers keep a complete picture; the `--all` filter only
        // affects the human-readable rendering.
        println!("{}", serde_json::to_string_pretty(&statuses).unwrap());
    } else {
        print!("{}", format_status(&statuses, all));
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
pub async fn cmd_toggle(
    config_path: &str,
    extra_config_path: Option<&str>,
    name: &str,
    enable: bool,
) -> i32 {
    match watcher_toggle(config_path, extra_config_path, name, enable).await {
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

/// Which config layer decided an entry, for the `SOURCE` column of
/// `watcher-ctl list`: `base`, `override` (entry introduced there), or
/// `base+override(<fields>)` naming the fields the override changed.
pub fn entry_source_label(e: &WatcherEntry) -> String {
    if e.layer == crate::status::WATCHER_LAYER_OVERRIDE {
        crate::status::WATCHER_LAYER_OVERRIDE.to_string()
    } else if e.overridden.is_empty() {
        crate::status::WATCHER_LAYER_BASE.to_string()
    } else {
        format!("base+override({})", e.overridden.join(","))
    }
}

/// Pure function: format watcher list output (for testing without I/O).
///
/// Rows carry the effective values (after the override layer is applied);
/// the `SOURCE` column says which layer set them, and the trailing `layers:`
/// block names both files and whether the override is present, so "which
/// file do I edit to flip this?" is answered by the listing itself.
pub fn format_list(entries: &[WatcherEntry], base_path: &str, override_path: Option<&str>) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "{:<20} {:<8} {:<8} {:<24} {}\n",
        "NAME", "ENABLED", "MODE", "SOURCE", "PATTERN"
    ));
    out.push_str(&format!(
        "{:<20} {:<8} {:<8} {:<24} {}\n",
        "----", "-------", "----", "------", "-------"
    ));
    for e in entries {
        out.push_str(&format!(
            "{:<20} {:<8} {:<8} {:<24} {}\n",
            e.name,
            e.enabled,
            e.mode.as_str(),
            entry_source_label(e),
            e.pattern
        ));
    }
    out.push('\n');
    out.push_str(&format!("layers: base     = {}\n", base_path));
    match override_path {
        Some(p) => {
            let state = if std::path::Path::new(p).is_file() {
                "active"
            } else {
                "absent — base only"
            };
            out.push_str(&format!("        override = {} ({})\n", p, state));
        }
        None => out.push_str("        override = (disabled: WATCHERS_CONFIG_EXTRA is empty)\n"),
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
///
/// `show_all`: when `false` (the default `watcher-ctl status` view), the
/// `off (disabled)` rows are omitted entirely so the listing shows only the
/// watchers that are SUPPOSED to be running. Disabled watchers are
/// intentionally off and are never part of the health picture (see
/// [`any_unhealthy`]), so hiding them keeps the default view focused on the
/// enabled set. When `true` (`watcher-ctl status --all`) the full list —
/// including the `off (disabled)` rows — is rendered, matching the historical
/// behaviour. Health/WARNING/recovery logic is identical in both modes: it
/// has always considered only enabled watchers, so the footer never changes
/// based on the filter.
pub fn format_status(statuses: &[WatcherStatus], show_all: bool) -> String {
    let mut out = String::new();
    let mut all_healthy = true;
    let mut down_names: Vec<String> = Vec::new();
    let mut down_monitor_names: Vec<String> = Vec::new();
    let mut arming_names: Vec<String> = Vec::new();
    let mut has_duplicate = false;
    for s in statuses {
        if s.status == "off" {
            // Default view hides disabled watchers; only `--all` shows them.
            if !show_all {
                continue;
            }
            out.push_str(&format!("{:<20} {:<9} (disabled)\n", s.name, s.status));
        } else {
            if s.status == "DOWN" || s.status == "DUPLICATE" {
                all_healthy = false;
            }
            if s.status == "DOWN" {
                down_names.push(s.name.clone());
                if s.mode == "monitor" {
                    down_monitor_names.push(s.name.clone());
                }
            }
            // ARMING is healthy-pending: it never flips `all_healthy` (so
            // `--unhealthy-only` stays silent and the obligations gate does
            // not trip) but gets its own footer so the reader knows the
            // Monitor still has to actually be armed.
            if s.status == "ARMING" {
                arming_names.push(s.name.clone());
            }
            if s.status == "DUPLICATE" {
                has_duplicate = true;
            }
            // Oneshot rows keep their historical byte-exact shape; a
            // monitor-mode row carries a trailing ` [monitor]` tag so a
            // reader knows it is re-ARMED (Monitor tool), not re-run.
            let mode_tag = if s.mode == "monitor" { "  [monitor]" } else { "" };
            out.push_str(&format!(
                "{:<20} {:<9} ({}/{})  {}{}\n",
                s.name, s.status, s.count, s.required, s.pids, mode_tag
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
        // don't have to filter prose. DUPLICATE always wins because only
        // `watcher-restart` clears duplicate pollers/supervisors (kills
        // everything + cleans PID files) — but it STARTS nothing, so it
        // must always be followed by `watcher-ctl run <name>` for each
        // enabled watcher. A per-watcher `watcher-ctl run <name>` alone
        // wouldn't clear the duplicates.
        if has_duplicate {
            out.push_str(
                "Recovery for DUPLICATE state: `watcher-restart` \
                 (STOPS all watchers + cleans PID files — it starts NOTHING), \
                 then `watcher-ctl run <name>` for each enabled watcher.\n",
            );
        } else if !down_names.is_empty() {
            // DOWN-only: per-watcher restart is the surgical fix.
            let names = down_names.join(" ");
            out.push_str(&format!(
                "Recovery for DOWN state: `watcher-ctl run <name>` for each \
                 DOWN watcher (e.g. {}). Note: `watcher-restart` only STOPS \
                 all watchers — after it, every watcher must still be \
                 started with `watcher-ctl run`.\n",
                names
            ));
        }
        if !down_monitor_names.is_empty() {
            out.push_str(&format!(
                "Monitor-mode watcher(s) DOWN: {} — `watcher-ctl run <name>` does NOT \
                 exec them; it prints the Monitor-tool command for the main loop to \
                 re-ARM (persistent: true). Arm it, then read every stdout line.\n",
                down_monitor_names.join(" ")
            ));
        }
    }
    if !arming_names.is_empty() {
        out.push_str(&format!(
            "Monitor-mode watcher(s) ARMING: {} — `watcher-ctl run <name>` recorded the arm \
             intent; the row flips to ok once the Monitor is live (pidfile shows a live pid) \
             and back to DOWN if it is not armed within the arming grace. If you have not \
             armed it yet, arm it NOW (Monitor tool, persistent: true) — this state is not \
             a substitute for arming.\n",
            arming_names.join(" ")
        ));
    }
    out
}

/// Pure function: rewrite config content toggling the enabled field for a watcher.
/// Returns the new config content, or None if the watcher was not found.
///
/// Preserves every OTHER field on the line — including the optional trailing
/// `on_restart_cmd`, `mode` and `monitor_cmd` slots, which the previous
/// five-field rewrite silently dropped — and handles a keyed `enabled=...`
/// field in place. A line shorter than four fields is padded so the enabled
/// slot exists (`name|pat` → `name|pat|1|<val>|`).
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

        let mut parts: Vec<String> = line.split('|').map(|s| s.to_string()).collect();
        if parts.len() >= 2 && parts[0].trim() == name {
            found = true;
            if let Some(keyed) = parts
                .iter_mut()
                .skip(1)
                .find(|f| f.trim().starts_with("enabled="))
            {
                *keyed = format!("enabled={}", new_val);
            } else {
                while parts.len() < 3 {
                    parts.push("1".to_string());
                }
                if parts.len() < 4 {
                    parts.push(new_val.to_string());
                    // Keep the historical 5-field shape for a minimal line.
                    parts.push(String::new());
                } else {
                    parts[3] = new_val.to_string();
                }
            }
            output_lines.push(parts.join("|"));
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

    // --- poller comm filter ------------------------------------------------
    //
    // The duplicate-poller check used a raw `pgrep -f <pattern>`, which matches
    // the pattern ANYWHERE in a process's joined command line. Any process that
    // merely quoted the pattern therefore counted as a live production poller:
    // it blocked `watcher-ctl run` (which refuses to start when it sees a live
    // poller) and it was signalled by restart/disable. These tests pin the comm
    // filter that removes those false positives without ever removing a real
    // watcher.

    fn comms(pattern: &str, start: Option<&str>) -> Vec<String> {
        expected_poller_comms(pattern, start)
    }

    #[test]
    fn test_expected_comms_from_start_cmd_basename() {
        let e = comms("bin/claude-event-watch", Some("claude-event-watch --quiet 10"));
        assert!(e.contains(&"claude-event-watch".to_string()), "got {:?}", e);
    }

    #[test]
    fn test_expected_comms_strip_launcher_suffix() {
        // The `.sh` launcher `exec`s the bare binary, so BOTH names are valid.
        let e = comms("/opt/watchers/cew.sh", Some("/opt/watchers/cew.sh"));
        assert!(e.contains(&"cew.sh".to_string()), "got {:?}", e);
        assert!(e.contains(&"cew".to_string()), "got {:?}", e);
    }

    #[test]
    fn test_expected_comms_ignores_flag_shaped_pattern() {
        // `--tag dm` is not a program name; it must not be derived as one.
        let e = comms("--tag dm", Some("signal-wait --dm --tag dm"));
        assert_eq!(e, vec!["signal-wait".to_string()], "got {:?}", e);
    }

    #[test]
    fn test_expected_comms_empty_when_nothing_derivable() {
        // No start_cmd and a non-path pattern -> nothing to filter on. Callers
        // must then fall back to the unfiltered list rather than guess.
        assert!(comms("--tag dm", None).is_empty());
    }

    #[test]
    fn test_comm_matches_allows_kernel_truncation() {
        // Linux truncates /proc/PID/comm to 15 chars: the real watcher reports
        // `claude-event-wa`. A strict equality check would never match it.
        let e = vec!["claude-event-watch".to_string()];
        assert!(comm_matches_expected("claude-event-wa", &e));
        assert!(comm_matches_expected("claude-event-watch", &e));
        // A short unrelated comm must NOT be accepted as a truncation.
        assert!(!comm_matches_expected("bash", &e));
        assert!(!comm_matches_expected("claude", &e));
    }

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn test_pattern_matches_argv_tokens_whole_token_and_path_suffix() {
        assert!(pattern_matches_argv_tokens(
            &argv(&["/bin/bash", "/home/u/bin/claude-event-watch", "--quiet", "10"]),
            "bin/claude-event-watch"
        ));
        // A multi-token pattern must line up across consecutive arguments.
        assert!(pattern_matches_argv_tokens(
            &argv(&["signal-wait", "--dm", "--tag", "dm", "--quiet", "12"]),
            "--tag dm"
        ));
    }

    #[test]
    fn test_pattern_matches_argv_tokens_rejects_mid_argument_mention() {
        // The exact false-positive shape: the pattern appears inside a single
        // quoted argument (a drafted message, a scratch test) rather than as
        // the program being run.
        assert!(!pattern_matches_argv_tokens(
            &argv(&["some-tool", "--message", "restart bin/claude-event-watch please"]),
            "bin/claude-event-watch"
        ));
        // Same text, not at a path boundary.
        assert!(!pattern_matches_argv_tokens(
            &argv(&["bash", "-c", "echo=bin/claude-event-watch"]),
            "bin/claude-event-watch"
        ));
        // A multi-token pattern quoted inside ONE argument must not match.
        // This is precisely what a space-joined cmdline would have accepted.
        assert!(!pattern_matches_argv_tokens(
            &argv(&["some-tool", "--message", "use --tag dm for direct messages"]),
            "--tag dm"
        ));
    }

    #[test]
    fn test_is_poller_candidate_accepts_real_watcher() {
        let e = comms("bin/claude-event-watch", Some("claude-event-watch --quiet 10"));
        // Shebang launch: kernel sets comm from the script name (truncated).
        assert!(is_poller_candidate(
            Some("claude-event-wa"),
            Some(&argv(&["/bin/bash", "/home/u/bin/claude-event-watch", "--quiet", "10"])),
            "bin/claude-event-watch",
            &e
        ));
    }

    #[test]
    fn test_is_poller_candidate_accepts_interpreter_launch() {
        // `bash /path/to/watcher` -> comm is the interpreter, identity is argv.
        let e = comms("bin/claude-event-watch", Some("claude-event-watch --quiet 10"));
        assert!(is_poller_candidate(
            Some("bash"),
            Some(&argv(&["bash", "/home/u/bin/claude-event-watch", "--quiet", "10"])),
            "bin/claude-event-watch",
            &e
        ));
    }

    #[test]
    fn test_is_poller_candidate_rejects_unrelated_process_quoting_pattern() {
        let e = comms("bin/claude-event-watch", Some("claude-event-watch --quiet 10"));
        // A message-sending tool whose argument names the watcher. Rejected
        // on comm alone -- it is not the watcher and not an interpreter.
        assert!(!is_poller_candidate(
            Some("signal-send"),
            Some(&argv(&["signal-send", "--dm", "andrew", "restart bin/claude-event-watch now"])),
            "bin/claude-event-watch",
            &e
        ));
        // An interpreter whose argument merely quotes the pattern: rejected on
        // argument-boundary alignment.
        assert!(!is_poller_candidate(
            Some("bash"),
            Some(&argv(&["bash", "-c", "sleep 60 # bin/claude-event-watch"])),
            "bin/claude-event-watch",
            &e
        ));
    }

    #[test]
    fn test_is_poller_candidate_fails_open_without_proc_facts() {
        // The filter may only ever REMOVE processes we can positively identify
        // as something else. Unknown comm, or nothing derivable from config,
        // must keep the candidate so we can never invent a false DOWN.
        let e = comms("bin/claude-event-watch", Some("claude-event-watch"));
        assert!(is_poller_candidate(None, None, "bin/claude-event-watch", &e));
        assert!(is_poller_candidate(Some(""), None, "bin/claude-event-watch", &e));
        assert!(is_poller_candidate(Some("anything"), None, "--tag dm", &[]));
        // Interpreter comm with an unreadable argv is also kept.
        assert!(is_poller_candidate(Some("bash"), None, "bin/claude-event-watch", &e));
    }

    // --- restart descendant reaping ---------------------------------------
    //
    // A watcher's blocking child (`inotifywait`) carries its own argv, so it
    // never matched the watcher's configured pattern and outlived a restart as
    // an orphan running on its own timeout.

    #[test]
    fn test_descendants_of_collects_transitive_children() {
        // 100 -> 200 -> 300, plus an unrelated 400.
        let map = vec![(100, 1), (200, 100), (300, 200), (400, 1)];
        let mut d = descendants_of(&[100], &map);
        d.sort();
        assert_eq!(d, vec![200, 300]);
    }

    #[test]
    fn test_descendants_of_excludes_roots_and_survives_cycles() {
        // A malformed/cyclic ppid map must terminate, not spin.
        let map = vec![(100, 200), (200, 100)];
        assert_eq!(descendants_of(&[100], &map), vec![200]);
        // A pid that is itself a root is never reported as a descendant.
        let d = descendants_of(&[100, 200], &map);
        assert!(d.is_empty(), "roots must not be reported as descendants: {:?}", d);
    }

    #[test]
    fn test_descendants_of_never_collects_pid_0_or_1() {
        let map = vec![(1, 0), (2, 1), (100, 1)];
        // Root 1 would otherwise sweep every reparented process on the box.
        let d = descendants_of(&[1], &map);
        assert!(d.contains(&2) && d.contains(&100));
        // But pid 0/1 themselves are never collected.
        assert!(!d.contains(&1) && !d.contains(&0));
    }

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
                layer: "base".to_string(),
                ..Default::default()
            },
            WatcherEntry {
                name: "torrent".to_string(),
                pattern: "torrent$".to_string(),
                min_count: 1,
                enabled: false,
                start_cmd: None,
                on_restart_cmd: None,
                mode: WatcherMode::Monitor,
                layer: "base".to_string(),
                overridden: vec!["mode".to_string(), "enabled".to_string()],
                ..Default::default()
            },
        ];
        let output = format_list(&entries, "/etc/x/watchers.conf", Some("/nonexistent/override.conf"));
        assert!(output.contains("alerts"));
        assert!(output.contains("torrent"));
        assert!(output.contains("true"));
        assert!(output.contains("false"));
        // Mode + which-layer-won are visible per row, and both layer paths
        // are named in the footer (override reported absent here).
        assert!(output.contains("MODE"), "header has MODE column: {}", output);
        assert!(output.contains("SOURCE"), "header has SOURCE column: {}", output);
        let torrent_row = output.lines().find(|l| l.starts_with("torrent")).unwrap();
        assert!(torrent_row.contains("monitor"), "row shows mode: {}", torrent_row);
        assert!(
            torrent_row.contains("base+override(mode,enabled)"),
            "row names the overriding layer + fields: {}",
            torrent_row
        );
        let alerts_row = output.lines().find(|l| l.starts_with("alerts")).unwrap();
        assert!(alerts_row.contains("oneshot"), "{}", alerts_row);
        assert!(alerts_row.contains(" base "), "{}", alerts_row);
        assert!(output.contains("base     = /etc/x/watchers.conf"));
        assert!(output.contains("override = /nonexistent/override.conf (absent"));
    }

    #[test]
    fn test_format_list_override_layer_disabled() {
        let output = format_list(&[], "/etc/x/watchers.conf", None);
        assert!(output.contains("override = (disabled"), "{}", output);
    }

    #[test]
    fn test_monitor_arm_instructions_shape() {
        let e = WatcherEntry {
            name: "claude-event-watch".to_string(),
            pattern: "bin/claude-event-watch".to_string(),
            min_count: 1,
            enabled: true,
            start_cmd: Some("claude-event-watch --debounce 60 --quiet 10".to_string()),
            mode: WatcherMode::Monitor,
            layer: "base".to_string(),
            overridden: vec!["mode".to_string()],
            ..Default::default()
        };
        let cmd = e.effective_monitor_cmd().unwrap();
        assert_eq!(cmd, "claude-event-watch --debounce 60 --quiet 10 --mode monitor");
        let text = format_monitor_arm_instructions(&e, &cmd, Some("/run/x/claude-event-watch.monitor-intent"));
        // The exact command string the main loop must arm, stderr merged.
        assert!(
            text.contains("command:     claude-event-watch --debounce 60 --quiet 10 --mode monitor 2>&1"),
            "{}",
            text
        );
        assert!(text.contains("persistent:  true"), "{}", text);
        assert!(text.contains("Monitor"), "{}", text);
        assert!(text.contains("read each EVENT[...] line and ACT"), "{}", text);
        assert!(text.contains("mode set by the override layer"), "{}", text);
        assert!(text.contains("intent recorded: /run/x/claude-event-watch.monitor-intent"), "{}", text);
        // Explicit monitor_cmd wins over the derived `--mode monitor` form,
        // and an already-merged stderr is not doubled.
        let e2 = WatcherEntry {
            monitor_cmd: Some("my-watch --stream 2>&1".to_string()),
            ..e.clone()
        };
        assert_eq!(e2.effective_monitor_cmd().unwrap(), "my-watch --stream 2>&1");
        assert_eq!(monitor_launch_command("my-watch --stream 2>&1"), "my-watch --stream 2>&1");
    }

    #[test]
    fn test_format_status_monitor_row_tag_and_recovery_hint() {
        let mut up = ok_status("evw", 1, 1, "4242");
        up.mode = "monitor".to_string();
        let out = format_status(&[up], false);
        let row = out.lines().next().unwrap();
        assert!(row.contains("ok"), "{}", row);
        assert!(row.ends_with("[monitor]"), "monitor row is tagged: {}", row);
        assert!(out.contains("All watchers healthy."));
        // Oneshot rows are byte-for-byte unchanged (no tag).
        let plain = format_status(&[ok_status("sig", 1, 1, "7")], false);
        assert!(!plain.contains("[monitor]"));

        let mut down = down_status("evw", 1);
        down.mode = "monitor".to_string();
        let out = format_status(&[down], false);
        assert!(out.contains("Monitor-mode watcher(s) DOWN: evw"), "{}", out);
        assert!(out.contains("re-ARM"), "{}", out);
    }

    /// ARMING is healthy-PENDING: the row renders with its own status word +
    /// the `[monitor]` tag, the footer tells the reader the Monitor still has
    /// to be armed, but it is NOT a WARNING state — `all_healthy` holds (so
    /// `--unhealthy-only` stays silent and the obligations gate does not
    /// trip) and `any_unhealthy` is false. Mixed with a real DOWN the warning
    /// + both footers appear.
    #[test]
    fn test_format_status_arming_is_healthy_pending_not_down() {
        let mut arming = down_status("evw", 1);
        arming.status = "ARMING".to_string();
        arming.mode = "monitor".to_string();
        let out = format_status(std::slice::from_ref(&arming), false);
        let row = out.lines().next().unwrap();
        assert!(row.starts_with("evw"), "{}", row);
        assert!(row.contains(" ARMING "), "{}", row);
        assert!(row.ends_with("[monitor]"), "{}", row);
        assert!(out.contains("All watchers healthy."), "ARMING is not unhealthy: {}", out);
        assert!(!out.contains("WARNING"), "{}", out);
        assert!(out.contains("Monitor-mode watcher(s) ARMING: evw"), "{}", out);
        assert!(out.contains("arm it NOW"), "{}", out);
        assert!(!any_unhealthy(std::slice::from_ref(&arming)), "ARMING must not count as unhealthy");

        // Mixed: a genuinely DOWN oneshot + an ARMING monitor -> WARNING for
        // the DOWN one, ARMING footer still present, DOWN recovery names only
        // the DOWN watcher.
        let out = format_status(&[down_status("sig", 1), arming], false);
        assert!(out.contains("WARNING"), "{}", out);
        assert!(out.contains("Recovery for DOWN state"), "{}", out);
        assert!(out.contains("(e.g. sig)"), "{}", out);
        assert!(!out.contains("Monitor-mode watcher(s) DOWN"), "{}", out);
        assert!(out.contains("Monitor-mode watcher(s) ARMING: evw"), "{}", out);
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
            mode: "oneshot".to_string(),
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
            mode: "oneshot".to_string(),
            dup_supervisors: Vec::new(),
            dup_pollers: Vec::new(),
        }
    }

    #[test]
    fn test_format_status_all_ok() {
        let statuses = vec![ok_status("alerts", 1, 1, "1234")];
        let output = format_status(&statuses, true);
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
        let output = format_status(&statuses, true);
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
            mode: "oneshot".to_string(),
            dup_supervisors: Vec::new(),
            dup_pollers: Vec::new(),
        }];
        let output = format_status(&statuses, true);
        assert!(output.contains("off"));
        assert!(output.contains("disabled"));
        assert!(output.contains("All watchers healthy."));
    }

    /// Test helper: build an `off` (disabled) watcher status.
    fn off_status(name: &str) -> WatcherStatus {
        WatcherStatus {
            name: name.to_string(),
            status: "off".to_string(),
            count: 0,
            required: 1,
            pids: String::new(),
            enabled: false,
            mode: "oneshot".to_string(),
            dup_supervisors: Vec::new(),
            dup_pollers: Vec::new(),
        }
    }

    #[test]
    fn test_format_status_default_hides_disabled() {
        // Default view (show_all=false): enabled watchers render, disabled
        // (`off`) ones are omitted entirely.
        let statuses = vec![
            ok_status("alerts", 1, 1, "1234"),
            off_status("torrent-wait"),
            off_status("tv-remind"),
        ];
        let output = format_status(&statuses, false);
        assert!(output.contains("alerts"), "enabled watcher must show, got:\n{output}");
        assert!(
            !output.contains("torrent-wait"),
            "disabled watcher must be hidden in default view, got:\n{output}"
        );
        assert!(
            !output.contains("tv-remind"),
            "disabled watcher must be hidden in default view, got:\n{output}"
        );
        assert!(
            !output.contains("disabled"),
            "no `(disabled)` rows in default view, got:\n{output}"
        );
        // Health is unaffected: disabled watchers never count, so an all-ok
        // enabled set is still healthy.
        assert!(output.contains("All watchers healthy."));
    }

    #[test]
    fn test_format_status_all_shows_disabled() {
        // `--all` (show_all=true): the disabled rows reappear.
        let statuses = vec![ok_status("alerts", 1, 1, "1234"), off_status("torrent-wait")];
        let output = format_status(&statuses, true);
        assert!(output.contains("alerts"));
        assert!(
            output.contains("torrent-wait"),
            "disabled watcher must show under --all, got:\n{output}"
        );
        assert!(output.contains("(disabled)"));
        assert!(output.contains("All watchers healthy."));
    }

    #[test]
    fn test_format_status_default_hides_disabled_but_keeps_warning() {
        // A disabled watcher must NOT suppress (or affect) the WARNING for a
        // genuinely-DOWN enabled watcher. Default view hides the `off` row but
        // still flags the DOWN one.
        let statuses = vec![
            down_status("claude-event-watch", 1),
            off_status("context-watch"),
        ];
        let output = format_status(&statuses, false);
        assert!(output.contains("DOWN"));
        assert!(
            !output.contains("context-watch"),
            "disabled watcher hidden in default view, got:\n{output}"
        );
        assert!(output.contains("WARNING: Some watchers are down or duplicated!"));
        // Recovery hint reflects the filtered (enabled) view: it names the
        // DOWN watcher, never the disabled one.
        assert!(output.contains("claude-event-watch"));
        assert!(!output.contains("context-watch"));
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
    fn test_rewrite_config_preserves_trailing_fields() {
        // on_restart_cmd (6th), mode (7th) and monitor_cmd (8th) must survive
        // a toggle — the old 5-field rewrite silently dropped them.
        let config = "evw|bin/evw|1|true|evw --quiet 10|hist --since 5m|monitor|evw --stream\n";
        let result = rewrite_config_toggle(config, "evw", false).unwrap();
        assert_eq!(
            result,
            "evw|bin/evw|1|false|evw --quiet 10|hist --since 5m|monitor|evw --stream\n"
        );
    }

    #[test]
    fn test_rewrite_config_keyed_enabled_field() {
        let config = "evw|bin/evw|mode=monitor|enabled=true\n";
        let result = rewrite_config_toggle(config, "evw", false).unwrap();
        assert_eq!(result, "evw|bin/evw|mode=monitor|enabled=false\n");
    }

    #[test]
    fn test_format_list_empty() {
        let entries: Vec<WatcherEntry> = vec![];
        let output = format_list(&entries, "/tmp/base.conf", Some("/tmp/nope.conf"));
        assert!(output.contains("NAME"));
        // Two header lines, a blank separator, then the two `layers:` lines.
        let lines: Vec<&str> = output.lines().collect();
        assert_eq!(lines.len(), 5, "{:?}", lines);
        assert!(lines[0].starts_with("NAME"));
        assert!(lines[1].starts_with("----"));
        assert!(lines[3].starts_with("layers:"));
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
            mode: "oneshot".to_string(),
            dup_supervisors: Vec::new(),
            dup_pollers: vec![111, 222, 333],
        }];
        let output = format_status(&statuses, true);
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
            mode: "oneshot".to_string(),
            dup_supervisors: vec![358036, 359170, 705775, 761576],
            dup_pollers: Vec::new(),
        }];
        let output = format_status(&statuses, true);
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
            mode: "oneshot".to_string(),
            dup_supervisors: vec![10, 20],
            dup_pollers: vec![100, 200],
        }];
        let output = format_status(&statuses, true);
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
            mode: "oneshot".to_string(),
            dup_supervisors: vec![10, 20],
            dup_pollers: Vec::new(),
        }];
        let output = format_status(&statuses, true);
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
            mode: "oneshot".to_string(),
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
            mode: "oneshot".to_string(),
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
            mode: "oneshot".to_string(),
            dup_supervisors: vec![3, 4],
            dup_pollers: vec![1, 2],
        }];
        let output = format_status(&statuses, true);
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
    // duplicate pollers/supervisors) followed by `watcher-ctl run <name>`
    // per watcher (watcher-restart STOPS everything and starts nothing);
    // DOWN-only => per-watcher `watcher-ctl run <name>` (surgical), with
    // `watcher-restart` mentioned only alongside its stop-only caveat.

    #[test]
    fn test_format_status_duplicate_suggests_watcher_restart() {
        let statuses = vec![WatcherStatus {
            name: "alerts-watcher".to_string(),
            status: "DUPLICATE".to_string(),
            count: 3,
            required: 1,
            pids: "111 222 333".to_string(),
            enabled: true,
            mode: "oneshot".to_string(),
            dup_supervisors: Vec::new(),
            dup_pollers: vec![111, 222, 333],
        }];
        let output = format_status(&statuses, true);
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
        // watcher-restart only STOPS — the footer must spell out the
        // mandatory `watcher-ctl run <name>` follow-up so the operator
        // doesn't stop everything and restart nothing.
        assert!(
            output.contains("then `watcher-ctl run <name>` for each"),
            "expected the `watcher-ctl run <name>` follow-up after \
             watcher-restart in the DUPLICATE recovery line, got:\n{}",
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
        let output = format_status(&statuses, true);
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
        // `watcher-restart` may be mentioned, but ONLY with the stop-only
        // caveat — never as a command that restarts watchers by itself.
        assert!(
            output.contains("`watcher-restart` only STOPS"),
            "expected `watcher-restart` mentioned with its stop-only \
             caveat, got:\n{}",
            output
        );
    }

    #[test]
    fn test_format_status_mixed_down_and_duplicate_prefers_watcher_restart() {
        // When DOWN and DUPLICATE coexist, `watcher-restart` (then
        // `watcher-ctl run <name>` per watcher) is the superset fix:
        // the stop pass clears the duplicates, and the per-watcher run
        // pass covers the DOWN ones too. A per-watcher `watcher-ctl run`
        // path alone would still leave the duplicates in place, so the
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
                mode: "oneshot".to_string(),
                dup_supervisors: Vec::new(),
                dup_pollers: vec![111, 222, 333],
            },
        ];
        let output = format_status(&statuses, true);
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
        let output = format_status(&statuses, true);
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

        let msg = watcher_toggle(cfg.to_str().unwrap(), None, "toggle-test", true)
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

        let _ = watcher_toggle(cfg.to_str().unwrap(), None, "toggle-test", true)
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

    // /proc-dependent: pid_cmdline reads /proc/PID/cmdline, Linux-only.
    // On macOS pid_cmdline returns None so the self-match assert fails.
    // Gate to Linux (CI runs Linux). Sibling non-match tests stay unguarded.
    #[cfg(target_os = "linux")]
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

    #[test]
    fn test_watcher_run_lock_uses_runlock_not_child_lock_path() {
        // SELF-DEADLOCK regression: the parent spawn-serialization lock MUST
        // live on `<name>.runlock`, NOT the `<name>.lock` path the watcher
        // *script* (e.g. claude-event-watch) takes its OWN flock singleton
        // guard on. If the parent held `<name>.lock` across the child's
        // spawn+wait, the child could never acquire its guard → it would
        // refuse with "already running (pid unknown)" + exit 3 forever.
        //
        // We assert the invariant structurally: while the parent lock is held,
        // a manual flock on the SAME `<name>.runlock` path is blocked (proves
        // that IS the parent's path), but a flock on the child's `<name>.lock`
        // path is FREE (proves the parent is NOT squatting the child's guard).
        use std::os::unix::io::AsRawFd;

        let dir = tempfile::tempdir().unwrap();
        let pid_dir = dir.path().to_str().unwrap();
        let name = "claude-event-watch";

        let _held = WatcherLock::try_acquire(pid_dir, name)
            .expect("acquire should not error")
            .expect("acquire should win");

        // The child's singleton-guard lockfile path must be FREE while the
        // parent holds its run-lock — otherwise the child self-deadlocks.
        let child_lock_path = format!("{}/{}.lock", pid_dir, name);
        let child_file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(&child_lock_path)
            .expect("open child lock path");
        let child_rc =
            unsafe { libc::flock(child_file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        assert_eq!(
            child_rc, 0,
            "the child's <name>.lock guard path MUST be free while the parent              holds its run-lock (else the spawned watcher self-deadlocks)"
        );

        // And the parent's actual lock path IS `<name>.runlock` (a manual
        // non-blocking flock on it must fail — the parent holds it).
        let run_lock_path = format!("{}/{}.runlock", pid_dir, name);
        let run_file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(&run_lock_path)
            .expect("open runlock path");
        let run_rc =
            unsafe { libc::flock(run_file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        assert_ne!(
            run_rc, 0,
            "the parent's run-lock path must be <name>.runlock and be held              (a concurrent flock on it must fail)"
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
            // Empty = explicitly no override layer (an UNSET var would resolve
            // to the real `$XDG_CONFIG_HOME/watchmen/watchers.override.conf`
            // and let a developer's own override leak into the test config).
            std::env::set_var("WATCHERS_CONFIG_EXTRA", "");
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

    /// `watcher_run` for a `mode=monitor` watcher must NOT exec the start_cmd:
    /// it records the arm intent (command included) and returns 0. The
    /// override layer is what flips the mode here — the base line is a plain
    /// oneshot entry — so this also pins "one line in the override file is
    /// the whole flip".
    #[tokio::test]
    async fn test_watcher_run_monitor_mode_prints_arm_and_does_not_spawn() {
        let dir = tempfile::tempdir().unwrap();
        let pid_dir = dir.path().join("pids");
        std::fs::create_dir_all(&pid_dir).unwrap();
        let cfg = dir.path().join("watchers.conf");
        let ov = dir.path().join("watchers.override.conf");
        let sentinel = format!("cw-runtest-monitor-{}", unique_token("w"));
        let script = make_poller_script(dir.path(), &sentinel, "30");
        std::fs::write(&cfg, format!("runtest|{}|1|true|{} --quiet 10\n", sentinel, script)).unwrap();
        std::fs::write(&ov, "runtest|mode=monitor\n").unwrap();

        let _env = RunEnv::new(pid_dir.to_str().unwrap(), cfg.to_str().unwrap());
        std::env::set_var("WATCHERS_CONFIG_EXTRA", ov.to_str().unwrap());

        let code = watcher_run(&config_path(), config_path_extra().as_deref(), "runtest")
            .await
            .expect("monitor-mode run should succeed");
        assert_eq!(code, 0);

        // Intent recorded with the exact command to arm.
        let intent = std::fs::read_to_string(pid_dir.join("runtest.monitor-intent"))
            .expect("monitor intent file written");
        assert!(intent.contains("epoch="), "{}", intent);
        assert!(
            intent.contains(&format!("command={} --quiet 10 --mode monitor", script)),
            "{}",
            intent
        );
        // Nothing was spawned: no pid file, no live poller matching the sentinel.
        assert!(!pid_dir.join("runtest.pid").exists(), "monitor mode must not claim the one-shot pid slot");
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        let pids = process_pids(&sentinel).await;
        assert!(pids.is_empty(), "monitor mode must not exec the start_cmd, found {:?}", pids);
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
        //
        // `process_pids` is a bare `pgrep -f`, and the poller is a `/bin/sh`
        // script whose FILENAME carries the sentinel. That shell forks a child
        // to run its `sleep`, and between the fork() and the child's execve()
        // the child still carries the PARENT's argv — so a `pgrep` that lands
        // inside that window reports TWO pids for ONE poller. Measured at
        // ~1-in-3000 on an idle box with a tight sampling loop; a loaded CI
        // runner widens the window and it turns the job red for a reason that
        // has nothing to do with the guard under test.
        //
        // Filter the poller's own descendants out before counting. This does
        // not weaken the assertion: a genuine second instance is spawned by
        // `watcher_run`, so it would be a child of THIS test process, never a
        // child of the first poller.
        let raw = process_pids(&sentinel).await;
        let own_descendants = descendants_of(&[first_pid], &read_ppid_map());
        let pollers: Vec<u32> = raw
            .iter()
            .copied()
            .filter(|pid| !own_descendants.contains(pid))
            .collect();
        assert_eq!(
            pollers.len(),
            1,
            "only the first instance should be alive, got pids {:?} \
             (raw pgrep matches {:?}, first poller's descendants {:?})",
            pollers,
            raw,
            own_descendants
        );

        // Cleanup.
        let _ = first.start_kill();
        let _ = first.wait().await;
    }

    /// REGRESSION (exec-argv false-DOWN, the bug this PR fixes): a watcher
    /// whose launcher `exec`s the bare binary (so the live cmdline has the
    /// `.sh` STRIPPED) and whose PID is recorded in a `<name>.lock` file MUST
    /// read as UP — `watcher_status` must report `ok`, NOT `DOWN`.
    ///
    /// Before the fix, `watcher_status` derived liveness from
    /// `pgrep -f -- <pattern>` where `<pattern>` was the `.sh` launcher path
    /// from watchers.conf. After the launcher execs `/usr/local/bin/<name>`,
    /// the `.sh` is gone from argv, so the pgrep never matched and the CLI
    /// reported `0/1` DOWN while the daemon (pidfile-based since PR #339)
    /// correctly saw it UP. This test pins the migrated CLI to the pidfile
    /// model: we record a live process's PID in `<name>.lock`, give the
    /// watcher a `.sh` start_cmd and a poller `pattern` that DELIBERATELY
    /// cannot match the live process, and assert the status is `ok`.
    // Reads /proc (PID liveness + cmdline identity) via the shared liveness
    // helpers, so it only runs on Linux — the container deploy target. On a
    // macOS dev host there is no /proc, so cmdline identity can't be verified;
    // the pure decision logic is covered by
    // `test_watcher_status_decision_is_pidfile_based` below (runs everywhere).
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn test_watcher_status_exec_argv_is_up_via_pidfile() {
        let dir = tempfile::tempdir().unwrap();
        let pid_dir = dir.path().join("pids");
        std::fs::create_dir_all(&pid_dir).unwrap();
        let cfg = dir.path().join("watchers.conf");

        // The live process's argv[0] carries the bare STEM (no `.sh`) — exactly
        // the post-`exec` shape. We name the script `claude-event-watch` (stem)
        // and sleep so it stays alive across the status read.
        let stem = format!("claude-event-watch-{}", unique_token("exec"));
        let script = make_poller_script(dir.path(), &stem, "30");

        // start_cmd is the `.sh` LAUNCHER (what watchers.conf records); the
        // live cmdline is the stem (no `.sh`) — the identity check must still
        // match via suffix-stripping. The `pattern` is the `.sh` path too,
        // which `pgrep -f` can NEVER find against the exec'd process — proving
        // the status decision does NOT depend on pgrep.
        let launcher_sh = format!("{}.sh", script); // `<stem>.sh` — never spawned
        std::fs::write(
            &cfg,
            format!("evw|{}|1|true|{}\n", launcher_sh, launcher_sh),
        )
        .unwrap();

        // Spawn the live "exec'd binary" (argv carries the bare stem) and
        // record its PID in `<name>.lock` (the file a real watcher writes).
        let mut child = tokio::process::Command::new(&script)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn live watcher");
        let live_pid = child.id().expect("child pid");
        let lock_file = pid_dir.join("evw.lock");
        std::fs::write(&lock_file, live_pid.to_string()).unwrap();

        let _env = RunEnv::new(pid_dir.to_str().unwrap(), cfg.to_str().unwrap());

        let statuses = watcher_status(&config_path(), config_path_extra().as_deref()).await;
        let evw = statuses
            .iter()
            .find(|s| s.name == "evw")
            .expect("evw status present");

        assert_eq!(
            evw.status, "ok",
            "a live watcher recorded in <name>.lock whose argv lost the `.sh` \
             (exec-to-binary transform) MUST read as UP — got {:?} (this is the \
             exec-argv false-DOWN regression)",
            evw
        );
        assert_eq!(evw.count, 1, "pidfile model: one live matching instance");
        // The reported PID is the authoritative recorded watcher PID.
        assert_eq!(evw.pids, live_pid.to_string());

        // Cleanup.
        let _ = child.start_kill();
        let _ = child.wait().await;
    }

    /// Negative companion: a STALE `<name>.lock` (recorded PID dead) with no
    /// live process must read as DOWN — proves the pidfile model still detects
    /// a genuinely-dead watcher (and doesn't paper over it).
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn test_watcher_status_stale_lock_is_down() {
        let dir = tempfile::tempdir().unwrap();
        let pid_dir = dir.path().join("pids");
        std::fs::create_dir_all(&pid_dir).unwrap();
        let cfg = dir.path().join("watchers.conf");

        let stem = format!("claude-event-watch-{}", unique_token("dead"));
        let launcher_sh = format!("/opt/x/{}.sh", stem);
        std::fs::write(&cfg, format!("evw|{}|1|true|{}\n", launcher_sh, launcher_sh)).unwrap();

        // Record a definitely-dead PID in <name>.lock.
        let lock_file = pid_dir.join("evw.lock");
        std::fs::write(&lock_file, (u32::MAX - 1).to_string()).unwrap();

        let _env = RunEnv::new(pid_dir.to_str().unwrap(), cfg.to_str().unwrap());

        let statuses = watcher_status(&config_path(), config_path_extra().as_deref()).await;
        let evw = statuses.iter().find(|s| s.name == "evw").expect("evw present");
        assert_eq!(
            evw.status, "DOWN",
            "a stale <name>.lock (dead recorded PID) must read as DOWN, got {:?}",
            evw
        );
        assert_eq!(evw.count, 0);
    }

    // --- monitor-mode ARMING (status) tests --------------------------------
    //
    // Fixture: a `mode=monitor` watcher (flipped by the override layer, as in
    // production) with NO live process. What decides ARMING vs DOWN is the
    // `<name>.monitor-intent` file `watcher-ctl run` writes, its age, and
    // whether a runtime file (`.lock`) has been written since.

    fn epoch_now() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    /// Write a monitor-mode fixture (base line + override flip) + pid dir and
    /// return (pid_dir, cfg, ov).
    fn monitor_fixture(dir: &std::path::Path) -> (std::path::PathBuf, std::path::PathBuf, std::path::PathBuf) {
        let pid_dir = dir.join("pids");
        std::fs::create_dir_all(&pid_dir).unwrap();
        let cfg = dir.join("watchers.conf");
        let ov = dir.join("watchers.override.conf");
        std::fs::write(&cfg, "evw|/opt/x/evw.sh|1|true|/opt/x/evw.sh\n").unwrap();
        std::fs::write(&ov, "evw|mode=monitor\n").unwrap();
        (pid_dir, cfg, ov)
    }

    fn write_intent(pid_dir: &std::path::Path, age_secs: u64) {
        std::fs::write(
            pid_dir.join("evw.monitor-intent"),
            format!("epoch={}\ncommand=/opt/x/evw.sh --mode monitor\n", epoch_now() - age_secs),
        )
        .unwrap();
    }

    async fn evw_status(grace: f64) -> WatcherStatus {
        let statuses = watcher_status_with(&config_path(), config_path_extra().as_deref(), grace).await;
        statuses.into_iter().find(|s| s.name == "evw").expect("evw present")
    }

    /// Fresh intent, no runtime file: the loop just ran `watcher-ctl run` and
    /// has not armed the Monitor yet -> ARMING, count 0, NOT unhealthy.
    #[tokio::test]
    async fn test_watcher_status_monitor_fresh_intent_is_arming() {
        let dir = tempfile::tempdir().unwrap();
        let (pid_dir, cfg, ov) = monitor_fixture(dir.path());
        write_intent(&pid_dir, 10);
        let _env = RunEnv::new(pid_dir.to_str().unwrap(), cfg.to_str().unwrap());
        std::env::set_var("WATCHERS_CONFIG_EXTRA", ov.to_str().unwrap());

        let evw = evw_status(120.0).await;
        assert_eq!(evw.status, "ARMING", "{:?}", evw);
        assert_eq!(evw.count, 0);
        assert_eq!(evw.mode, "monitor");
        assert!(!any_unhealthy(std::slice::from_ref(&evw)), "ARMING must not trip --unhealthy-only");

        // A STALE lock (dead pid, older than the intent — e.g. left by the
        // one-shot era) does not consume the intent: still ARMING.
        let lock = pid_dir.join("evw.lock");
        std::fs::write(&lock, (u32::MAX - 1).to_string()).unwrap();
        filetime::set_file_mtime(
            &lock,
            filetime::FileTime::from_unix_time((epoch_now() - 3600) as i64, 0),
        )
        .unwrap();
        let evw = evw_status(120.0).await;
        assert_eq!(evw.status, "ARMING", "stale lock older than intent: {:?}", evw);

        // Grace 0 disables the state entirely -> plain DOWN.
        let evw = evw_status(0.0).await;
        assert_eq!(evw.status, "DOWN", "arming grace 0 => DOWN: {:?}", evw);
        assert!(any_unhealthy(std::slice::from_ref(&evw)));
    }

    /// Past the arming grace with nothing live -> DOWN again (the existing
    /// re-ARM footer path), and no intent at all -> DOWN.
    #[tokio::test]
    async fn test_watcher_status_monitor_stale_or_missing_intent_is_down() {
        let dir = tempfile::tempdir().unwrap();
        let (pid_dir, cfg, ov) = monitor_fixture(dir.path());
        let _env = RunEnv::new(pid_dir.to_str().unwrap(), cfg.to_str().unwrap());
        std::env::set_var("WATCHERS_CONFIG_EXTRA", ov.to_str().unwrap());

        // No intent ever written.
        let evw = evw_status(120.0).await;
        assert_eq!(evw.status, "DOWN", "no intent => DOWN: {:?}", evw);

        // Intent older than the grace.
        write_intent(&pid_dir, 1000);
        let evw = evw_status(120.0).await;
        assert_eq!(evw.status, "DOWN", "stale intent => DOWN: {:?}", evw);
        let out = format_status(std::slice::from_ref(&evw), false);
        assert!(out.contains("Monitor-mode watcher(s) DOWN: evw"), "{}", out);
        assert!(!out.contains("ARMING"), "{}", out);
    }

    /// The monitor went live AFTER the intent (its flock guard rewrote
    /// `<name>.lock`, so the lock is YOUNGER than the intent) and is now dead:
    /// a real outage, reported DOWN at once — it must not ride out the rest of
    /// the arming window as ARMING.
    #[tokio::test]
    async fn test_watcher_status_monitor_lock_newer_than_intent_is_down() {
        let dir = tempfile::tempdir().unwrap();
        let (pid_dir, cfg, ov) = monitor_fixture(dir.path());
        write_intent(&pid_dir, 30);
        // Lock written "now" (after the 30s-old intent), recording a dead pid.
        std::fs::write(pid_dir.join("evw.lock"), (u32::MAX - 1).to_string()).unwrap();
        let _env = RunEnv::new(pid_dir.to_str().unwrap(), cfg.to_str().unwrap());
        std::env::set_var("WATCHERS_CONFIG_EXTRA", ov.to_str().unwrap());

        let evw = evw_status(120.0).await;
        assert_eq!(evw.status, "DOWN", "consumed intent + dead monitor => DOWN: {:?}", evw);
    }

    /// A ONESHOT watcher never reads ARMING, even with a (stray) intent file:
    /// the state is monitor-mode only.
    #[tokio::test]
    async fn test_watcher_status_oneshot_ignores_intent_file() {
        let dir = tempfile::tempdir().unwrap();
        let pid_dir = dir.path().join("pids");
        std::fs::create_dir_all(&pid_dir).unwrap();
        let cfg = dir.path().join("watchers.conf");
        std::fs::write(&cfg, "evw|/opt/x/evw.sh|1|true|/opt/x/evw.sh\n").unwrap();
        write_intent(&pid_dir, 5);
        let _env = RunEnv::new(pid_dir.to_str().unwrap(), cfg.to_str().unwrap());

        let evw = evw_status(120.0).await;
        assert_eq!(evw.status, "DOWN", "oneshot + intent => still DOWN: {:?}", evw);
    }

    /// `watcher-restart` voids a pending arm: it removes `<name>.monitor-intent`
    /// along with the `.pid` files, so a stopped monitor-mode watcher reads
    /// DOWN (re-ARM footer) rather than ARMING for the rest of the window.
    #[tokio::test]
    async fn test_watcher_restart_clears_monitor_intent() {
        let dir = tempfile::tempdir().unwrap();
        let (pid_dir, cfg, ov) = monitor_fixture(dir.path());
        write_intent(&pid_dir, 5);
        std::fs::write(pid_dir.join("other.pid"), "1").unwrap();
        let _env = RunEnv::new(pid_dir.to_str().unwrap(), cfg.to_str().unwrap());
        std::env::set_var("WATCHERS_CONFIG_EXTRA", ov.to_str().unwrap());

        let msg = watcher_restart(&config_path(), config_path_extra().as_deref()).await;
        assert!(msg.contains("Cleaned PID files"), "{}", msg);
        assert!(!pid_dir.join("evw.monitor-intent").exists(), "intent removed by restart");
        assert!(!pid_dir.join("other.pid").exists(), "pid files still cleaned");
        let evw = evw_status(120.0).await;
        assert_eq!(evw.status, "DOWN", "after restart, no intent => DOWN: {:?}", evw);
    }

    /// Portable (macOS + Linux) PURE coverage of the exec-argv fix decision
    /// logic — no `/proc`, so it runs on the dev host too (unlike the gated
    /// integration tests above, which need Linux `/proc`).
    ///
    /// Pins the two facts that make the false-DOWN bug impossible now:
    ///   1. The shared `cmdline_matches_watcher` matches the exec'd bare binary
    ///      cmdline against the `.sh` launcher start_cmd (suffix-stripping).
    ///   2. Given a recorded PID that is alive AND whose cmdline matches, the
    ///      pidfile decision says UP (`!is_down`) — the case the old pgrep path
    ///      wrongly reported DOWN.
    #[test]
    fn test_watcher_status_decision_is_pidfile_based() {
        // (1) The exec-to-binary identity match (the crux of the bug).
        let start_cmd = "/opt/claude-container/watchers/claude-event-watch.sh";
        let exec_cmdline = "/bin/bash /usr/local/bin/claude-event-watch"; // .sh gone
        assert!(
            crate::status::cmdline_matches_watcher(exec_cmdline, start_cmd),
            "the exec'd bare-binary cmdline (no .sh) MUST match the .sh launcher \
             start_cmd via suffix-stripping — this is what defeats the old \
             pgrep-on-.sh-path approach"
        );

        // (2) The pidfile UP/DOWN decision, in the four canonical states.
        // Live + identity-matched recorded PID → UP (the false-DOWN case).
        assert!(
            !crate::status::pidfile_watcher_is_down(Some(4242), true, true),
            "a live, identity-matched recorded PID must read as UP"
        );
        // Missing pidfile → DOWN.
        assert!(crate::status::pidfile_watcher_is_down(None, false, false));
        // Stale pidfile (recorded PID dead) → DOWN.
        assert!(crate::status::pidfile_watcher_is_down(Some(4242), false, false));
        // Recycled PID (alive but cmdline mismatch) → DOWN.
        assert!(crate::status::pidfile_watcher_is_down(Some(4242), true, false));
    }

    /// `pid_matches_watcher` must tolerate the exec-to-binary transform too:
    /// a `.sh` launcher start_cmd whose live cmdline carries only the bare stem
    /// is a match. (Pure string-level coverage of the suffix-stripping; the
    /// `/proc`-reading path is exercised by the Linux-gated integration tests.)
    #[test]
    fn test_pid_matches_watcher_strips_sh_suffix_via_shared_helper() {
        // The identity helper `pid_matches_watcher` delegates suffix-stripping
        // to `crate::status::strip_script_suffix`; verify the stem the live
        // cmdline would carry is what we'd match on.
        assert_eq!(
            crate::status::strip_script_suffix("claude-event-watch.sh"),
            "claude-event-watch"
        );
        assert_eq!(
            crate::status::strip_script_suffix("memory-remind.bash"),
            "memory-remind"
        );
        assert_eq!(
            crate::status::strip_script_suffix("emit.py"),
            "emit"
        );
        // No known extension → unchanged.
        assert_eq!(
            crate::status::strip_script_suffix("claude-event-watch"),
            "claude-event-watch"
        );
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
