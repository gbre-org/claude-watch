//! Inject probe — out-of-process inject path for IDE-panel-mode Claude Code agents.
//!
//! Empirically discovered during the 2026-05-16 re-probe (see
//! `docs/sse-protocol.md` § "EMPIRICAL re-probe 2026-05-16"). Replaces the
//! prior "no out-of-process inject path exists" conclusion, which was reached
//! by reasoning from VSIX strings rather than running a live panel-mode agent.
//!
//! ## Mechanism
//!
//! When the VSCode panel extension (or any `@anthropic-ai/claude-agent-sdk`
//! caller) spawns `claude --input-format stream-json --output-format
//! stream-json` with `stdio: ['pipe','pipe','pipe']`, Node's `child_process`
//! module creates **AF_UNIX `SOCK_STREAM` socketpairs** (not anonymous pipes)
//! for each of stdin/stdout/stderr. The agent's fd 0 is one end of the
//! stdin socketpair; the parent (extension host) holds the other end.
//!
//! From `/proc/PID/fd/0` we see `socket:[N]` instead of `pipe:[N]`. The
//! `/proc/PID/fd/0` symlink, for unix-socket fds, is **not openable** —
//! `open(2)` returns ENXIO. So `echo X > /proc/PID/fd/0` does NOT work.
//!
//! However, `pidfd_getfd(2)` (Linux 5.6+) CAN dup an arbitrary file
//! descriptor from another process when we have the privilege to ptrace it.
//! On a stock distro with `kernel.yama.ptrace_scope = 0` (default on Debian
//! desktop), any same-uid process can pidfd_getfd. We use that to duplicate
//! the **parent extension host's** end of the stdin socketpair, then write
//! a stream-json line to it. The kernel routes the bytes to the agent's
//! stdin receive queue and the agent processes the message as a normal
//! user turn.
//!
//! Discovery: the parent's matching socketpair end has the **agent fd 0
//! inode minus 1** (Linux allocates the two socketpair endpoints with
//! consecutive inode numbers; the parent-side end is the lower of the two).
//! See `find_parent_stdin_fd` for the walk.
//!
//! ## Scope of this module
//!
//! This module hosts both the one-shot probe CLI (`cw inject-probe`) and
//! the library-callable inject function (`inject`) wired into the daemon's
//! deployment-mode dispatcher in `inject_dispatch.rs`. The dispatcher
//! routes panel-mode agents here; terminal-mode agents continue to use
//! `tmux::inject_text`; unknown / failed cases fall through to the
//! claude-event escalation tier. See `inject_dispatch::inject_to_agent`
//! for the call site.
//!
//! ## Constraints
//!
//! - Linux only. The `pidfd_open` / `pidfd_getfd` syscalls are Linux-specific
//!   and require kernel ≥ 5.6 / 5.6 respectively. Module is gated on `cfg(target_os = "linux")`.
//! - Same uid (or root). On kernels with `ptrace_scope > 0`, an unprivileged
//!   process cannot pidfd_getfd a sibling. Module returns a clear error in that case.
//! - Target must currently have its stdin in stream-json mode. For terminal-
//!   mode agents (fd 0 → /dev/pts/N), use tmux-inject instead — `cw inject-probe`
//!   refuses with `WrongMode` and points the operator at `tmux send-keys`.

use serde::Serialize;
use std::fs;
#[cfg(target_os = "linux")]
use std::os::unix::io::{FromRawFd, OwnedFd};

