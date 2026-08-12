//! Regression tests for how `claude-watch agent list` (the `agent-ctl`
//! multi-call name) decides WHICH session's agents it is reporting.
//!
//! Incident being pinned down: agent transcripts live under
//! `~/.claude/projects/<slug>/<session-uuid>/subagents/`, while Claude
//! Code's task output lives under
//! `/tmp/claude-<uid>/<slug>/<session-uuid>/tasks/`. Those two UUIDs
//! diverge the moment the session's context is reset — task output keeps
//! landing under the pre-reset directory while new transcripts are
//! written under a brand-new session UUID. Resolving the transcript
//! directory by taking the task directory's UUID therefore reads the
//! PREVIOUS session's agents: a long list of finished agents, every row
//! "no child process", while the agents that are actually running are in
//! a directory that resolution never visits. The output was not merely
//! stale, it was indistinguishable from a correct one.
//!
//! These tests drive the built binary with a synthetic `$HOME` that
//! reproduces that exact layout.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, SystemTime};

const OLD_SESSION: &str = "11111111-1111-4111-8111-111111111111";
const NEW_SESSION: &str = "22222222-2222-4222-8222-222222222222";
const OLD_AGENT: &str = "aoldagent00000001";
const NEW_AGENT: &str = "anewagent00000002";

fn binary() -> PathBuf {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let status = Command::new("cargo")
        .args(["build"])
        .current_dir(manifest_dir)
        .status()
        .expect("cargo build");
    assert!(status.success(), "cargo build failed");
    PathBuf::from(format!("{}/target/debug/claude-watch", manifest_dir))
}

fn scoped_root(name: &str) -> PathBuf {
    let p = PathBuf::from(format!(
        "/tmp/claude-watch-agent-list-test-{}-{}",
        name,
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&p);
    fs::create_dir_all(&p).unwrap();
    p
}

/// Project-slug form of a path: `/` replaced by `-`, matching Claude
/// Code's own per-project directory naming.
fn slug(path: &Path) -> String {
    path.to_string_lossy()
        .chars()
        .map(|c| if c == '/' { '-' } else { c })
        .collect()
}

fn write_with_mtime(path: &Path, content: &str, age: Duration) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
    let f = fs::OpenOptions::new().write(true).open(path).unwrap();
    f.set_modified(SystemTime::now() - age).unwrap();
}

/// Minimal transcript line carrying one Bash tool call, so the listing
/// has a "Last command" to print.
fn transcript(cmd: &str) -> String {
    format!(
        r#"{{"message":{{"content":[{{"name":"Bash","input":{{"command":"{}"}}}}]}}}}"#,
        cmd
    ) + "\n"
}

fn meta(description: &str, agent_type: &str) -> String {
    format!(
        r#"{{"description":"{}","agentType":"{}"}}"#,
        description, agent_type
    )
}

/// Write one agent (transcript + spawn metadata) into a session's
/// `subagents/` dir, backdated by `age`.
fn write_agent(home: &Path, session: &str, agent_id: &str, description: &str, age: Duration) {
    let dir = home
        .join(".claude/projects")
        .join(slug(home))
        .join(session)
        .join("subagents");
    write_with_mtime(
        &dir.join(format!("agent-{}.jsonl", agent_id)),
        &transcript(&format!("echo {}", agent_id)),
        age,
    );
    write_with_mtime(
        &dir.join(format!("agent-{}.meta.json", agent_id)),
        &meta(description, "general-purpose"),
        age,
    );
}

/// Recreate the task-output tree that the old resolver keyed off: a
/// FRESH `.output` file under the OLD session's UUID. This is the trap —
/// the freshest task output belongs to the pre-reset session id.
fn write_stale_session_trap(home: &Path) {
    let tasks = PathBuf::from("/tmp/claude-1000")
        .join(slug(home))
        .join(OLD_SESSION)
        .join("tasks");
    // Best effort: on a runner where /tmp/claude-1000 is not creatable
    // the trap simply doesn't exist, and the assertions below still
    // describe the behaviour we require.
    if fs::create_dir_all(&tasks).is_ok() {
        let _ = fs::write(tasks.join("fresh.output"), "task output\n");
    }
}

/// Stand up a fake Claude Code process so `find_claude_pid` resolves:
/// it matches on `/proc/<pid>/exe` starting with
/// `$HOME/.local/share/claude/versions/`, so a copy of `sleep` parked
/// there and executed is indistinguishable from the real thing.
fn spawn_fake_claude(home: &Path) -> Child {
    let dir = home.join(".local/share/claude/versions/1.0.0");
    fs::create_dir_all(&dir).unwrap();
    let exe = dir.join("claude");
    fs::copy("/bin/sleep", &exe).expect("copy sleep");
    Command::new(&exe)
        .arg("120")
        .spawn()
        .expect("spawn fake claude")
}

