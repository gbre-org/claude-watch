//! E2e tests for the operator-typing guard on the daemon's fire-and-forget
//! injectors (`inject_text` / `inject_text_queued`).
//!
//! The bug this guards against: a human attaching to the pane (classically,
//! right at container boot) can be mid-keystroke in the input box when the
//! daemon's automatic interruption fires. `inject_text`'s Escape/`dd`/type
//! choreography (and `inject_text_queued`'s bare type) previously ran BLIND
//! into whatever was already on the prompt line, splicing the daemon's
//! payload into the middle of the operator's own typing — the garbled
//! "❯ ddYou are a fresh session..." shape reported in the field. Both
//! injectors now check the prompt line FIRST and skip entirely (no
//! keystrokes sent at all) when it already holds unsubmitted text.

use claude_watch::tmux::{inject_text, inject_text_queued};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

static TEST_COUNTER: AtomicU32 = AtomicU32::new(0);

struct TmuxSession {
    name: String,
}

impl TmuxSession {
    fn new(name: &str) -> Self {
        TmuxSession {
            name: name.to_string(),
        }
    }
}

impl Drop for TmuxSession {
    fn drop(&mut self) {
        let _ = Command::new("tmux")
            .args(["kill-session", "-t", &self.name])
            .output();
    }
}

fn unique_session_name(prefix: &str) -> String {
    let n = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
    format!("cw-typing-guard-{}-{}-{}", prefix, std::process::id(), n)
}

fn capture_pane(session: &str) -> String {
    let output = Command::new("tmux")
        .args(["capture-pane", "-t", session, "-p"])
        .output()
        .expect("capture-pane");
    String::from_utf8_lossy(&output.stdout).to_string()
}

fn send_literal(session: &str, text: &str) {
    let _ = Command::new("tmux")
        .args(["send-keys", "-t", session, "-l", text])
        .output();
}

fn send_keys(session: &str, keys: &[&str]) {
    let mut args = vec!["send-keys", "-t", session];
    args.extend_from_slice(keys);
    let _ = Command::new("tmux").args(&args).output();
}

/// Render a static idle-prompt pane whose input line is `❯ <prompt_line>`
/// (empty string for a bare prompt), with a `sleep` keeping the pane alive
/// so nothing clears the line out from under the assertion.
fn render_idle_prompt(session: &str, prompt_line: &str) {
    let script = format!(
        r#"
clear
printf '● Some prior output\n'
printf '\n'
printf '──────────────────────────────────────────────────────────────────\n'
printf '❯ {}\n'
printf '──────────────────────────────────────────────────────────────────\n'
printf '  -- INSERT -- 50000 tokens\n'
sleep 30
"#,
        prompt_line
    );
    let script_path = format!(
        "/tmp/cw-typing-guard-test-{}-{}.sh",
        std::process::id(),
        session
    );
    std::fs::write(&script_path, script).expect("write script");
    let _ = Command::new("chmod").args(["+x", &script_path]).output();
    send_literal(session, &format!("bash {}", script_path));
    send_keys(session, &["Enter"]);

    // Poll until the script has actually rendered its prompt line, rather
    // than a fixed sleep-and-hope -- a fixed delay is a race against however
    // long the shell takes to echo the command and exec bash, and capturing
    // "before" while the pane still shows the un-executed command line (not
    // yet the rendered ❯ prompt) makes the whole scenario a no-op: the guard
    // sees no ❯ line at all and correctly does nothing, but for the wrong
    // reason.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if capture_pane(session).contains('❯') || std::time::Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    // Small settle margin after the marker appears.
    std::thread::sleep(Duration::from_millis(150));
}

fn new_session(name: &str) {
    let status = Command::new("tmux")
        .args(["new-session", "-d", "-s", name, "-x", "120", "-y", "40"])
        .status()
        .expect("create tmux session");
    assert!(status.success());
    std::thread::sleep(Duration::from_millis(500));
}

#[test]
fn inject_text_skips_when_operator_has_unsubmitted_text() {
    let session_name = unique_session_name("cancel");
    let _guard = TmuxSession::new(&session_name);
    new_session(&session_name);
    render_idle_prompt(&session_name, "half-typed operator input");

    let before = capture_pane(&session_name);
    let pane = format!("{}:0.0", session_name);
    let start = std::time::Instant::now();
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        inject_text(&pane, "[CLAUDE-WATCH] DAEMON PAYLOAD").await;
    });
    let elapsed = start.elapsed();

    // The guard returns before any keystroke choreography (no Escape-wait,
    // no dd, no typing loop), so it must be fast -- a real inject_text run
    // (see e2e_inject_timing.rs) takes multiple seconds.
    assert!(
        elapsed < Duration::from_secs(1),
        "inject_text should have short-circuited on unsubmitted text, but took {:?}",
        elapsed
    );

    let after = capture_pane(&session_name);
    assert_eq!(
        before, after,
        "inject_text must not touch the pane at all when the operator has \
         unsubmitted text on the prompt line. before:\n{}\nafter:\n{}",
        before, after
    );
    assert!(
        !after.contains("DAEMON PAYLOAD"),
        "the daemon payload must not have been typed into the pane. Pane:\n{}",
        after
    );
    assert!(
        after.contains("half-typed operator input"),
        "the operator's own unsubmitted text must survive untouched. Pane:\n{}",
        after
    );
}

#[test]
fn inject_text_queued_skips_when_operator_has_unsubmitted_text() {
    let session_name = unique_session_name("queued-cancel");
    let _guard = TmuxSession::new(&session_name);
    new_session(&session_name);
    render_idle_prompt(&session_name, "/config theme=light");

    let before = capture_pane(&session_name);
    let pane = format!("{}:0.0", session_name);
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        inject_text_queued(&pane, "[CLAUDE-WATCH] WATCHER DOWN").await;
    });

    let after = capture_pane(&session_name);
    assert_eq!(
        before, after,
        "inject_text_queued must not touch the pane at all when the operator \
         has unsubmitted text on the prompt line. before:\n{}\nafter:\n{}",
        before, after
    );
    // This is the exact real-world splice this guard prevents (see
    // src/tmux.rs send_focus_main_keys doc comment): the daemon payload
    // gluing onto the operator's half-typed slash command.
    assert!(
        !after.contains("WATCHER DOWN"),
        "the daemon payload must not have glued onto the operator's typed \
         text. Pane:\n{}",
        after
    );
}

#[test]
fn inject_text_queued_proceeds_on_a_bare_idle_prompt() {
    let session_name = unique_session_name("queued-proceed");
    let _guard = TmuxSession::new(&session_name);
    new_session(&session_name);
    render_idle_prompt(&session_name, "");

    let pane = format!("{}:0.0", session_name);
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        inject_text_queued(&pane, "TEST-PROCEEDS-OK").await;
    });

    std::thread::sleep(Duration::from_millis(300));
    let after = capture_pane(&session_name);
    assert!(
        after.contains("TEST-PROCEEDS-OK"),
        "a genuinely bare idle prompt must still receive the inject \
         (no regression from the new guard). Pane:\n{}",
        after
    );
}
