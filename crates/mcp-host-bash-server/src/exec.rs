//! Tool execution: `run_command`, `run_script`, `show_security_rules`.
//!
//! All three run against the [`Policy`] resolved at startup. Every path is
//! bounded by `command_timeout`; output is captured (stdout+stderr merged) and
//! returned as MCP text content. Nothing here forwards to an upstream process —
//! that is the whole point of the redesign: `tools/list` and `tools/call` are
//! answered in-process, so there is no proxy hop to wedge.

use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

use crate::config::Policy;

/// Result of a tool invocation, mapped to MCP `{content, isError}` by the caller.
pub struct ToolOutput {
    pub text: String,
    pub is_error: bool,
}

impl ToolOutput {
    fn err(text: impl Into<String>) -> Self {
        ToolOutput { text: text.into(), is_error: true }
    }
    fn ok(text: impl Into<String>) -> Self {
        ToolOutput { text: text.into(), is_error: false }
    }
}

/// Shell metacharacters that mean "this needs a shell". Used to reject
/// shell-y command strings when `allow_shell_operators` is false.
fn contains_shell_operator(s: &str) -> bool {
    // Mirrors the operators cli-mcp-server split on, plus command/process
    // substitution and newlines.
    const OPS: [&str; 11] = ["|", "&", ";", "<", ">", "`", "$(", "${", "\n", "(", ")"];
    OPS.iter().any(|op| s.contains(op))
}

/// Minimal quote-aware tokenizer for the no-shell path. Handles single and
/// double quotes and backslash escaping outside quotes. This path only runs
/// when `allow_shell_operators` is false (conservative profile), where inputs
/// are simple `prog arg arg` forms; it is not a full POSIX word splitter.
fn tokenize(s: &str) -> Result<Vec<String>, String> {
    let mut tokens = Vec::new();
    let mut cur = String::new();
    let mut in_single = false;
    let mut in_double = false;
    let mut has_token = false;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' if !in_double => {
                in_single = !in_single;
                has_token = true;
            }
            '"' if !in_single => {
                in_double = !in_double;
                has_token = true;
            }
            '\\' if !in_single => {
                if let Some(&next) = chars.peek() {
                    cur.push(next);
                    chars.next();
                    has_token = true;
                }
            }
            c if c.is_whitespace() && !in_single && !in_double => {
                if has_token {
                    tokens.push(std::mem::take(&mut cur));
                    has_token = false;
                }
            }
            c => {
                cur.push(c);
                has_token = true;
            }
        }
    }
    if in_single || in_double {
        return Err("unterminated quote in command".to_string());
    }
    if has_token {
        tokens.push(cur);
    }
    Ok(tokens)
}

/// Basename of a program spec, for allow-list checks (e.g. `/bin/ls` -> `ls`).
fn basename(prog: &str) -> &str {
    prog.rsplit('/').next().unwrap_or(prog)
}

