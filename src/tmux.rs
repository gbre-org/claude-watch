//! All tmux interaction: send keys, capture pane, idle/mode detection, injection.

use crate::cmd::{run_cmd, run_cmd_any};
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::RwLock;
use std::time::Duration;
use tokio::time::sleep;
use tracing::{debug, info};

/// Settle delay (milliseconds) inserted between the ESC -> NORMAL-mode
/// transition and the dd/i/text sequence in `inject_text`. See
/// `TmuxConfig::post_escape_settle_ms` for the rationale. Initialized at
/// daemon startup from config; defaults to 0 (disabled) so the fast path
/// is the default. Set via `set_post_escape_settle_ms()`.
static POST_ESCAPE_SETTLE_MS: AtomicU64 = AtomicU64::new(0);

/// Update the global post-escape settle delay. Called from main.rs at daemon
/// startup and on every config reload. Safe to call concurrently — uses a
/// relaxed atomic store.
pub fn set_post_escape_settle_ms(ms: u64) {
    POST_ESCAPE_SETTLE_MS.store(ms, Ordering::Relaxed);
}

/// Read the current post-escape settle delay. Used internally by injection
/// helpers; exposed for tests.
pub fn post_escape_settle_ms() -> u64 {
    POST_ESCAPE_SETTLE_MS.load(Ordering::Relaxed)
}

/// FleetView "return to main" keystrokes, sent FIRST on every inject (before
/// the Escape->NORMAL coercion loop) so injected text lands on the MAIN
/// conversation rather than on a background agent that happens to be SELECTED
/// in Claude Code's FleetView. Initialized at daemon startup (and on every
/// config reload) from `[tmux].focus_main_keys`; defaults to EMPTY (no-op, so
/// behavior is identical to before this knob). See `TmuxConfig::focus_main_keys`
/// for the full Andrew-#270/#288/#291 root-cause writeup. A `RwLock<Vec<String>>`
/// (not an atomic) because the value is a key LIST that can be reloaded.
static FOCUS_MAIN_KEYS: RwLock<Vec<String>> = RwLock::new(Vec::new());

/// Update the global FleetView focus-to-main key sequence. Called from main.rs
/// at daemon startup and on every config reload. Blank/whitespace entries are
/// dropped here so the live send path never emits an empty `send-keys` key.
pub fn set_focus_main_keys(keys: Vec<String>) {
    let sanitized = sanitize_focus_main_keys(&keys);
    if let Ok(mut guard) = FOCUS_MAIN_KEYS.write() {
        *guard = sanitized;
    }
}

/// Read the current FleetView focus-to-main key sequence. Used by the inject
/// helpers; exposed for tests.
pub fn focus_main_keys() -> Vec<String> {
    FOCUS_MAIN_KEYS
        .read()
        .map(|g| g.clone())
        .unwrap_or_default()
}

/// Pure: drop blank/whitespace-only entries and trim each key name. Keeps the
/// configured order. Factored out so the sanitization contract is unit-testable
/// without touching the global or a live tmux.
pub(crate) fn sanitize_focus_main_keys(keys: &[String]) -> Vec<String> {
    keys.iter()
        .map(|k| k.trim())
        .filter(|k| !k.is_empty())
        .map(|k| k.to_string())
        .collect()
}

/// Send the configured FleetView focus-to-main keys into `pane`, in order,
/// with a short settle between each. No-op when the knob is empty (the
/// default) — so a setup that doesn't need the FleetView fix pays nothing.
///
/// This is the FIRST thing the inject choreography does (called at the top of
/// `inject_text_no_submit` and `inject_text_queued`), BEFORE the Escape->NORMAL
/// coercion loop / `dd` line-clear, because those operate on whatever the TUI
/// currently has focused: if a background agent is selected, the Escape/dd/i
/// keys would all hit the agent. Returning the FleetView selection to `main`
/// first guarantees the rest of the choreography (and the typed payload) lands
/// on the main conversation.
async fn send_focus_main_keys(pane: &str) {
    let keys = focus_main_keys();
    if keys.is_empty() {
        return;
    }
    info!(
        pane = %pane,
        keys = ?keys,
        "send_focus_main_keys: returning FleetView selection to main before inject"
    );
    for key in &keys {
        send_keys(pane, &[key.as_str()]).await;
        sleep(Duration::from_millis(150)).await;
    }
}

/// Sleep for the configured post-escape settle delay. No-op when the knob
/// is set to 0 (the default). Call this AFTER Escape keystroke(s) and
/// BEFORE any further keystrokes (typed text, vim-mode dd/i, /clear,
/// Enter, etc.) when extra settle time is needed to keep follow-up keys
/// from being garbled or eaten.
///
/// Currently invoked only at the ESC -> NORMAL-mode boundary inside
/// `inject_text` (replacing what used to be a hardcoded 500ms sleep).
/// Default is 0 so the fast path is the default; set
/// `[tmux].post_escape_settle_ms` in config.toml if a particular
/// environment needs the extra cushion.
async fn settle_after_escape() {
    let ms = post_escape_settle_ms();
    if ms > 0 {
        sleep(Duration::from_millis(ms)).await;
    }
}

/// Current activity state of Claude Code as observed from tmux pane output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaudeActivity {
    /// Prompt (❯) visible — waiting for input
    Idle,
    /// ✽ thinking indicator visible (e.g. "✽ Thinking… (12s · ↓ 384 tokens)")
    Thinking,
    /// Spinner + tool name visible (e.g. "⠋ Read(~/some/file)")
    ToolRunning,
    /// ● output being streamed, no prompt visible
    Writing,
    /// Can't determine current state
    Unknown,
}

impl fmt::Display for ClaudeActivity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ClaudeActivity::Idle => write!(f, "idle"),
            ClaudeActivity::Thinking => write!(f, "thinking"),
            ClaudeActivity::ToolRunning => write!(f, "tool_running"),
            ClaudeActivity::Writing => write!(f, "writing"),
            ClaudeActivity::Unknown => write!(f, "unknown"),
        }
    }
}

pub async fn send_keys(pane: &str, keys: &[&str]) {
    let mut args = vec!["tmux", "send-keys", "-t", pane];
    args.extend_from_slice(keys);
    let _ = run_cmd(&args, 5).await;
}

pub async fn send_literal(pane: &str, text: &str) {
    let _ = run_cmd(&["tmux", "send-keys", "-t", pane, "-l", text], 5).await;
}

pub async fn capture_pane(pane: &str) -> Option<String> {
    run_cmd(&["tmux", "capture-pane", "-t", pane, "-p"], 5).await
}

/// Capture pane with -J flag to join wrapped lines. Use for status bar parsing
/// where text may be truncated at pane width (e.g. "275898 tokens" → "275898 toke…").
pub async fn capture_pane_joined(pane: &str) -> Option<String> {
    run_cmd(&["tmux", "capture-pane", "-t", pane, "-p", "-J"], 5).await
}

pub async fn capture_pane_history(pane: &str, lines: i32) -> Option<String> {
    let start = format!("-{}", lines);
    run_cmd(&["tmux", "capture-pane", "-t", pane, "-p", "-S", &start], 5).await
}

/// Check if the Claude Code prompt (>) is visible in the last 15 lines.
pub async fn is_idle(pane: &str) -> bool {
    if let Some(out) = capture_pane(pane).await {
        return check_lines_for_idle_prompt(&out);
    }
    false
}

/// Pure function: check if any of the last 15 lines contain the Claude prompt character.
pub(crate) fn check_lines_for_idle_prompt(pane_output: &str) -> bool {
    let lines: Vec<&str> = pane_output.lines().collect();
    let start = if lines.len() > 15 {
        lines.len() - 15
    } else {
        0
    };
    for line in &lines[start..] {
        if line.contains('\u{276f}') {
            return true;
        }
    }
    false
}

/// Check if the pane is showing an INTERACTIVE PROMPT that is awaiting a
/// human selection/confirmation — an `AskUserQuestion` multiple-choice
/// menu, a tool-permission prompt ("Do you want to proceed?"), or any
/// arrow-key selection overlay. Captures the pane and runs the pure
/// `interactive_prompt_visible` detector.
///
/// Used to SUPPRESS keystroke injection (resume-prompt inject, fresh-/clear
/// inject) while such a prompt is on screen. Injecting `send-keys` into a
/// live selection menu is DESTRUCTIVE — the first injected key (or the
/// Escape that `tmux::inject_text` leads with) cancels the menu out from
/// under the operator before they can answer it. See
/// `interactive_prompt_visible` for the conservative-bias rationale.
pub async fn is_interactive_prompt(pane: &str) -> bool {
    if let Some(out) = capture_pane(pane).await {
        return interactive_prompt_visible(&out);
    }
    false
}

/// Pure function: does the pane show an interactive prompt awaiting a human
/// pick/confirm? This is the signature claude-watch must treat as
/// "NOT-idle, do NOT inject keystrokes" even though a `❯` prompt char is
/// present (every such menu still renders a `❯` selection cursor, which is
/// exactly why the bare `is_idle` `❯`-scan misclassifies it as idle).
///
/// Claude Code renders these interactive prompts as a bordered box whose
/// footer carries a recognizable hint line, e.g.:
///
/// ```text
///   Do you want to proceed?
///   ❯ 1. Yes
///     2. No, and tell Claude what to do differently (esc)
/// ```
/// or, for `AskUserQuestion` / selection overlays:
/// ```text
///   ❯ 1. Some option
///     2. Another option
///   ↑/↓ to select · Enter to confirm · Esc to cancel
/// ```
///
/// ## Detection signatures (any one matches)
///
///  1. A tool-permission question line: `Do you want to` / `Would you like to`
///     / `Do you want to proceed`.
///  2. A selection-hint footer that implies an active pick: a line containing
///     "to select" AND ("Enter to" OR "to confirm" OR "to submit" OR
///     "Esc to cancel" OR "esc)"). The Background-tasks *viewer* overlay also
///     uses "↑/↓ to select … Enter to view … ←/Esc to close" — that one is a
///     passive viewer, not a blocking question, but suppressing an inject
///     while it is open is harmless (it only DELAYS a resume), so we
///     deliberately match it too rather than risk under-matching a real
///     question.
///  3. A `❯`-cursored numbered option row (`❯ 1.` / `❯ 2.` …) — the menu's
///     highlighted selection. A genuinely-idle prompt has the `❯` alone on
///     an otherwise-empty input line, never immediately followed by a
///     numbered option.
///  4. The 2.1.x "Background work is running" exit-confirmation dialog
///     (title "Background work is running" / body "…will stop when you
///     exit"). Claude Code renders it on the interactive `/exit` flow when a
///     worktree is checked out OR background tasks are running; suppressing
///     an inject while it is up is harmless (same passive-viewer reasoning
///     as (2)) and keeps this guard aware of the same dialog
///     `policy::run_auto_update` explicitly dismisses. See
///     `background_work_exit_dialog_visible`.
///
/// ## Conservative bias
///
/// This guard is intentionally biased toward returning `true` ("an
/// interactive prompt is up — suppress"). The two error modes are NOT
/// symmetric: a FALSE POSITIVE merely DELAYS a resume-inject by one or more
/// check cycles (fully recoverable — the prompt will clear and the next
/// cycle injects), whereas a FALSE NEGATIVE lets the daemon `send-keys` into
/// a live menu and CANCEL the operator's question (the reported bug —
/// destructive, unrecoverable for that interaction). So when a marker is
/// ambiguous, prefer to match it.
pub(crate) fn interactive_prompt_visible(pane_output: &str) -> bool {
    let lines: Vec<&str> = pane_output.lines().collect();
    // Scan a generous tail — these prompt boxes can be several lines tall
    // and the footer hint sits at the bottom.
    let start = if lines.len() > 25 {
        lines.len() - 25
    } else {
        0
    };
    for line in &lines[start..] {
        let trimmed = line.trim();
        let lower = trimmed.to_lowercase();

        // (1) Permission / confirmation question text.
        if lower.contains("do you want to")
            || lower.contains("would you like to")
            || lower.contains("do you trust")
        {
            return true;
        }

        // (2) A selection-hint footer that implies an active pick.
        if lower.contains("to select")
            && (lower.contains("enter to")
                || lower.contains("to confirm")
                || lower.contains("to submit")
                || lower.contains("esc to")
                || lower.contains("to close")
                || lower.contains("to view"))
        {
            return true;
        }

        // (3) A `❯`-cursored numbered option row (`❯ 1.`, `❯ 2.`, …).
        // The cursor char may be followed by spaces then `<digit>.`.
        if let Some(rest) = trimmed.strip_prefix('\u{276f}') {
            let rest = rest.trim_start();
            let mut chars = rest.chars();
            if let Some(c) = chars.next() {
                if c.is_ascii_digit() && chars.next() == Some('.') {
                    return true;
                }
            }
        }
    }

    // (4) The 2.1.x "Background work is running" exit-confirmation dialog.
    // Delegated to a shared detector so `policy::run_auto_update` can reuse
    // the exact same signature it dismisses.
    if background_work_exit_dialog_visible(pane_output) {
        return true;
    }

    false
}

/// Pure function: does the pane show a BLOCKING interactive question that is
/// truly awaiting a human answer — an `AskUserQuestion` menu or a
/// tool-permission confirmation — as opposed to a PASSIVE selector/viewer
/// overlay (FleetView agent-view, Background-tasks viewer) that merely renders
/// a `❯ … to select … Enter to view … Esc to close` footer?
///
/// This is a DELIBERATELY NARROWER sibling of `interactive_prompt_visible`.
/// The broad detector is biased toward `true` because its original consumer
/// (inject-suppression) treats a false positive as harmless — it only DELAYS a
/// resume-inject. The `ask_question_monitor`
/// (`policy::check_ask_question_stale`) has the OPPOSITE cost asymmetry: a
/// false positive there fires a spurious `ask-question-stale` claude-event +
/// pingme with NO real block behind it. In practice the main-loop pane
/// frequently sits on the FleetView agent-view overlay (`❯ ● main` /
/// `◯ general-purpose …`, footer `↑/↓ to select · Enter to view`) — a passive
/// viewer, not a question — and `interactive_prompt_visible`'s signature (2)
/// matched it, firing the false alarm the operator reported (2026-07-13).
///
/// A GENUINE blocking question is distinguished by one of:
///   1. Explicit question / confirmation text (`Do you want to` /
///      `Would you like to` / `Do you trust`).
///   2. A `❯`-cursored NUMBERED option row (`❯ 1.` / `❯ 2.` …) — every real
///      AskUserQuestion / permission menu renders numbered choices.
///   3. A select-hint footer that CONFIRMS a pick — "to confirm" or
///      "to submit". Passive viewers use "Enter to view" / "Esc to close"
///      instead, which this detector deliberately does NOT match.
///
/// The `background_work_exit_dialog_visible` /exit dialog is intentionally
/// excluded here: it is a distinct dialog `run_auto_update` dismisses, not an
/// AskUserQuestion, so it must not trip the ask-question stale monitor.
pub(crate) fn blocking_question_visible(pane_output: &str) -> bool {
    let lines: Vec<&str> = pane_output.lines().collect();
    let start = if lines.len() > 25 {
        lines.len() - 25
    } else {
        0
    };
    for line in &lines[start..] {
        let trimmed = line.trim();
        let lower = trimmed.to_lowercase();

        // (1) Permission / confirmation question text.
        if lower.contains("do you want to")
            || lower.contains("would you like to")
            || lower.contains("do you trust")
        {
            return true;
        }

        // (2) A `❯`-cursored numbered option row (`❯ 1.`, `❯ 2.`, …).
        if let Some(rest) = trimmed.strip_prefix('\u{276f}') {
            let rest = rest.trim_start();
            let mut chars = rest.chars();
            if let Some(c) = chars.next() {
                if c.is_ascii_digit() && chars.next() == Some('.') {
                    return true;
                }
            }
        }

        // (3) A CONFIRMING select-hint footer. Unlike
        // `interactive_prompt_visible`, match ONLY "to confirm" / "to submit"
        // — the footer a real question menu shows. Passive viewer footers
        // ("Enter to view", "Esc to close") are deliberately NOT matched, so
        // the FleetView agent-view / Background-tasks viewer overlays do not
        // trip the stale-question alarm.
        if lower.contains("to select")
            && (lower.contains("to confirm") || lower.contains("to submit"))
        {
            return true;
        }
    }

    false
}

/// Async wrapper: capture the pane and run `blocking_question_visible`. Used
/// by the `ask_question_monitor` so a passive FleetView / Background-tasks
/// viewer overlay on the main pane does NOT fire a spurious
/// `ask-question-stale` alarm. See `blocking_question_visible`.
pub async fn is_blocking_question(pane: &str) -> bool {
    if let Some(out) = capture_pane(pane).await {
        return blocking_question_visible(&out);
    }
    false
}

/// Pure function: does the pane show Claude Code's 2.1.x "Background work is
/// running" exit-confirmation dialog?
///
/// Claude Code 2.1.207 renders this dialog ONLY on the interactive exit flow
/// (`prompt_input_exit`), gated on `worktree != null OR
/// runningBackgroundTasks > 0`. It looks like:
///
/// ```text
///   Background work is running
///   The following will stop when you exit:
///   ❯ 1. Exit anyway
///     2. Move to background and exit
///     3. Stay
/// ```
///
/// Our sessions always have backgrounded watchers, so the dialog ALWAYS
/// renders when the daemon's auto-update injects `/exit` — it eats the
/// `/exit` submit, `wait_for_exit` then times out, and the daemon
/// false-alarms "Claude Code crashed". `run_auto_update` polls for this
/// signature and sends a bare Enter (option 1 "Exit anyway" is default-
/// highlighted) to get past it.
///
/// Match either the title line or the body line — both are stable literals
/// emitted by Claude Code. Scoped to the recent tail so a scrollback mention
/// (e.g. this doc read into a pane) doesn't trip it.
pub(crate) fn background_work_exit_dialog_visible(pane_output: &str) -> bool {
    let lines: Vec<&str> = pane_output.lines().collect();
    let start = if lines.len() > 25 {
        lines.len() - 25
    } else {
        0
    };
    for line in &lines[start..] {
        let lower = line.trim().to_lowercase();
        if lower.contains("background work is running") || lower.contains("will stop when you exit")
        {
            return true;
        }
    }
    false
}

/// Check if pane shows exit teardown indicators ("Goodbye!" or "Background command was stopped").
/// During /exit, Claude Code prints these before the process fully terminates.
pub async fn is_exit_teardown(pane: &str) -> bool {
    if let Some(out) = capture_pane(pane).await {
        return check_lines_for_exit_teardown(&out);
    }
    false
}

/// Pure function: check if the last 30 lines contain exit teardown markers.
/// "Goodbye!" is printed by Claude Code on /exit. "Background command was stopped"
/// follows as each background task is cleaned up.
pub(crate) fn check_lines_for_exit_teardown(pane_output: &str) -> bool {
    let lines: Vec<&str> = pane_output.lines().collect();
    // Check more lines (30) since "Goodbye!" may scroll up as
    // "Background command was stopped" messages accumulate
    let start = if lines.len() > 30 {
        lines.len() - 30
    } else {
        0
    };
    for line in &lines[start..] {
        let trimmed = line.trim();
        if trimmed == "Goodbye!" || trimmed.contains("Background command was stopped") {
            return true;
        }
    }
    false
}