struct Fixture {
    home: PathBuf,
    claude: Child,
}

impl Fixture {
    fn run(&self, args: &[&str]) -> std::process::Output {
        Command::new(binary())
            .arg("agent")
            .args(args)
            .env("HOME", &self.home)
            .output()
            .expect("run agent list")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.claude.kill();
        let _ = self.claude.wait();
        let _ = fs::remove_dir_all(&self.home);
        let _ = fs::remove_dir_all(PathBuf::from("/tmp/claude-1000").join(slug(&self.home)));
    }
}

/// Two sessions: an OLD one (agents finished 8h ago, and the one the
/// task-output tree points at) and a NEW post-context-reset one holding
/// an agent whose transcript was written seconds ago.
fn fixture(name: &str) -> Fixture {
    let home = scoped_root(name);
    write_agent(
        &home,
        OLD_SESSION,
        OLD_AGENT,
        "finished long ago",
        Duration::from_secs(8 * 3600),
    );
    write_agent(
        &home,
        NEW_SESSION,
        NEW_AGENT,
        "agent mid-work",
        Duration::from_secs(5),
    );
    write_stale_session_trap(&home);
    let claude = spawn_fake_claude(&home);
    Fixture { home, claude }
}

#[test]
fn list_reports_the_live_post_context_reset_agent() {
    let fx = fixture("live");
    let out = fx.run(&["list"]);
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();

    assert!(
        out.status.success(),
        "agent list should succeed; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // The agent that is actually running must be listed...
    assert!(
        stdout.contains(NEW_AGENT),
        "live agent {} missing from listing:\n{}",
        NEW_AGENT,
        stdout
    );
    // ...and must read as ALIVE. It has no OS child process (in-process
    // agents only spawn one while a tool call is in flight), which is
    // precisely the case that used to render as "no child process".
    assert!(
        stdout.contains("LIVE (in-process, no child)"),
        "live agent not marked live:\n{}",
        stdout
    );
    assert!(
        stdout.contains("agent mid-work"),
        "live agent's spawn metadata missing:\n{}",
        stdout
    );
    // The header must say which session the row came from, so a wrong
    // answer is auditable instead of merely plausible.
    assert!(
        stdout.contains(NEW_SESSION),
        "listing does not name the session it resolved:\n{}",
        stdout
    );
    assert!(
        stdout.contains("Subagent transcript dirs scanned:"),
        "listing does not report its own provenance:\n{}",
        stdout
    );
    // A finished agent from the pre-reset session must not be presented
    // alongside it as if it were current.
    assert!(
        !stdout.contains(OLD_AGENT),
        "stale agent shown in default listing:\n{}",
        stdout
    );
    assert!(
        stdout.contains("hidden, use --all to show"),
        "hidden stale agents not accounted for:\n{}",
        stdout
    );
}

#[test]
fn list_all_shows_the_older_sessions_agents_marked_idle() {
    let fx = fixture("all");
    let out = fx.run(&["list", "--all"]);
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();

    assert!(out.status.success());
    assert!(
        stdout.contains(OLD_AGENT),
        "--all should still show finished agents:\n{}",
        stdout
    );
    assert!(
        stdout.contains("idle — no child process"),
        "finished agent should be labelled idle:\n{}",
        stdout
    );
    // Newest-first ordering: what is running now comes before history.
    let new_at = stdout.find(NEW_AGENT).expect("live agent listed");
    let old_at = stdout.find(OLD_AGENT).expect("old agent listed");
    assert!(
        new_at < old_at,
        "live agent should sort above the finished one:\n{}",
        stdout
    );
}

#[test]
fn list_fails_loudly_when_no_session_can_be_resolved() {
    // A HOME with a running Claude Code process but no transcript tree
    // at all: the tool must refuse rather than print a confident table.
    let home = scoped_root("unresolved");
    let claude = spawn_fake_claude(&home);
    let fx = Fixture { home, claude };
    let out = fx.run(&["list"]);

    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    assert_eq!(
        out.status.code(),
        Some(2),
        "unresolvable session must exit non-zero (2); stdout: {} stderr: {}",
        stdout,
        stderr
    );
    assert!(
        stderr.contains("Refusing to print an agent list"),
        "failure must be explained on stderr, got: {}",
        stderr
    );
    assert!(
        !stdout.contains("=== Agents"),
        "no agent table may be printed when the session is unknown: {}",
        stdout
    );
}