/// Outcome of a probe attempt. Serializable so the CLI can emit `--json`.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub enum ProbeOutcome {
    /// Wrote `bytes` bytes to the agent's stdin via pidfd_getfd. The agent
    /// SHOULD process the message on its next event-loop tick; this probe
    /// does NOT block waiting for the response (caller inspects the agent's
    /// stdout / transcript independently).
    // Only constructed inside the cfg(target_os = "linux") arm of probe().
    // On non-Linux the variant is read-only (pattern-matched), which trips
    // dead_code under CI/pre-commit -D warnings. Allow it off-Linux only.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    Ok { bytes: usize, parent_pid: u32, parent_fd: u32 },
    /// Agent's stdin is a pty — wrong deployment mode for pidfd inject.
    /// Caller should fall back to `tmux send-keys`.
    WrongMode { stdin_target: String },
    /// Couldn't read /proc/AGENT_PID — process gone or perms.
    AgentUnreadable { reason: String },
    /// Couldn't find the parent-side socketpair fd.
    ParentFdNotFound { agent_pid: u32, expected_inode: u64 },
    /// pidfd_open / pidfd_getfd / write failed.
    SyscallFailed { stage: &'static str, errno: i32, msg: String },
}

/// Find the agent's stdin socket inode from /proc/PID/fd/0.
///
/// Returns `Some(inode)` when the fd 0 link resolves to `socket:[N]`,
/// `None` when it's a pty (terminal mode), regular file, or unreadable.
///
/// `#[allow(dead_code)]`: exposed for downstream callers (and future
/// daemon integration) that want the agent inode without running the
/// full probe — `probe()` inlines this logic to keep the I/O path
/// contiguous. Tested via `parse_socket_inode` (separately) and via
/// `probe()` integration paths.
#[allow(dead_code)]
pub fn agent_stdin_socket_inode(pid: u32) -> Result<u64, String> {
    let path = format!("/proc/{}/fd/0", pid);
    let target = fs::read_link(&path).map_err(|e| format!("read_link({}): {}", path, e))?;
    let s = target.to_string_lossy().into_owned();
    parse_socket_inode(&s).ok_or_else(|| format!("fd 0 is {}, not a socket", s))
}

/// Parse `socket:[12345]` to `12345`. Returns None for `pipe:[...]`,
/// `/dev/pts/...`, regular paths, or malformed strings.
pub fn parse_socket_inode(target: &str) -> Option<u64> {
    let rest = target.strip_prefix("socket:[")?;
    let inode_str = rest.strip_suffix(']')?;
    inode_str.parse::<u64>().ok()
}

/// Get a process's parent PID (Linux /proc/PID/status PPid: line).
pub fn parent_pid(pid: u32) -> Result<u32, String> {
    let path = format!("/proc/{}/status", pid);
    let content = fs::read_to_string(&path).map_err(|e| format!("read({}): {}", path, e))?;
    for line in content.lines() {
        if let Some(rest) = line.strip_prefix("PPid:") {
            let v = rest.trim();
            return v
                .parse::<u32>()
                .map_err(|e| format!("parse PPid '{}': {}", v, e));
        }
    }
    Err("no PPid: line".to_string())
}

/// Walk /proc/PARENT_PID/fd/* looking for a socket fd with the target inode.
/// Returns the fd number (as u32) on a hit.
pub fn find_parent_stdin_fd(parent_pid: u32, want_inode: u64) -> Option<u32> {
    let fds_dir = format!("/proc/{}/fd", parent_pid);
    let entries = fs::read_dir(&fds_dir).ok()?;
    for entry in entries.flatten() {
        let fd_name = entry.file_name();
        let fd_str = fd_name.to_string_lossy();
        let fd_num: u32 = match fd_str.parse() {
            Ok(n) => n,
            Err(_) => continue,
        };
        let link = match fs::read_link(entry.path()) {
            Ok(l) => l,
            Err(_) => continue,
        };
        let link_str = link.to_string_lossy();
        if let Some(inode) = parse_socket_inode(&link_str) {
            if inode == want_inode {
                return Some(fd_num);
            }
        }
    }
    None
}

/// Compute the expected parent-side socketpair inode for an agent stdin
/// socket. Linux allocates the two endpoints with consecutive inode numbers;
/// the parent-side end gets the lower inode. (Empirically verified
/// 2026-05-16; see docs/sse-protocol.md.)
pub fn expected_parent_inode(agent_fd0_inode: u64) -> u64 {
    agent_fd0_inode.saturating_sub(1)
}