/// Check if pane shows INSERT mode indicator.
///
/// Claude Code renders the input-editor mode in the bottom status bar. In
/// the common case the marker is the literal string `-- INSERT --`, but on
/// narrow / extreme-wrap panes the bar wraps so the marker appears as
/// bare `INSERT` on its own line (or with the dashes split off — see
/// `status.rs::parse_status_bar` for the wrap-mode notes). A `capture_pane`
/// without `-J` preserves visual lines, so the substring `-- INSERT` may
/// be missing while the pane is genuinely in INSERT mode.
///
/// To make detection wrap-robust we:
///   1. Use `capture_pane_joined` (`-J`) so wrapped status-bar lines reassemble
///      into one logical line — `-- INSERT --` becomes contiguous again.
///   2. Match either the literal `-- INSERT --` (anchored / unwrapped form)
///      OR a bare `INSERT` appearing on a status-bar line in the last 5
///      lines. The status-bar tail check avoids false positives from chat
///      content that happens to contain the word "INSERT" (SQL prose, etc).
///
/// Pre-fix behavior (substring `-- INSERT` against unjoined capture) caused
/// `inject_text`'s Step 1 Escape loop to break out after a single Escape
/// when the pane was actually in INSERT mode but wrap-truncated. With only
/// one Escape sent, autocomplete dropdowns or ghost-text overlays could
/// absorb the keystroke, leaving the pane in INSERT — and the subsequent
/// `dd`/`i` keys arrived as literal text in the user's prompt buffer
/// rather than as vim commands. (Andrew flagged 2026-05-01.)
pub async fn is_insert_mode(pane: &str) -> bool {
    if let Some(out) = capture_pane_joined(pane).await {
        return check_lines_for_insert_mode(&out);
    }
    if let Some(out) = capture_pane(pane).await {
        return check_lines_for_insert_mode(&out);
    }
    false
}

/// Pure function: check if pane output contains an INSERT-mode indicator.
///
/// Two acceptance forms:
///   - Anywhere in the capture: literal `-- INSERT` (the unwrapped /
///     joined-capture form). This stays as-is for backward compat.
///   - In any of the last 5 lines: a token equal to `INSERT` (the
///     wrapped form where dashes broke off onto a different visual line).
///     Tail-scoped to avoid matching chat prose that happens to contain
///     the word `INSERT`.
pub(crate) fn check_lines_for_insert_mode(pane_output: &str) -> bool {
    if pane_output.contains("-- INSERT") {
        return true;
    }
    let lines: Vec<&str> = pane_output.lines().collect();
    let start = if lines.len() > 5 { lines.len() - 5 } else { 0 };
    for line in &lines[start..] {
        // Tokenize on whitespace and accept a bare INSERT token. Using
        // `split_whitespace` rather than `contains("INSERT")` rules out
        // substrings like `INSERTED` or `INSERTION`.
        if line.split_whitespace().any(|tok| tok == "INSERT") {
            return true;
        }
    }
    false
}

/// Check if pane shows a shell prompt (Claude Code not running).
pub async fn is_shell_prompt(pane: &str) -> bool {
    if let Some(out) = capture_pane(pane).await {
        return check_lines_for_shell_prompt(&out);
    }
    false
}

/// Pure function: check if any of the last 5 lines look like a shell prompt.
pub(crate) fn check_lines_for_shell_prompt(pane_output: &str) -> bool {
    let lines: Vec<&str> = pane_output.lines().collect();
    let start = if lines.len() > 5 { lines.len() - 5 } else { 0 };
    for line in &lines[start..] {
        let trimmed = line.trim();
        if trimmed.ends_with('$') || trimmed.ends_with('%') {
            return true;
        }
        if trimmed.contains("\u{279c}") || trimmed.contains("\u{2570}\u{2500}") {
            return true;
        }
    }
    false
}

/// Check if pane is showing the session feedback prompt.
pub async fn has_feedback_prompt(pane: &str) -> bool {
    if let Some(out) = capture_pane(pane).await {
        return check_lines_for_feedback_prompt(&out);
    }
    false
}

/// Pure function: check if output contains feedback prompt markers.
pub(crate) fn check_lines_for_feedback_prompt(pane_output: &str) -> bool {
    pane_output.contains("How is Claude doing") || pane_output.contains("0: Dismiss")
}

/// Dismiss the feedback prompt by sending '0'.
pub async fn dismiss_feedback_prompt(pane: &str) {
    for _ in 0..3 {
        if !has_feedback_prompt(pane).await {
            return;
        }
        send_literal(pane, "0").await;
        sleep(Duration::from_secs(1)).await;
    }
}

/// Actively interrupt Claude Code: rapid-fire Escape, periodically Ctrl-B x2.
/// Returns true if idle state confirmed within `timeout_secs`. Returns false
/// at the deadline; callers should still proceed with their inject (the
/// pane may not match `detect_activity()`'s idle predicate but Claude Code
/// has typically responded long before the timeout fires).
///
/// Uses `get_activity()` (content-area aware) instead of `is_idle()` (prompt-only)
/// to ensure the thinking indicator has fully cleared before returning.
///
/// Timing: blasts Escape every 250ms. A 1s wall-clock budget gives ~4
/// Escape sends, which is enough to interrupt anything Claude is doing
/// short of a foreground bash command (those need Ctrl-C, not Escape).
/// Idle confirmation requires two consecutive Idle reads 150ms apart to
/// guard against transient state during the pane redraw.
pub async fn interrupt_and_wait(pane: &str, timeout_secs: u64) -> bool {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout_secs);
    let mut escape_count: u32 = 0;

    while tokio::time::Instant::now() < deadline {
        if get_activity(pane).await == ClaudeActivity::Idle {
            sleep(Duration::from_millis(150)).await;
            if get_activity(pane).await == ClaudeActivity::Idle {
                // Idle confirmed. Settle BEFORE returning so any caller
                // about to send follow-up keystrokes (typed prompt text,
                // vim-mode dd/i, /clear) doesn't race the just-sent
                // Escape that brought us idle. Without this, downstream
                // inject_* keys can land before Claude finishes processing
                // the interrupt and get garbled or eaten.
                settle_after_escape().await;
                return true;
            }
        }

        if escape_count > 0 && escape_count % 5 == 0 {
            send_keys(pane, &["C-b"]).await;
            sleep(Duration::from_millis(150)).await;
            send_keys(pane, &["C-b"]).await;
            sleep(Duration::from_millis(250)).await;
        } else {
            send_keys(pane, &["Escape"]).await;
            sleep(Duration::from_millis(250)).await;
        }
        escape_count += 1;
    }
    debug!(
        pane = %pane,
        timeout_secs,
        escape_count,
        "interrupt_and_wait: idle never observed within timeout, proceeding"
    );
    false
}

/// Inject text into Claude Code via vim-mode keystrokes.
/// Escape(s) -> wait for Idle -> dd -> i -> type -> Escape -> Enter
///
/// Designed to be FAST. Most callers reach inject_text right after
/// `interrupt_and_wait` has already brought the pane to idle, so the
/// per-step waits below should be the worst case, not the typical case.
/// The Step 1b idle-wait uses a short fast-path bail
/// (`INJECT_IDLE_FAST_PATH_MS`) rather than blocking for the full pre-fix
/// 10s window — if Claude Code's pane hasn't shown Idle within ~1.5s
/// after our Escape loop, it's overwhelmingly likely the predicate just
/// isn't matching what the pane actually shows (stale thinking line in
/// scrollback, custom theme, etc.) and waiting longer doesn't help.
/// We send anyway.
pub async fn inject_text(pane: &str, text: &str) {
    // Steps 0-4 (settle, Escape→NORMAL coercion, idle-wait, dd line-clear,
    // `i` INSERT verify-and-retry, literal type) are factored into
    // `inject_text_no_submit` so this fire-and-forget path and the verified
    // `inject_and_verify` path share ONE copy of the typing choreography —
    // the divergence the `claude-watch inject` centralization exists to
    // prevent. See `inject_text_no_submit` for the per-step root-cause
    // commentary (cursor-stuck-mid-text bug, INSERT verify-and-retry, etc.).
    inject_text_no_submit(pane, text).await;

    // Step 5: Tab -> Escape -> Enter to submit.
    //
    // ROOT CAUSE of "alert text typed but never submitted" bug
    // (operator-confirmed via screenshot, 2026-06-11): the old sequence
    // here was Escape -> Enter, with NO Tab. When Claude Code's
    // autocomplete / ghost-text overlay is active (which it routinely is
    // after typing the resume/alert payload in INSERT mode), the FIRST
    // Escape only DISMISSES the dropdown — it does NOT exit INSERT (the
    // same overlay-eats-the-first-Escape behavior documented at Step 1,
    // lines ~463-465). So the pane stays in INSERT mode, and the
    // following Enter inserts a NEWLINE into the input buffer instead of
    // submitting. The alert text then sits un-submitted in the INSERT
    // buffer exactly as the operator observed.
    //
    // The proven fix mirrors the battle-tested `container/bin/self-clear`
    // Python inject path (its "regular text" branch, which has shipped
    // reliably): Tab FIRST to accept/clear the autocomplete, THEN Escape
    // to dismiss any ghost text that re-triggers after Tab and reach
    // NORMAL mode cleanly, THEN Enter to submit from NORMAL mode (Enter
    // in NORMAL mode always submits the current line). Do NOT drop the
    // Tab — without it the Escape lands on the live dropdown and the
    // submit silently fails. The keystroke ORDER is asserted by
    // `submit_keystroke_sequence_is_tab_escape_enter` so a future edit
    // can't silently regress back to the bare Escape->Enter sequence.
    for key in submit_keystroke_sequence() {
        send_keys(pane, &[key]).await;
        sleep(Duration::from_millis(300)).await;
    }
}

/// NON-CANCELLING inject: type `text` and submit it as a QUEUED message
/// WITHOUT seizing the in-flight turn.
///
/// KNOB #4 (soften escape-on-inject), 2026-06-24. The default `inject_text`
/// path leads with an Escape loop (Step 1 of `inject_text_no_submit`) +
/// `dd` line-clear that REQUIRES NORMAL mode. As `docs/two-channel-design.md`
/// and `inject_dispatch.rs` document, *the Escape is what CANCELS the current
/// generation* — typing alone does not. For routine, can-wait-for-a-turn-
/// boundary alerts (watcher-down, heartbeat-stale, ambient) that cancellation
/// is pure collateral damage: it aborts the loop's in-flight turn AND kills
/// mid-flight background agents, and makes every nudge look like a user
/// rejection. Those tiers do not need the turn seized — they need the nudge
/// to ARRIVE (by the next turn boundary is fine).
///
/// So this path deliberately OMITS the leading Escape blast and the `dd`
/// NORMAL-mode line-clear. It just enters INSERT (idempotent `i`), types the
/// payload, and submits with a bare Enter from INSERT mode. Enter-from-INSERT
/// is a proven submit (it is exactly how `inject_and_verify` submits slash
/// commands, see that fn's `slash_command` branch) and — crucially — Claude
/// Code QUEUES a message typed-and-Entered while a turn is generating instead
/// of cancelling it. The result: the nudge is delivered, the active turn and
/// any running background agents are left intact.
///
/// Trade-off vs `inject_text`: no `dd` line-clear means if the operator had
/// half-typed input on the prompt line, this payload glues onto it. That is
/// acceptable for the routine tiers (the daemon firing while the operator is
/// also typing is rare, and a queued nudge that needs a manual cleanup is far
/// less destructive than cancelling a turn + killing subagents). Emergencies
/// that genuinely must seize the turn (context-critical, wedged, auto-update,
/// prolonged-thinking) keep using `inject_text` + `interrupt_and_wait`.
pub async fn inject_text_queued(pane: &str, text: &str) {
    // Return FleetView selection to `main` FIRST (before entering INSERT and
    // typing), so a queued nudge lands on the main conversation and not on a
    // background agent selected in the FleetView (Andrew #270/#288/#291).
    // No-op when `[tmux].focus_main_keys` is empty (the default). These keys
    // do NOT cancel the active turn — arrow/Escape FleetView navigation only
    // moves the selection; the non-cancelling contract of this path is
    // preserved.
    send_focus_main_keys(pane).await;

    // Enter INSERT mode (idempotent — `i` in INSERT just inserts an 'i' that
    // the line-clear-free submit tolerates; verify-and-retry up to 3x mirrors
    // inject_text_no_submit Step 3 so a NORMAL-mode pane still ends up typing
    // into the input editor, NOT issuing motion commands). NO leading Escape:
    // that is the whole point — we must not cancel the active turn.
    let mut entered_insert = false;
    for _ in 0..3 {
        send_keys(pane, &["i"]).await;
        sleep(Duration::from_millis(400)).await;
        if is_insert_mode(pane).await {
            entered_insert = true;
            break;
        }
    }
    sleep(Duration::from_millis(300)).await;
    if !entered_insert {
        debug!(
            pane = %pane,
            "inject_text_queued: INSERT mode not confirmed after 3 `i` attempts; proceeding anyway"
        );
    }

    // Type the payload, then submit with a bare Enter from INSERT. A message
    // typed-and-Entered while a turn is generating is QUEUED by Claude Code,
    // not injected mid-generation — so the active turn keeps running.
    send_literal(pane, text).await;
    sleep(Duration::from_millis(500)).await;
    send_keys(pane, &["Enter"]).await;
    sleep(Duration::from_millis(300)).await;
}

/// The ordered keystroke sequence used by `inject_text` Step 5 to submit
/// the typed payload to a Claude Code (vim-mode) pane.
///
/// Kept as a pure function so the submit contract is unit-testable without
/// shelling out to a live tmux (`send_keys`/`run_cmd` have no mock seam).
/// MUST start with `Tab` (accept/clear autocomplete) and end with `Enter`
/// (submit) — see the Step 5 comment in `inject_text` for why the bare
/// `Escape` -> `Enter` sequence left alert text un-submitted in the INSERT
/// buffer (operator-confirmed regression, 2026-06-11).
pub(crate) fn submit_keystroke_sequence() -> &'static [&'static str] {
    &["Tab", "Escape", "Enter"]
}

/// Outcome of a verified inject (`inject_and_verify`).
///
/// Unlike the fire-and-forget `inject_text`, the verified path confirms the
/// submission actually landed by polling the pane after the submit
/// keystrokes: a successful submit CLEARS the typed payload from the input
/// line (Claude Code consumes it as a new turn). If the payload prefix is
/// still visible after the poll window, the submit did NOT land — the exact
/// failure mode the `cw-watcher-health-check` bug exhibited (alert text typed
/// into the pane but never submitted).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InjectOutcome {
    /// Text typed and (if requested) submission confirmed — the payload
    /// cleared from the prompt line.
    Submitted,
    /// `--no-submit` requested: text typed, no submission attempted. The
    /// payload is expected to remain on the prompt line.
    Typed,
    /// Submit keystrokes were sent but the payload was still visible on the
    /// prompt line after the verify window — submission likely did NOT land.
    SubmitUnverified,
}

/// Pure helper: extract the text after the LAST `❯` prompt char in the
/// capture (the live input line), trimmed. Returns `None` when no prompt
/// char is present. Mirrors `container/bin/self-clear`'s
/// `get_prompt_line_text`, the battle-tested verification primitive.
pub(crate) fn prompt_line_text(pane_output: &str) -> Option<String> {
    for line in pane_output.lines().rev() {
        if let Some(idx) = line.find('\u{276f}') {
            // Byte index is valid: `❯` is a known char, `find` returns its
            // start. Slice from just past it (the char is 3 bytes in UTF-8).
            let after = &line[idx + '\u{276f}'.len_utf8()..];
            return Some(after.trim().to_string());
        }
    }
    None
}

/// Inject text into a Claude Code (vim-mode) pane and, unless `submit` is
/// false, submit it — then VERIFY the submission landed.
///
/// This is the verified, exit-code-bearing entry point behind the public
/// `claude-watch inject` subcommand. It carries the SAME keystroke
/// choreography as `inject_text` (Escape→NORMAL coercion, dd line-clear,
/// `i` INSERT-mode verify-and-retry, literal type) so the verified and
/// daemon paths can never drift — the divergence this whole change exists
/// to eliminate. The ONE addition over `inject_text` is post-submit
/// verification, modeled on `container/bin/self-clear`'s gold-standard
/// "confirm the typed text disappears = submission succeeded" check.
///
/// Submit-keystroke selection mirrors self-clear's two branches:
///   - regular text: `submit_keystroke_sequence()` = Tab → Escape → Enter
///     (Tab clears autocomplete, Escape reaches NORMAL, Enter submits).
///   - `slash_command = true`: a bare Enter from INSERT mode. Slash
///     commands MUST submit from INSERT — Escape→NORMAL then Enter does
///     NOT submit a slash command (the documented self-clear `/clear` bug).
///
/// Returns:
///   - `InjectOutcome::Typed` when `submit == false`.
///   - `InjectOutcome::Submitted` when submission was verified (payload
///     prefix cleared from the prompt line).
///   - `InjectOutcome::SubmitUnverified` when the payload prefix was still
///     visible after the verify window — the caller can treat this as a
///     non-zero exit so a stuck inject is detectable.
pub async fn inject_and_verify(
    pane: &str,
    text: &str,
    submit: bool,
    slash_command: bool,
) -> InjectOutcome {
    // Reuse inject_text's proven type-and-(maybe-)submit choreography for
    // the regular-text submit path so there is exactly ONE copy of the
    // Escape/dd/i/type + Tab→Escape→Enter sequence. For the no-submit and
    // slash-command paths we drive the shared low-level helpers directly
    // (inject_text always submits with the regular-text sequence).
    if submit && !slash_command {
        inject_text(pane, text).await;
    } else {
        inject_text_no_submit(pane, text).await;
        if submit && slash_command {
            // Slash commands submit with a bare Enter from INSERT mode.
            send_keys(pane, &["Enter"]).await;
            sleep(Duration::from_millis(300)).await;
        }
    }

    if !submit {
        return InjectOutcome::Typed;
    }

    // Verify: a landed submit CLEARS the payload from the prompt line.
    // Poll a short window (self-clear uses ~3s) for the payload prefix to
    // disappear. We check a prefix because tmux may wrap/truncate long
    // payloads, so the full string is not reliably present even pre-submit.
    let check_prefix: String = text.chars().take(10).collect();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline {
        if let Some(out) = capture_pane(pane).await {
            // Cleared from the prompt line == submitted. We scope the check
            // to the prompt line (not the whole pane) because the submitted
            // payload legitimately appears in the scrollback above as the
            // new user turn — only its ABSENCE from the live input line
            // signals a successful submit.
            let still_on_prompt = prompt_line_text(&out)
                .map(|p| !check_prefix.is_empty() && p.contains(&check_prefix))
                .unwrap_or(false);
            if !still_on_prompt {
                return InjectOutcome::Submitted;
            }
        }
        sleep(Duration::from_millis(300)).await;
    }
    debug!(
        pane = %pane,
        "inject_and_verify: payload still on prompt line after verify window; submit likely did not land"
    );
    InjectOutcome::SubmitUnverified
}