async fn run_with_timeout(mut cmd: Command, timeout: u64, stdin_data: Option<&str>) -> ToolOutput {
    cmd.stdin(if stdin_data.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return ToolOutput::err(format!("failed to spawn: {e}")),
    };

    if let Some(data) = stdin_data {
        if let Some(mut stdin) = child.stdin.take() {
            let data = data.to_string();
            // Best-effort feed; ignore broken-pipe if the child exits early.
            let _ = stdin.write_all(data.as_bytes()).await;
            let _ = stdin.shutdown().await;
        }
    }

    // Drain stdout/stderr concurrently so a child that produces more than a
    // pipe buffer's worth of output doesn't deadlock waiting for us to read.
    // `child.wait()` only borrows the child, so we retain a kill handle for
    // the timeout path (unlike `wait_with_output`, which consumes it).
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    let stdout_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        if let Some(s) = stdout.as_mut() {
            let _ = s.read_to_end(&mut buf).await;
        }
        buf
    });
    let stderr_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        if let Some(s) = stderr.as_mut() {
            let _ = s.read_to_end(&mut buf).await;
        }
        buf
    });

    match tokio::time::timeout(Duration::from_secs(timeout), child.wait()).await {
        Ok(Ok(status)) => {
            let stdout_buf = stdout_task.await.unwrap_or_default();
            let stderr_buf = stderr_task.await.unwrap_or_default();
            let mut text = String::new();
            text.push_str(&String::from_utf8_lossy(&stdout_buf));
            let stderr = String::from_utf8_lossy(&stderr_buf);
            if !stderr.is_empty() {
                if !text.is_empty() && !text.ends_with('\n') {
                    text.push('\n');
                }
                text.push_str(&stderr);
            }
            let is_error = !status.success();
            if is_error {
                let suffix = match status.code() {
                    Some(c) => format!("\n[exit code {c}]"),
                    None => "\n[terminated by signal]".to_string(),
                };
                text.push_str(&suffix);
            }
            ToolOutput { text, is_error }
        }
        Ok(Err(e)) => ToolOutput::err(format!("wait failed: {e}")),
        Err(_) => {
            // Timed out — kill the child so we don't leak it, then reap.
            let _ = child.start_kill();
            let _ = child.wait().await;
            stdout_task.abort();
            stderr_task.abort();
            ToolOutput::err(format!("command timed out after {timeout}s"))
        }
    }
}

/// `run_command(command)` — execute a command string on the host.
///
/// With `allow_shell_operators=true` the string is handed to `bash -c` verbatim
/// (redirects/pipes/`&&`/`2>&1` all work — no fragment re-tokenization, which is
/// the bug that forced `ALLOWED_COMMANDS=all` under cli-mcp-server). With it
/// false, shell metacharacters are rejected and the first token must be an
/// allow-listed binary, exec'd directly with no shell.
pub async fn run_command(policy: &Policy, command: &str) -> ToolOutput {
    if command.trim().is_empty() {
        return ToolOutput::err("run_command: 'command' is required");
    }
    if command.len() > policy.max_command_length {
        return ToolOutput::err(format!(
            "run_command: command exceeds MAX_COMMAND_LENGTH ({} > {})",
            command.len(),
            policy.max_command_length
        ));
    }

    if policy.allow_shell_operators {
        // First-token allow-list check (best-effort; moot under allow-all).
        if !policy.allow_all_commands {
            let first = command.split_whitespace().next().unwrap_or("");
            if !policy.command_allowed(basename(first)) {
                return ToolOutput::err(format!(
                    "run_command: '{}' is not in the allow-list",
                    basename(first)
                ));
            }
        }
        let mut cmd = Command::new("bash");
        cmd.arg("-c").arg(command).current_dir(&policy.allowed_dir);
        return run_with_timeout(cmd, policy.command_timeout, None).await;
    }

    // No-shell path: reject shell-y strings, tokenize, exec whitelisted binary.
    if contains_shell_operator(command) {
        return ToolOutput::err(
            "run_command: shell operators are disabled (ALLOW_SHELL_OPERATORS=false). \
             Use run_script for scripts, or enable shell operators.",
        );
    }
    let tokens = match tokenize(command) {
        Ok(t) => t,
        Err(e) => return ToolOutput::err(format!("run_command: {e}")),
    };
    let Some((prog, args)) = tokens.split_first() else {
        return ToolOutput::err("run_command: empty command");
    };
    if !policy.command_allowed(basename(prog)) {
        return ToolOutput::err(format!(
            "run_command: '{}' is not in the allow-list",
            basename(prog)
        ));
    }
    let mut cmd = Command::new(prog);
    cmd.args(args).current_dir(&policy.allowed_dir);
    run_with_timeout(cmd, policy.command_timeout, None).await
}