#[cfg(target_os = "linux")]
mod linux_inject {
    use super::*;
    use std::os::unix::io::AsRawFd;

    /// pidfd_open syscall number on x86_64 / aarch64 (both 434).
    const SYS_PIDFD_OPEN: libc::c_long = 434;
    /// pidfd_getfd syscall number (both 438).
    const SYS_PIDFD_GETFD: libc::c_long = 438;

    fn pidfd_open(pid: libc::pid_t) -> std::io::Result<OwnedFd> {
        // SAFETY: pidfd_open is a syscall with no userspace pointer args.
        let raw = unsafe { libc::syscall(SYS_PIDFD_OPEN, pid, 0i32) as i32 };
        if raw < 0 {
            Err(std::io::Error::last_os_error())
        } else {
            // SAFETY: kernel returned a fresh fd we now own.
            Ok(unsafe { OwnedFd::from_raw_fd(raw) })
        }
    }

    fn pidfd_getfd(pidfd: &OwnedFd, target_fd: libc::c_int) -> std::io::Result<OwnedFd> {
        // SAFETY: pidfd_getfd is a syscall with no userspace pointer args.
        let raw = unsafe {
            libc::syscall(SYS_PIDFD_GETFD, pidfd.as_raw_fd(), target_fd, 0i32) as i32
        };
        if raw < 0 {
            Err(std::io::Error::last_os_error())
        } else {
            // SAFETY: kernel returned a dup'd fd we now own.
            Ok(unsafe { OwnedFd::from_raw_fd(raw) })
        }
    }

    /// libc::write wrapper. Treats partial write as error (we send small
    /// NDJSON lines; if write returns short we want to know).
    fn write_all_libc(fd: &OwnedFd, payload: &[u8]) -> std::io::Result<usize> {
        // SAFETY: fd is valid, payload pointer + len are valid.
        let n = unsafe {
            libc::write(
                fd.as_raw_fd(),
                payload.as_ptr() as *const libc::c_void,
                payload.len(),
            )
        };
        if n < 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(n as usize)
        }
    }

    /// Perform the inject. Best-effort: caller should treat ProbeOutcome::Ok
    /// as "kernel accepted the write", not "agent has acted on the message".
    pub fn inject(
        parent_pid_u: u32,
        parent_fd_u: u32,
        payload: &[u8],
    ) -> Result<usize, (String, i32, String)> {
        let pid = parent_pid_u as libc::pid_t;
        let pidfd = pidfd_open(pid).map_err(|e| (
            "pidfd_open".to_string(),
            e.raw_os_error().unwrap_or(-1),
            e.to_string(),
        ))?;
        let dup_fd = pidfd_getfd(&pidfd, parent_fd_u as libc::c_int).map_err(|e| (
            "pidfd_getfd".to_string(),
            e.raw_os_error().unwrap_or(-1),
            e.to_string(),
        ))?;
        let written = write_all_libc(&dup_fd, payload).map_err(|e| (
            "write".to_string(),
            e.raw_os_error().unwrap_or(-1),
            e.to_string(),
        ))?;
        // OwnedFd Drop closes dup_fd and pidfd here.
        Ok(written)
    }
}

/// Build a `stream-json` user-message line for `claude --input-format stream-json`.
/// Trailing newline included (the SDK protocol is NDJSON).
pub fn build_user_message(text: &str) -> Vec<u8> {
    let msg = serde_json::json!({
        "type": "user",
        "message": {
            "role": "user",
            "content": [{"type": "text", "text": text}],
        },
    });
    let mut s = serde_json::to_string(&msg).expect("json serialize");
    s.push('\n');
    s.into_bytes()
}