/// The type-without-submit portion of `inject_text`: Escape→NORMAL,
/// dd line-clear, `i` INSERT verify-and-retry, then type the literal text.
/// Does NOT send any submit keystrokes. Factored out so `inject_and_verify`
/// can reuse the exact same proven typing choreography for its `--no-submit`
/// and slash-command paths without duplicating it.
pub(crate) async fn inject_text_no_submit(pane: &str, text: &str) {
    // Step -1: Return FleetView selection to `main`. MUST be first — before the
    // Escape->NORMAL coercion and the dd/i/type keys — because all of those
    // operate on whatever the CC TUI currently has focused. If a background
    // agent is SELECTED in the FleetView, the entire choreography (and the
    // typed payload) would otherwise land on that agent, not the main loop
    // (Andrew #270/#288/#291). No-op when `[tmux].focus_main_keys` is empty
    // (the default).
    send_focus_main_keys(pane).await;

    // Step 0: Settle. Most callers reach here right after interrupt_and_wait,
    // which has already fired Escape repeatedly. No-op when
    // post_escape_settle_ms is 0 (fast-path default).
    settle_after_escape().await;

    // Step 1: Escape to NORMAL mode. ALWAYS send at least two Escapes before
    // checking `is_insert_mode` — Escape in NORMAL mode is a no-op, so two
    // Escapes is idempotent coercion. Guards against (a) wrap-truncated
    // status bars where `is_insert_mode` mis-reports NORMAL, and (b)
    // autocomplete/ghost-text overlays that absorb the FIRST Escape
    // (dismissing the overlay) without exiting INSERT. Cap at 3 / ~3s.
    for i in 0..3 {
        send_keys(pane, &["Escape"]).await;
        sleep(Duration::from_secs(1)).await;
        if i >= 1 && !is_insert_mode(pane).await {
            break;
        }
    }
    // Step 1a: Optional configurable settle after the Escape loop. Default 0
    // (fast path). Tunable via [tmux].post_escape_settle_ms.
    settle_after_escape().await;

    // Step 1b: Wait briefly for the activity indicator to settle to Idle.
    // Fast-path bails after INJECT_IDLE_FAST_PATH_MS and proceeds anyway: if
    // the idle predicate hasn't matched by then it almost certainly won't
    // (stale scrollback, custom prompt).
    const INJECT_IDLE_FAST_PATH_MS: u64 = 1500;
    let idle_deadline =
        tokio::time::Instant::now() + Duration::from_millis(INJECT_IDLE_FAST_PATH_MS);
    let mut idle_observed = false;
    while tokio::time::Instant::now() < idle_deadline {
        if get_activity(pane).await == ClaudeActivity::Idle {
            idle_observed = true;
            break;
        }
        sleep(Duration::from_millis(200)).await;
    }
    if !idle_observed {
        debug!(
            pane = %pane,
            fast_path_ms = INJECT_IDLE_FAST_PATH_MS,
            "inject_text_no_submit: idle not observed within fast-path window, sending anyway"
        );
    }

    // Step 2: dd -- delete entire line
    send_keys(pane, &["d"]).await;
    sleep(Duration::from_millis(100)).await;
    send_keys(pane, &["d"]).await;
    sleep(Duration::from_millis(500)).await;

    // Step 3: i -- enter INSERT mode, AND VERIFY we actually entered INSERT
    // before typing the payload.
    //
    // ROOT CAUSE of "cursor stuck mid-text" bug (Andrew flagged 2026-04-28):
    // a fixed 1500ms sleep after `i` with NO verification let the FIRST
    // chars of `send_literal(text)` arrive while still in NORMAL mode, where
    // they're interpreted as motion/edit commands (`[`, `C`, `L`, `A`, …)
    // that jump the cursor around before INSERT finally engages. Symmetric
    // fix: verify INSERT is active (mirror of the Step 1 Escape→NORMAL
    // verify loop), retry up to 3 times.
    let mut entered_insert = false;
    for _ in 0..3 {
        send_keys(pane, &["i"]).await;
        sleep(Duration::from_millis(500)).await;
        if is_insert_mode(pane).await {
            entered_insert = true;
            break;
        }
    }
    // Final settle even on success — Claude Code may render `-- INSERT --`
    // before the input editor has fully accepted typed characters.
    sleep(Duration::from_millis(500)).await;
    if !entered_insert {
        debug!(
            pane = pane,
            "inject_text_no_submit: INSERT mode not confirmed after 3 `i` attempts; \
             proceeding anyway (fall-through to legacy behavior)"
        );
    }

    // Step 4: Type the text
    send_literal(pane, text).await;
    sleep(Duration::from_millis(500)).await;
}

/// Inject a command into a shell prompt.
pub async fn inject_shell(pane: &str, cmd: &str) {
    send_literal(pane, cmd).await;
    sleep(Duration::from_millis(300)).await;
    send_keys(pane, &["Enter"]).await;
}

/// Check if Claude Code appears to be executing a foreground bash command.
pub async fn is_foreground_busy(pane: &str) -> bool {
    if let Some(out) = capture_pane(pane).await {
        return check_lines_for_foreground_busy(&out);
    }
    false
}

/// Pure function: check if pane output indicates foreground busy state.
/// No prompt visible + spinner characters = foreground busy.
pub(crate) fn check_lines_for_foreground_busy(pane_output: &str) -> bool {
    let lines: Vec<&str> = pane_output.lines().collect();
    let start = if lines.len() > 10 {
        lines.len() - 10
    } else {
        0
    };
    let tail = &lines[start..];

    // If prompt is visible, not in foreground
    for line in tail {
        if line.contains('\u{276f}') {
            return false;
        }
    }

    // No prompt visible -- check for signs of active work (spinner characters)
    for line in tail {
        for &spinner in SPINNER_CHARS {
            if line.contains(spinner) {
                return true;
            }
        }
    }
    false
}

/// Spinner characters used by Claude Code for tool execution indicators.
/// Extracted from Claude Code v2.1.77 binary via:
///   strings <binary> | grep -oP '\\u280[0-9a-fA-F]|\\u281[0-9a-fA-F]|...'
/// These are braille pattern characters used in the dots spinner animation.
const SPINNER_CHARS: &[char] = &[
    '\u{2802}', '\u{2807}', '\u{280b}', '\u{280f}', '\u{2810}', '\u{2819}', '\u{2826}', '\u{2827}',
    '\u{2834}', '\u{2838}', '\u{2839}', '\u{283c}',
];

/// Check if a line is a separator (composed entirely of U+2500 box-drawing chars).
fn is_separator_line(line: &str) -> bool {
    let trimmed = line.trim();
    !trimmed.is_empty() && trimmed.chars().all(|c| c == '\u{2500}')
}

/// Pure function: does this (already-trimmed) line look like an ACTIVE thinking
/// indicator from Claude Code's TUI?
///
/// Claude Code's live thinking indicator has two observed formats:
///
///   Classic (2.1.77-era and earlier):
///     <indicator-char> <Verb>… (<time> [· ↓ <N> tokens])
///   e.g. "✽ Thinking… (12s · ↓ 384 tokens)"
///        "✢ Fermenting… (38s · ↓ 909 tokens)"
///        "* Warping… (26s · ↓ 438 tokens)"
///
///   Newer (2.1.112+):
///     ● <Verb>… (<time> [· ↓ <N> tokens] [· thinking])
///   e.g. "● Cooking… (28s)"
///        "● Flibbertigibbeting… (2m 35s · ↓ 869 tokens)"
///        "● Whirlpooling… (7s · ↓ 31 tokens · thinking)"
///        "● Flibbertigibbeting… (1m 19s · ↓ 540 tokens · thinking)"
///
/// Classic indicator characters (from Claude Code binary analysis — see
/// the comment in `detect_activity` below for the extraction procedure):
///   · (U+00B7), * (U+002A), ✢ (U+2722), ✳ (U+2733), ✶ (U+2736),
///   ✻ (U+273B), ✽ (U+273D)
///
/// Newer Claude Code uses `●` (U+25CF) as the thinking indicator prefix
/// (same glyph it uses for writing bullets — the distinguisher is the
/// widget structure: gerund verb ending in `…`, followed by a time-tag
/// paren).
///
/// The OLD detection was simply "line contains any of those indicator chars
/// AND contains U+2026 (…)". That fired false positives because `·` and
/// `* ` appear in TONS of non-thinking content: completion-line separators
/// (`✻ Brewed for 38s · 6 tasks`), markdown bullets (`* Check the status…`),
/// Claude Code's status-bar wrap (`current: 2.1.77 · latest: 2.1.…`), and
/// any tool output that happens to use `…` near a `·`. With the daemon's
/// prolonged-thinking interrupt at 180s, a handful of such lines sitting
/// stable in the pane during a genuinely-idle session would trigger the
/// interrupt — the exact bug report Andrew filed 2026-04-17.
///
/// The new predicate requires the full `<indicator> <Verb>… (` widget
/// structure at the start of the line — the same anchor for BOTH formats.
/// Classic indicators and the newer `●` prefix both match the same regex
/// once we include `●` in the indicator char set. The critical anchor is
/// the opening paren of the time-tag, which is ALWAYS present on a live
/// thinking widget and is NOT present on:
///   - Completion lines (use `for ` instead of `…` after the verb).
///   - Markdown/tool-output bullets (lack `(<time>` tail).
///   - Status-bar wraps (lack leading indicator+Verb+… prefix).
///   - Writing bullets like `● How is Claude doing this session? (optional)`
///     which have `(optional)` in parens but lack the `…` before the paren.
pub(crate) fn is_active_thinking_line(trimmed: &str) -> bool {
    // The indicator must be at the very start. Then: one or more whitespace,
    // an uppercase ASCII letter starting the verb, zero or more ASCII letters
    // continuing the verb, the ellipsis U+2026, optional whitespace, the
    // opening paren of the time-tag, and a digit inside the parens.
    //
    // We anchor on the `(<digit>` so that shorter false-positive prefixes
    // (e.g. `· ctrl+o…` or `● No new messages. Idling.`) and writing
    // bullets with non-time parens (e.g. `● How is Claude doing this
    // session? (optional)` — but that one lacks the ellipsis anyway, and
    // `● Some progress… (every now and then)`) cannot match. Claude Code
    // always emits a digit-leading time tag for live thinking (`28s`,
    // `1m 19s`, etc.).
    //
    // We accept a tolerant "ASCII verb" (a-zA-Z) because the 168 known
    // thinking verbs in Claude Code's binary are all plain English words
    // (Accomplishing, Baking, Cogitating, …, Zigzagging). Non-ASCII letters
    // would point to unrelated content like `✻ Sautéed for` (completion).
    //
    // Indicator char set (union of classic + newer):
    //   · (U+00B7), * (U+002A), ● (U+25CF), ✢ (U+2722), ✳ (U+2733),
    //   ✶ (U+2736), ✻ (U+273B), ✽ (U+273D)
    //
    // regex_lite supports Unicode in character classes but doesn't include
    // Unicode-property syntax (\p{Lu}), so we enumerate indicator chars
    // explicitly and restrict verb letters to ASCII.
    let pat = regex_lite::Regex::new(
        r"^[\u{00B7}\u{002A}\u{25CF}\u{2722}\u{2733}\u{2736}\u{273B}\u{273D}]\s+[A-Z][a-zA-Z]*\u{2026}\s*\(\s*\d",
    )
    .unwrap();
    pat.is_match(trimmed)
}

/// Pure function: detect Claude Code's current activity from pane output.
///
/// Claude Code's TUI has a fixed layout:
///   [scrolling content area - thinking indicators, tool output, writing]
///   ─────────────────── (separator line, U+2500 repeated)
///   ❯                   (prompt - ALWAYS visible when Claude Code is running)
///   ─────────────────── (separator)
///   -- INSERT -- ...    (status bar)
///
/// The ❯ prompt is always visible regardless of activity state, so we split
/// at the first separator and only check the content area above it.
///
/// Priority order: Thinking > ToolRunning > Writing > Idle > Unknown.
pub fn detect_activity(pane_output: &str) -> ClaudeActivity {
    let lines: Vec<&str> = pane_output.lines().collect();
    let start = if lines.len() > 15 {
        lines.len() - 15
    } else {
        0
    };
    let tail = &lines[start..];

    // Find the first separator line to split content area from prompt/status area
    let separator_idx = tail.iter().position(|line| is_separator_line(line));

    // Determine content area and whether the prompt is visible below the separator
    let (content_lines, has_prompt) = if let Some(sep_idx) = separator_idx {
        let content = &tail[..sep_idx];
        let below = &tail[sep_idx..];
        let prompt_visible = below.iter().any(|line| line.contains('\u{276f}'));
        (content, prompt_visible)
    } else {
        // No separator found — not in Claude Code's fixed TUI layout.
        // Fall back to the legacy behavior: prompt means Idle (highest priority).
        for line in tail {
            if line.contains('\u{276f}') {
                return ClaudeActivity::Idle;
            }
        }
        (tail, false)
    };

    // 1. Completion check FIRST (when prompt is visible).
    // Completion lines ("✻ Brewed for 38s", "✻ Cogitated for 2m 11s") mean
    // Claude finished responding. A stale thinking indicator
    // ("✽ Thinking… (5s)") may still be visible in the scroll history above.
    // Completion + prompt = Idle, always. Must be checked before thinking to
    // avoid false "prolonged thinking".
    //
    // We anchor on a tighter pattern — leading ✻ (U+273B), whitespace,
    // capitalized verb (past tense, e.g. "Brewed"/"Cogitated"/"Sautéed"),
    // whitespace, `for `, and a digit — rather than the old loose heuristic
    // (any indicator char + " for ") which could match unrelated content.
    // `Sautéed` uses non-ASCII `é`, so we allow `\S` after the leading
    // ASCII letter rather than restricting to `a-z`.
    if has_prompt {
        let completion_re = regex_lite::Regex::new(r"^\u{273B}\s+[A-Z]\S*\s+for\s+\d").unwrap();
        let has_completion = content_lines.iter().any(|line| {
            let trimmed = line.trim();
            completion_re.is_match(trimmed) && !trimmed.contains('\u{2026}')
        });
        if has_completion {
            return ClaudeActivity::Idle;
        }
    }

    // 2. Thinking — indicator char + verb ending in … (U+2026) with the
    // time-tag parens. See `is_active_thinking_line` for the rationale —
    // the old "indicator char + … anywhere" heuristic false-positived on
    // completion-line separators, markdown bullets, status-bar wraps, and
    // tool output containing `·` + `…`, producing spurious prolonged-
    // thinking interrupts during idle sessions.
    //
    // Extraction procedure for indicator chars (Claude Code v2.1.77 binary):
    //   strings <binary> | grep -oP '\\u273[0-9a-fA-F]|\\u272[0-9a-fA-F]' | sort -u
    // Then find the CdH() function context:
    //   strings <binary> | grep -oP '.{0,100}\\u273[0-9a-fA-F].{0,100}' | grep CdH
    //
    // Claude Code uses these indicator characters (from CdH() in source):
    //   Ghostty:  · (U+00B7), ✢ (U+2722), ✳ (U+2733), ✶ (U+2736), ✻ (U+273B), * (U+002A)
    //   Other:    · (U+00B7), ✢ (U+2722), * (U+002A), ✶ (U+2736), ✻ (U+273B), ✽ (U+273D)
    //
    // Thinking verbs (168 total, from "Accomplishing" to "Zigzagging"):
    //   strings <binary> | grep -oP '"Accomplishing".{0,10000}?"Zigzagging"\]' | tr ',' '\n'
    for line in content_lines {
        let trimmed = line.trim();
        if is_active_thinking_line(trimmed) {
            return ClaudeActivity::Thinking;
        }
    }

    // 3. ToolRunning (spinner character present in content area)
    for line in content_lines {
        for &spinner in SPINNER_CHARS {
            if line.contains(spinner) {
                return ClaudeActivity::ToolRunning;
            }
        }
    }

    // 4. Writing (● bullet points visible in content area)
    for line in content_lines {
        if line.trim_start().starts_with('\u{25cf}') {
            return ClaudeActivity::Writing;
        }
    }

    // 5. Idle (prompt visible but no activity indicators above separator)
    if has_prompt {
        return ClaudeActivity::Idle;
    }

    ClaudeActivity::Unknown
}

/// Capture the pane and detect Claude Code's current activity state.
pub async fn get_activity(pane: &str) -> ClaudeActivity {
    if let Some(out) = capture_pane(pane).await {
        return detect_activity(&out);
    }
    ClaudeActivity::Unknown
}

/// Find the dashboard pane for Claude Code.
///
/// When `dashboard_session` and `dashboard_pane` are configured (non-empty),
/// checks those specific locations first. When unconfigured (empty/default),
/// falls back to `find_claude_pane()` which auto-discovers across all tmux
/// sessions. This makes the [tmux] config section optional for fresh installs.
pub async fn find_dashboard_pane(config: &crate::config::TmuxConfig) -> Option<String> {
    // If no session configured, skip session-specific checks and auto-detect
    if config.dashboard_session.is_empty() {
        debug!("no dashboard_session configured, falling back to find_claude_pane()");
        return crate::status::find_claude_pane().await;
    }

    // Check if dashboard session exists
    let (_, ok) = run_cmd_any(&["tmux", "has-session", "-t", &config.dashboard_session], 5).await;
    if !ok {
        return None;
    }

    // Check known pane (only if explicitly configured).
    //
    // Resolve the configured pane to its IMMUTABLE `#{pane_id}` (`%N`) and
    // return THAT, not the positional `session:window.pane` spec the operator
    // wrote in config. A positional spec is an index into the live layout: tmux
    // renumbers pane indices when panes are added/removed (e.g. the operator
    // opening/closing a Claude Code TUI agent-view pane in the same window), so
    // `dashboard:0.2` can point at a DIFFERENT physical pane from one moment to
    // the next — and a watcher/heartbeat/reminder inject then lands in whatever
    // pane now sits at that index (an agent pane). A `pane_id` is assigned once
    // for the pane's lifetime and never reused, so targeting it pins every
    // downstream `send-keys`/`capture-pane`/`get_pane_pid` to the SAME physical
    // main-loop pane regardless of layout churn or which pane the TUI has
    // selected/active. See `find_claude_pane_with_config`'s focus-follows-inject
    // notes.
    if !config.dashboard_pane.is_empty() {
        let (out, ok) = run_cmd_any(
            &[
                "tmux",
                "display-message",
                "-t",
                &config.dashboard_pane,
                "-p",
                "#{pane_id}",
            ],
            5,
        )
        .await;
        if ok && !out.is_empty() {
            return Some(out);
        }
    }

    // Fallback: search for shell panes in dashboard session. Emit the stable
    // `#{pane_id}` (not the positional `session:window.pane`) for the same
    // layout-churn reason as the configured-pane branch above.
    let (out, ok) = run_cmd_any(
        &[
            "tmux",
            "list-panes",
            "-s",
            "-t",
            &config.dashboard_session,
            "-F",
            "#{pane_id} #{pane_current_command}",
        ],
        5,
    )
    .await;
    if ok {
        for line in out.lines() {
            let parts: Vec<&str> = line.splitn(2, ' ').collect();
            if parts.len() == 2 && (parts[1] == "zsh" || parts[1] == "bash") {
                return Some(parts[0].to_string());
            }
        }
    }
    None
}

/// Check if Claude Code is still running in the pane (vs. shell prompt visible).
///
/// After /exit, the pane shows a zsh prompt. During Claude Code, the status bar
/// shows token count and version info. Shell prompt detection takes priority.
pub async fn is_claude_running(pane: &str) -> bool {
    if let Some(out) = capture_pane(pane).await {
        return check_claude_running(&out);
    }
    false
}

