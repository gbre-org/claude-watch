//! Shared /proc scanning utilities.
//!
//! Used by both `agent.rs` and `task_watch.rs` for process introspection.

use std::path::Path;

/// Built-in watcher/service command patterns — processes matching these
/// are classified as persistent services, not ephemeral tasks.
/// `watcher-ctl` covers the canonical supervisor form (any watcher run
/// via `watcher-ctl run <name>`). The remaining entries cover stock
/// watcher binaries shipped under `tools/watchers/` plus the generic
/// `request-wait` / `task-watch` wrappers.
///
/// Operators with additional site-specific watcher names should extend
/// this list via `CLAUDE_WATCH_EXTRA_WATCHER_PATTERNS` (colon-separated)
/// — see `extra_service_patterns` for the merge logic.
pub const SERVICE_PATTERNS: &[&str] = &[
    "watcher-ctl",
    "watchmen",
    "memory-remind",
    "context-watch",
    "task-watch",
    "request-wait",
];

/// Read additional watcher/service patterns from the
/// `CLAUDE_WATCH_EXTRA_WATCHER_PATTERNS` env var (colon-separated).
/// Returns an empty Vec when unset / empty. Mirrors the same env var
/// used by `agent::extra_watcher_patterns` so a single operator-set
/// list covers both call sites.
pub fn extra_service_patterns() -> Vec<String> {
    std::env::var("CLAUDE_WATCH_EXTRA_WATCHER_PATTERNS")
        .ok()
        .map(|s| {
            s.split(':')
                .map(|p| p.trim().to_string())
                .filter(|p| !p.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// Output file content signatures — fallback for orphaned processes whose
/// parent chain is broken (reparented to init). Matched against first line.
pub const SERVICE_OUTPUT_SIGNATURES: &[&str] = &[
    "Watcher monitor started", // watchmen
];

/// Read a process's command line from /proc/PID/cmdline.
pub fn get_pid_cmdline(pid: &str) -> Option<String> {
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

/// Get a process's parent PID from /proc/PID/status.
pub fn get_pid_ppid(pid: &str) -> Option<String> {
    let path = format!("/proc/{}/status", pid);
    let content = std::fs::read_to_string(&path).ok()?;
    for line in content.lines() {
        if let Some(rest) = line.strip_prefix("PPid:") {
            let ppid = rest.trim().to_string();
            if !ppid.is_empty() {
                return Some(ppid);
            }
        }
    }
    None
}

/// Check if an fd is open for writing via /proc/PID/fdinfo/FD.
///
/// Reads the `flags:` line from fdinfo and checks the lowest 2 bits:
/// O_RDONLY=0, O_WRONLY=1, O_RDWR=2. Writable if (flags & 3) >= 1.
pub fn fd_is_writable(pid: &str, fd: &str) -> bool {
    let path = format!("/proc/{}/fdinfo/{}", pid, fd);
    match std::fs::read_to_string(&path) {
        Ok(content) => parse_fdinfo_writable(&content),
        Err(_) => false,
    }
}

/// Parse fdinfo content to determine if the fd is writable.
/// Exported for testing.
pub fn parse_fdinfo_writable(content: &str) -> bool {
    for line in content.lines() {
        if let Some(rest) = line.strip_prefix("flags:") {
            let flags_str = rest.trim();
            // Flags are in octal
            if let Ok(flags) = u32::from_str_radix(flags_str, 8) {
                return (flags & 3) >= 1;
            }
        }
    }
    false
}

/// Check if a process (or any ancestor up to 5 levels) is a known persistent service.
///
/// Walks the process tree checking cmdline against SERVICE_PATTERNS.
pub fn is_service_process(pid: &str) -> bool {
    let mut visited = std::collections::HashSet::new();
    let mut current = pid.to_string();

    for _ in 0..5 {
        if visited.contains(&current) || current.is_empty() || current == "0" || current == "1" {
            break;
        }
        visited.insert(current.clone());
        if let Some(cmdline) = get_pid_cmdline(&current) {
            if SERVICE_PATTERNS.iter().any(|pat| cmdline.contains(pat)) {
                return true;
            }
            let extras = extra_service_patterns();
            if extras.iter().any(|pat| cmdline.contains(pat)) {
                return true;
            }
        }
        match get_pid_ppid(&current) {
            Some(ppid) => current = ppid,
            None => break,
        }
    }
    false
}

/// Deployment mode for a Claude Code agent process, used to decide which
/// interruption channel to use.
///
/// See `docs/sse-protocol.md` for the full discussion of why the panel-mode
/// case has no out-of-process inject path.
///
/// `#[allow(dead_code)]` on this enum and the helpers below: this is a
/// building block landed alongside the SSE-protocol investigation doc.
/// First in-tree callsite will be the inject-suppression check in
/// `policy.rs` (planned in a follow-up — keeping this PR scoped to
/// detection + docs so reviewers can inspect the protocol findings before
/// any behavior changes ride along). Tests below cover the predicate
/// surface end-to-end so the helpers don't bit-rot in the meantime.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentDeploymentMode {
    /// Claude is running in a pty (terminal mode). `tmux send-keys` into
    /// the controlling pane is the correct injection channel. Covers:
    /// - Native CLI invocations (`claude` from any shell).
    /// - VSCode integrated terminal running `claude` directly or attached
    ///   to a tmux session that runs `claude`.
    /// - The workbot container's tmux-hosted claude.
    Terminal,
    /// Claude was spawned by an IDE extension (e.g. VSCode panel mode)
    /// with `stdio: ["pipe","pipe",...]`. The extension owns the stdin
    /// pipe; there is no out-of-process input channel. tmux-inject is a
    /// silent no-op in this mode. For the alternatives, see the
    /// "Implications for claude-watch" section of
    /// `docs/sse-protocol.md`.
    IdePanel,
    /// Detection failed (process gone, permission denied, etc.). Caller
    /// should default to Terminal behavior (the historical default) since
    /// it's strictly broader than IdePanel.
    Unknown,
}

/// Read the env block for a process from /proc/PID/environ.
///
/// Returns `None` if the file is unreadable (permission, gone, etc.).
/// The returned string is the raw NUL-separated env block — call
/// `env_contains_key` to check for a specific key without parsing.
#[allow(dead_code)]
pub fn get_pid_environ(pid: &str) -> Option<Vec<u8>> {
    let path = format!("/proc/{}/environ", pid);
    std::fs::read(&path).ok()
}

/// Check whether a raw NUL-separated env block contains the named key
/// with any value. Matches whole-key (must be preceded by NUL or be the
/// first byte, followed by '=').
#[allow(dead_code)]
pub fn env_contains_key(environ: &[u8], key: &str) -> bool {
    let needle = format!("{}=", key);
    let needle_b = needle.as_bytes();
    if environ.starts_with(needle_b) {
        return true;
    }
    let mut hay = environ;
    while let Some(pos) = hay.iter().position(|&b| b == 0) {
        if pos + 1 >= hay.len() {
            return false;
        }
        let rest = &hay[pos + 1..];
        if rest.starts_with(needle_b) {
            return true;
        }
        hay = rest;
    }
    false
}

/// Resolve /proc/PID/fd/0 to its target and classify it.
///
/// Returns:
/// - `Some(true)` if stdin is a pty (target starts with `/dev/pts/` or
///   exactly `/dev/tty`).
/// - `Some(false)` if stdin is a pipe / socket / regular file (target
///   like `pipe:[12345]`, `socket:[...]`, or a path that's not a pty).
/// - `None` if the symlink can't be read.
#[allow(dead_code)]
pub fn stdin_is_pty(pid: &str) -> Option<bool> {
    let path = format!("/proc/{}/fd/0", pid);
    let target = std::fs::read_link(&path).ok()?;
    let s = target.to_string_lossy();
    Some(s.starts_with("/dev/pts/") || s == "/dev/tty")
}

/// Determine the deployment mode of an agent process.
///
/// The check order:
/// 1. If stdin is a pty → Terminal. This catches every CLI launch,
///    including those connected to a VSCode-extension MCP server via
///    `/ide` (which sets CLAUDE_CODE_SSE_PORT but keeps stdin as a tty).
/// 2. If stdin is a pipe AND `CLAUDE_CODE_SSE_PORT` is in the env →
///    IdePanel. The combination is the signature of an extension
///    spawning the agent with piped stdio.
/// 3. If stdin is a pipe but no SSE port → Terminal (some unusual CLI
///    invocation with stdin redirected, e.g. `claude < script.txt`).
///    Falling back to Terminal here is the safer default — tmux-inject
///    is harmless on a process that won't see it, while incorrectly
///    classifying as IdePanel would suppress a legitimate interrupt.
/// 4. Anything else → Unknown.
#[allow(dead_code)]
pub fn agent_deployment_mode(pid: &str) -> AgentDeploymentMode {
    match stdin_is_pty(pid) {
        Some(true) => AgentDeploymentMode::Terminal,
        Some(false) => {
            let environ = match get_pid_environ(pid) {
                Some(e) => e,
                None => return AgentDeploymentMode::Unknown,
            };
            if env_contains_key(&environ, "CLAUDE_CODE_SSE_PORT") {
                AgentDeploymentMode::IdePanel
            } else {
                AgentDeploymentMode::Terminal
            }
        }
        None => AgentDeploymentMode::Unknown,
    }
}

/// Check if a task's output file content matches known service signatures.
///
/// Fallback detection for orphaned processes whose parent chain is broken.
pub fn is_service_output(tasks_dir: &Path, task_id: &str) -> bool {
    let output_file = tasks_dir.join(format!("{}.output", task_id));
    match std::fs::read_to_string(&output_file) {
        Ok(content) => {
            if let Some(first_line) = content.lines().next() {
                SERVICE_OUTPUT_SIGNATURES
                    .iter()
                    .any(|sig| first_line.contains(sig))
            } else {
                false
            }
        }
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_fdinfo_writable_rdonly() {
        let content = "pos:\t0\nflags:\t0100000\nmnt_id:\t28\n";
        assert!(!parse_fdinfo_writable(content));
    }

    #[test]
    fn test_parse_fdinfo_writable_wronly() {
        // O_WRONLY = 1, plus O_LARGEFILE = 0100000 → 0100001
        let content = "pos:\t0\nflags:\t0100001\nmnt_id:\t28\n";
        assert!(parse_fdinfo_writable(content));
    }

    #[test]
    fn test_parse_fdinfo_writable_rdwr() {
        // O_RDWR = 2, plus O_LARGEFILE = 0100000 → 0100002
        let content = "pos:\t0\nflags:\t0100002\nmnt_id:\t28\n";
        assert!(parse_fdinfo_writable(content));
    }

    #[test]
    fn test_parse_fdinfo_writable_append() {
        // O_WRONLY|O_APPEND = 02001
        let content = "pos:\t0\nflags:\t0102001\nmnt_id:\t28\n";
        assert!(parse_fdinfo_writable(content));
    }

    #[test]
    fn test_parse_fdinfo_writable_empty() {
        assert!(!parse_fdinfo_writable(""));
    }

    #[test]
    fn test_parse_fdinfo_writable_no_flags_line() {
        let content = "pos:\t0\nmnt_id:\t28\n";
        assert!(!parse_fdinfo_writable(content));
    }

    #[test]
    fn test_is_service_output_no_file() {
        let dir = Path::new("/tmp/nonexistent-task-watch-test");
        assert!(!is_service_output(dir, "fake-task-id"));
    }

    #[test]
    fn test_env_contains_key_at_start() {
        let env = b"CLAUDE_CODE_SSE_PORT=43473\0HOME=/home/test\0";
        assert!(env_contains_key(env, "CLAUDE_CODE_SSE_PORT"));
    }

    #[test]
    fn test_env_contains_key_in_middle() {
        let env = b"PATH=/usr/bin\0CLAUDE_CODE_SSE_PORT=12345\0HOME=/home/test\0";
        assert!(env_contains_key(env, "CLAUDE_CODE_SSE_PORT"));
    }

    #[test]
    fn test_env_contains_key_at_end() {
        let env = b"PATH=/usr/bin\0HOME=/home/test\0CLAUDE_CODE_SSE_PORT=9999\0";
        assert!(env_contains_key(env, "CLAUDE_CODE_SSE_PORT"));
    }

    #[test]
    fn test_env_contains_key_absent() {
        let env = b"PATH=/usr/bin\0HOME=/home/test\0";
        assert!(!env_contains_key(env, "CLAUDE_CODE_SSE_PORT"));
    }

    #[test]
    fn test_env_contains_key_substring_not_a_match() {
        // A var named MY_CLAUDE_CODE_SSE_PORT_ALT should not match
        // CLAUDE_CODE_SSE_PORT — the helper must require whole-key match.
        let env = b"MY_CLAUDE_CODE_SSE_PORT_ALT=oops\0HOME=/home/test\0";
        assert!(!env_contains_key(env, "CLAUDE_CODE_SSE_PORT"));
    }

    #[test]
    fn test_env_contains_key_empty_value() {
        let env = b"CLAUDE_CODE_SSE_PORT=\0HOME=/home/test\0";
        assert!(env_contains_key(env, "CLAUDE_CODE_SSE_PORT"));
    }

    #[test]
    fn test_env_contains_key_empty_environ() {
        assert!(!env_contains_key(b"", "CLAUDE_CODE_SSE_PORT"));
    }

    #[test]
    fn test_env_contains_key_no_trailing_nul() {
        // Real /proc/PID/environ ends in NUL; defensive case for one
        // without a final NUL terminator.
        let env = b"PATH=/usr/bin\0CLAUDE_CODE_SSE_PORT=42";
        assert!(env_contains_key(env, "CLAUDE_CODE_SSE_PORT"));
    }

    #[test]
    fn test_agent_deployment_mode_unknown_for_bogus_pid() {
        // /proc/0 / /proc/-1 won't resolve. Function should return Unknown.
        let m = agent_deployment_mode("99999999");
        assert_eq!(m, AgentDeploymentMode::Unknown);
    }

    #[test]
    fn test_agent_deployment_mode_self_is_terminal_or_unknown() {
        // The test process is running under cargo nextest / cargo test; its
        // stdin may be a pty (interactive test run) or a pipe (CI). Either
        // way, it doesn't have CLAUDE_CODE_SSE_PORT set, so the result
        // must be Terminal or Unknown — never IdePanel.
        let pid = std::process::id().to_string();
        let m = agent_deployment_mode(&pid);
        assert_ne!(
            m,
            AgentDeploymentMode::IdePanel,
            "test process must not be classified as IdePanel"
        );
    }
}