/// Run the probe end-to-end. `payload_text` is wrapped in a stream-json user
/// message; pass plain UTF-8.
pub fn probe(agent_pid: u32, payload_text: &str) -> ProbeOutcome {
    // 1. Resolve agent fd 0.
    let fd0_link_path = format!("/proc/{}/fd/0", agent_pid);
    let fd0_target = match fs::read_link(&fd0_link_path) {
        Ok(t) => t.to_string_lossy().into_owned(),
        Err(e) => return ProbeOutcome::AgentUnreadable { reason: e.to_string() },
    };
    let agent_inode = match parse_socket_inode(&fd0_target) {
        Some(i) => i,
        None => return ProbeOutcome::WrongMode { stdin_target: fd0_target },
    };

    // 2. Find parent and its matching socketpair fd.
    let parent_pid_u = match parent_pid(agent_pid) {
        Ok(p) => p,
        Err(e) => return ProbeOutcome::AgentUnreadable { reason: e },
    };
    let want_inode = expected_parent_inode(agent_inode);
    let parent_fd_u = match find_parent_stdin_fd(parent_pid_u, want_inode) {
        Some(fd) => fd,
        None => {
            return ProbeOutcome::ParentFdNotFound {
                agent_pid,
                expected_inode: want_inode,
            }
        }
    };

    // 3. Build payload.
    let payload = build_user_message(payload_text);

    // 4. Inject (Linux only).
    #[cfg(target_os = "linux")]
    {
        match linux_inject::inject(parent_pid_u, parent_fd_u, &payload) {
            Ok(bytes) => ProbeOutcome::Ok {
                bytes,
                parent_pid: parent_pid_u,
                parent_fd: parent_fd_u,
            },
            Err((stage, errno, msg)) => ProbeOutcome::SyscallFailed {
                stage: Box::leak(stage.into_boxed_str()),
                errno,
                msg,
            },
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (parent_pid_u, parent_fd_u, payload);
        ProbeOutcome::SyscallFailed {
            stage: "platform",
            errno: -1,
            msg: "pidfd inject is Linux-only".to_string(),
        }
    }
}

/// Library-callable inject. Wraps `probe` so non-CLI callers (the
/// daemon's `inject_dispatch::inject_to_agent`) can use the IDE-panel
/// inject path without reaching into CLI plumbing.
///
/// Returns the raw `ProbeOutcome` so callers can pattern-match on the
/// failure stages (pidfd_open EPERM → fall through to claude-event,
/// WrongMode → caller mis-classified, etc.). Same error semantics as
/// `probe` — non-Ok variants are non-fatal for the caller; they describe
/// why the inject didn't land.
///
/// Library API stability: this is the supported entry point for daemon
/// integration. `probe` remains exported for the CLI but is documented
/// as a probe (one-shot test), not the daemon path.
pub fn inject(pid: u32, text: &str) -> ProbeOutcome {
    probe(pid, text)
}

/// CLI entry point for `claude-watch inject-probe`.
/// Returns the process exit code.
pub fn cmd_inject_probe(pid: u32, text: &str, json: bool) -> i32 {
    let outcome = probe(pid, text);
    if json {
        let v = serde_json::json!({
            "agent_pid": pid,
            "outcome": &outcome,
        });
        println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
    } else {
        match &outcome {
            ProbeOutcome::Ok { bytes, parent_pid, parent_fd } => {
                println!(
                    "ok: wrote {} bytes via pidfd_getfd(parent_pid={}, parent_fd={})",
                    bytes, parent_pid, parent_fd
                );
                println!("note: agent will process the message on its next event-loop tick");
            }
            ProbeOutcome::WrongMode { stdin_target } => {
                eprintln!(
                    "wrong-mode: agent stdin is {} (terminal mode); use `tmux send-keys` instead",
                    stdin_target
                );
            }
            ProbeOutcome::AgentUnreadable { reason } => {
                eprintln!("agent-unreadable: {}", reason);
            }
            ProbeOutcome::ParentFdNotFound { agent_pid, expected_inode } => {
                eprintln!(
                    "parent-fd-not-found: agent pid {} expected parent socket inode {}",
                    agent_pid, expected_inode
                );
            }
            ProbeOutcome::SyscallFailed { stage, errno, msg } => {
                eprintln!("syscall-failed at {}: errno={} ({})", stage, errno, msg);
            }
        }
    }
    match outcome {
        ProbeOutcome::Ok { .. } => 0,
        ProbeOutcome::WrongMode { .. } => 2,
        _ => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_socket_inode_ok() {
        assert_eq!(parse_socket_inode("socket:[1972860609]"), Some(1972860609));
        assert_eq!(parse_socket_inode("socket:[1]"), Some(1));
    }

    #[test]
    fn test_parse_socket_inode_pipe_is_none() {
        assert_eq!(parse_socket_inode("pipe:[12345]"), None);
        assert_eq!(parse_socket_inode("/dev/pts/1"), None);
        assert_eq!(parse_socket_inode("/dev/null"), None);
        assert_eq!(parse_socket_inode(""), None);
    }

    #[test]
    fn test_parse_socket_inode_malformed() {
        assert_eq!(parse_socket_inode("socket:[abc]"), None);
        assert_eq!(parse_socket_inode("socket:[123"), None);
        assert_eq!(parse_socket_inode("socket:123]"), None);
    }

    #[test]
    fn test_expected_parent_inode_basic() {
        assert_eq!(expected_parent_inode(1972860851), 1972860850);
        assert_eq!(expected_parent_inode(1), 0);
    }

    #[test]
    fn test_expected_parent_inode_zero_saturates() {
        // Saturating sub guards the (theoretical) zero-inode case so we
        // don't underflow to u64::MAX and then chase a nonsense inode in
        // /proc.
        assert_eq!(expected_parent_inode(0), 0);
    }

    #[test]
    fn test_build_user_message_shape() {
        let bytes = build_user_message("hello world");
        let s = std::str::from_utf8(&bytes).unwrap();
        assert!(s.ends_with('\n'), "NDJSON requires trailing newline");
        let trimmed = s.trim_end();
        let v: serde_json::Value = serde_json::from_str(trimmed).expect("valid json");
        assert_eq!(v["type"], "user");
        assert_eq!(v["message"]["role"], "user");
        assert_eq!(v["message"]["content"][0]["type"], "text");
        assert_eq!(v["message"]["content"][0]["text"], "hello world");
    }

    #[test]
    fn test_build_user_message_escapes_quotes() {
        let bytes = build_user_message("hi \"there\" \\backslash");
        let s = std::str::from_utf8(&bytes).unwrap();
        let trimmed = s.trim_end();
        let v: serde_json::Value = serde_json::from_str(trimmed).expect("must round-trip");
        // The whole point: serde escapes for us so payloads with quotes
        // don't break the NDJSON line.
        assert_eq!(v["message"]["content"][0]["text"], "hi \"there\" \\backslash");
    }

    #[test]
    fn test_probe_bogus_pid_returns_agent_unreadable() {
        let out = probe(99_999_999, "test");
        match out {
            ProbeOutcome::AgentUnreadable { .. } => {}
            other => panic!("expected AgentUnreadable for bogus pid, got {:?}", other),
        }
    }

    #[test]
    fn test_probe_self_returns_wrong_mode_or_unreadable() {
        // /proc/self/fd/0 in a test binary is almost certainly a pty or
        // pipe — neither is a unix socket — so probe should bail with
        // WrongMode without attempting any syscall. (Or AgentUnreadable
        // if perms are weird; either way, never Ok.)
        let pid = std::process::id();
        let out = probe(pid, "test");
        assert!(
            !matches!(out, ProbeOutcome::Ok { .. }),
            "test process must not be Ok (no live panel agent here); got {:?}",
            out
        );
    }
}