/// `run_script(interpreter, script)` — feed `script` to `interpreter` on STDIN.
///
/// The interpreter must be a bare basename on the allow-list and resolvable on
/// PATH. Because the body arrives on STDIN it is never tokenized, so arbitrary
/// shell/redirect/heredoc content is safe regardless of `allow_shell_operators`.
pub async fn run_script(policy: &Policy, interpreter: &str, script: &str) -> ToolOutput {
    if interpreter.is_empty() {
        return ToolOutput::err("run_script: 'interpreter' is required");
    }
    if interpreter.contains('/') {
        return ToolOutput::err(format!(
            "run_script: interpreter must be a bare basename, got {interpreter:?}"
        ));
    }
    if !policy.command_allowed(interpreter) {
        return ToolOutput::err(format!(
            "run_script: interpreter {interpreter:?} is not in the allow-list"
        ));
    }
    if script.len() > policy.max_command_length {
        return ToolOutput::err(format!(
            "run_script: script exceeds MAX_COMMAND_LENGTH ({} > {})",
            script.len(),
            policy.max_command_length
        ));
    }
    let mut cmd = Command::new(interpreter);
    cmd.current_dir(&policy.allowed_dir);
    run_with_timeout(cmd, policy.command_timeout, Some(script)).await
}

/// `show_security_rules()` — report the effective policy.
pub fn show_security_rules(policy: &Policy) -> ToolOutput {
    ToolOutput::ok(policy.describe())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenize_handles_quotes() {
        assert_eq!(tokenize("ls -la /tmp").unwrap(), vec!["ls", "-la", "/tmp"]);
        assert_eq!(
            tokenize("echo 'a b' \"c d\"").unwrap(),
            vec!["echo", "a b", "c d"]
        );
        assert!(tokenize("echo 'unterminated").is_err());
    }

    #[test]
    fn shell_operator_detection() {
        assert!(contains_shell_operator("a | b"));
        assert!(contains_shell_operator("a && b"));
        assert!(contains_shell_operator("cmd 2>&1"));
        assert!(contains_shell_operator("echo $(whoami)"));
        assert!(!contains_shell_operator("ls -la /tmp"));
    }

    fn trusted_policy() -> Policy {
        Policy {
            allow_all_commands: true,
            allowed_commands: Default::default(),
            allowed_flags_all: true,
            allowed_flags: Default::default(),
            allowed_dir: "/".to_string(),
            command_timeout: 10,
            max_command_length: 8192,
            allow_shell_operators: true,
            profile: "test".to_string(),
            profile_warning: None,
        }
    }

    #[tokio::test]
    async fn run_command_shell_mode_handles_redirects() {
        let out = run_command(&trusted_policy(), "echo hi 2>&1").await;
        assert!(!out.is_error, "got error: {}", out.text);
        assert!(out.text.contains("hi"));
    }

    #[tokio::test]
    async fn run_command_times_out() {
        let mut p = trusted_policy();
        p.command_timeout = 1;
        let out = run_command(&p, "sleep 5").await;
        assert!(out.is_error);
        assert!(out.text.contains("timed out"));
    }

    #[tokio::test]
    async fn run_script_feeds_stdin() {
        let out = run_script(&trusted_policy(), "bash", "echo from-stdin; echo two").await;
        assert!(!out.is_error, "got error: {}", out.text);
        assert!(out.text.contains("from-stdin"));
        assert!(out.text.contains("two"));
    }

    #[tokio::test]
    async fn run_command_no_shell_rejects_operators() {
        let mut p = trusted_policy();
        p.allow_shell_operators = false;
        let out = run_command(&p, "echo hi | cat").await;
        assert!(out.is_error);
        assert!(out.text.contains("shell operators are disabled"));
    }

    #[tokio::test]
    async fn run_script_rejects_non_allowlisted_interpreter() {
        let mut p = trusted_policy();
        p.allow_all_commands = false;
        p.allowed_commands = ["bash".to_string()].into_iter().collect();
        let out = run_script(&p, "perl", "print 1").await;
        assert!(out.is_error);
        assert!(out.text.contains("not in the allow-list"));
    }
}