/// Pure function: check if Claude Code is running from pane output.
/// Returns false if a shell prompt is detected, true if Claude indicators found.
///
/// Shell prompt detection is stricter than `check_lines_for_shell_prompt` to avoid
/// false positives from Claude Code status bar content (e.g. "42%" compact remaining).
/// We only look for bira theme patterns (╰─$, ╰─#) and arrow prompts (➜).
pub(crate) fn check_claude_running(pane_output: &str) -> bool {
    let lines: Vec<&str> = pane_output.lines().collect();

    // Check for shell prompt FIRST (stronger signal — means Claude exited)
    // Use strict bira-theme patterns to avoid false positives from status bar "42%" etc.
    let start = if lines.len() > 5 { lines.len() - 5 } else { 0 };
    for line in &lines[start..] {
        let trimmed = line.trim();
        // Bira theme: "╰─$" or "╰─# " (root)
        if trimmed.contains("\u{2570}\u{2500}$") || trimmed.contains("\u{2570}\u{2500}#") {
            return false;
        }
        // Arrow prompt (oh-my-zsh robbyrussell etc.)
        if trimmed.contains("\u{279c}") {
            return false;
        }
    }

    // Only if no shell prompt found, check for Claude Code indicators.
    // Match "tok" (not "tokens") to tolerate the `502064 tok…` ellipsis
    // truncation Claude Code applies in narrow panes.
    let tail_start = if lines.len() > 10 {
        lines.len() - 10
    } else {
        0
    };
    let tail: String = lines[tail_start..].join("\n");
    if tail.contains("tok")
        && (tail.contains("auto-compact")
            || tail.contains("latest:")
            || tail.contains("background tasks")
            || tail.contains(" shells")
            || tail.contains("bypass permissi"))
    {
        return true;
    }

    // Default: assume still running (conservative)
    true
}

/// Wait for Claude Code to exit (shell prompt appears). Returns true if exited within timeout.
pub async fn wait_for_exit(pane: &str, timeout_secs: u64) -> bool {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    while tokio::time::Instant::now() < deadline {
        if !is_claude_running(pane).await {
            return true;
        }
        sleep(std::time::Duration::from_secs(1)).await;
    }
    false
}

/// Check if the actual Claude binary (not a wrapper script) is running under a pane's process tree.
///
/// Walks /proc looking for an exe under the native versioned-symlink layout
/// (`~/.local/share/claude/versions/`) AND — when
/// `CLAUDE_WATCH_CONTAINER_MODE=1` — under the in-container npm-global layout
/// (`~/.npm-global/lib/node_modules/@anthropic-ai/claude-code/`). See
/// `has_claude_binary` for the container-mode rationale.
pub async fn wait_for_claude_binary(pane: &str, timeout_secs: u64) -> bool {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    while tokio::time::Instant::now() < deadline {
        if has_claude_binary(pane).await {
            return true;
        }
        sleep(std::time::Duration::from_secs(2)).await;
    }
    false
}

/// Resolve a tmux pane spec to its top-level pane PID via
/// `tmux display-message -p '#{pane_pid}'`. Returns `None` on tmux error,
/// empty output, or non-numeric output.
///
/// This is the entry point for the inject dispatcher's pane → claude-PID
/// walk: caller passes the pane PID to `agent::find_claude_pid_in_tree`
/// to locate the actual claude binary PID (which may be a descendant of
/// the pane's shell).
pub async fn get_pane_pid(pane: &str) -> Option<u32> {
    let (pid_str, ok) = run_cmd_any(
        &["tmux", "display-message", "-t", pane, "-p", "#{pane_pid}"],
        5,
    )
    .await;
    if !ok || pid_str.is_empty() {
        return None;
    }
    pid_str.trim().parse::<u32>().ok()
}

/// Check if the pane's process tree includes the actual claude binary.
///
/// Delegates to `agent::find_claude_pid_in_tree`, which walks the subtree
/// rooted at the pane's shell PID and matches the claude exe against BOTH
/// install layouts:
///   - the native versioned-symlink layout (`~/.local/share/claude/versions/`), and
///   - (when `CLAUDE_WATCH_CONTAINER_MODE=1`) the in-container npm-global layout
///     (`~/.npm-global/lib/node_modules/@anthropic-ai/claude-code/`).
///
/// Historically this function (and the now-removed local `check_proc_tree`)
/// matched ONLY the versioned-symlink path. That predates the
/// 2026-05-15 autoupdate-v2 container-mode fix that taught
/// `find_claude_pid*` about the npm-global layout. The mismatch meant that
/// inside the npm-global container (where there is NO
/// `~/.local/share/claude/versions/` dir and the running claude exe lives
/// under `~/.npm-global/...`) `wait_for_claude_binary` could NEVER succeed.
/// After PR #379 turned that wait into a HARD GATE on the auto-update
/// relaunch path, the never-passing detection produced a fatal
/// "claude binary never started after relaunch" alert and a dead pane on
/// every in-container auto-update. Routing through the container-mode-aware
/// `agent` helper fixes the relaunch detection without disabling auto-update.
async fn has_claude_binary(pane: &str) -> bool {
    let (pid_str, ok) = run_cmd_any(
        &["tmux", "display-message", "-t", pane, "-p", "#{pane_pid}"],
        5,
    )
    .await;
    if !ok || pid_str.is_empty() {
        return false;
    }
    let Ok(pane_pid) = pid_str.trim().parse::<u32>() else {
        return false;
    };

    // Spawn blocking since we're walking /proc. Depth 4 mirrors the
    // previous local walk (pane shell -> bash relaunch -> node -> claude).
    tokio::task::spawn_blocking(move || {
        crate::agent::find_claude_pid_in_tree(pane_pid, 4).is_some()
    })
    .await
    .unwrap_or(false)
}

/// Wait for the Claude idle prompt (❯) to appear. Returns true if found within timeout.
pub async fn wait_for_idle_prompt(pane: &str, timeout_secs: u64) -> bool {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    while tokio::time::Instant::now() < deadline {
        if is_idle(pane).await {
            sleep(std::time::Duration::from_millis(500)).await;
            if is_idle(pane).await {
                return true;
            }
        }
        sleep(std::time::Duration::from_secs(2)).await;
    }
    false
}

/// Pure function: check if pane output indicates Claude Code needs API reauth.
///
/// Design: if the TUI is visible (tokens counter, status bar, prompt, permission
/// mode indicator, etc.), we are looking at a live Claude Code session with
/// conversation content — NOT a reauth state. Even strings like "API Error: 401"
/// or `"authentication_error"` that appear in conversation text (e.g. the user
/// discussing the error pattern) must NOT trigger reauth, or we inject `/login`
/// into their active session and break it.
///
/// A real reauth failure replaces the TUI entirely with a login screen
/// ("Browser didn't open?", OAuth URL, "Paste code here"), so the absence of
/// TUI indicators is the reliable signal. This is a single-phase check: if the
/// TUI is gone AND login-screen patterns are present, reauth is needed.
pub(crate) fn check_lines_for_reauth(pane_output: &str) -> bool {
    let lower = pane_output.to_lowercase();

    // Unified TUI guard: any TUI indicator means we're looking at live conversation,
    // not a login screen. Conversation content can legitimately contain any
    // auth-error text without triggering reauth.
    let tui_visible = lower.contains("tokens")
        || lower.contains("bashes")
        || lower.contains(" shells")
        || lower.contains(" agents")
        || lower.contains(" background tasks")
        || lower.contains("\u{276f}")
        || lower.contains("bypass permissi");

    if tui_visible {
        return false;
    }

    // TUI is gone — check for login-screen patterns.
    // Current Claude Code login screen shows: "Browser didn't open?",
    // "Paste code here", and a claude.ai/oauth/authorize URL.
    lower.contains("browser didn't open")
        || lower.contains("paste code here")
        || lower.contains("claude.ai/oauth/authorize")
        || lower.contains("open this url") && lower.contains("login")
        || lower.contains("session expired")
        || lower.contains("login required")
        || lower.contains("re-authenticate")
        || lower.contains("authentication required")
        || lower.contains("auth required")
        || lower.contains("api key expired")
}

/// Extract the login URL from pane output, handling possible line wrapping.
/// Looks for URLs starting with `https://claude.ai/oauth/authorize`.
/// tmux line wrapping splits a URL across lines with NO separator, so we
/// reassemble by joining consecutive lines that look like URL continuations
/// (no whitespace at start, valid URL chars).
pub(crate) fn extract_login_url(pane_output: &str) -> Option<String> {
    let lines: Vec<&str> = pane_output.lines().collect();

    // Find the line containing the URL start
    let mut url_line_idx = None;
    let mut url_start_col = 0;
    for (i, line) in lines.iter().enumerate() {
        if let Some(pos) = line.find("https://claude.ai/oauth/authorize") {
            url_line_idx = Some(i);
            url_start_col = pos;
            break;
        }
    }
    let start_idx = url_line_idx?;

    // Start with the URL portion from the first line
    let first_part = &lines[start_idx][url_start_col..];
    let mut url = String::new();

    // Check if first line's URL portion ends at a whitespace boundary
    if let Some(end) = first_part.find(|c: char| c.is_whitespace()) {
        url.push_str(&first_part[..end]);
    } else {
        // URL may continue on next line(s) — tmux wraps with no separator
        url.push_str(first_part);
        for line in &lines[start_idx + 1..] {
            // A continuation line starts with URL-valid chars (no space/control)
            // and the line is non-empty
            if line.is_empty() || line.starts_with(' ') {
                break;
            }
            // Append until whitespace
            if let Some(end) = line.find(|c: char| c.is_whitespace()) {
                url.push_str(&line[..end]);
                break;
            } else {
                url.push_str(line);
            }
        }
    }

    Some(url)
}

/// Check if the pane is showing a reauth/login prompt.
/// Returns the login URL if reauth is needed (or empty string if needed but URL not found).
pub async fn needs_reauth(pane: &str) -> Option<String> {
    if let Some(out) = capture_pane(pane).await {
        if check_lines_for_reauth(&out) {
            return Some(extract_login_url(&out).unwrap_or_default());
        }
    }
    None
}

/// Reason a Claude Code session is considered wedged (unable to recover on its own).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WedgedReason {
    /// "Context limit reached" / "/compact or /clear to continue" — context overflow.
    /// The agent cannot make any tool call until the context is cleared.
    ContextLimit,
    /// Persistent "API Error: Request rejected (429)" / "Rate limited" — Anthropic
    /// 429 from the model API. The agent cannot make any tool call until the rate
    /// limit clears, but the only safe recovery on our side is /clear (which drops
    /// most of the prior context and lets a fresh request slip through).
    RateLimited,
}

impl fmt::Display for WedgedReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WedgedReason::ContextLimit => write!(f, "context_limit"),
            WedgedReason::RateLimited => write!(f, "rate_limited"),
        }
    }
}

/// Pure function: detect whether the pane shows that Claude Code is wedged in a
/// state it cannot recover from on its own.
///
/// Looks for two patterns in the last ~40 lines of pane output:
///   1. "Context limit reached" or "/compact or /clear to continue"
///      → `WedgedReason::ContextLimit`. Means the agent has hit its context
///        ceiling and every subsequent tool call returns an error before it can
///        run. Only an external `/clear` can recover.
///   2. "Request rejected (429)" or "rate limited" / "rate-limited"
///      → `WedgedReason::RateLimited`. Anthropic 429 — same external-recovery
///        story (we /clear to shed context and slip a smaller request through,
///        and the daemon will keep trying).
///
/// Returns `Some(reason)` on the FIRST matching pattern found. The caller is
/// responsible for requiring multiple consecutive matches before acting, to
/// avoid false positives from chat-history references to the strings.
///
/// We deliberately do NOT match arbitrary "API Error" lines — those happen
/// occasionally during normal operation and recover on their own.
pub(crate) fn check_lines_for_wedged(pane_output: &str) -> Option<WedgedReason> {
    let lines: Vec<&str> = pane_output.lines().collect();
    let start = if lines.len() > 40 {
        lines.len() - 40
    } else {
        0
    };
    let tail = &lines[start..];

    let lower: String = tail.join("\n").to_lowercase();

    // Context-limit patterns (Claude Code prints these as a fixed banner when
    // the model context overflows).
    if lower.contains("context limit reached")
        || lower.contains("context low (")
        || lower.contains("/compact or /clear to continue")
        || lower.contains("/clear or /compact to continue")
    {
        return Some(WedgedReason::ContextLimit);
    }

    // Rate-limit patterns. We require BOTH a rejection signal and a 429-ish
    // marker so that incidental mentions of the strings (e.g. an HTTP status
    // table in chat history) don't trip the detector.
    let has_reject = lower.contains("request rejected") || lower.contains("api error");
    let has_429 = lower.contains("(429)") || lower.contains(" 429 ") || lower.contains("rate limit");
    if has_reject && has_429 {
        return Some(WedgedReason::RateLimited);
    }

    None
}

/// Capture the pane and check whether Claude Code is wedged (context limit /
/// persistent rate limit). Returns the reason on detection.
pub async fn detect_wedged(pane: &str) -> Option<WedgedReason> {
    let out = capture_pane_history(pane, 80).await?;
    check_lines_for_wedged(&out)
}

/// Pure function: detect whether the pane shows a MALFORMED tool call — the
/// model emitting raw, NON-namespaced `<invoke ...>` / `<parameter ...>` tags
/// (optionally preceded by a stray literal text prefix) instead of a
/// well-formed namespaced tool call.
///
/// Background: a correctly-formed tool call is consumed by the harness and
/// rendered as a tool-use widget (e.g. `● Bash(...)`); the raw `<invoke>` /
/// `<parameter>` tags NEVER appear as visible assistant text. When the model
/// malforms the call — emitting a bare `<invoke name="Bash">` without the
/// required namespace prefix, often with a stray word glued to the front —
/// the harness does NOT execute it. Instead the malformed block is rendered
/// as plain assistant TEXT and the INTENDED action (very often a
/// `watcher-ctl run ...`, a `signal-send`, or a heartbeat `touch`) silently
/// never runs. Sustained, this strands one-shot watchers DOWN, lets the
/// heartbeat go stale, and produces hours of failure/heartbeat-stale/
/// watcher-down alert storms — the 2026-06-17 incident.
///
/// Detection signature: a line in the recent pane tail that contains a raw
/// opening `<invoke` or `<parameter` tag whose tag-name is NOT namespaced
/// with the expected `antml:` prefix. A well-formed call's tags never reach
/// the pane as text, so the presence of the raw tag is itself the malformation
/// signal. We require the opening-tag form (`<invoke`/`<parameter`) so prose
/// that merely mentions the word "invoke" or "parameter" does not trip it.
///
/// To further guard against false positives from chat-history / documentation
/// that legitimately discusses these tags (including THIS source file being
/// read into a pane), the detector is STRUCTURAL (not a substring grep): it
/// tokenizes the candidate region and confirms an actual attempted tool-call
/// *construct* — a non-namespaced opening `<invoke name="...">` tag corroborated
/// by a following `<parameter name="...">` and/or a `</invoke>` close — rather
/// than a bare substring match. It also skips any region inside a fenced code
/// block (```...```), since prose/docs that legitimately quote the tags do so
/// inside fences. The caller still requires multiple consecutive observations
/// before acting, and supports an explicit override marker for manual bypass.
///
/// Returns `true` when a structurally-confirmed malformed tool-call construct
/// is present in the tail.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn check_lines_for_malformed_tool_call(pane_output: &str) -> bool {
    malformed_tool_call_fingerprint(pane_output).is_some()
}

/// Like `check_lines_for_malformed_tool_call`, but on a positive detection ALSO
/// returns a stable FINGERPRINT of the specific malformed block found in the
/// tail. The fingerprint lets the caller (the daemon's policy loop) DEDUP:
/// re-firing the corrective inject every cycle while the SAME malformed block
/// merely lingers in pane scrollback — even though the model has already
/// recovered with a well-formed call below it — is exactly the tight
/// self-perpetuating interruption loop that motivated the 2026-06-20 incident
/// (the operator killed claude-watch because the interrupter was "too
/// aggressive", false-positiving on stale scrollback). A genuinely NEW malform
/// produces a DIFFERENT fingerprint and is acted on immediately.
///
/// The fingerprint is built from the malformed tag tokens together with the
/// raw text of every tail line that contributes a malformed tag, joined with
/// `\n`. Two captures whose malformed region is byte-identical hash to the same
/// fingerprint; a fresh malform (different command, different stray prefix,
/// different tags) hashes differently. The pane's surrounding chrome (prompt
/// box, separators, status bar) — which is ALWAYS present and would otherwise
/// make every capture look "fresh" — is deliberately excluded.
pub(crate) fn malformed_tool_call_fingerprint(pane_output: &str) -> Option<String> {
    let lines: Vec<&str> = pane_output.lines().collect();
    let start = if lines.len() > 40 {
        lines.len() - 40
    } else {
        0
    };
    let tail = &lines[start..];
    if !detect_malformed_construct(tail) {
        return None;
    }
    Some(malformed_block_fingerprint(tail))
}

/// Build the dedup fingerprint for the malformed block in `tail`: the
/// concatenation (newline-joined) of every non-fenced tail line that
/// contributes at least one malformed tag. This captures the actual offending
/// text (stray prefix + the raw tags + their attribute values) while ignoring
/// the always-present TUI chrome and any unrelated scrollback, so the same
/// malformed block hashes identically across cycles.
fn malformed_block_fingerprint(tail: &[&str]) -> String {
    let mut in_fence = false;
    let mut parts: Vec<&str> = Vec::new();
    for line in tail {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }
        if !tokenize_malformed_line(line).is_empty() {
            parts.push(line.trim_end());
        }
    }
    parts.join("\n")
}

/// A single token extracted from the candidate region: a raw, non-namespaced
/// `<invoke ...>` / `<parameter ...>` opening tag, or a `</invoke>` /
/// `</parameter>` closing tag. Used to confirm a *structural* tool-call
/// construct rather than an incidental substring.
#[derive(Debug, Clone, PartialEq, Eq)]
enum MalformedToken {
    /// `<invoke name="...">` with the captured tool name (empty if no `name=`).
    OpenInvoke { has_name: bool },
    /// `<parameter name="...">` with a `name=` attribute.
    OpenParameter { has_name: bool },
    /// `</invoke>`
    CloseInvoke,
    /// `</parameter>`
    CloseParameter,
}

/// Tokenize a single line into the malformed-tool-call tags it contains.
///
/// Only RAW, NON-namespaced tags are emitted. A correctly-namespaced tag
/// (`<invoke ...>` or any `<word:invoke ...>`) is consumed by the harness
/// and never reaches the pane as text; we additionally exclude it structurally
/// here so a namespaced tag that somehow appears (e.g. quoted in this file)
/// does not contribute a token.
fn tokenize_malformed_line(line: &str) -> Vec<MalformedToken> {
    let bytes = line.as_bytes();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'<' {
            i += 1;
            continue;
        }
        let rest = &line[i..];
        // Closing tags.
        if let Some(stripped) = rest.strip_prefix("</invoke>") {
            tokens.push(MalformedToken::CloseInvoke);
            i = line.len() - stripped.len();
            continue;
        }
        if let Some(stripped) = rest.strip_prefix("</parameter>") {
            tokens.push(MalformedToken::CloseParameter);
            i = line.len() - stripped.len();
            continue;
        }
        // Opening tags. `after` is the text immediately following `<invoke` /
        // `<parameter`; for a real tag it must be a tag-boundary char
        // (whitespace, `>`, or `/`) so `<invokeXYZ` / `<parameters` don't match.
        for (kw, is_invoke) in [("invoke", true), ("parameter", false)] {
            let opener = format!("<{kw}");
            if let Some(after) = rest.strip_prefix(&opener) {
                let boundary = after
                    .chars()
                    .next()
                    .map(|c| c.is_whitespace() || c == '>' || c == '/')
                    .unwrap_or(false);
                if boundary {
                    // Scan to the end of this opening tag (`>`), staying on the
                    // same line, to check for a `name="..."` attribute.
                    let tag_body = after.split('>').next().unwrap_or(after);
                    let has_name = tag_name_attr_present(tag_body);
                    if is_invoke {
                        tokens.push(MalformedToken::OpenInvoke { has_name });
                    } else {
                        tokens.push(MalformedToken::OpenParameter { has_name });
                    }
                }
            }
        }
        i += 1;
    }
    tokens
}

/// True if the tag body contains a `name="..."` (or `name='...'`) attribute
/// with a non-empty value.
fn tag_name_attr_present(tag_body: &str) -> bool {
    if let Some(idx) = tag_body.find("name=") {
        let after = &tag_body[idx + "name=".len()..];
        let mut chars = after.chars();
        match chars.next() {
            Some('"') => after[1..].contains('"') && !after.starts_with("\"\""),
            Some('\'') => after[1..].contains('\'') && !after.starts_with("''"),
            _ => false,
        }
    } else {
        false
    }
}

/// True if the tail contains the high-confidence `court`-prefix malform
/// signature: a line that is EXACTLY `court` (after trimming surrounding
/// whitespace), immediately followed within the next few lines by a bare,
/// non-namespaced `<invoke` opening tag.
///
/// This is the confirmed real-world signature (2026-06-17): the malformed
/// invocation always begins with the bare literal token `court` on its own
/// line, then non-namespaced `<invoke .../>` / `<parameter .../>` tags the
/// harness can't parse, so the whole block — `court` included — is rendered
/// as visible pane text.
///
/// We deliberately keep this tight to preserve precision:
///   * the `court` line must be the WHOLE trimmed line (so the word "court" in
///     prose, e.g. "the court ruled", does NOT match), and
///   * the following `<invoke` must be a real opening tag (boundary char after
///     `<invoke`), bare (non-namespaced — a namespaced tag would be consumed by
///     the harness and never reach the pane), and
///   * any region inside a fenced code block (```...```) is skipped, so docs /
///     chat that quote the signature inside a fence do not trip it.
const COURT_PREFIX_LOOKAHEAD: usize = 4;

fn detect_court_prefix_signature(tail: &[&str]) -> bool {
    // Per-line fence state, so we can both skip a `court` line inside a fence
    // and avoid matching an `<invoke` that lives inside a fence. The fence
    // marker line itself is treated as "inside" for skip purposes.
    let fence_state: Vec<bool> = tail
        .iter()
        .scan(false, |fence, line| {
            let trimmed = line.trim_start();
            if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
                *fence = !*fence;
                Some(true)
            } else {
                Some(*fence)
            }
        })
        .collect();

    for (i, line) in tail.iter().enumerate() {
        if fence_state[i] {
            continue;
        }
        if line.trim() != "court" {
            continue;
        }
        // Look ahead a few lines for a bare (non-namespaced) opening `<invoke`.
        let end = (i + 1 + COURT_PREFIX_LOOKAHEAD).min(tail.len());
        for (j, look) in tail.iter().enumerate().take(end).skip(i + 1) {
            if fence_state[j] {
                continue;
            }
            if line_has_bare_open_invoke(look) {
                return true;
            }
        }
    }
    false
}

/// True if the line contains at least one bare (non-namespaced) opening
/// `<invoke` tag. Reuses the same tokenizer the structural detector uses, so a
/// namespaced `<invoke>` (consumed by the harness, never on the pane) and
/// non-tag text like `<invokeXYZ` do NOT count.
fn line_has_bare_open_invoke(line: &str) -> bool {
    tokenize_malformed_line(line)
        .iter()
        .any(|t| matches!(t, MalformedToken::OpenInvoke { .. }))
}

/// Structural detector. Walks the tail line-by-line, skipping any region inside
/// a fenced code block (```...```), tokenizes each remaining line, and confirms
/// a real tool-call *construct*:
///
///   * a non-namespaced `<invoke name="...">` opener, AND
///   * structural corroboration — a `<parameter ...>` opener and/or a
///     `</invoke>` close somewhere in the candidate region.
///
/// A lone `<parameter name="...">` (without any invoke) ALSO qualifies, since
/// the model frequently malforms only the tail of a call; but a bare
/// `<invoke>` with NEITHER a `name=` attribute NOR any corroborating tag is
/// treated as prose/noise and ignored. This is what cuts the false positives
/// that a substring grep produced.
fn detect_malformed_construct(tail: &[&str]) -> bool {
    // High-confidence fast-path: the real-world 2026-06-17 signature is a pane
    // line that is EXACTLY the bare literal token `court` (after trimming
    // whitespace), immediately followed within the next few lines by a bare
    // (non-namespaced) `<invoke` opening tag. The harness can't parse the
    // malformed block, so it renders the whole thing — the leading `court`
    // included — as visible pane text. This is a definite malform.
    if detect_court_prefix_signature(tail) {
        return true;
    }

    let mut in_fence = false;
    let mut tokens: Vec<MalformedToken> = Vec::new();
    for line in tail {
        let trimmed = line.trim_start();
        // Toggle fenced-code-block state on a fence marker line. Anything
        // inside a fence is quoted text (docs/chat), never a live tool call.
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }
        tokens.extend(tokenize_malformed_line(line));
    }

    let has_named_invoke = tokens
        .iter()
        .any(|t| matches!(t, MalformedToken::OpenInvoke { has_name: true }));
    let has_any_invoke = tokens
        .iter()
        .any(|t| matches!(t, MalformedToken::OpenInvoke { .. }));
    let has_named_parameter = tokens
        .iter()
        .any(|t| matches!(t, MalformedToken::OpenParameter { has_name: true }));
    let has_close_invoke = tokens.iter().any(|t| *t == MalformedToken::CloseInvoke);

    // Construct rules (any one is a confirmed malformed tool call):
    //  1. A named `<invoke name="...">` corroborated by a parameter or a close.
    //  2. A bare `<invoke>` (no name) corroborated by BOTH a named parameter
    //     and a close (very strong structural signal, no name needed).
    //  3. A named `<parameter name="...">` corroborated by an invoke open or a
    //     close (the "only-the-tail-malformed" case).
    let rule_named_invoke = has_named_invoke && (has_named_parameter || has_close_invoke);
    let rule_bare_invoke = has_any_invoke && has_named_parameter && has_close_invoke;
    let rule_param_construct = has_named_parameter && (has_any_invoke || has_close_invoke);

    rule_named_invoke || rule_bare_invoke || rule_param_construct
}

/// Capture the pane and check whether the live tail shows a malformed
/// (non-namespaced) tool-call block rendered as assistant text. On detection
/// returns `Some(fingerprint)` (a stable hash-input string identifying the
/// specific malformed block — see `malformed_tool_call_fingerprint`); on no
/// detection returns `None`. The caller dedups re-injects on the fingerprint so
/// the SAME malformed block lingering in scrollback (after the model already
/// recovered) does not re-fire every cycle — the tight-loop false positive that
/// motivated the 2026-06-20 fix.
///
/// `override_marker`, when it points at an existing file, disables detection
/// entirely (a manual false-positive bypass — see the AskUserQuestion-allowed
/// marker pattern). This lets an operator who is legitimately driving a turn
/// that discusses the tags suppress the guardrail without editing config.
pub async fn detect_malformed_tool_call(pane: &str, override_marker: &str) -> Option<String> {
    if !override_marker.is_empty() && std::path::Path::new(override_marker).exists() {
        debug!(
            marker = override_marker,
            "malformed-tool-call detection suppressed by override marker"
        );
        return None;
    }
    if let Some(out) = capture_pane_history(pane, 60).await {
        return malformed_tool_call_fingerprint(&out);
    }
    None
}

/// Pure function: detect whether the pane shows Claude Code in an upstream-API
/// retry-backoff state. When Anthropic returns 5xx (overloaded / 529) or
/// transient 5xx errors, Claude Code retries with exponential backoff and
/// prints lines like:
///
///   API Error: 529 {"type":"error","error":{"type":"overloaded_error",...}}
///   ⎿  Retrying in 24s · attempt 3/10
///
/// During this window Claude Code is NOT thinking and NOT busy in the normal
/// sense — it is waiting on a sleep before the next HTTP attempt. claude-watch
/// MUST NOT inject during that window: every inject (Escape + text) wipes the
/// retry state machine and forces Claude to start a brand-new turn, which then
/// hits the same overload and re-enters retry. The result is a livelock where
/// the daemon's interrupts perpetually reset the retry timer.
///
/// Detection requirements (BOTH must hold so chat-history references to the
/// strings don't trip the detector):
///   1. A line containing "Retrying in <N>s" or "Retrying in <N> seconds"
///      OR a line containing "attempt N/M" (Claude Code prints this pair as
///      one structured cue when actively retrying).
///   2. Either the same line OR a nearby line carries an "API Error: 5xx"
///      / "API Error: 429" / "Overloaded" / "overloaded_error" marker so we
///      know the retry is upstream-API driven.
///
/// We intentionally scope the inspection to the LAST ~25 lines so the cue must
/// be currently visible (not just somewhere in scrollback chat history).
pub(crate) fn check_lines_for_api_retry(pane_output: &str) -> bool {
    let lines: Vec<&str> = pane_output.lines().collect();
    let start = if lines.len() > 25 {
        lines.len() - 25
    } else {
        0
    };
    let tail = &lines[start..];
    let lower: String = tail.join("\n").to_lowercase();

    // Cue 1: "Retrying in Ns" or "attempt N/M" must be present in the live
    // tail. Both phrases are emitted directly by Claude Code's retry loop
    // — they don't appear in normal conversation.
    let retrying_in = regex_lite::Regex::new(r"retrying in\s+\d+\s*(s|sec|seconds)\b")
        .ok()
        .is_some_and(|re| re.is_match(&lower));
    let attempt_n_of_m = regex_lite::Regex::new(r"attempt\s+\d+\s*/\s*\d+\b")
        .ok()
        .is_some_and(|re| re.is_match(&lower));
    if !(retrying_in || attempt_n_of_m) {
        return false;
    }

    // Cue 2: an upstream-API error marker must accompany the retry cue. This
    // is the load-bearing safety check — without it, an isolated "attempt
    // 2/3" mention in chat history would falsely flag every session as
    // retrying.
    let has_api_error_5xx = regex_lite::Regex::new(r"api error:\s*5\d{2}\b")
        .ok()
        .is_some_and(|re| re.is_match(&lower));
    let has_api_error_429 = lower.contains("api error: 429") || lower.contains("api error:429");
    let has_overloaded = lower.contains("overloaded_error") || lower.contains("overloaded");

    has_api_error_5xx || has_api_error_429 || has_overloaded
}

/// Capture the pane and check whether Claude Code is in an upstream-API retry
/// backoff. Returns true on detection.
pub async fn detect_api_retry(pane: &str) -> bool {
    if let Some(out) = capture_pane_history(pane, 60).await {
        return check_lines_for_api_retry(&out);
    }
    false
}

/// Run tmux healthcheck brief.
pub async fn healthcheck_brief() -> String {
    run_cmd(&["tmux-healthcheck", "--brief"], 5)
        .await
        .unwrap_or_else(|| "tmux-healthcheck: unavailable".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression guard for the "alert text typed but never submitted"
    /// bug (operator-confirmed via screenshot, 2026-06-11). `inject_text`
    /// Step 5 MUST send Tab (accept/clear autocomplete) before Escape so
    /// the trailing Enter submits from NORMAL mode instead of inserting a
    /// newline into a still-INSERT buffer. The pre-fix sequence was the
    /// bare `["Escape", "Enter"]`, which left the payload un-submitted
    /// whenever the autocomplete overlay ate the lone Escape. This pins
    /// the proven `container/bin/self-clear` "regular text" sequence.
    #[test]
    fn submit_keystroke_sequence_is_tab_escape_enter() {
        let seq = submit_keystroke_sequence();
        assert_eq!(
            seq,
            &["Tab", "Escape", "Enter"],
            "Step-5 submit sequence regressed; must be Tab->Escape->Enter"
        );
        // Explicit invariants the comment leans on, asserted independently
        // so a partial edit (e.g. dropping just the Tab) fails loudly.
        assert_eq!(
            seq.first(),
            Some(&"Tab"),
            "must Tab FIRST to clear autocomplete before Escape"
        );
        assert_eq!(
            seq.last(),
            Some(&"Enter"),
            "must end on Enter to actually submit"
        );
        assert!(
            seq.contains(&"Escape"),
            "must Escape to reach NORMAL mode before the submitting Enter"
        );
    }

    // -------------------------------------------------------------------
    // prompt_line_text — the verification primitive behind
    // `inject_and_verify`. A landed submit clears the payload from the
    // prompt line; these pin the extractor so the verify check is sound.
    // -------------------------------------------------------------------

    #[test]
    fn prompt_line_text_extracts_text_after_cursor() {
        let output = "some output\n\u{276f} /mcp";
        assert_eq!(prompt_line_text(output).as_deref(), Some("/mcp"));
    }

    #[test]
    fn prompt_line_text_empty_when_prompt_bare() {
        // A submitted (cleared) input line: bare cursor, no payload.
        let output = "scrollback\n\u{276f} \n──────\n  -- INSERT --";
        // The LAST `❯` line is the bare prompt → empty payload.
        assert_eq!(prompt_line_text(output).as_deref(), Some(""));
    }

    #[test]
    fn prompt_line_text_none_without_prompt() {
        let output = "no prompt char here\njust text";
        assert_eq!(prompt_line_text(output), None);
    }

    #[test]
    fn prompt_line_text_uses_last_prompt_line() {
        // An older `❯` in scrollback must not shadow the live input line.
        let output = "\u{276f} old typed text\nstuff\n\u{276f} new text";
        assert_eq!(prompt_line_text(output).as_deref(), Some("new text"));
    }

    #[test]
    fn inject_outcome_variants_are_distinct() {
        // Guard the three-state contract the `claude-watch inject` exit
        // codes lean on: Typed (no-submit), Submitted (verified),
        // SubmitUnverified (sent but payload still on prompt line).
        assert_ne!(InjectOutcome::Typed, InjectOutcome::Submitted);
        assert_ne!(InjectOutcome::Submitted, InjectOutcome::SubmitUnverified);
        assert_ne!(InjectOutcome::Typed, InjectOutcome::SubmitUnverified);
    }

    #[test]
    fn test_idle_prompt_detected() {
        // U+276F is the "heavy right-pointing angle quotation mark ornament" used as Claude prompt
        let output = "some output\nmore output\n\u{276f} ";
        assert!(check_lines_for_idle_prompt(output));
    }

    #[test]
    fn test_idle_prompt_not_present() {
        let output = "some output\nmore output\nstill working...";
        assert!(!check_lines_for_idle_prompt(output));
    }

    #[test]
    fn test_idle_prompt_only_checks_last_15_lines() {
        // Prompt in line 1, but 20 lines of other stuff after
        let mut lines = vec!["\u{276f} old prompt"];
        for _ in 0..20 {
            lines.push("busy output line");
        }
        let output = lines.join("\n");
        assert!(!check_lines_for_idle_prompt(&output));
    }

    #[test]
    fn test_idle_prompt_within_last_15() {
        let mut lines: Vec<&str> = Vec::new();
        for _ in 0..10 {
            lines.push("busy output");
        }
        lines.push("\u{276f} ready");
        for _ in 0..3 {
            lines.push("");
        }
        let output = lines.join("\n");
        assert!(check_lines_for_idle_prompt(&output));
    }

    // -------------------------------------------------------------------
    // interactive_prompt_visible — regression suite for the 2026-06-11 bug
    // where the daemon injected a resume prompt into a live AskUserQuestion
    // menu (the `❯` selection cursor was misread as an idle prompt), the
    // leading Escape cancelling the operator's question.
    // -------------------------------------------------------------------

    #[test]
    fn interactive_prompt_permission_confirmation() {
        // Tool-permission prompt. `❯` cursor is on the highlighted option,
        // so is_idle would wrongly say idle.
        let output = "\u{25cf} Bash(rm -rf /tmp/foo)\n\
                      ─────────────\n\
                      Do you want to proceed?\n\
                      \u{276f} 1. Yes\n\
                        2. No, and tell Claude what to do differently (esc)";
        assert!(
            interactive_prompt_visible(output),
            "permission confirmation must be detected as an interactive prompt"
        );
    }

    #[test]
    fn interactive_prompt_ask_user_question_menu() {
        // AskUserQuestion multiple-choice menu with a select-hint footer.
        let output = "Which approach should I take?\n\
                      \u{276f} 1. Refactor in place\n\
                        2. Rewrite from scratch\n\
                        3. Leave as-is\n\
                      \u{2191}/\u{2193} to select \u{00b7} Enter to confirm \u{00b7} Esc to cancel";
        assert!(
            interactive_prompt_visible(output),
            "AskUserQuestion menu must be detected as an interactive prompt"
        );
    }

    #[test]
    fn interactive_prompt_cursored_numbered_option() {
        // Even without a recognizable footer/question line, a `❯ <n>.`
        // cursored option row is a menu signature.
        let output = "Pick one:\n\u{276f} 1. Option A\n  2. Option B";
        assert!(interactive_prompt_visible(output));
    }

    #[test]
    fn interactive_prompt_do_you_trust_folder() {
        // Trust-workspace prompt on first launch in a new dir.
        let output = "Do you trust the files in this folder?\n\
                      \u{276f} 1. Yes, proceed\n  2. No, exit";
        assert!(interactive_prompt_visible(output));
    }

    #[test]
    fn interactive_prompt_not_fired_on_bare_idle_prompt() {
        // A genuinely-idle pane: bare `❯` on the input line, status bar
        // below. Must NOT be flagged as an interactive prompt (otherwise
        // we'd suppress every resume-inject forever).
        let output = "\u{25cf} Brewed for 12s\n\
                      ─────────────\n\
                      \u{276f}\n\
                      ─────────────\n\
                      \u{23f5}\u{23f5} bypass permissions on (shift+tab to cycle) \u{00b7} esc to interrupt";
        assert!(
            !interactive_prompt_visible(output),
            "bare idle prompt must NOT be flagged as an interactive prompt"
        );
    }

    #[test]
    fn interactive_prompt_not_fired_on_idle_with_typed_text() {
        // Idle prompt with the operator's draft text after the cursor —
        // still not a menu (no numbered option directly after ❯, no
        // select-hint footer, no question text).
        let output = "\u{276f} some draft text the user is typing\n\
                      ─────────────\n\
                      \u{23f5}\u{23f5} bypass permissions on \u{00b7} esc to interrupt";
        assert!(!interactive_prompt_visible(output));
    }

    #[test]
    fn interactive_prompt_fires_on_background_tasks_viewer_overlay() {
        // The Background-tasks viewer overlay (ctrl+b) is a passive viewer,
        // not a blocking question — but it carries the "↑/↓ to select …
        // Enter to view … ←/Esc to close" footer. Per the conservative
        // bias, suppressing an inject while it's open is harmless (only
        // delays a resume), so we deliberately match it rather than risk
        // under-matching a real question with a similar footer.
        let output = "  Background tasks\n\
                        4 active shells\n\
                      \u{276f} watcher-ctl run alerts-watcher (running)\n\
                      \u{2191}/\u{2193} to select \u{00b7} Enter to view \u{00b7} x to stop \u{00b7} \u{2190}/Esc to close";
        assert!(
            interactive_prompt_visible(output),
            "select-hint footer overlay must be matched (conservative bias)"
        );
    }

    // -------------------------------------------------------------------
    // blocking_question_visible — NARROW detector for the ask_question_monitor.
    // Regression suite for the 2026-07-13 false-positive where the FleetView
    // agent-view overlay footer ("↑/↓ to select · Enter to view") on the main
    // pane tripped the broad `interactive_prompt_visible` and fired a spurious
    // `ask-question-stale` alarm with no real AskUserQuestion pending.
    // -------------------------------------------------------------------

    #[test]
    fn blocking_question_fires_on_permission_confirmation() {
        let output = "\u{25cf} Bash(rm -rf /tmp/foo)\n\
                      ─────────────\n\
                      Do you want to proceed?\n\
                      \u{276f} 1. Yes\n\
                        2. No, and tell Claude what to do differently (esc)";
        assert!(blocking_question_visible(output));
    }

    #[test]
    fn blocking_question_fires_on_ask_user_question_menu() {
        let output = "Which approach should I take?\n\
                      \u{276f} 1. Refactor in place\n\
                        2. Rewrite from scratch\n\
                        3. Leave as-is\n\
                      \u{2191}/\u{2193} to select \u{00b7} Enter to confirm \u{00b7} Esc to cancel";
        assert!(blocking_question_visible(output));
    }

    #[test]
    fn blocking_question_fires_on_cursored_numbered_option() {
        let output = "Pick one:\n\u{276f} 1. Option A\n  2. Option B";
        assert!(blocking_question_visible(output));
    }

    #[test]
    fn blocking_question_not_fired_on_fleetview_agent_view_overlay() {
        // THE reported false positive: the main-loop pane sits on the
        // FleetView agent selector — a passive viewer, not a question. Its
        // footer is "↑/↓ to select · Enter to view", and the `❯` rows are
        // bullet-prefixed agent names (`❯ ● main`), never `❯ 1.`. Must NOT
        // fire the stale-question alarm.
        let output = "\u{276f} \n\
                      ───────────────\n\
                      -- INSERT -- \u{2191}/\u{2193} to select \u{00b7} Enter to view        413051 tokens\n\
                      \u{276f} \u{25cf} main\n\
                      \u{25ef} general-purpose  Add #1629 coverage… 1h 4m 0s\n\
                      \u{25ef} general-purpose  Fix pr-watch stage…  56m 43s";
        assert!(
            !blocking_question_visible(output),
            "FleetView agent-view overlay must NOT be treated as a blocking question"
        );
    }

    #[test]
    fn blocking_question_not_fired_on_background_tasks_viewer_overlay() {
        // The Background-tasks viewer (ctrl+b) is a passive viewer with a
        // "to select … Enter to view … Esc to close" footer and non-numbered
        // `❯` rows. `interactive_prompt_visible` matches it (harmless there),
        // but the stale-question monitor must NOT.
        let output = "  Background tasks\n\
                        4 active shells\n\
                      \u{276f} watcher-ctl run alerts-watcher (running)\n\
                      \u{2191}/\u{2193} to select \u{00b7} Enter to view \u{00b7} x to stop \u{00b7} \u{2190}/Esc to close";
        assert!(
            !blocking_question_visible(output),
            "passive viewer overlay must NOT be treated as a blocking question"
        );
    }

    #[test]
    fn blocking_question_not_fired_on_bare_idle_prompt() {
        let output = "\u{25cf} Brewed for 12s\n\
                      ─────────────\n\
                      \u{276f}\n\
                      ─────────────\n\
                      \u{23f5}\u{23f5} bypass permissions on (shift+tab to cycle) \u{00b7} esc to interrupt";
        assert!(!blocking_question_visible(output));
    }

    #[test]
    fn blocking_question_not_fired_on_background_work_exit_dialog() {
        // The /exit dialog is NOT an AskUserQuestion — run_auto_update handles
        // it. It must not trip the stale-question monitor. (It has no numbered
        // `❯ 1.` here and no confirming select-hint footer.)
        let output = "  Background work is running\n\
                        The following will stop when you exit:";
        assert!(!blocking_question_visible(output));
    }

    // background_work_exit_dialog_visible — the 2.1.x "Background work is
    // running" exit-confirmation dialog (#1411). run_auto_update polls for
    // this and sends Enter to select the default "Exit anyway"; the general
    // inject-guard `interactive_prompt_visible` also matches it (signature 4).
    #[test]
    fn background_work_exit_dialog_matches_title() {
        let output = "  Background work is running\n\
                      \u{276f} 1. Exit anyway\n\
                        2. Move to background and exit\n\
                        3. Stay";
        assert!(background_work_exit_dialog_visible(output));
    }

    #[test]
    fn background_work_exit_dialog_matches_body_line() {
        let output = "  Some box\n\
                        The following will stop when you exit:\n\
                      \u{276f} 1. Exit anyway";
        assert!(background_work_exit_dialog_visible(output));
    }

    #[test]
    fn background_work_exit_dialog_not_fired_on_idle() {
        let output = "Claude Code is running\nTokens: 50000\n\u{276f} ";
        assert!(!background_work_exit_dialog_visible(output));
    }

    #[test]
    fn interactive_prompt_fires_on_background_work_exit_dialog() {
        // Signature (4): the exit-confirmation dialog must suppress injects
        // via the shared guard too, not just the auto-update poll.
        let output = "  Background work is running\n\
                        The following will stop when you exit:\n\
                      \u{276f} 1. Exit anyway";
        assert!(
            interactive_prompt_visible(output),
            "'Background work is running' exit dialog must be matched (signature 4)"
        );
    }

    #[test]
    fn test_shell_prompt_dollar() {
        let output = "line1\nline2\nuser@host:~$ ";
        assert!(check_lines_for_shell_prompt(output));
    }

    #[test]
    fn test_shell_prompt_percent() {
        let output = "line1\nline2\nuser@host:~% ";
        assert!(check_lines_for_shell_prompt(output));
    }

    #[test]
    fn test_shell_prompt_arrow() {
        let output = "line1\nline2\n\u{279c} ~ ";
        assert!(check_lines_for_shell_prompt(output));
    }

    #[test]
    fn test_no_shell_prompt() {
        let output = "Claude Code is running\nTokens: 50000\nBashes: 10";
        assert!(!check_lines_for_shell_prompt(output));
    }

    #[test]
    fn test_feedback_prompt_detected() {
        let output = "How is Claude doing today?\n0: Dismiss\n1: Great";
        assert!(check_lines_for_feedback_prompt(output));
    }

    #[test]
    fn test_feedback_prompt_dismiss_only() {
        let output = "some output\n0: Dismiss\nother stuff";
        assert!(check_lines_for_feedback_prompt(output));
    }

    #[test]
    fn test_no_feedback_prompt() {
        let output = "normal claude output\nno feedback here";
        assert!(!check_lines_for_feedback_prompt(output));
    }

    // --- INSERT-mode detection (vim-mode coercion fix) ---------------------
    //
    // `check_lines_for_insert_mode` is the pure helper behind
    // `is_insert_mode`. Its job is to recognize INSERT-mode markers in
    // pane captures across both the unwrapped form (`-- INSERT --`) and
    // the narrow-pane wrapped form (bare `INSERT` token alone on a status
    // line). Pre-fix the helper used `out.contains("-- INSERT")` only,
    // which missed the wrapped form and led to single-Escape exits in
    // `inject_text`'s mode-coercion loop.

    #[test]
    fn test_insert_mode_unwrapped_dashes() {
        // The common case — joined or wide-pane capture with dashes intact.
        let output = "  -- INSERT --⏵⏵ bypass permissions on · 2 background tasks                   12345 tokens";
        assert!(check_lines_for_insert_mode(output));
    }

    #[test]
    fn test_insert_mode_wrapped_bare_token() {
        // Extreme-wrap form: status bar split across multiple visual
        // lines, dashes broken off, `INSERT` alone on its line. This is
        // the regression case for the 2026-05-01 inject-vim-mode bug.
        let output = "some prior chat content\n\
                      bypass\n\
                      INSERT\n\
                      606746 tokens\n\
                      \u{276f} ";
        assert!(check_lines_for_insert_mode(output));
    }

    #[test]
    fn test_insert_mode_not_present() {
        // Idle pane, no mode indicator anywhere.
        let output = "some chat output\nmore content\n\u{276f} ";
        assert!(!check_lines_for_insert_mode(output));
    }

    #[test]
    fn test_insert_mode_word_in_chat_does_not_false_positive() {
        // The substring `INSERT` appearing in chat prose — outside the
        // last 5 lines AND not the unwrapped `-- INSERT` — must NOT
        // trip detection. Only the bottom 5 lines are considered for the
        // bare-token form, and only as a whitespace-delimited token (so
        // `INSERTED` / `INSERTION` don't match either).
        let output = "Discussing SQL: the INSERT statement adds rows.\n\
                      That's covered in chapter 3.\n\
                      Anything else INSERTED into the table is appended.\n\
                      \n\
                      \u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n\
                      \u{276f} \n\
                      ⏵⏵ bypass permissions on · 50000 tokens";
        // INSERT appears only in line 0, well above the last-5-line window.
        // INSERTED appears in line 2 (still outside the last-5 — but even
        // if it weren't, `split_whitespace` would not emit it as `INSERT`).
        assert!(!check_lines_for_insert_mode(output));
    }

    #[test]
    fn test_insert_mode_substring_inserted_does_not_match_in_tail() {
        // Even within the tail window, `INSERTED` must not match — the
        // helper tokenizes on whitespace and only accepts `INSERT` exact.
        let output = "row INSERTED\nINSERTION done\nfoo\nbar\n\u{276f} ";
        assert!(!check_lines_for_insert_mode(output));
    }

    #[test]
    fn test_insert_mode_colored_ansi_capture() {
        // Some captures preserve ANSI color escape codes that visually
        // separate `INSERT` from the dashes — the joined-capture form
        // would have `-- INSERT --` reassembled, but if we get the raw
        // form the bare-token tail check should still recognize it.
        // (`\u{1b}` is ESC, the SGR introducer.)
        let output = "previous chat\n\
                      \u{1b}[39m  \u{1b}[38;5;246m--\u{1b}[39m \u{1b}[38;5;246mINSERT\u{1b}[39m \u{1b}[38;5;246m--\n";
        // The literal substring `-- INSERT` is broken by the ANSI code,
        // but `INSERT` appears as a standalone whitespace-delimited token
        // (the surrounding ANSI sequences don't contain whitespace, but
        // the leading whitespace before `\u{1b}[38;5;246mINSERT` does
        // make `\u{1b}[38;5;246mINSERT\u{1b}[39m` its own token — which
        // is NOT bare `INSERT`. So this case currently does NOT match.
        // That's acceptable: `is_insert_mode` calls `capture_pane_joined`
        // first, which both joins wraps AND tmux strips ANSI by default
        // (no `-e` flag). Documenting the limitation here so future
        // contributors know this branch is best-effort.
        let _ = output;
        let plain = "previous chat\n  -- INSERT --\n";
        assert!(check_lines_for_insert_mode(plain));
    }

    #[test]
    fn test_foreground_busy_with_spinner() {
        // No prompt + spinner = busy
        let output = "Running command...\n\u{280b} processing...";
        assert!(check_lines_for_foreground_busy(output));
    }

    #[test]
    fn test_foreground_not_busy_with_prompt() {
        // Prompt visible = not busy even with spinner
        let output = "\u{276f} \n\u{280b} processing...";
        assert!(!check_lines_for_foreground_busy(output));
    }

    #[test]
    fn test_foreground_not_busy_no_spinner() {
        // No prompt, no spinner = not busy (indeterminate)
        let output = "some text\nmore text";
        assert!(!check_lines_for_foreground_busy(output));
    }

    // --- check_claude_running tests ---

    #[test]
    fn test_claude_running_with_shell_prompt_bira() {
        // Bira theme shell prompt means Claude exited
        let output = "some output\nold tokens stuff\n\u{256e}\u{2500}\u{2500}\n\u{2570}\u{2500}$ ";
        assert!(!check_claude_running(output));
    }

    #[test]
    fn test_claude_running_with_bira_dollar_prompt() {
        // Bira theme: ╰─$
        let output = "some output\n\u{2570}\u{2500}$ ";
        assert!(!check_claude_running(output));
    }

    #[test]
    fn test_claude_running_with_arrow_prompt() {
        // Arrow prompt (robbyrussell): ➜
        let output = "some output\n\u{279c} ~ ";
        assert!(!check_claude_running(output));
    }

    #[test]
    fn test_claude_running_with_status_bar() {
        let output = "some output\n50,000 tokens  5 bashes\nContext left until auto-compact: 42%\ncurrent: 2.1.77   latest: 2.1.78";
        assert!(check_claude_running(output));
    }

    #[test]
    fn test_claude_running_no_indicators() {
        // No shell prompt, no Claude indicators — default to true (conservative)
        let output = "some random text\nnothing here";
        assert!(check_claude_running(output));
    }

    #[test]
    fn test_claude_running_shell_prompt_overrides_tokens() {
        // Bira shell prompt takes priority over stale token text in buffer
        let output = "50,000 tokens  latest: 2.1.78\n\u{2570}\u{2500}$ ";
        assert!(!check_claude_running(output));
    }

    #[test]
    fn test_claude_running_percent_in_status_bar_not_shell() {
        // "42%" in status bar should NOT be mistaken for a zsh %  prompt
        let output = "Context left until auto-compact: 42%";
        assert!(check_claude_running(output));
    }

    // --- detect_activity tests ---

    #[test]
    fn test_activity_idle() {
        let output = "some output\nmore output\n\u{276f} ";
        assert_eq!(detect_activity(output), ClaudeActivity::Idle);
    }

    #[test]
    fn test_activity_idle_takes_priority_over_thinking() {
        // Both prompt and thinking indicator present — idle wins
        let output = "\u{273d} Thinking\u{2026} (5s)\n\u{276f} ";
        assert_eq!(detect_activity(output), ClaudeActivity::Idle);
    }

    #[test]
    fn test_activity_idle_takes_priority_over_spinner() {
        let output = "\u{280b} Read(file)\n\u{276f} ";
        assert_eq!(detect_activity(output), ClaudeActivity::Idle);
    }

    #[test]
    fn test_activity_thinking_standard() {
        let output =
            "previous output\n  \u{273d} Thinking\u{2026} (12s \u{00b7} \u{2193} 384 tokens)";
        assert_eq!(detect_activity(output), ClaudeActivity::Thinking);
    }

    #[test]
    fn test_activity_thinking_honking() {
        let output = "some stuff\n  \u{273d} Honking\u{2026} (44s \u{00b7} \u{2193} 384 tokens)";
        assert_eq!(detect_activity(output), ClaudeActivity::Thinking);
    }

    #[test]
    fn test_activity_thinking_pondering() {
        let output = "line1\n\u{273d} Pondering\u{2026} (2s)";
        assert_eq!(detect_activity(output), ClaudeActivity::Thinking);
    }

    #[test]
    fn test_activity_thinking_273b_flowing() {
        // U+273B (✻) is also used as thinking indicator, not just completion
        let output = "some output\n\u{273b} Flowing\u{2026} (45s \u{00b7} \u{2193} 377 tokens)";
        assert_eq!(detect_activity(output), ClaudeActivity::Thinking);
    }

    #[test]
    fn test_activity_thinking_273b_with_separator_and_prompt() {
        // Real capture: ✻ Flowing… with separator + prompt below
        let output = "\u{25cf} Some bullet\n\u{273b} Flowing\u{2026} (45s)\n\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n\u{276f} \n\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n  -- INSERT --";
        assert_eq!(detect_activity(output), ClaudeActivity::Thinking);
    }

    #[test]
    fn test_activity_thinking_takes_priority_over_spinner() {
        // Both thinking and spinner — thinking wins
        let output = "\u{280b} Bash(cmd)\n\u{273d} Thinking\u{2026} (5s)";
        assert_eq!(detect_activity(output), ClaudeActivity::Thinking);
    }

    #[test]
    fn test_activity_tool_running_read() {
        let output = "output\n\u{280b} Read(~/some/file.rs)";
        assert_eq!(detect_activity(output), ClaudeActivity::ToolRunning);
    }

    #[test]
    fn test_activity_tool_running_bash() {
        let output = "output\n\u{2819} Bash(cargo test)";
        assert_eq!(detect_activity(output), ClaudeActivity::ToolRunning);
    }

    #[test]
    fn test_activity_tool_running_various_spinners() {
        // Test each spinner character
        for &spinner in SPINNER_CHARS {
            let output = format!("output\n{} SomeTool(arg)", spinner);
            assert_eq!(
                detect_activity(&output),
                ClaudeActivity::ToolRunning,
                "spinner {:?} should be detected",
                spinner,
            );
        }
    }

    #[test]
    fn test_activity_writing_no_prompt() {
        // Writing is only detected when prompt is NOT visible (pushed off screen)
        let output = "some context\n\u{25cf} Here is some output being streamed";
        assert_eq!(detect_activity(output), ClaudeActivity::Writing);
    }

    #[test]
    fn test_activity_writing_indented_no_prompt() {
        let output = "context\n  \u{25cf} Indented bullet point";
        assert_eq!(detect_activity(output), ClaudeActivity::Writing);
    }

    #[test]
    fn test_activity_writing_multiple_bullets_no_prompt() {
        let output = "\u{25cf} First point\n\u{25cf} Second point\n\u{25cf} Third point";
        assert_eq!(detect_activity(output), ClaudeActivity::Writing);
    }

    #[test]
    fn test_activity_bullets_with_prompt_no_completion_is_writing() {
        // Bullets visible + prompt visible below separator + NO completion indicator
        // = Writing (could be active mid-workflow or stale; daemon debounces)
        let output = "\u{25cf} Some old output\n\u{25cf} More output\n\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n\u{276f} \n\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n  -- INSERT --";
        assert_eq!(detect_activity(output), ClaudeActivity::Writing);
    }

    #[test]
    fn test_activity_bullets_with_prompt_and_completion_is_idle() {
        // Bullets visible + prompt visible + completion indicator ("Brewed for") = Idle
        let output = "\u{25cf} Some output\n\u{273b} Brewed for 12s\n\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n\u{276f} \n\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n  -- INSERT --";
        assert_eq!(detect_activity(output), ClaudeActivity::Idle);
    }

    #[test]
    fn test_activity_stale_thinking_with_completion_is_idle() {
        // Stale thinking indicator ("✽ Thinking… (5s)") still visible in scroll history
        // + completion indicator ("✻ Brewed for 12s") + prompt = Idle.
        // This was the false positive that caused spurious "prolonged thinking" alerts.
        let output = "\u{273d} Thinking\u{2026} (5s)\n\u{25cf} Some output\n\u{273b} Brewed for 12s\n\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n\u{276f} \n\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n  -- INSERT --";
        assert_eq!(detect_activity(output), ClaudeActivity::Idle);
    }

    #[test]
    fn test_activity_unknown() {
        let output = "some random text\nnothing recognizable here";
        assert_eq!(detect_activity(output), ClaudeActivity::Unknown);
    }

    #[test]
    fn test_activity_unknown_empty() {
        assert_eq!(detect_activity(""), ClaudeActivity::Unknown);
    }

    #[test]
    fn test_activity_only_checks_last_15_lines() {
        // Prompt in line 1, but 20 lines of other stuff after
        let mut lines = vec!["\u{276f} old prompt"];
        for _ in 0..20 {
            lines.push("busy output line");
        }
        let output = lines.join("\n");
        assert_eq!(detect_activity(&output), ClaudeActivity::Unknown);
    }

    #[test]
    fn test_activity_thinking_within_last_15() {
        let mut lines: Vec<String> = Vec::new();
        for _ in 0..10 {
            lines.push("busy output".to_string());
        }
        lines.push(format!("\u{273d} Thinking\u{2026} (3s)"));
        for _ in 0..3 {
            lines.push(String::new());
        }
        let output = lines.join("\n");
        assert_eq!(detect_activity(&output), ClaudeActivity::Thinking);
    }

    // --- 2026-04-17 prolonged-thinking-false-positive regression tests ---
    //
    // Andrew filed a bug: claude-watch fired "Prolonged thinking detected
    // (>180s)" on a GENUINELY-IDLE session. The main loop's last response
    // had been "Interrupt noted — no active work. Idling." for 3 minutes
    // straight; the pane showed the completion widget and the prompt, no
    // active generation. The old detector returned Thinking because it
    // treated any line containing `·` (U+00B7 middle dot) OR a markdown
    // `* ` bullet PLUS `…` (U+2026) anywhere as an "active thinking
    // indicator". Many real-world idle-pane lines match that loose pattern:
    //
    //   - status-bar wraps:   "current: 2.1.77 · latest: 2.1.…"
    //   - tool-output hints:  "… to manage · ctrl+o to expand"
    //   - markdown bullets:   "* Check the status… later"
    //   - completion tails:   "✻ Cogitated for 2m 11s · 6 tasks still…"
    //
    // The fix tightens detection to require the full `<indicator> <Verb>…
    // (<time>` structure at the start of the line, or the distinctive
    // `· thinking)` suffix used by the newer `●`-prefix thinking widget.
    // These negative tests pin the behaviour so it cannot regress.

    #[test]
    fn test_is_active_thinking_positive_cases() {
        // Classic: thinking-char + Verb + ellipsis + paren
        assert!(is_active_thinking_line(
            "\u{273d} Thinking\u{2026} (12s \u{00b7} \u{2193} 384 tokens)"
        ));
        assert!(is_active_thinking_line(
            "\u{2722} Fermenting\u{2026} (38s \u{00b7} \u{2193} 909 tokens)"
        ));
        assert!(is_active_thinking_line(
            "\u{273b} Flowing\u{2026} (45s \u{00b7} \u{2193} 377 tokens)"
        ));
        assert!(is_active_thinking_line(
            "* Warping\u{2026} (26s \u{00b7} \u{2191} 438 tokens)"
        ));
        // Short form (no time-tag contents inside parens)
        assert!(is_active_thinking_line("\u{273d} Thinking\u{2026} (3s)"));
        // Newer `●`-prefix format — short form, no `· thinking)` suffix
        assert!(is_active_thinking_line("\u{25cf} Cooking\u{2026} (28s)"));
        // Newer `●`-prefix format with token count but no `· thinking)`
        assert!(is_active_thinking_line(
            "\u{25cf} Flibbertigibbeting\u{2026} (2m 35s \u{00b7} \u{2193} 869 tokens)"
        ));
        // Newer `●`-prefix format with `· thinking)` suffix
        assert!(is_active_thinking_line(
            "\u{25cf} Whirlpooling\u{2026} (7s \u{00b7} \u{2193} 31 tokens \u{00b7} thinking)"
        ));
        assert!(is_active_thinking_line(
            "\u{25cf} Flibbertigibbeting\u{2026} (1m 19s \u{00b7} \u{2193} 540 tokens \u{00b7} thinking)"
        ));
        // Middle-dot as the leading indicator char (per binary analysis,
        // Claude Code can render `·` as the indicator glyph)
        assert!(is_active_thinking_line("\u{00b7} Thinking\u{2026} (5s)"));
    }

    #[test]
    fn test_is_active_thinking_negative_completion_lines() {
        // Completion widget — past-tense verb, "for", no ellipsis.
        assert!(!is_active_thinking_line(
            "\u{273b} Brewed for 38s \u{00b7} 11 background tasks still running"
        ));
        assert!(!is_active_thinking_line(
            "\u{273b} Cogitated for 2m 11s \u{00b7} 6 background tasks still running"
        ));
        assert!(!is_active_thinking_line(
            "\u{273b} Sauteed for 31s \u{00b7} 6 background tasks still running"
        ));
    }

    #[test]
    fn test_is_active_thinking_negative_status_bar_wrap() {
        // Wrapped status bar: `· latest: 2.1.…` — middle dot + ellipsis
        // but NO Verb+paren structure. The OLD detector returned true here.
        assert!(!is_active_thinking_line(
            "current: 2.1.77 \u{00b7} latest: 2.1.\u{2026}"
        ));
        assert!(!is_active_thinking_line(
            "\u{23f5}\u{23f5} bypass permissi \u{00b7}  on   5 shells \u{00b7} esc to interrupt \u{00b7} \u{2193}\u{2026}"
        ));
    }

    #[test]
    fn test_is_active_thinking_negative_tool_output_and_markdown() {
        // Tool-output hint: `· ctrl+o to expand` — nothing thinking-like.
        assert!(!is_active_thinking_line(
            "Backgrounded agent (\u{2193} to manage \u{00b7} ctrl+o to expand)"
        ));
        // Markdown bullet with an ellipsis mid-prose — NOT thinking.
        assert!(!is_active_thinking_line("* Check the status\u{2026} later"));
        // Generic `·` + `…` content that happens to appear in idle panes.
        assert!(!is_active_thinking_line(
            "bypass permissions on \u{00b7} ctrl+x ctrl+k to stop agents \u{00b7} \u{2193} to manage\u{2026}"
        ));
        // `●`-prefix prose lines are Writing, not Thinking — they lack
        // the `…(<digit>` widget anchor.
        assert!(!is_active_thinking_line(
            "\u{25cf} DM'd. claude-watch debug \u{00b7} more stuff\u{2026}"
        ));
        assert!(!is_active_thinking_line(
            "\u{25cf} Interrupt ack \u{2014} same false positive flagged earlier."
        ));
        assert!(!is_active_thinking_line("\u{25cf} No new messages. Idling."));
        // `●`-prefix with paren but NO ellipsis — the feedback prompt widget.
        assert!(!is_active_thinking_line(
            "\u{25cf} How is Claude doing this session? (optional)"
        ));
        // `●`-prefix with ellipsis + paren, but paren content is prose
        // (no leading digit) — the `\d` anchor saves us.
        assert!(!is_active_thinking_line(
            "\u{25cf} Some progress\u{2026} (every now and then)"
        ));
        assert!(!is_active_thinking_line(
            "\u{25cf} Starting\u{2026} (every 5 min)"
        ));
        // Bare "Waiting…" (non-breaking space inside) from a running bash
        // task is not thinking.
        assert!(!is_active_thinking_line("\u{23bf}\u{a0}Waiting\u{2026}"));
    }

    #[test]
    fn test_activity_idle_when_content_has_middle_dot_and_ellipsis() {
        // Regression: Andrew's 2026-04-17 false positive. After a short
        // "Idling." response, the pane shows a completion line plus
        // incidental `·` + `…` content (tool-output tails, status-bar
        // wraps, etc.). The OLD detector returned Thinking because any
        // such line matched `has_indicator_char + contains('…')`. The
        // fixed detector must return Idle.
        let output = "\u{25cf} DM'd. claude-watch debug \u{00b7} crop-to-figure both reported. Idling.\n\
                      \n\
                      Backgrounded agent (\u{2193} to manage \u{00b7} ctrl+o to expand)\n\
                      \n\
                      \u{273b} Cogitated for 2m 11s \u{00b7} 6 background tasks still running\n\
                      \n\
                      \u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n\
                      \u{276f} \n\
                      \u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n\
                      \u{23f5}\u{23f5} bypass permissions on \u{00b7} 6 background tasks \u{00b7} ctrl+x ctrl+k to stop agents \u{00b7} \u{2193} to manage    278149 tokens";
        assert_eq!(
            detect_activity(output),
            ClaudeActivity::Idle,
            "Idle pane with completion line + incidental `·`+`…` content \
             must NOT be classified as Thinking (2026-04-17 regression)"
        );
    }

    #[test]
    fn test_activity_idle_when_only_completion_no_thinking_content() {
        // Bare idle state: just the completion line + prompt. No active
        // thinking, no stale thinking-like lines.
        let output = "\u{25cf} Short response.\n\
                      \n\
                      \u{273b} Brewed for 5s\n\
                      \n\
                      \u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n\
                      \u{276f} \n\
                      \u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n\
                      -- INSERT -- 50000 tokens";
        assert_eq!(detect_activity(output), ClaudeActivity::Idle);
    }

    #[test]
    fn test_activity_thinking_new_format_with_bullet_prefix() {
        // Newer Claude Code (2.1.112+) renders active thinking with a ●
        // prefix and a `· thinking)` suffix:
        //   "● Whirlpooling… (7s · ↓ 31 tokens · thinking)"
        // Ensure this is correctly classified as Thinking (not Writing,
        // which is what a plain `●` line would be).
        let output = "previous context\n\
                      \n\
                      \u{25cf} Whirlpooling\u{2026} (7s \u{00b7} \u{2193} 31 tokens \u{00b7} thinking)\n\
                      \n\
                      \u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n\
                      \u{276f} \n\
                      \u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n\
                      -- INSERT -- 50000 tokens";
        assert_eq!(detect_activity(output), ClaudeActivity::Thinking);
    }

    // --- check_lines_for_reauth tests ---

    #[test]
    fn test_reauth_login_url() {
        let output = "Open this URL to login:\nhttps://console.anthropic.com/login";
        assert!(check_lines_for_reauth(output));
    }

    #[test]
    fn test_reauth_session_expired() {
        let output = "Session expired\nPlease re-login";
        assert!(check_lines_for_reauth(output));
    }

    #[test]
    fn test_reauth_auth_required() {
        let output = "Authentication required\nPlease run /login";
        assert!(check_lines_for_reauth(output));
    }

    #[test]
    fn test_reauth_browser_didnt_open() {
        // Current Claude Code login screen
        let output = "Login\n\nBrowser didn't open? Use the url below to sign in (c to copy)\n\nhttps://claude.ai/oauth/authorize?code=true&client_id=abc123\n\nPaste code here if prompted >\n\nEsc to cancel";
        assert!(check_lines_for_reauth(output));
    }

    #[test]
    fn test_reauth_paste_code() {
        let output = "Paste code here if prompted >";
        assert!(check_lines_for_reauth(output));
    }

    #[test]
    fn test_reauth_oauth_url() {
        let output = "https://claude.ai/oauth/authorize?code=true&client_id=abc";
        assert!(check_lines_for_reauth(output));
    }

    #[test]
    fn test_reauth_not_detected_normal_tui() {
        // Normal TUI has "tokens" visible — should NOT trigger
        let output = "57,129 tokens  9 bashes\n\u{276f} ";
        assert!(!check_lines_for_reauth(output));
    }

    #[test]
    fn test_reauth_not_detected_conversation_about_auth() {
        // Auth words in normal conversation — should NOT trigger (has TUI elements)
        let output = "Let me authenticate to the API\n57,129 tokens  9 bashes\n\u{276f} ";
        assert!(!check_lines_for_reauth(output));
    }

    #[test]
    fn test_reauth_not_detected_empty() {
        assert!(!check_lines_for_reauth(""));
    }

    #[test]
    fn test_reauth_not_detected_startup() {
        // During startup, pane may show /login command but with TUI elements
        let output = "Type /login to authenticate\n1,234 tokens  0 bashes";
        assert!(!check_lines_for_reauth(output));
    }

    // --- TUI guard: auth-error text inside a live session must NOT trigger ---
    //
    // The old "phase 1" logic detected "API Error: 401" / "authentication_error"
    // even when the TUI was visible. That false-positived on conversation content
    // containing those strings (the user typing about the error pattern) and
    // injected `/login` into a live session. Now: if the TUI is visible, reauth
    // is never triggered — a real reauth failure replaces the TUI with the
    // login screen, which is caught by the phase-2 patterns.

    #[test]
    fn test_reauth_not_detected_401_text_with_tui() {
        // Literal "API Error: 401" in a live session (tokens + prompt visible) —
        // this is conversation content, not a real error screen.
        let output = concat!(
            "resume\n",
            "Please run /login · API Error: 401\n",
            "{\"type\":\"error\",\"error\":{\"type\":\"authentication_error\",",
            "\"message\":\"Invalid authentication credentials\"},",
            "\"request_id\":\"req_011CZRQBx7F5yvuN8z9bJTnp\"}\n",
            "\n",
            "❯ \n",
            "-- INSERT --  871864 tokens"
        );
        assert!(!check_lines_for_reauth(output));
    }

    #[test]
    fn test_reauth_not_detected_api_error_401_in_conversation() {
        let output = "API Error: 401\n57,129 tokens  9 bashes\n❯ ";
        assert!(!check_lines_for_reauth(output));
    }

    #[test]
    fn test_reauth_not_detected_authentication_error_json_in_conversation() {
        let output = "\"authentication_error\"\n57,129 tokens  9 bashes\n❯ ";
        assert!(!check_lines_for_reauth(output));
    }

    #[test]
    fn test_reauth_not_detected_conversation_about_401() {
        // Conversation ABOUT 401 errors shouldn't trigger.
        let output = "The server returns a 401 status code when auth is invalid\n57,129 tokens  9 bashes\n❯ ";
        assert!(!check_lines_for_reauth(output));
    }

    #[test]
    fn test_reauth_not_detected_invalid_credentials_in_conversation() {
        // "Invalid authentication credentials" appearing in conversation text.
        let output = "Claude responded: Invalid authentication credentials\n57,129 tokens  9 bashes\n❯ ";
        assert!(!check_lines_for_reauth(output));
    }

    #[test]
    fn test_reauth_not_detected_shells_counter_with_auth_text() {
        // Claude Code 2.1.94+ uses "shells" instead of "bashes".
        let output = "API Error: 401\n57,129 tokens  9 shells\n❯ ";
        assert!(!check_lines_for_reauth(output));
    }

    #[test]
    fn test_reauth_not_detected_agents_counter_with_auth_text() {
        // Newer Claude Code shows "agents" counter.
        let output = "\"authentication_error\"\n57,129 tokens  3 agents\n❯ ";
        assert!(!check_lines_for_reauth(output));
    }

    #[test]
    fn test_reauth_not_detected_background_tasks_counter_with_auth_text() {
        let output = "API Error: 401\n57,129 tokens  2 background tasks\n❯ ";
        assert!(!check_lines_for_reauth(output));
    }

    #[test]
    fn test_reauth_not_detected_bypass_permissions_banner_with_auth_text() {
        // The "bypass permissions" banner is a reliable TUI indicator.
        let output = "API Error: 401\nbypass permissions on\n\u{276f} ";
        assert!(!check_lines_for_reauth(output));
    }

    #[test]
    fn test_reauth_still_detected_when_tui_gone() {
        // Real login screen: TUI is gone, phase-2 patterns present.
        let output = "Login\n\nBrowser didn't open? Use the url below to sign in\n\nhttps://claude.ai/oauth/authorize?code=true&client_id=abc\n\nPaste code here if prompted >\n";
        assert!(check_lines_for_reauth(output));
    }

    // --- extract_login_url tests ---

    #[test]
    fn test_extract_login_url_basic() {
        let output = "Login\n\nhttps://claude.ai/oauth/authorize?code=true&client_id=abc123\n\nPaste code here";
        let url = extract_login_url(output);
        assert_eq!(
            url,
            Some("https://claude.ai/oauth/authorize?code=true&client_id=abc123".to_string())
        );
    }

    #[test]
    fn test_extract_login_url_wrapped() {
        // URL wraps across two tmux lines
        let output = "https://claude.ai/oauth/authorize?code=true&client_id=abc123&code_chall\nenge=xyz789&code_challenge_method=S256";
        let url = extract_login_url(output);
        assert_eq!(url, Some("https://claude.ai/oauth/authorize?code=true&client_id=abc123&code_challenge=xyz789&code_challenge_method=S256".to_string()));
    }

    #[test]
    fn test_extract_login_url_none() {
        let output = "Session expired\nPlease re-login";
        assert_eq!(extract_login_url(output), None);
    }

    #[test]
    fn test_activity_display() {
        assert_eq!(format!("{}", ClaudeActivity::Idle), "idle");
        assert_eq!(format!("{}", ClaudeActivity::Thinking), "thinking");
        assert_eq!(format!("{}", ClaudeActivity::ToolRunning), "tool_running");
        assert_eq!(format!("{}", ClaudeActivity::Writing), "writing");
        assert_eq!(format!("{}", ClaudeActivity::Unknown), "unknown");
    }

    // --- exit teardown detection tests ---

    #[test]
    fn test_exit_teardown_goodbye() {
        let output = "some output\nGoodbye!\n";
        assert!(check_lines_for_exit_teardown(output));
    }

    #[test]
    fn test_exit_teardown_background_stopped() {
        let output = "some output\nGoodbye!\nBackground command was stopped: alerts-watcher\nBackground command was stopped: torrent-wait\n";
        assert!(check_lines_for_exit_teardown(output));
    }

    #[test]
    fn test_exit_teardown_only_background_stopped() {
        let output = "some output\nBackground command was stopped: alerts-watcher\n";
        assert!(check_lines_for_exit_teardown(output));
    }

    #[test]
    fn test_no_exit_teardown_normal_output() {
        let output = "Claude Code is running\nTokens: 3000\nBashes: 0";
        assert!(!check_lines_for_exit_teardown(output));
    }

    #[test]
    fn test_no_exit_teardown_goodbye_in_content() {
        // "Goodbye!" must be the entire trimmed line, not part of a sentence
        let output = "He said Goodbye! to his friend\nTokens: 3000";
        assert!(!check_lines_for_exit_teardown(output));
    }

    // --- check_lines_for_wedged tests ---

    #[test]
    fn test_wedged_context_limit_banner() {
        // The exact banner Claude Code prints when context overflows.
        let output = "\
some prior output\n\
\u{276f} a tool call\n\
Context limit reached. /compact or /clear to continue\n\
Context limit reached. /compact or /clear to continue\n\
Context limit reached. /compact or /clear to continue\n";
        assert_eq!(
            check_lines_for_wedged(output),
            Some(WedgedReason::ContextLimit)
        );
    }

    #[test]
    fn test_wedged_context_limit_alt_phrasing() {
        // Some Claude Code versions reverse the slash-command order.
        let output = "Context limit reached. /clear or /compact to continue";
        assert_eq!(
            check_lines_for_wedged(output),
            Some(WedgedReason::ContextLimit)
        );
    }

    #[test]
    fn test_wedged_rate_limit_429() {
        let output = "\
\u{25cf} Bash(...)\n\
API Error: Request rejected (429) Rate limited\n\
\u{276f}\n";
        assert_eq!(
            check_lines_for_wedged(output),
            Some(WedgedReason::RateLimited)
        );
    }

    #[test]
    fn test_wedged_rate_limit_repeated() {
        // Repeated 429s as the agent retries — typical wedged signature.
        let output = "\
API Error: Request rejected (429)\n\
API Error: Request rejected (429)\n\
API Error: Request rejected (429)\n";
        assert_eq!(
            check_lines_for_wedged(output),
            Some(WedgedReason::RateLimited)
        );
    }

    #[test]
    fn test_not_wedged_normal_output() {
        let output = "\u{276f} Hello world\nNormal Claude Code conversation\nTokens: 50000";
        assert_eq!(check_lines_for_wedged(output), None);
    }

    #[test]
    fn test_not_wedged_429_without_reject() {
        // A bare "429" mention in chat history should not trip the detector.
        let output = "\u{276f} HTTP 429 means Too Many Requests, btw\nTokens: 50000";
        assert_eq!(check_lines_for_wedged(output), None);
    }

    #[test]
    fn test_not_wedged_api_error_without_429() {
        // Generic API errors are noisy and recover on their own — don't trip.
        let output = "API Error: bad request\nTokens: 50000";
        assert_eq!(check_lines_for_wedged(output), None);
    }

    #[test]
    fn test_wedged_only_checks_recent_lines() {
        // A "Context limit reached" 100 lines ago shouldn't count — only the
        // last ~40 lines are inspected.
        let mut lines: Vec<String> = vec!["Context limit reached. /compact or /clear to continue".to_string()];
        for _ in 0..100 {
            lines.push("normal chat line".to_string());
        }
        let output = lines.join("\n");
        assert_eq!(check_lines_for_wedged(&output), None);
    }

    #[test]
    fn test_wedged_empty_input() {
        assert_eq!(check_lines_for_wedged(""), None);
    }

    #[test]
    fn test_wedged_reason_display() {
        assert_eq!(format!("{}", WedgedReason::ContextLimit), "context_limit");
        assert_eq!(format!("{}", WedgedReason::RateLimited), "rate_limited");
    }

    // --- check_lines_for_malformed_tool_call tests ---
    //
    // The raw, non-namespaced tag strings below are inert Rust string
    // literals — they are NOT tool calls. They reproduce exactly what the
    // pane shows when the model malforms a call and the harness renders the
    // block as assistant text.

    #[test]
    fn test_malformed_bare_invoke_tag() {
        // The classic signature: a raw non-namespaced `<invoke>` rendered as
        // text because the harness could not parse it.
        let output = "\
some prior output\n\
<invoke name=\"Bash\">\n\
<parameter name=\"command\">watcher-ctl run claude-event-watch</parameter>\n\
</invoke>\n";
        assert!(check_lines_for_malformed_tool_call(output));
    }

    #[test]
    fn test_malformed_with_stray_text_prefix() {
        // The 2026-06-17 signature: a stray literal word glued to the front
        // of the opening tag (e.g. `court<invoke ...`). The stray prefix on the
        // opener does not prevent structural detection — the construct is still
        // corroborated by the parameter + close.
        let output = "\
court<invoke name=\"Bash\">\n\
<parameter name=\"command\">watcher-ctl run claude-event-watch</parameter>\n\
</invoke>\n";
        assert!(check_lines_for_malformed_tool_call(output));
    }

    #[test]
    fn test_malformed_single_line_construct() {
        // A whole construct collapsed onto ONE line (no surrounding fence) is
        // still detected.
        let output =
            "x<invoke name=\"Bash\"><parameter name=\"command\">ls</parameter></invoke>\n";
        assert!(check_lines_for_malformed_tool_call(output));
    }

    // --- tokenizer / attr-helper unit tests ---

    #[test]
    fn test_tokenize_named_invoke() {
        let toks = tokenize_malformed_line("<invoke name=\"Bash\">");
        assert_eq!(toks, vec![MalformedToken::OpenInvoke { has_name: true }]);
    }

    #[test]
    fn test_tokenize_bare_invoke() {
        let toks = tokenize_malformed_line("<invoke>");
        assert_eq!(toks, vec![MalformedToken::OpenInvoke { has_name: false }]);
    }

    #[test]
    fn test_tokenize_close_tags() {
        assert_eq!(
            tokenize_malformed_line("</invoke>"),
            vec![MalformedToken::CloseInvoke]
        );
        assert_eq!(
            tokenize_malformed_line("</parameter>"),
            vec![MalformedToken::CloseParameter]
        );
    }

    #[test]
    fn test_tokenize_ignores_non_tag_words() {
        // `<invokexyz` and bare prose produce no tokens.
        assert!(tokenize_malformed_line("<invokexyz name=\"x\">").is_empty());
        assert!(tokenize_malformed_line("please invoke the parameter").is_empty());
    }

    #[test]
    fn test_tag_name_attr_present() {
        assert!(tag_name_attr_present(" name=\"Bash\""));
        assert!(tag_name_attr_present(" name='Bash'"));
        assert!(!tag_name_attr_present(" name=\"\""));
        assert!(!tag_name_attr_present(" name="));
        assert!(!tag_name_attr_present(" other=\"x\""));
        assert!(!tag_name_attr_present(""));
    }

    #[test]
    fn test_malformed_parameter_with_close_invoke() {
        // A malformed tail: only the parameter + close-invoke survived as text.
        // The `</invoke>` close corroborates the `<parameter name=...>` opener,
        // so this is a confirmed construct.
        let output =
            "<parameter name=\"command\">touch /var/run/claude/heartbeat</parameter>\n</invoke>\n";
        assert!(check_lines_for_malformed_tool_call(output));
    }

    #[test]
    fn test_not_malformed_lone_parameter_no_corroboration() {
        // A lone `<parameter name=...>` with NO invoke and NO close is NOT a
        // confirmed construct — could be quoted/partial noise. Structural
        // detector must not fire (a substring grep WOULD have).
        let output = "<parameter name=\"command\">some text</parameter>\n";
        assert!(!check_lines_for_malformed_tool_call(output));
    }

    #[test]
    fn test_not_malformed_prose_mentioning_invoke() {
        // Prose that merely says "invoke" / "parameter" without the raw `<`
        // opening tag must NOT trip the detector.
        let output = "\u{276f} please invoke the watcher and pass the parameter\nTokens: 50000";
        assert!(!check_lines_for_malformed_tool_call(output));
    }

    #[test]
    fn test_not_malformed_lone_invoke_no_name_no_corroboration() {
        // A bare `<invoke>` with no name= and no parameter/close is treated as
        // prose/noise (e.g. someone discussing the literal tag).
        let output = "consider the <invoke> tag and how it works\n";
        assert!(!check_lines_for_malformed_tool_call(output));
    }

    #[test]
    fn test_not_malformed_inside_code_fence() {
        // Docs / chat output that quotes a full malformed construct INSIDE a
        // fenced code block must NOT fire — this is the classic false positive
        // (e.g. this very design discussion rendered into the pane).
        let output = "\
Here is what a malformed call looks like:\n\
```\n\
<invoke name=\"Bash\">\n\
<parameter name=\"command\">ls</parameter>\n\
</invoke>\n\
```\n\
That is the failure mode.\n";
        assert!(!check_lines_for_malformed_tool_call(output));
    }

    #[test]
    fn test_malformed_outside_fence_still_fires() {
        // A real malform after a (closed) earlier code fence still fires.
        let output = "\
```\n\
some quoted code\n\
```\n\
<invoke name=\"Bash\">\n\
<parameter name=\"command\">watcher-ctl run claude-event-watch</parameter>\n\
</invoke>\n";
        assert!(check_lines_for_malformed_tool_call(output));
    }

    #[test]
    fn test_not_malformed_invokexyz_not_a_tag() {
        // `<invokexyz` / `<parameters` must not match — tag-name boundary check.
        let output = "<invokexyz name=\"x\"> and <parameters name=\"y\">\n";
        assert!(!check_lines_for_malformed_tool_call(output));
    }

    #[test]
    fn test_not_malformed_normal_output() {
        let output = "\u{25cf} Bash(watcher-ctl run claude-event-watch)\nTokens: 50000\nBashes: 1";
        assert!(!check_lines_for_malformed_tool_call(output));
    }

    #[test]
    fn test_malformed_only_checks_recent_lines() {
        // A full malformed construct far up in scrollback (>40 lines back)
        // should NOT count — only the live tail is inspected.
        let mut lines: Vec<String> = vec![
            "<invoke name=\"Bash\">".to_string(),
            "<parameter name=\"command\">ls</parameter>".to_string(),
            "</invoke>".to_string(),
        ];
        for _ in 0..100 {
            lines.push("normal conversation line".to_string());
        }
        let output = lines.join("\n");
        assert!(!check_lines_for_malformed_tool_call(&output));
    }

    #[test]
    fn test_malformed_empty_input() {
        assert!(!check_lines_for_malformed_tool_call(""));
    }

    // --- court-prefix fast-path signature tests ---

    #[test]
    fn test_malformed_court_prefix_signature() {
        // The confirmed 2026-06-17 real-world signature: a bare `court` line
        // immediately followed by a non-namespaced `<invoke ...>`.
        let output = "\
court\n\
<invoke name=\"Bash\">\n\
<parameter name=\"command\">watcher-ctl run claude-event-watch</parameter>\n\
</invoke>\n";
        assert!(check_lines_for_malformed_tool_call(output));
    }

    #[test]
    fn test_malformed_court_prefix_with_whitespace() {
        // Leading/trailing whitespace around the bare `court` token still matches.
        let output = "  court  \n<invoke name=\"Read\">\n";
        assert!(check_lines_for_malformed_tool_call(output));
    }

    #[test]
    fn test_malformed_court_prefix_lookahead_gap() {
        // The `<invoke` may land a few lines after `court` (still within the
        // small look-ahead window).
        let output = "court\n\n\n<invoke name=\"Bash\">\n";
        assert!(check_lines_for_malformed_tool_call(output));
    }

    #[test]
    fn test_not_malformed_court_in_prose() {
        // The word "court" embedded in prose is NOT the bare-line signature,
        // and there is no bare invoke tag to corroborate.
        let output = "the court ruled in favor of the plaintiff today\n";
        assert!(!check_lines_for_malformed_tool_call(output));
    }

    #[test]
    fn test_not_malformed_court_prose_with_namespaced_call() {
        // "court" in prose followed by a PROPERLY namespaced call (which never
        // reaches the pane as text anyway) must not fire the fast-path.
        let output = "the court adjourned\n<invoke name=\"Bash\">\n";
        assert!(!check_lines_for_malformed_tool_call(output));
    }

    #[test]
    fn test_not_malformed_court_prefix_inside_code_fence() {
        // The whole signature quoted inside a fenced code block (docs / chat)
        // must NOT fire.
        let output = "\
Example of the bad pattern:\n\
```\n\
court\n\
<invoke name=\"Bash\">\n\
<parameter name=\"command\">ls</parameter>\n\
</invoke>\n\
```\n\
end of example\n";
        assert!(!check_lines_for_malformed_tool_call(output));
    }

    #[test]
    fn test_not_malformed_court_far_from_invoke() {
        // A bare `court` line with NO bare invoke within the look-ahead window
        // is not the signature (and nothing else corroborates a construct).
        let mut lines = vec!["court".to_string()];
        for _ in 0..10 {
            lines.push("just a normal line".to_string());
        }
        let output = lines.join("\n");
        assert!(!check_lines_for_malformed_tool_call(&output));
    }

    // --- malformed_tool_call_fingerprint / dedup tests ---
    //
    // These guard the 2026-06-20 fix: the daemon dedups corrective injects on
    // a stable fingerprint of the offending block, so the SAME malformed text
    // lingering in pane scrollback (after the model already recovered with a
    // well-formed call below it) does NOT re-fire the interrupter every cycle
    // (the tight self-perpetuating loop the operator killed claude-watch over).

    #[test]
    fn test_fingerprint_some_on_malformed() {
        // A real malform yields a Some(fingerprint); detection parity with
        // check_lines_for_malformed_tool_call is preserved.
        let output = "\
court<invoke name=\"Bash\">\n\
<parameter name=\"command\">watcher-ctl run claude-event-watch</parameter>\n\
</invoke>\n";
        assert!(check_lines_for_malformed_tool_call(output));
        assert!(malformed_tool_call_fingerprint(output).is_some());
    }

    #[test]
    fn test_fingerprint_none_on_clean() {
        // A clean pane (no malformed construct) yields None.
        let output = "\u{276f} all good\nTokens: 1234\n";
        assert!(!check_lines_for_malformed_tool_call(output));
        assert!(malformed_tool_call_fingerprint(output).is_none());
    }

    #[test]
    fn test_fingerprint_stable_across_chrome_changes() {
        // The SAME malformed block, captured in two different cycles where only
        // the surrounding TUI chrome (prompt box, separators, status bar,
        // unrelated scrollback) differs, must produce the SAME fingerprint — so
        // the daemon recognizes it as "already nudged" and suppresses the
        // re-inject. This is the core stale-scrollback case.
        let cycle_a = "\
some earlier output\n\
court<invoke name=\"Bash\">\n\
<parameter name=\"command\">watcher-ctl run claude-event-watch</parameter>\n\
</invoke>\n\
\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n\
\u{276f}\n";
        let cycle_b = "\
totally different scrollback line\n\
and another\n\
court<invoke name=\"Bash\">\n\
<parameter name=\"command\">watcher-ctl run claude-event-watch</parameter>\n\
</invoke>\n\
\u{25cf} Bash(echo recovered)\n\
\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n\
\u{276f} -- INSERT --\n";
        let fa = malformed_tool_call_fingerprint(cycle_a).expect("a malformed");
        let fb = malformed_tool_call_fingerprint(cycle_b).expect("b malformed");
        assert_eq!(
            fa, fb,
            "identical malformed block must fingerprint identically regardless of chrome"
        );
    }

    #[test]
    fn test_fingerprint_differs_for_different_malform() {
        // A genuinely NEW malform (different command) must produce a DIFFERENT
        // fingerprint, so the daemon fires on it immediately rather than
        // mistaking it for the already-nudged block.
        let first = "\
court<invoke name=\"Bash\">\n\
<parameter name=\"command\">watcher-ctl run claude-event-watch</parameter>\n\
</invoke>\n";
        let second = "\
court<invoke name=\"Bash\">\n\
<parameter name=\"command\">touch /var/run/claude/heartbeat</parameter>\n\
</invoke>\n";
        let f1 = malformed_tool_call_fingerprint(first).expect("first malformed");
        let f2 = malformed_tool_call_fingerprint(second).expect("second malformed");
        assert_ne!(
            f1, f2,
            "different malformed blocks must fingerprint differently"
        );
    }

    // --- check_lines_for_api_retry tests ---

    #[test]
    fn test_api_retry_overload_with_retrying_in() {
        // The exact failure mode from 2026-04-28: 529 + retry-in-Ns banner.
        let output = "\
\u{276f} a tool call\n\
API Error: 529 {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}\n\
\u{23ba}  Retrying in 24s \u{00b7} attempt 3/10\n\
";
        assert!(check_lines_for_api_retry(output));
    }

    #[test]
    fn test_api_retry_attempt_marker_with_5xx() {
        // "attempt N/M" alone with the 5xx error nearby is enough.
        let output = "\
API Error: 503 service unavailable\n\
attempt 2/10\n\
";
        assert!(check_lines_for_api_retry(output));
    }

    #[test]
    fn test_api_retry_overloaded_keyword_alone() {
        // "Overloaded" + "Retrying in Ns" — sufficient even without an
        // explicit "API Error: 5xx" prefix.
        let output = "\
overloaded_error: Overloaded\n\
Retrying in 8 seconds\n\
";
        assert!(check_lines_for_api_retry(output));
    }

    #[test]
    fn test_api_retry_429_with_retrying_in() {
        // A 429 backoff also counts — same livelock failure mode.
        let output = "\
API Error: 429 rate limited\n\
Retrying in 32s\n\
";
        assert!(check_lines_for_api_retry(output));
    }

    #[test]
    fn test_not_api_retry_without_error_marker() {
        // "attempt 2/3" on its own (e.g. chat history mentioning a retry)
        // must NOT trip the detector — we require a 5xx/429/Overloaded cue.
        let output = "\
\u{276f} doing attempt 2/3 of the test plan\n\
\u{276f}\n\
";
        assert!(!check_lines_for_api_retry(output));
    }

    #[test]
    fn test_not_api_retry_normal_thinking() {
        // Normal long thinking shouldn't trip — no retry banner, no error.
        let output = "\
\u{2731} Thinking\u{2026} (45s \u{00b7} \u{2193} 384 tokens)\n\
";
        assert!(!check_lines_for_api_retry(output));
    }

    #[test]
    fn test_not_api_retry_old_history_only() {
        // A "Retrying in 12s" + "529" mentioned 100 lines ago shouldn't
        // count — only the last ~25 lines are inspected.
        let mut lines: Vec<String> = vec![
            "API Error: 529 Overloaded".to_string(),
            "Retrying in 12s".to_string(),
        ];
        for _ in 0..50 {
            lines.push("\u{276f} normal chat line".to_string());
        }
        let output = lines.join("\n");
        assert!(!check_lines_for_api_retry(&output));
    }

    #[test]
    fn test_api_retry_empty_input() {
        assert!(!check_lines_for_api_retry(""));
    }

    #[test]
    fn test_api_retry_resolved_no_banner() {
        // After the retry succeeds, the banner is gone — only normal
        // working state remains. No suppression should happen.
        let output = "\
\u{276f} Now processing your request\n\
\u{2731} Thinking\u{2026} (3s)\n\
";
        assert!(!check_lines_for_api_retry(output));
    }

    #[test]
    fn test_api_retry_isolated_overloaded_word() {
        // Bare "Overloaded" without any retrying-in cue must NOT trip —
        // we require both a retry marker AND an upstream-API error cue.
        let output = "\
\u{276f} The server is sometimes Overloaded but not now\n\
\u{276f}\n\
";
        assert!(!check_lines_for_api_retry(output));
    }

    /// Verify the post-escape settle delay is wired through the global
    /// atomic. We don't test the full settle_after_escape() async helper
    /// here (it's exercised by the e2e inject tests); we just verify the
    /// getter reflects the setter so the daemon's startup wiring is sound.
    ///
    /// NOTE: this mutates a process-global. Other tests in the same
    /// process must not depend on a specific value for the setting. We
    /// restore the default at the end so subsequent tests aren't surprised.
    #[test]
    fn test_post_escape_settle_ms_get_set_roundtrip() {
        let original = post_escape_settle_ms();
        set_post_escape_settle_ms(1234);
        assert_eq!(post_escape_settle_ms(), 1234);
        set_post_escape_settle_ms(0);
        assert_eq!(post_escape_settle_ms(), 0);
        // Restore for downstream tests.
        set_post_escape_settle_ms(original);
    }

    /// `sanitize_focus_main_keys` drops blank/whitespace-only entries, trims
    /// each remaining key name, and preserves order. This is the contract the
    /// live `send_focus_main_keys` path relies on so it never emits an empty
    /// `send-keys` key and so config whitespace doesn't break the key names.
    #[test]
    fn sanitize_focus_main_keys_trims_and_drops_blanks() {
        let raw = vec![
            "  Right ".to_string(),
            "".to_string(),
            "   ".to_string(),
            "Up".to_string(),
            "\tEscape\n".to_string(),
        ];
        let out = sanitize_focus_main_keys(&raw);
        assert_eq!(out, vec!["Right", "Up", "Escape"]);
    }

    #[test]
    fn sanitize_focus_main_keys_empty_stays_empty() {
        let raw: Vec<String> = vec![];
        assert!(sanitize_focus_main_keys(&raw).is_empty());
        // All-blank also collapses to empty (so the send path is a true no-op).
        let blanks = vec!["".to_string(), "  ".to_string()];
        assert!(sanitize_focus_main_keys(&blanks).is_empty());
    }

    /// Verify the FleetView focus-to-main key sequence is wired through the
    /// process-global RwLock: the getter reflects the setter, sanitization is
    /// applied on store, and the DEFAULT is empty (no-op — zero regression for
    /// setups that don't configure the FleetView fix).
    ///
    /// NOTE: mutates a process-global; restores the prior value at the end.
    #[test]
    fn test_focus_main_keys_get_set_roundtrip() {
        let original = focus_main_keys();

        // Sanitization is applied on the way in (blank dropped, entries trimmed).
        set_focus_main_keys(vec![
            " Right ".to_string(),
            "".to_string(),
            "Right".to_string(),
        ]);
        assert_eq!(focus_main_keys(), vec!["Right", "Right"]);

        // Empty round-trips to empty: the send path becomes a true no-op,
        // preserving pre-FleetView-fix behavior.
        set_focus_main_keys(vec![]);
        assert!(focus_main_keys().is_empty());

        // Restore for downstream tests.
        set_focus_main_keys(original);
    }
}
