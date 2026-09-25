//! All tmux interaction: send keys, capture pane, idle/mode detection, injection.

use crate::cmd::{run_cmd, run_cmd_any};
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::RwLock;
use std::time::Duration;
use tokio::time::sleep;
use tracing::{debug, info, warn};

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
    // Snapshot the input line so we can tell who consumed the keys.
    let before = capture_pane(pane).await.and_then(|o| prompt_line_text(&o));

    for key in &keys {
        send_keys(pane, &[key.as_str()]).await;
        sleep(Duration::from_millis(150)).await;
    }

    // WHO ATE THE KEYS?
    //
    // These are FleetView NAVIGATION keys, and on this host they are ten
    // `Up`s (enough to walk the selection cursor to `main` from any row). That
    // is correct WHILE A FLEETVIEW IS RENDERED. When one is not — the ordinary
    // case, and the case every routine alert lands in — Claude Code's input
    // editor receives them instead, and `Up` on an input line means RECALL THE
    // PREVIOUS PROMPT. Ten of them load a prior submission into what was an
    // empty line, the caller then types its payload at the recalled text's
    // cursor position, and the result is two payloads spliced together:
    //
    //     ❯ /config theme=light[CLAUDE-WATCH] WATCHER DOWN: 3 event(s)…
    //
    // Verified by A/B on a live pane, same binary and same payload, with only
    // this setting varying: with the keys the line came back spliced, with
    // `focus_main_keys = []` it came back as exactly the payload. This — not
    // concurrency — is why injects kept arriving mangled after the inject lock
    // shipped: ONE injector is enough, because it corrupts its own line.
    //
    // So: if the input line changed, the editor ate them, and this inject is
    // now doomed — the exclusivity check below will refuse the submit rather
    // than splice onto the recalled text. Say so precisely, because the
    // symptom (an inject that refuses forever) otherwise looks like a bug in
    // the refusal rather than in the configuration that caused it.
    //
    // We deliberately do NOT try to undo the recall by walking history forward
    // with `Down`. That looks symmetric and is not: `Up` SATURATES at the
    // oldest entry, so N `Up`s against a shorter history leave the cursor
    // somewhere the same N `Down`s do not reverse. Measured on a live pane —
    // ten `Up`s then ten `Down`s landed on a different entry, not the empty
    // line it started from. An unreliable repair on a shared input line is
    // worse than a clean refusal.
    //
    // The real fix is configuration: `focus_main_keys` must be empty unless a
    // FleetView is genuinely being driven, since these keys are only meaningful
    // while a fleet list is rendered and are destructive whenever it is not.
    let after = capture_pane(pane).await.and_then(|o| prompt_line_text(&o));
    if after != before {
        warn!(
            pane = %pane,
            before = ?before,
            after = ?after,
            count = keys.len(),
            "send_focus_main_keys: the input editor consumed the FleetView keys (no fleet \
             list was rendered) and recalled prompt history onto the input line. This inject \
             will be REFUSED rather than spliced. Set [tmux].focus_main_keys = [] unless a \
             FleetView is actually in use."
        );
        // Also to stderr: `claude-watch inject` is a CLI whose callers read
        // stderr, and it installs no tracing subscriber, so the `warn!` above
        // reaches the daemon log but NOT the shell tooling. Without this an
        // operator sees a bare `rc=4` and no reason for it — which is how a
        // configuration bug gets mistaken for a bug in the refusal.
        eprintln!(
            "[claude-watch inject] {} FleetView focus key(s) were eaten by the input editor \
             (no fleet list rendered) and recalled prompt history onto the line: {:?}. \
             This inject will be refused. Fix: set [tmux].focus_main_keys = [] in \
             config.toml unless a FleetView is actually in use.",
            keys.len(),
            after.as_deref().unwrap_or("")
        );
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
///  5. A `❯`-cursored FleetView agent-selector row (`❯ ● main`,
///     `❯ ◯ general-purpose …`): the FleetView agent-view renders the
///     currently-selected agent as the `❯` cursor followed by a status
///     bullet (`●` running / `◯` idle) and the agent name — NOT a
///     numbered option. This matters because the FleetView "↑/↓ to
///     select · Enter to view" hint sits at the TOP of the agent box, so with
///     a long agent list (plus the trailing bypass-permissions / token /
///     version status lines) that footer scrolls ABOVE this 25-line tail scan
///     and signature (2) misses it — letting a watcher-down / resume inject
///     `send-keys` INTO the agent-view and clobber it (operator-reported,
///     2026-07-13). The selected-agent cursor row is always at/near the
///     bottom, so match it directly. Same conservative bias as (2): a false
///     positive only DELAYS a resume-inject. (Deliberately NOT added to the
///     narrower `blocking_question_visible` — the agent-view is a passive
///     viewer, not a blocking question; see #485.)
///  6. The Bypass-Permissions launch dialog ("WARNING: Claude Code running in
///     Bypass Permissions mode" + "Yes, I accept"). Claude Code renders it at
///     STARTUP under `--dangerously-skip-permissions` when the acceptance has
///     not been persisted. Its cancel row is a bare `❯ No, exit` and its
///     footer says "Enter to confirm · Esc to cancel" (no "to select"), so
///     none of (1)–(5) match it — yet an inject here submits the
///     default-selected "No, exit" and Claude EXITS. See
///     `bypass_permissions_dialog_visible`.
///  7. The `/login` OAuth modal ("Select login method" / "Browser didn't
///     open? …" / "Paste code here if prompted"), which claude-watch opens
///     itself via `self-login` when the credentials are about to lapse. It
///     covers the whole TUI, so the token count reads 0 and the session
///     looks freshly started; none of (1)–(6) match it; and an inject here
///     types the payload into the AUTHORIZATION-CODE field — the
///     non-cancelling path leaving one literal `i` per attempt (the
///     operator-observed `…iiiiii` in the code box, 2026-09-17), the
///     cancelling path's leading Escape destroying the login outright. See
///     `login_dialog_visible`.
///  8. The `/model` switch confirmation ("Switch model?" + "Yes, switch to
///     …"), which claude-watch opens itself when it demotes a loop that has
///     run out of usage credits. Signature (3) only matches a numbered row
///     whose line STARTS with the cursor, so a bordered render
///     (`│ ❯ 1. Yes, switch to …`) slips past it. See
///     `model_switch_dialog_visible`.
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

        // (3) A `❯`-cursored numbered option row (`❯ 1.`, `❯ 2.`, …),
        // or (5) a `❯`-cursored FleetView agent-selector row
        // (`❯ ● main`, `❯ ◯ general-purpose …`) — the cursor
        // followed by a status bullet (● running / ◯ idle) + agent name.
        // The cursor char may be followed by spaces then `<digit>.` (3) or a
        // bullet (5). A genuinely-idle prompt has the `❯` alone on an
        // otherwise-empty input line, never immediately followed by a numbered
        // option or a status bullet. See signature (5) in the doc comment for
        // why the agent-view footer alone (signature (2)) is not enough.
        if let Some(rest) = trimmed.strip_prefix('\u{276f}') {
            let rest = rest.trim_start();
            let mut chars = rest.chars();
            if let Some(c) = chars.next() {
                // (3) numbered option row (`❯ 1.`).
                if c.is_ascii_digit() && chars.next() == Some('.') {
                    return true;
                }
                // (5) FleetView agent-selector row (`❯ ● main` / `❯ ◯ …`).
                if c == '\u{25cf}' || c == '\u{25ef}' {
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

    // (6) The Bypass-Permissions launch dialog. Its footer reads "Enter to
    // confirm · Esc to cancel" with NO "to select", so signature (2) misses
    // it, and its cancel row (`❯ No, exit`) carries no digit or bullet, so
    // signature (3)/(5) miss it too. Left unmatched, the bare `❯` reads as an
    // idle prompt and an inject submits the default "No, exit" — which exits
    // Claude. Delegated to the shared detector `policy` also acts on.
    if bypass_permissions_dialog_visible(pane_output) {
        return true;
    }

    // (7) The `/login` OAuth modal. Same shape of hazard as (6), but the
    // daemon is usually the one that OPENED it (`self-login`), so suppressing
    // here is what stops claude-watch typing into its own dialog.
    if login_dialog_visible(pane_output) {
        return true;
    }

    // (8) The `/model` switch confirmation — likewise a dialog the daemon
    // itself opens (credit demotion), and one whose bordered option rows do
    // not satisfy signature (3).
    if model_switch_dialog_visible(pane_output) {
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

/// Keys that move the Bypass-Permissions launch dialog's selection from its
/// default ("No, exit") onto the confirm option ("Yes, I accept") and submit.
///
/// The dialog renders the cancel option FIRST and starts with it focused:
///
/// ```text
/// ❯ No, exit
///   Yes, I accept
/// ```
///
/// One `Down` moves the cursor onto the confirm row; `Enter` submits it.
/// Option indexes are hidden on this dialog, so there is no number-key
/// shortcut — the arrow is the only way to move the selection. Exposed as a
/// constant (rather than inlined at the send site) so the sequence itself is
/// unit-testable.
pub(crate) const BYPASS_PERMISSIONS_ACCEPT_KEYS: [&str; 2] = ["Down", "Enter"];

/// Pure function: does the pane show Claude Code's Bypass-Permissions launch
/// dialog — the full-screen consent screen rendered when Claude Code starts
/// with `--dangerously-skip-permissions` and the acceptance has not been
/// persisted to settings?
///
/// ```text
///   WARNING: Claude Code running in Bypass Permissions mode
///   In Bypass Permissions mode, Claude Code will not ask for your approval
///   before running potentially dangerous commands.
///   …
/// ❯ No, exit
///   Yes, I accept
///   Enter to confirm · Esc to cancel
/// ```
///
/// Why this matters to the daemon: the dialog's cancel row renders the SAME
/// `❯` cursor glyph that `check_lines_for_idle_prompt` treats as "Claude is
/// idle and ready for input". So a relaunch that lands on this dialog LOOKS
/// idle, the resume prompt gets typed into the dialog, and the
/// default-selected "No, exit" submits — Claude exits with code 0, the prompt
/// text spills into the bare pane shell, and the session is left
/// half-attached (operator-observed on Claude Code 2.1.251, 2026-08-29).
///
/// ## Both markers are required
///
/// A live Claude Code TUI running in bypass mode renders a PERSISTENT status
/// indicator containing "bypass permissions" on essentially every frame, so
/// that phrase alone is not a signature — matching on it would report the
/// dialog for the entire lifetime of a normal session. The confirm label
/// ("Yes, I accept") exists only while the dialog itself is on screen.
/// Requiring BOTH the dialog's mode wording and the confirm label is a
/// signature the status indicator can never satisfy.
///
/// Scoped to the recent tail (like the sibling detectors) so a scrollback
/// mention — this doc read into a pane, a transcript quoting the dialog —
/// does not trip it.
pub(crate) fn bypass_permissions_dialog_visible(pane_output: &str) -> bool {
    let lines: Vec<&str> = pane_output.lines().collect();
    let start = if lines.len() > 25 {
        lines.len() - 25
    } else {
        0
    };
    let mut saw_mode = false;
    let mut saw_confirm_label = false;
    for line in &lines[start..] {
        let lower = line.trim().to_lowercase();
        if lower.contains("bypass permissions mode") {
            saw_mode = true;
        }
        if lower.contains("yes, i accept") {
            saw_confirm_label = true;
        }
    }
    saw_mode && saw_confirm_label
}

/// Pure function: does the pane show Claude Code's `/login` dialog — the
/// modal the OAuth flow renders, in either of its two phases?
///
/// ```text
///   Select login method:
/// ❯ Claude account with subscription
///   Anthropic Console account
/// ```
/// then, after a method is picked:
/// ```text
///   Browser didn't open? Use the url below to sign in (c to copy):
///   https://claude.com/cai/oauth/authorize?…
///   Paste code here if prompted > ▊
/// ```
///
/// ## Why the daemon needs this
///
/// This modal covers the ENTIRE TUI: the status line with the token count,
/// the `❯` input prompt, and the "Your login expires in N days" warning all
/// vanish behind it. Every detector that keys on one of those reads the pane
/// wrong while it is up:
///
///   * the token parse returns 0, so the dead-process / fresh-session
///     machinery classifies a perfectly healthy session as freshly started;
///   * `interactive_prompt_visible`'s six existing signatures all MISS it
///     (no "do you want to", no `to select`+confirm footer, no `❯ 1.` /
///     `❯ ●` row), so nothing suppresses an inject — and an inject into this
///     modal types its payload into the authorization-code field. The
///     non-cancelling path's INSERT probe leaves a literal `i` behind each
///     time (see `ensure_insert_mode`); the cancelling path's leading Escape
///     destroys the login outright;
///   * the expiry warning is gone from the pane, so the proactive expiry
///     check reads "nothing is expiring" and RESOLVES its own window —
///     resetting the retry spacing, the attempt budget and the
///     one-dialog-at-a-time latch while the dialog it opened is still up.
///
/// So this is a "the pane is not the session's right now" signal, not a
/// cosmetic one.
///
/// ## Signatures
///
/// The same strings `container/bin/self-login` drives the flow with, kept in
/// step with it deliberately — one vocabulary for one dialog. Both the
/// typographic and the ASCII apostrophe are matched because the rendered
/// glyph has differed across builds.
///
/// Scoped to the recent tail, like the sibling detectors, so a scrollback
/// mention — this doc read into a pane, a transcript quoting the dialog —
/// does not trip it.
pub(crate) fn login_dialog_visible(pane_output: &str) -> bool {
    let lines: Vec<&str> = pane_output.lines().collect();
    let start = if lines.len() > 25 {
        lines.len() - 25
    } else {
        0
    };
    for line in &lines[start..] {
        let lower = line.trim().to_lowercase();
        if lower.contains("paste code here")
            || lower.contains("browser didn't open")
            || lower.contains("browser didn\u{2019}t open")
            || lower.contains("select login method")
            || lower.contains("claude account with subscription")
        {
            return true;
        }
    }
    false
}

/// Capture the pane and report whether the `/login` modal is on it.
///
/// Companion to `login_expiry_warning`, and a deliberately separate capture
/// for the same reason: the two answer different questions about the same
/// pane and neither may be inferred from the other's miss.
pub async fn login_dialog_on_pane(pane: &str) -> bool {
    capture_pane(pane)
        .await
        .map(|out| login_dialog_visible(&out))
        .unwrap_or(false)
}

/// Pure function: is the pane at a Claude idle prompt that is NOT the
/// Bypass-Permissions dialog's cancel row?
///
/// `check_lines_for_idle_prompt` keys on the bare `❯` glyph, which the dialog
/// also renders (`❯ No, exit`). Callers that must distinguish "ready for the
/// resume prompt" from "sitting on the consent dialog" use this instead.
pub fn idle_prompt_without_bypass_dialog(pane_output: &str) -> bool {
    check_lines_for_idle_prompt(pane_output) && !bypass_permissions_dialog_visible(pane_output)
}

/// Send the accept sequence for the Bypass-Permissions launch dialog.
///
/// `Down` then `Enter` as two separate `send-keys` calls with a short gap:
/// the dialog re-renders between keystrokes, and batching both into a single
/// `send-keys` gives it no render in between. The gap is cheap (this runs at
/// most a few times per relaunch) and removes the class of failure where the
/// `Enter` lands on the pre-move selection — which on this dialog means
/// "No, exit".
pub async fn accept_bypass_permissions_dialog(pane: &str) {
    for key in BYPASS_PERMISSIONS_ACCEPT_KEYS {
        send_keys(pane, &[key]).await;
        sleep(std::time::Duration::from_millis(300)).await;
    }
}

// -----------------------------------------------------------------------
// Tool-permission prompt detection (`permission_prompt_monitor`)
// -----------------------------------------------------------------------

/// A TOOL-PERMISSION dialog observed on the pane — the "Do you want to
/// proceed?" confirmation Claude Code renders when a tool call needs
/// approval (a guarded Bash command, an edit outside the workspace, an MCP
/// tool call, …).
///
/// This is deliberately a DIFFERENT thing from the broader "interactive
/// prompt" / "blocking question" detectors:
///
///   * `interactive_prompt_visible` — biased toward true, answers "is
///     ANYTHING selectable on screen" (inject suppression).
///   * `blocking_question_visible` — narrower, answers "is a human being
///     asked something" (the `ask-question-stale` alarm).
///   * `permission_prompt_visible` (this) — narrowest, answers "is a TOOL
///     CALL blocked on an approval this daemon may safely DECLINE".
///
/// Only the narrowest of the three may drive a keystroke, because only for
/// this dialog class is the safe answer knowable without a human: declining
/// a tool call returns control to the agent with a rejection it can react to,
/// whereas declining an operator question invents an answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionPrompt {
    /// The dialog's question line, box chrome stripped
    /// (e.g. `Do you want to proceed?`).
    pub question: String,
    /// The dialog block as rendered — the tool/command lines above the
    /// question plus the option rows below it, box chrome stripped and
    /// length-capped. Carried into the claude-event so the main loop can see
    /// WHICH tool call is blocked without re-capturing the pane.
    pub context: String,
    /// Stable hash of the normalized dialog (question + options + context).
    /// The monitor's timer only advances while this value is UNCHANGED, so a
    /// re-render into a different dialog restarts the clock instead of
    /// inheriting the previous dialog's elapsed time.
    pub signature: u64,
}

/// Maximum characters of captured dialog context carried into an alert.
/// Keeps a runaway pane capture from producing a multi-kilobyte pingme.
const PERMISSION_PROMPT_CONTEXT_MAX_CHARS: usize = 1200;

/// The keystroke claude-watch sends to DECLINE a stale permission prompt.
///
/// ## Why Escape and never a digit
///
/// 1. **Claude Code documents Escape as the decline affordance on this very
///    dialog** — the deny row renders its own hint, either as a trailing
///    `(esc)` on the "No, and tell Claude what to do differently" row or as
///    an `Esc to cancel` footer. Escape is the dialog's own contract, not an
///    inference about its layout.
/// 2. **Option numbering is not stable.** The deny row is `2.` on a two-option
///    dialog and `3.` when Claude Code also offers a "Yes, and don't ask
///    again for …" row. Sending `2` therefore lands on *"Yes, and don't ask
///    again"* on exactly the dialogs where a blanket approval is most
///    dangerous — the guarded ones. A daemon that can accidentally approve is
///    strictly worse than one that cannot act at all.
/// 3. **Escape has no approving interpretation.** Its failure mode is
///    "nothing happened" (verified by re-capturing the pane afterwards),
///    never "a tool call the operator never saw was approved". Auto-APPROVAL
///    is not a capability this monitor has, by construction: this constant is
///    the only key it ever sends.
pub const PERMISSION_PROMPT_DENY_KEY: &str = "Escape";

/// Strip tmux/Claude-Code box chrome (borders, rule lines, the selection
/// cursor) from one captured line, leaving the human text.
fn strip_box_chrome(line: &str) -> &str {
    line.trim_matches(|c: char| {
        c.is_whitespace()
            || matches!(
                c,
                '\u{2502}' // │
                | '\u{2503}' // ┃
                | '|'
                | '\u{250c}' | '\u{2510}' | '\u{2514}' | '\u{2518}' // ┌┐└┘
                | '\u{256d}' | '\u{256e}' | '\u{256f}' | '\u{2570}' // ╭╮╯╰
                | '\u{2500}' | '\u{2501}' | '\u{2550}' // ─━═
            )
    })
}

/// Parse one captured line as a numbered option row (`1. Yes`,
/// `❯ 2. No, and tell Claude what to do differently (esc)`), returning
/// `(number, lowercased label)`.
///
/// The `❯` selection cursor is optional — only ONE row carries it, and which
/// row that is depends on where the operator last moved the selection, so the
/// signature must not depend on it.
fn parse_option_row(line: &str) -> Option<(u32, String)> {
    let mut rest = strip_box_chrome(line);
    if let Some(stripped) = rest.strip_prefix('\u{276f}') {
        rest = stripped.trim_start();
    }
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    let after = &rest[digits.len()..];
    let label = after.strip_prefix('.')?.trim();
    let n = digits.parse::<u32>().ok()?;
    Some((n, label.to_lowercase()))
}

/// Tool-consent dialog headers: the title line Claude Code renders at the top
/// of a TOOL-permission box, immediately above the command / file / tool
/// preview and the question.
///
/// This is marker (4)'s second form, and it exists because the first form —
/// an Escape affordance printed *by the dialog* — is not something every
/// Claude Code build renders. A build that dropped both the trailing `(esc)`
/// on the deny row and the `Esc to cancel` footer made the whole monitor
/// inert: every real dialog failed marker (4), fell through the fail-closed
/// path, and sat unanswered exactly as it had before the monitor existed.
/// Keying the dialog's IDENTITY off its title instead of off a keyboard hint
/// survives that, because the title names the tool whose call is blocked and
/// is the same string the box has carried across versions.
///
/// Matched as a case-insensitive PREFIX of a chrome-stripped line, so
/// decorated variants (`Bash command (unsandboxed)`, `Bash command (runs on
/// …)`, `MCP tool call`) match the bare stem.
///
/// Every entry names a blocked TOOL CALL — the only dialog class where the
/// safe unattended answer is knowable: declining returns a rejection the
/// agent can read and adapt to. Startup consent screens and operator
/// questions are deliberately absent (see `PERMISSION_DIALOG_NEVER_MATCH`).
const PERMISSION_DIALOG_TOOL_HEADERS: &[&str] = &[
    "bash command",
    "edit file",
    "create file",
    "write file",
    "overwrite file",
    "edit notebook",
    "read file",
    "mcp tool",
    "network request outside of sandbox",
];

/// Dialog text that vetoes a match outright, whatever else lines up.
///
/// These are the two dialogs where a keystroke is not a rejected tool call but
/// a decision with its own consequences: declining folder-trust or the
/// Bypass-Permissions launch screen makes Claude Code EXIT. Both already miss
/// the question marker today; the veto is here so that a future rewording
/// (`Do you want to trust the files in this folder?`) cannot quietly promote
/// either one into the auto-deny path.
const PERMISSION_DIALOG_NEVER_MATCH: &[&str] = &[
    "trust the files in this folder",
    "bypass permissions",
];

/// Pure function: does the pane show a TOOL-PERMISSION dialog that is safe to
/// decline unattended? Returns the parsed dialog (question + context +
/// stability signature) or `None`.
///
/// ## The signature — all four markers required
///
/// Claude Code renders the dialog as a bordered box. Three shapes seen in the
/// wild:
///
/// ```text
///   Bash command
///     rm -f /tmp/pc.json /tmp_stderr.log
///     Remove scratch files
///   Dangerous rm operation on critical path: /tmp_stderr.log
///   Do you want to proceed?
///   ❯ 1. Yes
///     2. No                                    Esc to cancel · Tab to amend
/// ```
/// ```text
///   Edit file
///   Do you want to make this edit to config.rs?
///   ❯ 1. Yes
///     2. Yes, and don't ask again this session
///     3. No, and tell Claude what to do differently (esc)
/// ```
/// ```text
///   Bash command
///     rm -f $SP/*.m4v $SP/*.mkv
///     Clean up scratch files
///   Dangerous rm operation on possibly-empty variable path: $SP/*.m4v
///   Do you want to proceed?
///   ❯ 1. Yes
///     2. No
/// ```
///
/// The third shape is the one that motivated marker (4)'s second form: no
/// trailing `(esc)`, no `Esc to cancel` footer, nothing on screen that names a
/// key at all. A dialog of that shape blocked a subagent's Bash call for
/// eighteen minutes and was cleared by a human, while the monitor logged
/// nothing — it had failed closed on the missing Escape affordance. Escape
/// still answers the dialog; the build simply stopped advertising it.
///
/// A match requires ALL of:
///
///   1. A question line beginning `Do you want to …` (box chrome stripped).
///   2. An option row `1.` whose label begins `yes` — every permission dialog
///      offers approval as the FIRST option. Operator questions
///      (`AskUserQuestion`) carry model-written labels instead.
///   3. Another numbered option row whose label begins `no` (or `deny`) — the
///      decline row Escape maps to.
///   4. The dialog identifies itself as a TOOL-consent box, by EITHER:
///      a. an Escape affordance in the dialog region — a trailing `(esc)` on
///         the deny row, or an `Esc to cancel` footer; OR
///      b. a recognized tool-consent header above the question
///         (`PERMISSION_DIALOG_TOOL_HEADERS`) — `Bash command`, `Edit file`,
///         `MCP tool call`, …
///
/// …and no entry of `PERMISSION_DIALOG_NEVER_MATCH` anywhere in the dialog
/// region.
///
/// Form (b) is narrower than it looks: it is reached only by something that
/// has already cleared markers (1)-(3), so "a Yes/No dialog, about a named
/// tool call, asking whether to proceed". What it does NOT do is make the
/// detector match on the header alone — a pane with a `Bash command` preview
/// and no Yes/No question still returns `None`.
///
/// ## Fails CLOSED
///
/// Any missing marker returns `None`, which means "no auto-deny". A dialog
/// this detector does not recognize is still caught by the broader
/// `blocking_question_visible` / `ask-question-stale` alarm path, so the
/// operator is told about it — the daemon just does not touch it. That
/// asymmetry is deliberate: an unrecognized dialog costs a notification, a
/// mis-recognized one costs a keystroke into a live session.
///
/// Deliberately NOT matched:
///
///   * The folder-trust dialog (`Do you trust the files in this folder?`) —
///     its question does not start with "Do you want to", and declining it
///     makes Claude Code exit rather than continue. A startup consent dialog
///     is an operator decision, not a blocked tool call. Also vetoed by text,
///     so a reworded question cannot promote it.
///   * The Bypass-Permissions launch dialog — same reasoning; it has its own
///     handler (`accept_bypass_permissions_dialog`) driven by explicit
///     config, its rows carry no numbers at all, and it is vetoed by text too.
///   * `AskUserQuestion` menus — question text is model-written and the
///     options are not Yes/No, so markers (1)-(3) do not line up. The residual
///     case is a menu that happens to read `Do you want to …` over literal
///     `1. Yes` / `2. No` rows: such a menu still carries no tool-consent
///     header, so it matches only through its `Esc to cancel` footer, which is
///     exactly the exposure this detector has always had. Escape there SKIPS
///     the question rather than inventing an answer, and only after the same
///     five-minute unanswered ladder.
///   * Prose that merely contains the word "proceed" in scrollback.
/// `context_lines` is how many lines above the question to keep as context.
pub(crate) fn permission_prompt_visible(
    pane_output: &str,
    context_lines: usize,
) -> Option<PermissionPrompt> {
    let lines: Vec<&str> = pane_output.lines().collect();
    // Scan a generous tail: the dialog box plus the tool preview above it can
    // run 20+ rows, and the question sits in the middle of it.
    let scan_start = lines.len().saturating_sub(40);
    let tail = &lines[scan_start..];

    // (1) The question line — take the LAST one so a dialog quoted earlier in
    // scrollback never wins over the live one.
    let q_idx = tail.iter().rposition(|l| {
        let s = strip_box_chrome(l).to_lowercase();
        s.starts_with("do you want to")
    })?;
    let question = strip_box_chrome(tail[q_idx]).to_string();

    // (2)/(3) Option rows below the question.
    let mut yes_first = false;
    let mut deny_row: Option<u32> = None;
    let mut esc_affordance = false;
    let mut last_option_idx = q_idx;
    for (offset, line) in tail[q_idx + 1..].iter().enumerate() {
        let idx = q_idx + 1 + offset;
        let lower = strip_box_chrome(line).to_lowercase();
        // (4a) The Escape affordance may ride on a deny row or a footer line.
        // Present on some Claude Code builds and absent on others, which is
        // why it is one of two accepted forms rather than a hard requirement.
        if lower.contains("(esc)") || lower.contains("esc to cancel") {
            esc_affordance = true;
        }
        if let Some((n, label)) = parse_option_row(line) {
            last_option_idx = idx;
            if n == 1 && label.starts_with("yes") {
                yes_first = true;
            }
            // "Deny, and tell Claude what to do differently" is the same row
            // under a different word; accepting both keeps a rename from
            // silencing the monitor the way the missing Escape hint did.
            if n > 1
                && (label.starts_with("no") || label.starts_with("deny"))
                && deny_row.is_none()
            {
                deny_row = Some(n);
            }
        }
    }

    if !yes_first || deny_row.is_none() {
        return None;
    }

    // Context block: the tool/command preview above the question through the
    // last option row, chrome-stripped, blank rows collapsed. The `❯`
    // selection cursor is dropped as well — it says where the operator last
    // moved the highlight, which is not part of the dialog's identity (see
    // the signature note below).
    let ctx_start = q_idx.saturating_sub(context_lines);

    // (4b) A recognized tool-consent header ABOVE the question — the dialog
    // naming the tool whose call is blocked. Searched over the same window
    // that becomes the alert context, so whatever the operator would be shown
    // is what the decision was made on.
    let tool_header = tail[ctx_start..q_idx].iter().any(|line| {
        let s = strip_box_chrome(line).to_lowercase();
        PERMISSION_DIALOG_TOOL_HEADERS
            .iter()
            .any(|h| s.starts_with(h))
    });

    if !esc_affordance && !tool_header {
        return None;
    }

    // The veto: a dialog whose decline is an operator decision with its own
    // consequences, not a rejected tool call. Checked over the whole dialog
    // region (preview + question + rows) and AFTER the markers, so it can only
    // ever subtract matches.
    let region_lower = tail[ctx_start..=last_option_idx]
        .iter()
        .map(|l| strip_box_chrome(l).to_lowercase())
        .collect::<Vec<_>>()
        .join("\n");
    if PERMISSION_DIALOG_NEVER_MATCH
        .iter()
        .any(|veto| region_lower.contains(veto))
    {
        return None;
    }

    let mut context_rows: Vec<String> = Vec::new();
    for line in &tail[ctx_start..=last_option_idx] {
        let s = strip_box_chrome(line);
        let s = s.strip_prefix('\u{276f}').map(str::trim_start).unwrap_or(s);
        if s.is_empty() {
            continue;
        }
        context_rows.push(s.to_string());
    }
    let mut context = context_rows.join("\n");
    if context.chars().count() > PERMISSION_PROMPT_CONTEXT_MAX_CHARS {
        context = context
            .chars()
            .take(PERMISSION_PROMPT_CONTEXT_MAX_CHARS)
            .collect::<String>()
            + "…";
    }

    // Stability signature, over the CHROME-STRIPPED, CURSOR-FREE text. The
    // cursor exclusion is load-bearing, not tidiness: an operator arrowing
    // between "Yes" and "No" repaints the highlight onto a different row, and
    // if that read as a new dialog the stale clock would reset every time
    // anyone glanced at the prompt — the monitor would never reach a
    // threshold on exactly the dialogs someone is hesitating over. Reuses the
    // hasher behind the pane-unchanged respawn signal.
    let signature = crate::respawn::hash_pane_content(&format!("{}\n{}", question, context));

    Some(PermissionPrompt {
        question,
        context,
        signature,
    })
}

/// Async wrapper: capture the pane and run `permission_prompt_visible`.
pub async fn detect_permission_prompt(pane: &str, context_lines: usize) -> Option<PermissionPrompt> {
    let out = capture_pane(pane).await?;
    permission_prompt_visible(&out, context_lines)
}

/// Send the DECLINE keystroke to a permission dialog and report whether the
/// dialog actually cleared.
///
/// One `Escape`, a settle window, then a re-capture: the return value is the
/// OBSERVED outcome, not the fact that a key was sent. "I pressed a key" is
/// not evidence the prompt was answered — a pane that has scrolled, a dialog
/// that re-rendered, or a send-keys that landed in the wrong pane all look
/// identical from the sender's side.
pub async fn deny_permission_prompt(pane: &str, context_lines: usize) -> bool {
    send_keys(pane, &[PERMISSION_PROMPT_DENY_KEY]).await;
    sleep(std::time::Duration::from_millis(1500)).await;
    detect_permission_prompt(pane, context_lines).await.is_none()
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

/// Pure: parse the `#{pane_active},#{window_active}` output of a tmux
/// `display-message` query into "is this pane fully FOREGROUND-selected?" —
/// i.e. it is the active pane in its window AND its window is the active
/// window in its session. Both flags must be `1`.
///
/// Returns `None` when the output is empty or malformed (missing a field),
/// which the caller treats as "the tmux query failed — fall back to the
/// current behavior and do NOT reselect".
///
/// Factored out so the selected-pane decision is unit-testable without a
/// live tmux (`run_cmd*` have no mock seam — see the repo's other pure
/// tmux predicates).
pub(crate) fn parse_pane_selected(display_output: &str) -> Option<bool> {
    let line = display_output.trim();
    if line.is_empty() {
        return None;
    }
    let mut parts = line.split(',');
    let pane_active = parts.next()?.trim();
    let window_active = parts.next()?.trim();
    if pane_active.is_empty() || window_active.is_empty() {
        return None;
    }
    Some(pane_active == "1" && window_active == "1")
}

/// Query tmux for whether `pane` is currently the FOREGROUND-selected pane
/// (active pane in an active window). Returns `Some(true)` when it is,
/// `Some(false)` when some OTHER pane/window is selected (e.g. a Claude Code
/// agent-view pane), and `None` when the tmux query fails (pane gone, tmux
/// error, unparseable) — the caller then falls back to the pre-existing
/// behavior without reselecting.
pub async fn is_pane_selected(pane: &str) -> Option<bool> {
    let (out, ok) = run_cmd_any(
        &[
            "tmux",
            "display-message",
            "-t",
            pane,
            "-p",
            "#{pane_active},#{window_active}",
        ],
        5,
    )
    .await;
    if !ok {
        return None;
    }
    parse_pane_selected(&out)
}

/// Pure: the ordered tmux commands that reselect `pane` as the foreground
/// (active) pane — select its window FIRST (in case a different window is
/// active), then the pane within that window. Factored out so the reselect
/// contract (order + target) is unit-testable without a live tmux.
pub(crate) fn reselect_pane_commands(pane: &str) -> Vec<Vec<String>> {
    vec![
        vec![
            "tmux".to_string(),
            "select-window".to_string(),
            "-t".to_string(),
            pane.to_string(),
        ],
        vec![
            "tmux".to_string(),
            "select-pane".to_string(),
            "-t".to_string(),
            pane.to_string(),
        ],
    ]
}

/// Ensure the MAIN-LOOP pane will receive the interrupt keystrokes we are
/// about to send — reselect it as the active tmux pane if some OTHER pane is
/// currently selected.
///
/// Andrew #1803 / #1804: "interrupts shouldnt be going to agents period. only
/// main loop … cw should detect when agents are selected and use tmux to
/// reselect main loop before interrupting." Modern Claude Code renders its
/// FleetView agent views as SEPARATE tmux panes in the same window (see
/// `find_dashboard_pane`'s pane_id/layout-churn notes). When an agent-view
/// pane is the active one, an Escape blast can cancel that subagent's turn —
/// exactly what must never happen. `pane` here is the configured main-loop
/// pane (resolved from `[tmux].dashboard_pane`, e.g. `claude-container:0.0`,
/// to its immutable `#{pane_id}` by `find_dashboard_pane`), so reselecting
/// `pane` IS reselecting the configured main loop — no separate hardcoded
/// target.
///
/// Best-effort and NON-FATAL: on a failed tmux active-pane query we log a
/// warning and return WITHOUT reselecting; the caller proceeds to interrupt
/// against `pane` regardless (its `send-keys` is already `-t`-targeted at
/// `pane`, so the fallback is exactly the pre-fix behavior). We never crash
/// the interrupt path.
async fn reselect_main_loop_pane(pane: &str) {
    match is_pane_selected(pane).await {
        Some(true) => {
            // Already the selected pane — nothing to do.
            debug!(
                pane = %pane,
                "reselect_main_loop_pane: main-loop pane already selected; no reselect needed"
            );
        }
        Some(false) => {
            info!(
                pane = %pane,
                "reselect_main_loop_pane: a non-main pane is selected (agent-view?); \
                 reselecting main-loop pane before interrupt (Andrew #1803/#1804)"
            );
            for argv in reselect_pane_commands(pane) {
                let args: Vec<&str> = argv.iter().map(String::as_str).collect();
                let _ = run_cmd(&args, 5).await;
            }
        }
        None => {
            info!(
                pane = %pane,
                "reselect_main_loop_pane: tmux active-pane query failed; \
                 proceeding with interrupt on configured pane (fallback)"
            );
        }
    }
}

/// Pure: resolve the `self-clear` coordination lockfile path from the two env
/// inputs, mirroring `container/bin/self-clear`'s `_default_lock_file()` EXACTLY
/// so the daemon and the self-clear tool agree on the same file:
///   1. `env_lock` ($CLAUDE_SELF_CLEAR_LOCK) if set & non-empty,
///   2. else `$XDG_RUNTIME_DIR/claude-self-clear.lock` if XDG set & non-empty,
///   3. else `/var/run/claude/claude-self-clear.lock`.
pub(crate) fn resolve_self_clear_lock_path(
    env_lock: Option<&str>,
    xdg_runtime_dir: Option<&str>,
) -> String {
    if let Some(v) = env_lock {
        let v = v.trim();
        if !v.is_empty() {
            return v.to_string();
        }
    }
    if let Some(rt) = xdg_runtime_dir {
        let rt = rt.trim();
        if !rt.is_empty() {
            return format!("{}/claude-self-clear.lock", rt.trim_end_matches('/'));
        }
    }
    "/var/run/claude/claude-self-clear.lock".to_string()
}

/// Resolve the live `self-clear` lockfile path from the process environment.
pub(crate) fn self_clear_lock_path() -> String {
    resolve_self_clear_lock_path(
        std::env::var("CLAUDE_SELF_CLEAR_LOCK").ok().as_deref(),
        std::env::var("XDG_RUNTIME_DIR").ok().as_deref(),
    )
}

/// Best-effort probe: is the advisory `flock` on `path` currently HELD by some
/// other process? Non-blocking exclusive acquire: acquired -> release + false;
/// EWOULDBLOCK -> true; missing file / other error -> false (fail-open). Matches
/// `container/bin/self-clear`'s `fcntl.flock(LOCK_EX | LOCK_NB)` semantics.
pub(crate) fn lockfile_held(path: &str) -> bool {
    use std::os::unix::io::AsRawFd;
    // Open WITHOUT O_CREAT: a missing lockfile means self-clear never ran.
    let file = match std::fs::OpenOptions::new().read(true).open(path) {
        Ok(f) => f,
        Err(_) => return false,
    };
    let fd = file.as_raw_fd();
    // SAFETY: `fd` is a valid descriptor owned by `file` for this call.
    let rc = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        unsafe {
            libc::flock(fd, libc::LOCK_UN);
        }
        false
    } else {
        matches!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(e) if e == libc::EWOULDBLOCK
        )
    }
}

/// Is a `self-clear` (`/clear` + resume-handoff tool) currently RUNNING?
///
/// self-clear holds an exclusive `flock` on its lockfile for its ENTIRE
/// lifecycle -- from the `/clear` inject, through polling for the fresh session,
/// to the resume-prompt inject (see `container/bin/self-clear` main(), which
/// takes `LOCK_EX | LOCK_NB` and holds it until `child_main` returns). The
/// daemon MUST NOT `send-keys` into the pane while that handoff is mid-flight: a
/// daemon stuck-state / alert inject landing between self-clear's `/clear` and
/// its resume submit OVERWRITES the handoff prompt (operator-reported,
/// 2026-08-17: the "Daemon detected stuck state" inject clobbered the self-clear
/// resume prompt after a `/clear`). So both `interrupt_and_wait` and
/// `inject_dispatch::inject_to_agent*` consult this and DEFER while it is true.
///
/// The daemon-spawned self-clear (`policy::spawn_immediate_clear`) inherits the
/// daemon env with no `--lock-file` override, so `self_clear_lock_path` resolves
/// to the SAME file. Fail-open so a probe glitch never wedges recovery injects.
pub(crate) fn self_clear_in_progress() -> bool {
    lockfile_held(&self_clear_lock_path())
}

/// Pure: resolve the `self-clear` HANDOFF-COMPLETION marker path from the two
/// env inputs, mirroring `container/bin/self-clear`'s `_default_handoff_file()`
/// EXACTLY so the daemon and the self-clear tool agree on the same file:
///   1. `env_marker` ($CLAUDE_SELF_CLEAR_HANDOFF) if set & non-empty,
///   2. else `$XDG_RUNTIME_DIR/claude-self-clear-handoff` if XDG set & non-empty,
///   3. else `/var/run/claude/claude-self-clear-handoff`.
///
/// This is DISTINCT from the coordination lockfile: the lock signals "a
/// self-clear is RUNNING" (held only for the `/clear`->resume handoff), whereas
/// this marker is TOUCHED once, at the moment the resume prompt is delivered,
/// so the daemon can suppress its own fresh-session / post-clear injects for a
/// grace window AFTER the lock releases while the fresh session bootstraps.
pub(crate) fn resolve_self_clear_handoff_path(
    env_marker: Option<&str>,
    xdg_runtime_dir: Option<&str>,
) -> String {
    if let Some(v) = env_marker {
        let v = v.trim();
        if !v.is_empty() {
            return v.to_string();
        }
    }
    if let Some(rt) = xdg_runtime_dir {
        let rt = rt.trim();
        if !rt.is_empty() {
            return format!("{}/claude-self-clear-handoff", rt.trim_end_matches('/'));
        }
    }
    "/var/run/claude/claude-self-clear-handoff".to_string()
}

/// Resolve the live `self-clear` handoff-marker path from the process env.
pub(crate) fn self_clear_handoff_path() -> String {
    resolve_self_clear_handoff_path(
        std::env::var("CLAUDE_SELF_CLEAR_HANDOFF").ok().as_deref(),
        std::env::var("XDG_RUNTIME_DIR").ok().as_deref(),
    )
}

/// Pure: is a handoff-marker mtime "recent" (within `grace_secs` of `now`)?
/// A marker in the (small clock-skew) future also counts as recent. `None`
/// mtime (marker absent) => not recent. `grace_secs == 0` => never recent
/// (feature disabled) is handled by the caller.
pub(crate) fn handoff_is_recent(marker_mtime: Option<f64>, now: f64, grace_secs: u64) -> bool {
    match marker_mtime {
        Some(m) => m >= now - grace_secs as f64,
        None => false,
    }
}

/// Did a `self-clear` tool FINISH delivering its resume/handoff prompt within
/// the last `grace_secs`? The self-clear tool touches
/// `self_clear_handoff_path()` immediately after it submits the resume prompt.
/// The daemon consults this so its fresh-session / post-clear inject gates DEFER
/// while the freshly-cleared session is still bootstrapping (tokens still 0,
/// pane idle) — the window where the daemon would otherwise CLOBBER the handoff
/// prompt with its generic "You are a fresh session ..." text (operator #4799).
///
/// Distinct from `self_clear_in_progress` (lock HELD during the handoff): that
/// covers only the in-flight window; this covers the post-release bootstrap
/// window. Fail-open (returns false) so a probe glitch never wedges recovery.
/// Filesystem mtime (epoch float secs) of the `self-clear` handoff marker, or
/// `None` when the marker is absent. The `self-clear` tool touches
/// `self_clear_handoff_path()` the instant it finishes delivering its resume
/// prompt, so this mtime is the completion time of the most recent self-clear.
/// Exposed so the daemon can stamp `last_context_clear` from it when the
/// token-drop detector missed a poll-gap self-clear (see
/// `policy::maybe_stamp_self_clear_handoff`).
pub(crate) fn self_clear_handoff_mtime() -> Option<f64> {
    std::fs::metadata(self_clear_handoff_path())
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs_f64())
}

pub(crate) fn self_clear_handoff_recent(grace_secs: u64) -> bool {
    if grace_secs == 0 {
        return false;
    }
    let mtime = self_clear_handoff_mtime();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    handoff_is_recent(mtime, now, grace_secs)
}

/// Grace window (seconds) for `self_clear_handoff_recent` when consulted from a
/// lib-level primitive that has no `Config` in hand (e.g. `interrupt_and_wait`).
/// `$CLAUDE_SELF_CLEAR_HANDOFF_GRACE_SECS` overrides; defaults to 120 to match
/// `config::default_self_clear_handoff_grace_secs`. The daemon's policy gates
/// use the config field directly; this env fallback keeps the primitive
/// config-free (same convention as the lockfile-path env resolution).
pub(crate) fn self_clear_handoff_grace_secs_env() -> u64 {
    std::env::var("CLAUDE_SELF_CLEAR_HANDOFF_GRACE_SECS")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(120)
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
///
/// BEFORE blasting any Escape, this GUARANTEES the interrupt lands on the
/// main loop and never on a subagent, at BOTH layers where a subagent can be
/// "selected" (Andrew #1803/#1804 — supersedes the #498 suppression approach):
///   1. tmux-pane layer: `reselect_main_loop_pane` reselects `pane` (the
///      configured main-loop pane) if a different tmux pane — e.g. a Claude
///      Code agent-view pane — is currently the active one.
///   2. Claude Code FleetView layer: `send_focus_main_keys` returns the
///      in-pane FleetView selection to `main` (the same mechanism the inject
///      path already uses). No-op when `[tmux].focus_main_keys` is empty
///      (the default), so zero regression risk for setups not using it.
pub async fn interrupt_and_wait(pane: &str, timeout_secs: u64) -> bool {
    // Coordinate with an in-flight `self-clear`: if the self-clear tool is
    // mid-handoff (holding its lockfile), DEFER -- an Escape blast here would
    // clobber the `/clear`->resume-prompt sequence it drives into this same pane
    // (operator-reported, 2026-08-17). Fail-open. See `self_clear_in_progress`.
    if self_clear_in_progress() || self_clear_handoff_recent(self_clear_handoff_grace_secs_env())
    {
        info!(
            pane = %pane,
            "interrupt_and_wait: self-clear in progress or recent handoff -- deferring interrupt (not seizing pane)"
        );
        return false;
    }
    // Andrew #1803/#1804: an interrupt must ONLY ever land on the main loop.
    // Reselect the main-loop tmux pane (if some other pane is active) and
    // return Claude Code's in-pane FleetView selection to main BEFORE the
    // Escape blast, so the interrupt can never cancel a subagent's turn.
    reselect_main_loop_pane(pane).await;
    send_focus_main_keys(pane).await;

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
/// PRE-check for `inject_text` / `inject_text_queued`: is there unsubmitted
/// human-typed text on the prompt line right now?
///
/// This is the fire-and-forget daemon injectors' guard, distinct from
/// `inject_and_verify`'s (the `claude-watch inject` CLI path) deliberately
/// advisory-only `settle_prompt_line`: that path is invoked by an operator or
/// script that EXPECTS to possibly race a human and already has a
/// post-type/retract recovery dance for it, so a hard pre-refusal there would
/// just add a second, redundant failure mode. The daemon's automatic
/// interruption tier has no such recovery — its choreography (Escape, `dd`
/// line-clear, literal type) runs BLIND into whatever is on the line, so the
/// only safe move is to not send it at all. Skipping this cycle costs
/// nothing; the daemon re-evaluates on the next tick.
async fn operator_typing_in_progress(pane: &str) -> bool {
    let prompt = capture_pane(pane).await.and_then(|out| prompt_line_text(&out));
    if prompt_has_unsubmitted_text(prompt.as_deref()) {
        info!(
            pane = %pane,
            residue = ?prompt,
            "inject: unsubmitted text already on the prompt line (operator likely \
             mid-keystroke) -- skipping this inject rather than typing over it"
        );
        true
    } else {
        false
    }
}

pub async fn inject_text(pane: &str, text: &str) {
    if operator_typing_in_progress(pane).await {
        return;
    }
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

/// Pure: after sending a single `i` keystroke, did it land as a LITERAL char
/// appended to the prompt-line input, rather than being consumed as a vim
/// NORMAL->INSERT mode switch? True iff the prompt-line text after the `i` is
/// exactly the before-text with a trailing `i` appended.
///
/// editorMode-agnostic signal (Andrew 2026-08-17): in vim NORMAL mode `i`
/// switches to INSERT and the prompt line is unchanged; in NON-vim mode (or vim
/// already-INSERT) the `i` is typed as text, so the prompt line gains a trailing
/// `i`. Detecting the literal lets the caller Backspace exactly that one char
/// before typing the real payload -- so an injected `[CLAUDE-WATCH] ...` /
/// `/config ...` never arrives as `i[CLAUDE-WATCH]` / `i/config ...`.
pub(crate) fn insert_key_landed_literal(before: Option<&str>, after: Option<&str>) -> bool {
    let b = before.unwrap_or("");
    let expected = format!("{b}i");
    after == Some(expected.as_str())
}

/// Ensure the (vim-mode) input editor is in INSERT before typing a payload,
/// leaving NO stray literal `i` on the prompt -- robust across Claude Code's
/// `editorMode` (vim / normal) setting.
///
/// Historically the injectors blind-sent an `i` to enter vim INSERT mode. With
/// `"editorMode": "vim"` that works ONLY from NORMAL mode, but the idle prompt
/// is frequently ALREADY in INSERT (a prior inject left it there), so the `i`
/// was typed as a LITERAL char -- the operator-reported `i[CLAUDE-WATCH] ...` /
/// `i/config theme=...` (2026-08-17). In NON-vim mode there is no INSERT
/// concept, so an `i` is ALWAYS literal.
///
/// Robust, setting-agnostic strategy:
///   1. Already in INSERT (`-- INSERT --`) -> send nothing (vim, common idle).
///   2. Else snapshot the prompt line, send exactly ONE `i`, then poll:
///      a. now in INSERT -> the `i` was consumed as a vim NORMAL->INSERT switch.
///      b. else the prompt line gained a trailing `i`
///         (`insert_key_landed_literal`) -> it landed as LITERAL text (non-vim,
///         or vim couldn't switch): Backspace exactly that one char.
///      c. else ambiguous -> proceed (fail-open); the payload types either way.
///   Only ONE `i` is ever sent, so a stuck probe can never accumulate `iii`.
///
/// ## The `/login` modal is exempt
///
/// Step (2b) is the only thing that erases a literal `i`, and it recognizes
/// one by diffing `prompt_line_text` — which keys entirely on the `❯` glyph.
/// The `/login` modal draws no `❯`, so `prompt_line_text` returns `None`
/// before AND after, `insert_key_landed_literal` cannot fire, and the probe
/// falls through to (2c) and leaves the `i` sitting in the authorization-code
/// field. Per inject. That is the operator-observed `…iiiiii` in the paste-code
/// box (2026-09-17): the probe has no way to clean up after itself there, so
/// it must not run there at all.
///
/// `interactive_prompt_visible` (signature 7) already suppresses the inject
/// tiers before they reach this function; this is the backstop for the ones
/// that do not consult it and for the race where the modal arrives between
/// the guard and the keystroke. Skipping the probe costs nothing on a modal —
/// there is no INSERT mode to reach — and the caller's payload is no worse off
/// than it already was.
async fn ensure_insert_mode(pane: &str) {
    // (0) A `/login` modal owns the pane: probing it deposits a literal `i`
    // that nothing downstream can take back. Send NOTHING.
    if login_dialog_on_pane(pane).await {
        debug!(
            pane = %pane,
            "ensure_insert_mode: /login modal on the pane -- skipping the INSERT probe \
             (a literal `i` here lands in the authorization-code field)"
        );
        return;
    }
    // (1) Already INSERT -- never send a redundant `i`.
    if is_insert_mode(pane).await {
        return;
    }
    let before = capture_pane(pane).await.and_then(|o| prompt_line_text(&o));
    send_keys(pane, &["i"]).await;
    // Poll for the mode switch (2a) or a literal `i` (2b); never send more `i`.
    for _ in 0..3 {
        sleep(Duration::from_millis(300)).await;
        if is_insert_mode(pane).await {
            return; // (2a) vim NORMAL->INSERT
        }
        let after = capture_pane(pane).await.and_then(|o| prompt_line_text(&o));
        if insert_key_landed_literal(before.as_deref(), after.as_deref()) {
            // (2b) literal `i` on the prompt -- erase exactly that one char.
            send_keys(pane, &["BSpace"]).await;
            sleep(Duration::from_millis(150)).await;
            return;
        }
        // (2c) neither signal yet -- re-poll (redraw lag).
    }
    debug!(
        pane = %pane,
        "ensure_insert_mode: INSERT mode ambiguous after `i`; proceeding (fail-open)"
    );
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
    if operator_typing_in_progress(pane).await {
        return;
    }
    type_text_non_cancelling(pane, text).await;

    // Submit with a bare Enter from INSERT. A message typed-and-Entered while a
    // turn is generating is QUEUED by Claude Code, not injected mid-generation
    // — so the active turn keeps running.
    send_keys(pane, &["Enter"]).await;
    sleep(Duration::from_millis(300)).await;
}

/// The TYPE-ONLY half of the non-cancelling choreography: focus-return, enter
/// INSERT without leaving a stray literal `i`, type the payload. Sends NO
/// submit keystroke and — the load-bearing property — NO Escape.
///
/// Factored out of `inject_text_queued` so the non-cancelling `--no-submit`
/// path (`inject_and_verify` without `--escape`) shares exactly one copy of it
/// rather than falling back to the Escape-leading `inject_text_no_submit`.
pub(crate) async fn type_text_non_cancelling(pane: &str, text: &str) {
    // Return FleetView selection to `main` FIRST (before entering INSERT and
    // typing), so a queued nudge lands on the main conversation and not on a
    // background agent selected in the FleetView (Andrew #270/#288/#291).
    // No-op when `[tmux].focus_main_keys` is empty (the default). These keys
    // do NOT cancel the active turn — arrow/Escape FleetView navigation only
    // moves the selection; the non-cancelling contract of this path is
    // preserved.
    send_focus_main_keys(pane).await;

    // Enter INSERT mode WITHOUT leaving a stray literal `i` (Andrew 2026-08-17).
    // The idle vim-mode prompt is frequently ALREADY in INSERT, so the old
    // blind `i` was typed as text (`i[CLAUDE-WATCH] ...` / `i/config theme=...`).
    // `ensure_insert_mode` sends `i` only when needed and Backspaces it if it
    // lands literally. NO leading Escape -- the non-cancelling contract of this
    // path (never seize the active turn) is preserved.
    ensure_insert_mode(pane).await;
    sleep(Duration::from_millis(300)).await;

    send_literal(pane, text).await;
    sleep(Duration::from_millis(500)).await;
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
    /// REFUSED: after typing, the prompt line held more than our payload, so
    /// we retracted what we typed and submitted NOTHING.
    ///
    /// Either the line was already dirty (residue from an operator's
    /// half-typed input, or from a previous inject whose submit did not land)
    /// or it acquired foreign text while we typed. Submitting such a line
    /// splices two payloads into one string that is neither — and the old
    /// "the payload cleared from the prompt line" check calls that a success.
    /// See `inject_and_verify` for the autopsy.
    PromptDirty,
}

/// How long to let a dirty prompt line clear before typing anyway.
///
/// Short on purpose. The inject lock already serialises us against other
/// injectors, so a line still dirty here is residue nobody is actively
/// clearing — most likely an operator's half-typed input. Waiting longer does
/// not make it go away.
const PROMPT_CLEAR_WAIT: Duration = Duration::from_secs(5);

/// How long to let the typed payload settle before deciding the line is
/// contaminated. Covers TUI redraw lag, not a competing writer.
const TYPED_SETTLE_WAIT: Duration = Duration::from_millis(1500);

const PROMPT_POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Pure: is the prompt line empty (or absent)?
///
/// `None` (no `❯` rendered at all) counts as NOT empty: we cannot see the
/// input line, so we cannot assert anything about it.
pub(crate) fn prompt_line_is_empty(prompt: Option<&str>) -> bool {
    matches!(prompt, Some(p) if p.is_empty())
}

/// The TUI's own hint text for a never-touched input line, e.g.
/// `❯ Try "edit <file>"`. See `prompt_line_is_empty`'s doc comment for
/// the live-pane example this is lifted from.
const PROMPT_PLACEHOLDER_PREFIX: &str = "Try \"";

/// Pure: does the prompt line hold genuine unsubmitted operator text — as
/// opposed to being bare, or showing the TUI's own placeholder hint?
///
/// This is the inverse question from `prompt_line_is_empty`, asked for a
/// different purpose: `prompt_line_is_empty` feeds a POST-type submit
/// check, where treating a placeholder as "not empty" is the conservative
/// (cheap-to-retry) choice. This helper feeds a PRE-type check that decides
/// whether to send ANY keystrokes into the pane at all — treating a
/// placeholder as "occupied" here would mean a daemon-driven inject could
/// never land on a pane that has simply never been touched, which is
/// exactly the boot-time deadlock this check exists to avoid. So the
/// placeholder is excluded; anything else non-empty counts as real,
/// unsubmitted human input that an inject would collide with.
pub(crate) fn prompt_has_unsubmitted_text(prompt: Option<&str>) -> bool {
    match prompt {
        Some(p) if !p.is_empty() => !p.starts_with(PROMPT_PLACEHOLDER_PREFIX),
        _ => false,
    }
}

/// Pure: after typing `text`, is the prompt line EXACTLY our payload and
/// nothing else?
///
/// The load-bearing check. `inject_and_verify`'s historical success criterion
/// was "the payload prefix disappeared from the prompt line after Enter" —
/// which is just as true when a SPLICED line gets submitted, because a splice
/// submits and clears exactly like the real thing. On 2026-08-19 that reported
/// `(verified)` for a `/config theme=light` that had been glued into the
/// middle of a `WATCHER DOWN` banner; Claude Code answered
/// `Expected key=value, got "theme=light[CLAUDE-WATCH] WATCHER DO…"` and the
/// theme never changed.
///
/// tmux truncates the prompt line at pane width, so we can only see a PREFIX
/// of a long payload. We therefore assert the strongest property the capture
/// can support: everything visible on the line must be a prefix of what we
/// typed. That rejects text PREPENDED to our payload
/// (`WATCHER DOWN…/config theme=light`) and text APPENDED to it
/// (`/config theme=light[CLAUDE-WATCH] WATCHER DOWN…`) as long as the extra
/// characters fall inside the visible row — exactly the regime the observed
/// failures live in, since the theme payload is 19 characters and the pane is
/// 66 wide.
pub(crate) fn typed_line_is_exclusively_payload(prompt: Option<&str>, text: &str) -> bool {
    let Some(prompt) = prompt else {
        return false;
    };
    let want: Vec<char> = text.trim().chars().collect();
    let got: Vec<char> = prompt.chars().collect();
    if want.is_empty() {
        return got.is_empty();
    }
    // Nothing on the line (or it was cleared under us) is not "clean" — it
    // means our payload is not on the line we are about to submit.
    if got.is_empty() {
        return false;
    }
    // More characters on the line than we typed => something else is there.
    if got.len() > want.len() {
        return false;
    }
    got[..] == want[..got.len()]
}

/// Give a dirty prompt line a bounded chance to clear before we type.
///
/// ADVISORY ONLY — it never blocks the inject. The authoritative gate is the
/// post-type [`wait_for_clean_payload`] check. Refusing here would be the
/// wrong shape of guard: `prompt_line_text` cannot distinguish an empty input
/// from an empty input the TUI has painted a placeholder hint into (a virgin
/// session renders `❯ Try "edit …"` with nothing actually typed), so a
/// blocking pre-check could refuse forever on a perfectly clean pane — and a
/// guard that can suppress a WATCHER DOWN alert indefinitely is worse than the
/// bug it fixes. Waiting costs nothing and usually lets a peer's in-flight
/// submit land first.
async fn settle_prompt_line(pane: &str) {
    let deadline = tokio::time::Instant::now() + PROMPT_CLEAR_WAIT;
    loop {
        let prompt = capture_pane(pane).await.and_then(|o| prompt_line_text(&o));
        if prompt_line_is_empty(prompt.as_deref()) {
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            debug!(
                pane = %pane,
                residue = ?prompt,
                "inject: prompt line still not empty after settle window; typing anyway \
                 (the post-type exclusivity check decides whether we submit)"
            );
            return;
        }
        sleep(PROMPT_POLL_INTERVAL).await;
    }
}

/// Wait (bounded) for the prompt line to settle to EXACTLY our payload.
async fn wait_for_clean_payload(pane: &str, text: &str) -> bool {
    let deadline = tokio::time::Instant::now() + TYPED_SETTLE_WAIT;
    loop {
        let prompt = capture_pane(pane).await.and_then(|o| prompt_line_text(&o));
        if typed_line_is_exclusively_payload(prompt.as_deref(), text) {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            debug!(
                pane = %pane,
                on_line = ?prompt,
                "inject: prompt line is not exclusively our payload; refusing to submit"
            );
            return false;
        }
        sleep(PROMPT_POLL_INTERVAL).await;
    }
}

/// Send `count` presses of `key` in a single tmux call (`send-keys -N`).
async fn send_key_repeated(pane: &str, key: &str, count: usize) {
    if count == 0 {
        return;
    }
    let n = count.to_string();
    let _ = run_cmd(&["tmux", "send-keys", "-t", pane, "-N", &n, key], 5).await;
}

/// Undo our own typing after the exclusivity check failed, leaving the prompt
/// line exactly as we found it.
///
/// Backspace removes the characters immediately BEFORE the caret, and the
/// caret is sitting at the end of what we just typed — so this retracts our
/// payload and nothing else, even when it landed spliced into the MIDDLE of
/// someone else's text (the 2026-08-19 shape). Non-cancelling: no Escape, no
/// `dd`, so an in-flight turn and any operator input are untouched.
///
/// Without this, every refused attempt would leave its payload behind for the
/// next one to glue onto — which is how five and six fragments came to pile up
/// in a single line.
async fn retract_typed_payload(pane: &str, text: &str) {
    send_key_repeated(pane, "BSpace", text.chars().count()).await;
    sleep(Duration::from_millis(200)).await;
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
/// `escape` selects the choreography, and it DEFAULTS OFF (Andrew, 2026-08-18:
/// make `claude-watch inject` not use Escape by default, and put it behind a
/// flag):
///   - `escape = false` (DEFAULT) — NON-CANCELLING. Never sends an Escape, so
///     an in-flight turn is not interrupted AND a modal standing on the pane
///     is not cancelled. Enters INSERT with an idempotent `i`, types, and
///     (when `submit`) commits with a bare Enter from INSERT — which Claude
///     Code QUEUES behind an active turn. `slash_command` makes no difference
///     here: bare-Enter-from-INSERT already IS the slash-command contract.
///     Trade-off: no `dd` line-clear, so half-typed operator input on the
///     prompt line glues onto the payload.
///   - `escape = true` — CANCELLING. The historical choreography:
///     Escape→NORMAL coercion, `dd` line-clear, `i`, type, then
///     Tab→Escape→Enter (or a bare Enter for `slash_command`). Opt in when the
///     caller genuinely needs the turn seized and the prompt line wiped
///     (self-clear's `/clear`, mcp-reconnect's `/mcp`).
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
    escape: bool,
) -> InjectOutcome {
    // DEFAULT PATH (`escape == false`): NEVER send a leading Escape.
    // `inject_text` / `inject_text_no_submit` both open with an Escape→NORMAL
    // coercion loop, and — as inject_dispatch.rs and docs/two-channel-design.md
    // document — *that Escape is what CANCELS the in-flight turn*. It also
    // cancels any MODAL standing on the pane, which is why the login flow could
    // not use this subcommand at all until the default flipped. So the
    // un-flagged behaviour is now the safe one: enter INSERT via an idempotent
    // `i` (NO Escape, NO `dd` line-clear), type the payload, and — when
    // submitting — commit with a bare Enter from INSERT, which is both the
    // slash-command submit contract AND queued behind an active turn rather
    // than interrupting it. Trade-off: no `dd` line-clear, so half-typed
    // operator input glues onto the payload. Callers that need the turn seized
    // and the prompt line wiped pass `--escape`. Then fall through to the
    // shared verify window below.
    if !escape {
        // This path has no `dd` line-clear (that needs NORMAL mode, reached by
        // an Escape that would cancel the in-flight turn), so whatever is
        // already on the input line gets our payload glued onto it. The inject
        // LOCK does not prevent that: it serialises injectors, but residue
        // OUTLIVES the lock — a previous injector whose submit did not land,
        // or an operator who half-typed something and walked away, leaves the
        // line dirty long after every lock is released.
        //
        // Give it a bounded chance to clear (advisory — see
        // `settle_prompt_line` for why this must not be a hard gate), then
        // type, then decide.
        settle_prompt_line(pane).await;
        type_text_non_cancelling(pane, text).await;
        // AUTHORITATIVE GATE: the line must be EXCLUSIVELY our payload before
        // we press Enter.
        //
        // This is the check whose absence is the whole bug. The historical
        // success criterion — "the payload cleared from the prompt line after
        // Enter" — is satisfied just as well by a SPLICED line, because a
        // splice submits and clears exactly like the real thing. So a
        // `/config theme=light` glued into a WATCHER DOWN banner submitted,
        // cleared, and reported `(verified)`, while Claude Code answered
        // `Expected key=value, got "theme=light[CLAUDE-WATCH] WATCHER DO…"`
        // and the theme never changed. Checking BEFORE Enter is what makes the
        // difference: a submitted splice is unrecoverable, an un-submitted one
        // we can simply take back.
        //
        // Deliberately not applied to `--no-submit`, whose entire contract is
        // to leave text sitting on the prompt line.
        if submit && !wait_for_clean_payload(pane, text).await {
            retract_typed_payload(pane, text).await;
            return InjectOutcome::PromptDirty;
        }
        if submit {
            // Bare Enter from INSERT — the same submit `inject_text_queued`
            // uses, and the same one slash commands require, so
            // `slash_command` makes no difference on this path.
            send_keys(pane, &["Enter"]).await;
            sleep(Duration::from_millis(300)).await;
        }
    } else if submit && !slash_command {
        // Reuse inject_text's proven type-and-submit choreography for the
        // regular-text submit path so there is exactly ONE copy of the
        // Escape/dd/i/type + Tab→Escape→Enter sequence.
        inject_text(pane, text).await;
    } else {
        // No-submit (any), or the CANCELLING slash-command path: drive the
        // shared low-level helpers directly (inject_text always submits with
        // the regular-text sequence).
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
    // Enter INSERT robustly and without leaving a stray literal `i` (Andrew
    // 2026-08-17): after the Step 1 Escape->NORMAL coercion + Step 2 `dd` the
    // pane is normally in NORMAL mode with an empty line, so `ensure_insert_mode`
    // sends one `i` to switch to INSERT; if the pane was actually already in
    // INSERT (Escape didn't take) or is non-vim, it sends no `i` / Backspaces a
    // literal one -- so `send_literal` below always types onto a clean INSERT
    // buffer instead of issuing motion commands or gluing an `i` prefix.
    ensure_insert_mode(pane).await;
    // Final settle even on success — Claude Code may render `-- INSERT --`
    // before the input editor has fully accepted typed characters.
    sleep(Duration::from_millis(500)).await;

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
///
/// The 401 banner Claude Code prints INSIDE a live TUI ("Please run /login ·
/// API Error: 401 OAuth access token has expired") is deliberately NOT this
/// function's business — it is text, and text on a live pane is conversation
/// until something off-screen says otherwise. `check_lines_for_401_banner`
/// detects that banner and the caller corroborates it against the credential
/// store before acting.
pub(crate) fn check_lines_for_reauth(pane_output: &str) -> bool {
    let lower = pane_output.to_lowercase();

    if tui_visible(&lower) {
        return false;
    }

    // TUI is gone — check for login-screen patterns.
    // Current Claude Code login screen shows: "Browser didn't open?",
    // "Paste code here", and a claude.ai/oauth/authorize URL.
    lower.contains("browser didn't open")
        || lower.contains("paste code here")
        || LOGIN_URL_PREFIXES
            .iter()
            .any(|p| lower.contains(&p.to_lowercase()))
        || lower.contains("open this url") && lower.contains("login")
        || lower.contains("session expired")
        || lower.contains("login required")
        || lower.contains("re-authenticate")
        || lower.contains("authentication required")
        || lower.contains("auth required")
        || lower.contains("api key expired")
}

/// Unified TUI guard: any TUI indicator (tokens counter, background-task
/// counters, the idle prompt glyph, the permission-mode banner) means we are
/// looking at a live Claude Code session with conversation content. Takes the
/// already-lowercased capture.
fn tui_visible(lower: &str) -> bool {
    lower.contains("tokens")
        || lower.contains("bashes")
        || lower.contains(" shells")
        || lower.contains(" agents")
        || lower.contains(" background tasks")
        || lower.contains("\u{276f}")
        || lower.contains("bypass permissi")
}

/// Pure function: is Claude Code's "access token has expired" banner on a
/// LIVE pane?
///
/// This is the hole between the other two detectors. When the OAuth access
/// token lapses and the silent refresh does not happen, Claude Code does not
/// replace the TUI with a login screen — it keeps the TUI up (tokens footer,
/// permission-mode banner, `❯` prompt all intact) and prints one inline line:
///
/// ```text
/// ● Please run /login · API Error: 401 OAuth access token has expired. Re-authenticate to continue.
/// ```
///
/// `check_lines_for_reauth` refuses to look at anything while the TUI is up
/// (correctly — that is the conversation-text false positive), and the
/// proactive expiry detector keys on a different warning about the REFRESH
/// token, which can be weeks from lapsing while the access token is already
/// dead. So nothing reacted, and the session sat on the banner until a human
/// typed `/login`.
///
/// This detector requires the TUI to be VISIBLE (the banner is an in-TUI
/// render; with the TUI gone the login-screen detector owns the pane) and
/// requires the COMBINATION of the `/login` instruction and the 401 / expired
/// phrasing, not any one phrase alone. It is still only text. The caller MUST
/// corroborate against the credential store (`credentials::read_access_token`)
/// before acting — a session reading this file, its tests, or the diff that
/// added them has this exact sentence on the pane while perfectly well
/// authenticated, and that case has to stay silent.
///
/// Matching is whitespace-insensitive for the same reason
/// `detect_login_expiry_warning` is: a tmux pane hard-wraps a line this long
/// at any column with no separator.
pub(crate) fn check_lines_for_401_banner(pane_output: &str) -> bool {
    let lower = pane_output.to_lowercase();
    if !tui_visible(&lower) {
        return false;
    }
    let squashed: String = lower.chars().filter(|c| !c.is_whitespace()).collect();
    squashed.contains("pleaserun/login")
        && (squashed.contains("apierror:401") || squashed.contains("oauthaccesstokenhasexpired"))
}

/// Claude Code's "you have no usage credits left" turn failure, as it appears
/// on the pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreditBanner {
    /// The model Claude Code offers to keep using, i.e. the one that ran out,
    /// as printed (`"Fable 5"`). A pane that wrapped mid-word yields a
    /// squashed, lowercased form instead (`"fable5"`); both are only ever used
    /// for display and for the alphanumeric-only comparison in
    /// `policy::model_ids_match`, so neither form changes a decision. `None`
    /// when the offer clause could not be read at all — the message is still
    /// the message without it.
    pub exhausted_model: Option<String>,
}

/// Pure function: is Claude Code's usage-credit exhaustion message on a LIVE
/// pane?
///
/// The third way to lose a session, and the one none of the other detectors
/// recognise. When the account's usage credits for the current model run out,
/// Claude Code keeps the TUI fully intact — tokens footer, permission-mode
/// banner, `❯` prompt — and answers EVERY turn with one line:
///
/// ```text
/// You're out of usage credits. Run /usage-credits to keep using Fable 5 or /model to switch models.
/// ```
///
/// Credentials are fine, the process is fine, nothing is "dead": the session
/// simply cannot produce a turn. Observed shape of the resulting outage — the
/// loop answers every injected notification with that sentence, events pile up
/// unconsumed, and the ack-staleness alert fires while every other monitor
/// reports a healthy session.
///
/// Matching strategy is `detect_login_expiry_warning`'s: strip ALL whitespace
/// and lowercase before matching, because a tmux pane hard-wraps with no
/// separator and a sentence this long can be split at any column. Both halves
/// are required — the phrase AND the `/usage-credits` remedy — so that prose
/// merely containing "out of usage credits" needs the command next to it too.
///
/// **This is text, and text on a live pane is conversation until something
/// off-screen says otherwise.** The sentence above is on the pane of any
/// session reading this file, its tests, or the diff that added them. The
/// false-positive guard is not here: it is the transcript corroboration in
/// `token_usage::recent_credit_failures_at`, consumed by
/// `policy::decide_credit_action`.
pub(crate) fn detect_credit_exhaustion(pane_output: &str) -> Option<CreditBanner> {
    let lower = pane_output.to_lowercase();
    if !tui_visible(&lower) {
        // The wedge this detects keeps the TUI up. A pane with no TUI on it is
        // some other detector's business (login screen, crashed process), and
        // guessing here would step on them.
        return None;
    }
    let squashed: String = lower.chars().filter(|c| !c.is_whitespace()).collect();
    if !squashed.contains("outofusagecredits") || !squashed.contains("/usage-credits") {
        return None;
    }

    // Which model ran out, read off the offer Claude Code makes. Two passes,
    // both with BOUNDED repetition so a pane where the sentence is interleaved
    // with other text cannot swallow half the screen into the "model name":
    //
    //  1. whitespace-normalized original, case preserved -> "Fable 5", which
    //     is what the operator's push notification should say;
    //  2. the squashed form as a fallback, which still matches when the pane
    //     wrapped mid-word inside the offer clause itself.
    static RE_PRETTY: std::sync::OnceLock<regex_lite::Regex> = std::sync::OnceLock::new();
    static RE_SQUASHED: std::sync::OnceLock<regex_lite::Regex> = std::sync::OnceLock::new();
    let re_pretty = RE_PRETTY.get_or_init(|| {
        regex_lite::Regex::new(r"(?i)to keep using (.{1,40}?) or /model")
            .expect("static credit-exhaustion display pattern is valid")
    });
    let re_squashed = RE_SQUASHED.get_or_init(|| {
        regex_lite::Regex::new(r"tokeepusing(.{1,40}?)or/model")
            .expect("static credit-exhaustion pattern is valid")
    });
    let normalized = pane_output.split_whitespace().collect::<Vec<_>>().join(" ");
    let exhausted_model = re_pretty
        .captures(&normalized)
        .or_else(|| re_squashed.captures(&squashed))
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().trim().to_string())
        .filter(|m| !m.is_empty());

    Some(CreditBanner { exhausted_model })
}

/// Capture the pane and report Claude Code's usage-credit exhaustion message.
///
/// A separate capture from `reauth_signal`, deliberately: the two signals are
/// independent (an out-of-credits pane is perfectly well authenticated) and
/// folding them into one classifier would make either able to mask the other.
pub async fn credit_exhaustion_banner(pane: &str) -> Option<CreditBanner> {
    let out = capture_pane(pane).await?;
    detect_credit_exhaustion(&out)
}

// ---------------------------------------------------------------------------
// The `/model` switch confirmation
//
// Typing `/model <id>` does not, on current Claude Code builds, change the
// model. It opens a confirmation the session then WAITS on:
//
//   Switch model?
//   Your next response will be slower and use more tokens
//
//   This conversation is cached for the current model. Switching
//   to <model> means the full history gets re-read on your next message.
//
//   ❯ 1. Yes, switch to <model>
//     2. No, go back
//
// Nothing answered it, so a demotion that "fired" left the session on the
// model that had run out until a human pressed a key (operator-observed,
// 2026-09-20). Answering it is therefore part of the demotion, not a nicety —
// and every keystroke below is gated on SEEING the dialog in a fresh capture,
// because a key typed at a pane that is NOT showing it lands in the
// conversation instead.
// ---------------------------------------------------------------------------

/// Title lines that mark the `/model` confirmation. The hook-driven variant
/// carries a different title with the same option rows.
const MODEL_SWITCH_TITLES: [&str; 2] = ["switch model?", "asked you to confirm"];
/// The option row that APPLIES the switch.
const MODEL_SWITCH_CONFIRM_ROW: &str = "yes, switch to";
/// The option row that abandons it.
const MODEL_SWITCH_DECLINE_ROW: &str = "no, go back";
/// Lines Claude Code prints once a `/model` has actually been applied.
const MODEL_SWITCH_APPLIED_MARKERS: [&str; 2] = ["set model to", "model set to"];

/// Where the selection cursor sits in the `/model` confirmation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelSwitchCursor {
    /// On the "Yes, switch to …" row — Enter applies the switch.
    Confirm,
    /// On the "No, go back" row — one Up reaches the confirm row.
    Decline,
    /// The dialog is up, but the cursor is on neither row this code knows.
    /// Never guessed at: see `model_switch_answer_keys`.
    Unknown,
}

/// Pure function: is Claude Code's `/model` switch confirmation on the pane?
///
/// BOTH markers are required — a title AND the "Yes, switch to …" option row —
/// for the same reason `bypass_permissions_dialog_visible` wants both: the
/// cost of a false positive here is a keystroke typed into a live session, so
/// one stray sentence must never be enough. Scoped to the recent tail so the
/// dialog quoted in scrollback (this file read into a pane, a transcript) is
/// history rather than a live modal.
///
/// Deliberately border-agnostic: it matches on `contains` over each line, so
/// the box-drawing characters Claude Code frames the dialog with — present or
/// not, in whatever style a build uses — change nothing.
pub(crate) fn model_switch_dialog_visible(pane_output: &str) -> bool {
    let mut title = false;
    let mut confirm_row = false;
    for line in recent_tail(pane_output, 25) {
        let lower = line.to_lowercase();
        if MODEL_SWITCH_TITLES.iter().any(|t| lower.contains(t)) {
            title = true;
        }
        if lower.contains(MODEL_SWITCH_CONFIRM_ROW) {
            confirm_row = true;
        }
    }
    title && confirm_row
}

/// Pure function: which row of the `/model` confirmation is selected?
///
/// `None` when the dialog is not on the pane at all. The cursor is read as the
/// `❯` (or plain `>`) glyph appearing BEFORE the row's label on the same line,
/// which survives the box border and the option number in front of it
/// (`│ ❯ 1. Yes, switch to …`).
pub fn model_switch_cursor(pane_output: &str) -> Option<ModelSwitchCursor> {
    if !model_switch_dialog_visible(pane_output) {
        return None;
    }
    for line in recent_tail(pane_output, 25) {
        let lower = line.to_lowercase();
        if cursor_precedes(&lower, MODEL_SWITCH_CONFIRM_ROW) {
            return Some(ModelSwitchCursor::Confirm);
        }
        if cursor_precedes(&lower, MODEL_SWITCH_DECLINE_ROW) {
            return Some(ModelSwitchCursor::Decline);
        }
    }
    Some(ModelSwitchCursor::Unknown)
}

/// Does a selection cursor sit before `needle` on this (lowercased) line?
fn cursor_precedes(lower_line: &str, needle: &str) -> bool {
    match lower_line.find(needle) {
        Some(idx) => {
            let prefix = &lower_line[..idx];
            prefix.contains('\u{276f}') || prefix.contains('>')
        }
        None => false,
    }
}

/// The ONLY place keystrokes for the `/model` confirmation are produced, and
/// the safety property of this whole path: it returns `None` — press nothing —
/// unless this exact frame shows the dialog AND says which row is selected.
///
/// * cursor on "Yes, switch to …" → `Enter` confirms it;
/// * cursor on "No, go back" → one `Up`, then `Enter`;
/// * dialog up but the cursor unreadable → `None`. A blind `Up`/`Enter`/`1`
///   could land on the wrong row or, if the frame was stale, in the
///   conversation itself. An unanswered dialog is recoverable and is alerted
///   on; a stray keystroke typed into a live session is not.
///
/// Note that no DIGIT is ever sent. `Enter` on a verified row cannot become
/// text if the dialog closes underneath it, whereas a `1` lands on the prompt
/// as a literal character.
pub fn model_switch_answer_keys(pane_output: &str) -> Option<&'static [&'static str]> {
    match model_switch_cursor(pane_output)? {
        ModelSwitchCursor::Confirm => Some(&["Enter"]),
        ModelSwitchCursor::Decline => Some(&["Up", "Enter"]),
        ModelSwitchCursor::Unknown => None,
    }
}

/// Pure function: has a `/model` switch actually been applied on this pane?
///
/// Claude Code prints its own confirmation line once the model changes. Used
/// as a POSITIVE signal that a switch landed without a dialog; never as
/// evidence that one did not.
pub fn model_switch_applied(pane_output: &str) -> bool {
    recent_tail(pane_output, 25).any(|line| {
        let lower = line.to_lowercase();
        MODEL_SWITCH_APPLIED_MARKERS
            .iter()
            .any(|m| lower.contains(m))
    })
}

/// The last `n` lines of a pane capture — the "is this live or is it
/// scrollback" scope every dialog detector here shares.
fn recent_tail(pane_output: &str, n: usize) -> impl Iterator<Item = &str> {
    let lines: Vec<&str> = pane_output.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines.into_iter().skip(start)
}

/// Capture the pane and report whether the `/model` confirmation is on it.
pub async fn model_switch_dialog_on_pane(pane: &str) -> bool {
    capture_pane(pane)
        .await
        .map(|out| model_switch_dialog_visible(&out))
        .unwrap_or(false)
}

/// Send one answer sequence to the `/model` confirmation.
///
/// Keys go one `send-keys` at a time with a gap between them, for the reason
/// `accept_bypass_permissions_dialog` does it: the dialog re-renders between
/// keystrokes, and a batched `Up Enter` can land the Enter on the pre-move
/// selection — which on this dialog means "No, go back".
///
/// `keys` always comes from `model_switch_answer_keys`, which produces nothing
/// at all unless a fresh capture showed the dialog.
pub async fn send_model_switch_answer(pane: &str, keys: &[&str]) {
    for key in keys {
        send_keys(pane, &[key]).await;
        sleep(Duration::from_millis(300)).await;
    }
}

/// Every OAuth authorize-URL prefix a Claude Code login screen can print.
///
/// Claude Code MOVED its subscription authorize endpoint: current builds use
/// `https://claude.com/cai/oauth/authorize` (the `CLAUDE_AI_AUTHORIZE_URL`
/// constant in the shipped bundle), and the Console login path uses
/// `https://platform.claude.com/oauth/authorize`. The original
/// `https://claude.ai/oauth/authorize` this parser was written against no
/// longer appears anywhere in a current build — matching only that string
/// meant the login screen was detected but the URL came back EMPTY, so the
/// operator got an alert with no link in it. Keep the legacy prefix for older
/// Claude Code versions; match whichever appears FIRST in the pane.
pub(crate) const LOGIN_URL_PREFIXES: &[&str] = &[
    "https://claude.com/cai/oauth/authorize",
    "https://platform.claude.com/oauth/authorize",
    "https://claude.ai/oauth/authorize",
];

/// Extract the login URL from pane output, handling possible line wrapping.
/// Looks for URLs starting with any prefix in `LOGIN_URL_PREFIXES`.
/// tmux line wrapping splits a URL across lines with NO separator, so we
/// reassemble by joining consecutive lines that look like URL continuations
/// (no whitespace at start, valid URL chars).
pub(crate) fn extract_login_url(pane_output: &str) -> Option<String> {
    let lines: Vec<&str> = pane_output.lines().collect();

    // Find the line containing the URL start. Scan line-by-line so the
    // EARLIEST line wins, and within a line take the leftmost prefix match,
    // regardless of which prefix matched.
    let mut url_line_idx = None;
    let mut url_start_col = 0;
    for (i, line) in lines.iter().enumerate() {
        let mut best: Option<usize> = None;
        for prefix in LOGIN_URL_PREFIXES {
            if let Some(pos) = line.find(prefix) {
                best = Some(match best {
                    Some(b) if b <= pos => b,
                    _ => pos,
                });
            }
        }
        if let Some(pos) = best {
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

/// Claude Code's PROACTIVE "your login is about to expire" warning.
///
/// This is a different signal from `check_lines_for_reauth`, and the two must
/// not be confused. `check_lines_for_reauth` fires when the credentials are
/// ALREADY dead: the TUI has been replaced by a login screen and the session
/// can no longer do anything. This one fires while the session is perfectly
/// healthy — the TUI is up, work is happening, and Claude Code has merely
/// started warning that the OAuth refresh token lapses soon.
///
/// The exact wording was read out of the shipped Claude Code bundle rather
/// than guessed, the same way `LOGIN_URL_PREFIXES` was. Two independent call
/// sites render it and both compose the identical visible text:
///
/// ```text
/// Your login expires in 2 days · run /login to renew
/// ```
///
/// One is a startup banner that renders whenever the refresh token is inside
/// its warning window; the other is a transient notice that renders only when
/// the window is down to a single day. Because the notice is transient, a
/// poller can legitimately MISS it — which is why the daemon corroborates
/// with, and can fall back to, the on-disk credential expiry.
///
/// Matching strategy: strip ALL whitespace and lowercase before matching. A
/// tmux pane hard-wraps with no separator and no hyphenation, so a phrase this
/// long can be split at any column; a whitespace-insensitive match is wrap
/// proof by construction, where a literal `"your login expires in"` is not.
///
/// Returns the number of days Claude Code claims are left.
pub(crate) fn detect_login_expiry_warning(pane_output: &str) -> Option<u32> {
    // No cheap literal pre-filter here, deliberately. The obvious one —
    // "does the pane contain `login expires in`?" — is exactly the literal
    // this function refuses to match on, and it silently defeats the whole
    // point: a pane that wrapped mid-word would fail the pre-filter and
    // return None while the squashed form matches perfectly. Squashing a
    // pane capture is a few microseconds; a wrap-blind fast path is a bug.
    static RE: std::sync::OnceLock<regex_lite::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| {
        regex_lite::Regex::new(r"yourloginexpiresin(\d{1,4})day")
            .expect("static login-expiry pattern is valid")
    });
    let squashed: String = pane_output
        .chars()
        .filter(|c| !c.is_whitespace())
        .flat_map(|c| c.to_lowercase())
        .collect();
    let caps = re.captures(&squashed)?;
    caps.get(1)?.as_str().parse::<u32>().ok()
}

/// Capture the pane and report Claude Code's proactive login-expiry warning.
///
/// Companion to `reauth_signal`, deliberately a separate capture: the two
/// signals are mutually exclusive (one needs the TUI gone, the other needs it
/// present) so neither can mask the other.
pub async fn login_expiry_warning(pane: &str) -> Option<u32> {
    let out = capture_pane(pane).await?;
    detect_login_expiry_warning(&out)
}

/// What the reactive reauth path can see on the pane this cycle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReauthSignal {
    /// Nothing auth-related on the pane.
    None,
    /// The TUI is gone and a login screen is up. Carries the OAuth URL if one
    /// is on the pane, or an empty string if the screen is up but the URL has
    /// not been reassembled yet.
    LoginScreen { url: String },
    /// The TUI is UP and Claude Code's "Please run /login · API Error: 401
    /// OAuth access token has expired" banner is on it. Text only — the
    /// caller corroborates against the credential store before acting.
    Banner401,
}

/// Pure function: classify ONE pane frame for the reactive reauth path.
///
/// One frame rather than two captures so the two detectors can never disagree
/// about what they are looking at: the login-screen check runs first because
/// it is the stronger claim (the TUI is gone), and the banner check only runs
/// on a frame the TUI was still on.
pub(crate) fn classify_reauth_frame(pane_output: &str) -> ReauthSignal {
    if check_lines_for_reauth(pane_output) {
        return ReauthSignal::LoginScreen {
            url: extract_login_url(pane_output).unwrap_or_default(),
        };
    }
    if check_lines_for_401_banner(pane_output) {
        return ReauthSignal::Banner401;
    }
    ReauthSignal::None
}

/// Capture the pane and classify it for the reactive reauth path.
pub async fn reauth_signal(pane: &str) -> ReauthSignal {
    match capture_pane(pane).await {
        Some(out) => classify_reauth_frame(&out),
        None => ReauthSignal::None,
    }
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
/// ALTERNATIVE (self-contained) signature: the spinner line states the retry
/// itself, with no separate API-error line to pair with —
///
///   ✳ Waiting for API response · will retry in 1m 14s · check your network
///
/// That banner is only ever painted by the client's retry loop, and it carries
/// its own countdown, so "waiting for API response" + a "retry in <duration>"
/// countdown is accepted on its own. Without this, a session parked on a
/// flaky endpoint (no 5xx line, no "attempt N/M") read as normal activity and
/// the daemon happily interrupted it — the exact livelock this guard exists to
/// prevent.
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

    // Self-contained banner: "Waiting for API response · will retry in <dur>".
    // The countdown is what makes it a retry state rather than a plain
    // in-flight request, and the phrase pair is painted only by the retry
    // loop, so no separate error marker is required.
    let waiting_for_api = lower.contains("waiting for api response");
    let retry_countdown = regex_lite::Regex::new(r"retry in\s+\d+\s*(h|m|s)")
        .ok()
        .is_some_and(|re| re.is_match(&lower));
    if waiting_for_api && retry_countdown {
        return true;
    }

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

/// Native, version-stable tmux healthcheck brief for the daemon's per-cycle
/// log line.
///
/// HISTORY: this previously shelled out to an external `tmux-healthcheck
/// --brief` binary that was NEVER shipped — it exists in neither the image
/// nor any host install, so `run_cmd` always returned `None` and the daemon
/// logged the literal fallback `tmux-healthcheck: unavailable` on EVERY
/// cycle (observed in the live in-container log). The string is purely
/// diagnostic (used only in the legacy log line + the `tmux_health` JSONL
/// field — it gates NOTHING), but "unavailable" every cycle is actively
/// misleading: it reads like a real tmux fault when tmux is perfectly
/// healthy. Replace the phantom-binary call with an inline probe of the
/// CONFIGURED dashboard session/pane.
///
/// Uses only subcommands + format variables that are stable across every
/// tmux version in play (`has-session`, `list-panes -F "#{pane_id}"`,
/// `display-message -p "#{pane_id}"` — all present since tmux 1.x, verified
/// on the in-container 3.3a and the host 3.7b). It is deliberately
/// command-NAME-independent (does not match `#{pane_current_command}`
/// against "claude"), so it is unaffected by the native-installer pane-comm
/// regression that PR #512 addressed elsewhere. Best-effort: any probe
/// failure degrades to a descriptive brief rather than a hard error.
pub async fn healthcheck_brief(config: &crate::config::TmuxConfig) -> String {
    // No configured session -> report server reachability only. `list-panes
    // -a` succeeds iff a tmux server is running and answering.
    if config.dashboard_session.is_empty() {
        let (_, ok) = run_cmd_any(&["tmux", "list-panes", "-a", "-F", "#{pane_id}"], 5).await;
        return if ok {
            "tmux: ok (server up, no dashboard_session configured)".to_string()
        } else {
            "tmux: no server / unreachable".to_string()
        };
    }

    // Session existence.
    let (_, session_ok) =
        run_cmd_any(&["tmux", "has-session", "-t", &config.dashboard_session], 5).await;
    if !session_ok {
        return format!("tmux: session '{}' missing", config.dashboard_session);
    }

    // Pane count in the session (proxy for "panes exist"). Stable format var.
    let (panes_out, panes_ok) = run_cmd_any(
        &[
            "tmux",
            "list-panes",
            "-s",
            "-t",
            &config.dashboard_session,
            "-F",
            "#{pane_id}",
        ],
        5,
    )
    .await;
    let pane_count = if panes_ok {
        panes_out.lines().filter(|l| !l.trim().is_empty()).count()
    } else {
        0
    };

    // Resolve the configured main-loop pane to its immutable pane_id
    // (command-name-independent — mirrors find_dashboard_pane). This is the
    // signal that actually matters: whether the daemon can target the pane
    // it injects into.
    let main_pane_ok = if config.dashboard_pane.is_empty() {
        // No explicit pane configured -> session presence is enough.
        panes_ok && pane_count > 0
    } else {
        let (_, ok) = run_cmd_any(
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
        ok
    };

    if main_pane_ok {
        format!(
            "tmux: ok (session={} panes={})",
            config.dashboard_session, pane_count
        )
    } else {
        format!(
            "tmux: session={} up but main pane '{}' unresolved (panes={})",
            config.dashboard_session, config.dashboard_pane, pane_count
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- Proactive login-expiry warning ----

    /// The literal string Claude Code renders, verbatim. Both of its render
    /// sites compose this same text.
    #[test]
    fn login_expiry_warning_is_read_off_a_normal_pane() {
        let pane = "\
  Some tool output here
  ⚠ Your login expires in 2 days · run /login to renew

╭──────────────────────────────────────────────────────────╮
│ > ";
        assert_eq!(detect_login_expiry_warning(pane), Some(2));
    }

    /// Singular vs plural: Claude Code pluralizes the unit, so "1 day" has to
    /// match as surely as "3 days".
    #[test]
    fn both_the_singular_and_plural_forms_match() {
        assert_eq!(
            detect_login_expiry_warning("Your login expires in 1 day · run /login to renew"),
            Some(1)
        );
        assert_eq!(
            detect_login_expiry_warning("Your login expires in 3 days · run /login to renew"),
            Some(3)
        );
    }

    /// The reason the matcher strips whitespace instead of matching a literal:
    /// a tmux pane hard-wraps with NO separator and no hyphenation, so the
    /// phrase can be cut at any column — including mid-word.
    #[test]
    fn a_hard_wrapped_warning_still_matches() {
        // Wrapped between words.
        assert_eq!(
            detect_login_expiry_warning("Your login expires\nin 2 days · run /login to renew"),
            Some(2)
        );
        // Wrapped MID-WORD, which is what tmux actually does at a narrow width.
        assert_eq!(
            detect_login_expiry_warning("Your login expi\nres in 2 days · run /log\nin to renew"),
            Some(2)
        );
        // And with the trailing half of the line missing entirely, because a
        // narrow pane truncates the notice with an ellipsis.
        assert_eq!(
            detect_login_expiry_warning("Your login expires in 2 days ·…"),
            Some(2)
        );
    }

    /// A live session with no warning must report nothing — including one
    /// whose conversation is full of auth vocabulary.
    #[test]
    fn an_ordinary_pane_reports_no_warning() {
        assert_eq!(detect_login_expiry_warning(""), None);
        assert_eq!(
            detect_login_expiry_warning(
                "⏵⏵ bypass permissions on · 3 shells · esc to interrupt\n337594 tokens"
            ),
            None
        );
        // Near-misses that are NOT the warning.
        assert_eq!(
            detect_login_expiry_warning("your session expires in 2 days"),
            None
        );
        assert_eq!(
            detect_login_expiry_warning("Your login expires in a couple of days"),
            None
        );
        assert_eq!(
            detect_login_expiry_warning("OAuth token has expired. Re-authenticate to continue."),
            None
        );
    }

    /// Documented, deliberately: the detector CANNOT tell Claude Code's banner
    /// from the same sentence sitting in conversation text, and this test pins
    /// that as a known property rather than leaving it as a surprise. It is
    /// why `decide_expiry_action` refuses to act on a pane sighting the
    /// credential store contradicts.
    #[test]
    fn conversation_text_matches_too_which_is_why_corroboration_exists() {
        let quoting_the_docs =
            "  I'm reading the detector docs, which quote \"Your login expires in 2 days\".";
        assert_eq!(detect_login_expiry_warning(quoting_the_docs), Some(2));
    }

    /// The reactive login SCREEN and the proactive warning are different
    /// states and must not be confused: the warning fires while the TUI is up,
    /// the login screen fires only once it is gone.
    #[test]
    fn the_expiry_warning_and_the_login_screen_are_disjoint_signals() {
        let warning = "Your login expires in 1 day · run /login to renew\n337594 tokens";
        assert_eq!(detect_login_expiry_warning(warning), Some(1));
        // TUI is visible, so the reactive path correctly stands down.
        assert!(!check_lines_for_reauth(warning));

        let login_screen = "Browser didn't open? Use the url below to sign in\n\n\
                            https://claude.com/cai/oauth/authorize?code=true";
        assert!(check_lines_for_reauth(login_screen));
        assert_eq!(detect_login_expiry_warning(login_screen), None);
    }

    // ---- Interrupt-only-hits-main-loop (Andrew #1803/#1804) ----

    /// `parse_pane_selected`: both `#{pane_active}` and `#{window_active}`
    /// must be `1` for the pane to count as foreground-selected. This is the
    /// signal that decides whether `interrupt_and_wait` reselects the
    /// main-loop pane before blasting Escape.
    #[test]
    fn parse_pane_selected_true_only_when_both_active() {
        // Active pane in the active window — selected.
        assert_eq!(parse_pane_selected("1,1"), Some(true));
        // Active pane but a DIFFERENT window is active — not foreground.
        assert_eq!(parse_pane_selected("1,0"), Some(false));
        // Some OTHER pane is active in this window (e.g. an agent-view pane).
        assert_eq!(parse_pane_selected("0,1"), Some(false));
        assert_eq!(parse_pane_selected("0,0"), Some(false));
        // Trailing newline / whitespace from tmux is tolerated.
        assert_eq!(parse_pane_selected("1,1\n"), Some(true));
        assert_eq!(parse_pane_selected(" 0 , 1 "), Some(false));
    }

    /// A failed / malformed tmux query yields `None` so the caller falls back
    /// to the pre-fix behavior (interrupt without reselecting) rather than
    /// guessing. Never crash the interrupt path.
    #[test]
    fn parse_pane_selected_none_on_malformed() {
        assert_eq!(parse_pane_selected(""), None);
        assert_eq!(parse_pane_selected("   "), None);
        // Missing the second field.
        assert_eq!(parse_pane_selected("1"), None);
        // Present-but-empty second field.
        assert_eq!(parse_pane_selected("1,"), None);
    }

    /// `reselect_pane_commands`: reselect must `select-window` BEFORE
    /// `select-pane` (a different window may be active), and both must
    /// target the passed-in (main-loop) pane spec.
    #[test]
    fn reselect_pane_commands_window_then_pane_targeting_main() {
        let cmds = reselect_pane_commands("claude-container:0.0");
        assert_eq!(cmds.len(), 2, "reselect = select-window then select-pane");
        assert_eq!(
            cmds[0],
            vec!["tmux", "select-window", "-t", "claude-container:0.0"],
            "must select the window FIRST (a different window may be active)"
        );
        assert_eq!(
            cmds[1],
            vec!["tmux", "select-pane", "-t", "claude-container:0.0"],
            "must then select the pane within that window"
        );
    }

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

    // ---- prompt-line exclusivity guards (the 2026-08-19 splice) ----

    #[test]
    fn empty_prompt_line_is_the_only_empty_state() {
        assert!(prompt_line_is_empty(Some("")));
        assert!(!prompt_line_is_empty(Some("half-typed operator input")));
        // No `❯` rendered at all: we cannot SEE the input line, so we must not
        // claim anything about it. A missing prompt is UNKNOWN, never empty.
        assert!(!prompt_line_is_empty(None));
    }

    #[test]
    fn prompt_has_unsubmitted_text_detects_real_typed_content() {
        assert!(prompt_has_unsubmitted_text(Some("half-typed operator input")));
        assert!(prompt_has_unsubmitted_text(Some("/config theme=light")));
    }

    #[test]
    fn prompt_has_unsubmitted_text_ignores_bare_and_placeholder_prompts() {
        // Bare submitted/cleared prompt: nothing to collide with.
        assert!(!prompt_has_unsubmitted_text(Some("")));
        // TUI-painted hint on a never-touched pane, not operator input.
        assert!(!prompt_has_unsubmitted_text(Some("Try \"edit <file>\"")));
        // No `❯` rendered at all: unknown, not "occupied" -- an inject must
        // not be blocked forever just because the pane state is unreadable.
        assert!(!prompt_has_unsubmitted_text(None));
    }

    #[test]
    fn clean_payload_accepts_exactly_what_we_typed() {
        assert!(typed_line_is_exclusively_payload(
            Some("/config theme=light"),
            "/config theme=light"
        ));
    }

    #[test]
    fn clean_payload_accepts_a_width_truncated_prefix() {
        // tmux truncates the prompt line at pane width, so a long banner is
        // only visible as a prefix. That prefix must still be OURS.
        let banner = "[CLAUDE-WATCH] WATCHER DOWN: 3 event(s) unconsumed >6min - dead";
        assert!(typed_line_is_exclusively_payload(
            Some("[CLAUDE-WATCH] WATCHER DOWN: 3 event(s)"),
            banner
        ));
    }

    #[test]
    fn clean_payload_rejects_appended_foreign_text() {
        // Reproduced 2026-08-19: cw-theme-sync typed first, the WATCHER DOWN
        // banner landed after it, and Claude Code answered
        // `Expected key=value, got "theme=light[CLAUDE-WATCH] WATCHER DO…"`.
        assert!(!typed_line_is_exclusively_payload(
            Some("/config theme=light[CLAUDE-WATCH] WATCHER DOWN: 3 event(s)"),
            "/config theme=light"
        ));
    }

    #[test]
    fn clean_payload_rejects_prepended_foreign_text() {
        // The 2026-08-19T10:08:47 shape: the theme payload spliced INTO the
        // middle of the banner, so the line does not even start with ours.
        assert!(!typed_line_is_exclusively_payload(
            Some("unconsumed >6mi/config theme=lightn - the event watcher is dead"),
            "/config theme=light"
        ));
    }

    #[test]
    fn clean_payload_rejects_an_empty_or_absent_line() {
        // Our payload is not on the line we are about to submit.
        assert!(!typed_line_is_exclusively_payload(
            Some(""),
            "/config theme=light"
        ));
        assert!(!typed_line_is_exclusively_payload(
            None,
            "/config theme=light"
        ));
    }

    #[test]
    fn prompt_dirty_is_a_distinct_outcome() {
        // Must not collapse into SubmitUnverified: they mean different things
        // to a caller. SubmitUnverified => keystrokes were sent and the
        // payload may yet land. PromptDirty => NOTHING was submitted, and the
        // pane still holds someone else's text.
        assert_ne!(InjectOutcome::PromptDirty, InjectOutcome::SubmitUnverified);
        assert_ne!(InjectOutcome::PromptDirty, InjectOutcome::Submitted);
        assert_ne!(InjectOutcome::PromptDirty, InjectOutcome::Typed);
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

    #[test]
    fn interactive_prompt_fires_on_fleetview_agent_view() {
        // THE 2026-07-13 bug: the main pane sits on the FleetView agent-view.
        // The "↑/↓ to select · Enter to view" hint sits at the TOP of the
        // agent box, so with a long agent list + trailing bypass-permissions /
        // token / version status lines the footer scrolls ABOVE the 25-line
        // tail scan and signature (2) misses it. The selected-agent cursor row
        // (`❯ ● main`) is at/near the bottom — signature (5) must match it so
        // a watcher-down / resume inject is SUPPRESSED instead of clobbering
        // the agent-view.
        let output = "\u{2191}/\u{2193} to select \u{00b7} Enter to view \u{00b7} Esc to close\n\
                      \u{276f} \u{25cf} main\n\
                        \u{25ef} general-purpose  Add #1629 coverage\u{2026} 1h 4m 0s\n\
                        \u{25ef} general-purpose  Fix pr-watch stage\u{2026}  56m 43s\n\
                        \u{25ef} general-purpose  Investigate flake\u{2026}   42m 10s\n\
                        \u{25ef} general-purpose  Sync worklog docs\u{2026}   38m  2s\n\
                        \u{25ef} general-purpose  Merge falcon-dev\u{2026}    31m 55s\n\
                      \u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n\
                      -- INSERT --\u{23f5}\u{23f5} bypass permissions on \u{00b7} 5 background tasks   141197 tokens\n\
                                                             current: 2.1.77 \u{00b7} latest: 2.1.\u{2026}";
        assert!(
            interactive_prompt_visible(output),
            "FleetView agent-view (footer scrolled out) must be matched via the agent-selector cursor row (signature 5)"
        );
    }

    #[test]
    fn interactive_prompt_fires_on_fleetview_agent_view_idle_bullet() {
        // A FleetView agent-view whose SELECTED row is an idle agent
        // (`❯ ◯ …`) rather than the running `main`. Signature (5) must
        // still match the hollow-bullet cursor row.
        let output = "\u{276f} \u{25ef} general-purpose  Some running task\u{2026} 12m 3s\n\
                        \u{25cf} main\n\
                      \u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n\
                      -- INSERT --\u{23f5}\u{23f5} bypass permissions on   90000 tokens";
        assert!(
            interactive_prompt_visible(output),
            "FleetView agent-view with idle-bullet selection must be matched (signature 5)"
        );
    }

    #[test]
    fn interactive_prompt_bullet_row_needs_cursor() {
        // A status bullet WITHOUT the `❯` cursor (e.g. a plain `● Bash(...)`
        // tool-output line, or an unselected agent row) must NOT match
        // signature (5) — only the `❯`-cursored selection row does.
        let output = "\u{25cf} Bash(ls -la) running\n\
                      \u{25ef} general-purpose  background task\u{2026}\n\
                      \u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n\
                      \u{276f}\n\
                      \u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n\
                      -- INSERT --\u{23f5}\u{23f5} bypass permissions on \u{00b7} esc to interrupt";
        assert!(
            !interactive_prompt_visible(output),
            "a status bullet without the ❯ cursor must NOT be flagged (bare idle prompt below)"
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

    // -------------------------------------------------------------------
    // permission_prompt_visible — the NARROWEST detector, the only one
    // allowed to drive a keystroke. Fixtures are the dialog shapes Claude
    // Code actually renders, plus the neighbours that must never match.
    // -------------------------------------------------------------------

    /// The dialog from the incident this monitor exists for: a guarded `rm`
    /// raised a permission prompt in the main pane and nobody was there to
    /// answer it for four and a half hours.
    const GUARDED_RM_DIALOG: &str = "\u{25cf} Bash(rm -f /tmp/pc.json /tmp_stderr.log)\n\
         \u{256d}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{256e}\n\
         \u{2502} Bash command                                          \u{2502}\n\
         \u{2502}                                                       \u{2502}\n\
         \u{2502}   rm -f /tmp/pc.json /tmp_stderr.log                  \u{2502}\n\
         \u{2502}   Remove scratch files                                \u{2502}\n\
         \u{2502}                                                       \u{2502}\n\
         \u{2502} Dangerous rm operation on critical path: /tmp_stderr.log \u{2502}\n\
         \u{2502}                                                       \u{2502}\n\
         \u{2502} Do you want to proceed?                               \u{2502}\n\
         \u{2502} \u{276f} 1. Yes                                            \u{2502}\n\
         \u{2502}   2. No                                               \u{2502}\n\
         \u{2570}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{256f}\n\
         Esc to cancel \u{00b7} Tab to amend";

    #[test]
    fn permission_prompt_fires_on_guarded_rm_dialog() {
        let p = permission_prompt_visible(GUARDED_RM_DIALOG, 16)
            .expect("the guarded-rm permission dialog must be detected");
        assert_eq!(p.question, "Do you want to proceed?");
        assert!(
            p.context.contains("rm -f /tmp/pc.json"),
            "captured context must carry the blocked COMMAND — that is the \
             whole point of the payload: {}",
            p.context
        );
        assert!(
            p.context.contains("Dangerous rm operation"),
            "captured context must carry the reason the prompt was raised: {}",
            p.context
        );
    }

    #[test]
    fn permission_prompt_fires_on_tell_claude_deny_row() {
        // The other shape in the wild: three options, with the Escape
        // affordance riding on the deny row as a trailing "(esc)" instead of
        // a footer. Note option 2 is a "Yes, and don't ask again" — sending
        // the digit `2` here would grant a BLANKET approval, which is why the
        // deny key is Escape and never a digit.
        let output = "\u{25cf} Update(src/config.rs)\n\
                      Edit file\n\
                      Do you want to make this edit to config.rs?\n\
                      \u{276f} 1. Yes\n\
                        2. Yes, and don't ask again this session\n\
                        3. No, and tell Claude what to do differently (esc)";
        let p = permission_prompt_visible(output, 16).expect("edit permission dialog");
        assert_eq!(p.question, "Do you want to make this edit to config.rs?");
    }

    #[test]
    fn permission_prompt_fires_on_mcp_tool_dialog() {
        let output = "MCP tool call\n\
                      \u{2502} mcp__github__create_issue                    \u{2502}\n\
                      \u{2502} Do you want to allow this tool call?          \u{2502}\n\
                      \u{2502} \u{276f} 1. Yes                                    \u{2502}\n\
                      \u{2502}   2. No, and tell Claude what to do differently (esc) \u{2502}";
        assert!(permission_prompt_visible(output, 16).is_some());
    }

    #[test]
    fn permission_prompt_selection_cursor_does_not_change_signature() {
        // The operator arrowing the selection from "Yes" to "No" repaints the
        // cursor onto a different row. That is the SAME dialog — if the
        // signature moved, the stale clock would reset every time someone
        // looked at it, and the monitor would never reach its thresholds.
        let on_yes = "Do you want to proceed?\n\
                      \u{276f} 1. Yes\n\
                        2. No, and tell Claude what to do differently (esc)";
        let on_no = "Do you want to proceed?\n\
                       1. Yes\n\
                     \u{276f} 2. No, and tell Claude what to do differently (esc)";
        let a = permission_prompt_visible(on_yes, 16).expect("cursor on yes");
        let b = permission_prompt_visible(on_no, 16).expect("cursor on no");
        assert_eq!(
            a.signature, b.signature,
            "moving the selection cursor must not read as a NEW dialog"
        );
    }

    #[test]
    fn permission_prompt_different_command_changes_signature() {
        // A different tool call IS a different dialog and must restart the
        // clock — otherwise a newly-raised prompt inherits an old one's age
        // and gets denied on sight.
        let first = "Bash command\n\
                       rm -rf /tmp/a\n\
                     Do you want to proceed?\n\
                     \u{276f} 1. Yes\n\
                       2. No, and tell Claude what to do differently (esc)";
        let second = "Bash command\n\
                        rm -rf /tmp/b\n\
                      Do you want to proceed?\n\
                      \u{276f} 1. Yes\n\
                        2. No, and tell Claude what to do differently (esc)";
        let a = permission_prompt_visible(first, 16).expect("first");
        let b = permission_prompt_visible(second, 16).expect("second");
        assert_ne!(a.signature, b.signature);
    }

    #[test]
    fn permission_prompt_not_fired_on_prose_containing_proceed() {
        // Ordinary assistant output that happens to discuss proceeding. No
        // option rows, no Escape affordance.
        let output = "\u{25cf} I'll go ahead and proceed with the migration now.\n\
                      Do you want to review the plan first? Let me know.\n\
                      ─────────────\n\
                      \u{276f}\n\
                      -- INSERT -- \u{23f5}\u{23f5} bypass permissions on   90000 tokens";
        assert!(
            permission_prompt_visible(output, 16).is_none(),
            "prose must never be treated as a dialog a keystroke can answer"
        );
    }

    #[test]
    fn permission_prompt_not_fired_on_folder_trust_dialog() {
        // The startup folder-trust dialog. Declining it makes Claude Code
        // EXIT, so it is an operator decision, not a blocked tool call —
        // and its question does not open with "Do you want to".
        let output = "Do you trust the files in this folder?\n\
                      /home/user/repos/some-repo\n\
                      \u{276f} 1. Yes, proceed\n\
                        2. No, exit\n\
                      Enter to confirm \u{00b7} Esc to cancel";
        assert!(
            permission_prompt_visible(output, 16).is_none(),
            "the folder-trust dialog must never be auto-denied"
        );
    }

    #[test]
    fn permission_prompt_not_fired_on_bypass_permissions_dialog() {
        // The launch consent screen: unnumbered rows, and its own handler.
        let output = "WARNING: Claude Code running in Bypass Permissions mode\n\
                      In Bypass Permissions mode, Claude Code will not ask for your\n\
                      approval before running potentially dangerous commands.\n\
                      \u{276f} No, exit\n\
                        Yes, I accept\n\
                      Enter to confirm \u{00b7} Esc to cancel";
        assert!(permission_prompt_visible(output, 16).is_none());
        // …and the dedicated detector still owns it.
        assert!(bypass_permissions_dialog_visible(output));
    }

    #[test]
    fn permission_prompt_not_fired_on_ask_user_question_menu() {
        // A real operator question: model-written options, no Yes/No pair.
        // The generic `blocking_question_visible` alarm still covers it.
        let output = "Which approach should I take?\n\
                      \u{276f} 1. Refactor in place\n\
                        2. Rewrite from scratch\n\
                        3. Leave as-is\n\
                      \u{2191}/\u{2193} to select \u{00b7} Enter to confirm \u{00b7} Esc to cancel";
        assert!(permission_prompt_visible(output, 16).is_none());
        assert!(
            blocking_question_visible(output),
            "an operator question must still reach the stale-question alarm"
        );
    }

    #[test]
    fn permission_prompt_not_fired_on_fleetview_agent_view() {
        let output = "\u{276f} \n\
                      ───────────────\n\
                      -- INSERT -- \u{2191}/\u{2193} to select \u{00b7} Enter to view   413051 tokens\n\
                      \u{276f} \u{25cf} main\n\
                      \u{25ef} general-purpose  Add coverage… 1h 4m 0s";
        assert!(permission_prompt_visible(output, 16).is_none());
    }

    #[test]
    fn permission_prompt_not_fired_on_partial_render() {
        // A mid-repaint frame: the question line has landed but the option
        // rows have not. Fails closed — the next capture, once the dialog is
        // whole, starts the clock.
        let output = "Bash command\n\
                        rm -f /tmp/pc.json\n\
                      Do you want to proceed?";
        assert!(permission_prompt_visible(output, 16).is_none());
    }

    #[test]
    fn permission_prompt_requires_escape_affordance_or_tool_header() {
        // Yes/No options, no "(esc)", no "Esc to cancel" footer AND no
        // tool-consent header: nothing here says this box is a blocked tool
        // call, so we do not touch it.
        let output = "Do you want to proceed?\n\
                      \u{276f} 1. Yes\n\
                        2. No";
        assert!(permission_prompt_visible(output, 16).is_none());
    }

    /// The dialog from the incident that motivated marker (4b): a subagent's
    /// scratch cleanup tripped the dangerous-`rm` guard and the box rendered
    /// NO Escape affordance at all — no trailing `(esc)` on the deny row, no
    /// `Esc to cancel` footer. The monitor failed closed, logged nothing, and
    /// the dialog blocked the pane for eighteen minutes until a human cleared
    /// it. The `Bash command` header is what now identifies it.
    const GUARDED_RM_DIALOG_NO_ESC_HINT: &str =
        "\u{25cf} Bash(rm -f $SP/*.m4v $SP/*.mkv)\n\
         \u{256d}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{256e}\n\
         \u{2502} Bash command                                          \u{2502}\n\
         \u{2502}                                                       \u{2502}\n\
         \u{2502}   rm -f $SP/*.m4v $SP/*.avi $SP/*.mkv                 \u{2502}\n\
         \u{2502}   Clean up scratch files                              \u{2502}\n\
         \u{2502}                                                       \u{2502}\n\
         \u{2502} Dangerous rm operation on possibly-empty variable path: $SP/*.m4v \u{2502}\n\
         \u{2502}                                                       \u{2502}\n\
         \u{2502} Do you want to proceed?                               \u{2502}\n\
         \u{2502} \u{276f} 1. Yes                                            \u{2502}\n\
         \u{2502}   2. No                                               \u{2502}\n\
         \u{2570}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{256f}";

    #[test]
    fn permission_prompt_fires_on_guarded_rm_dialog_without_escape_hint() {
        let p = permission_prompt_visible(GUARDED_RM_DIALOG_NO_ESC_HINT, 16)
            .expect("a Yes/No tool-consent dialog must match on its header alone");
        assert_eq!(p.question, "Do you want to proceed?");
        assert!(
            p.context.contains("possibly-empty variable path"),
            "captured context must carry the reason the prompt was raised: {}",
            p.context
        );
        assert!(
            p.context.contains("rm -f $SP/*.m4v"),
            "captured context must carry the blocked COMMAND: {}",
            p.context
        );
    }

    #[test]
    fn permission_prompt_fires_on_header_only_edit_and_mcp_dialogs() {
        // Every tool-consent class, stripped of the Escape hint: the header is
        // the only thing identifying them, and all of them are safe to decline.
        for header in [
            "Bash command (unsandboxed)",
            "Edit file",
            "Create file",
            "Write file",
            "Overwrite file",
            "Edit notebook",
            "Read file",
            "MCP tool call",
            "Network request outside of sandbox",
        ] {
            let output = format!(
                "{}\n  some/target\nDo you want to proceed?\n\u{276f} 1. Yes\n  2. No",
                header
            );
            assert!(
                permission_prompt_visible(&output, 16).is_some(),
                "tool-consent dialog with header {:?} must be detected",
                header
            );
        }
    }

    #[test]
    fn permission_prompt_accepts_a_deny_worded_decline_row() {
        // Defensive: some dialogs word the decline row "Deny, and tell Claude
        // what to do differently". A wording change must not silence the
        // monitor the way the dropped Escape hint did.
        let output = "Bash command\n\
                        rm -rf /tmp/a\n\
                      Do you want to proceed?\n\
                      \u{276f} 1. Yes\n\
                        2. Deny, and tell Claude what to do differently";
        assert!(permission_prompt_visible(output, 16).is_some());
    }

    #[test]
    fn permission_prompt_header_alone_is_not_a_dialog() {
        // A tool preview with no question and no option rows — marker (4b)
        // must never be sufficient on its own.
        let output = "Bash command\n  rm -rf /tmp/a\n  Remove scratch files";
        assert!(permission_prompt_visible(output, 16).is_none());
    }

    #[test]
    fn permission_prompt_vetoes_a_reworded_folder_trust_dialog() {
        // Hypothetical rewording that WOULD clear markers (1)-(3) and carries
        // an Escape footer. Declining folder-trust makes Claude Code EXIT, so
        // the text veto keeps it out regardless of shape.
        let output = "Do you want to trust the files in this folder?\n\
                      /home/user/repos/some-repo\n\
                      \u{276f} 1. Yes, proceed\n\
                        2. No, exit\n\
                      Enter to confirm \u{00b7} Esc to cancel";
        assert!(
            permission_prompt_visible(output, 16).is_none(),
            "the folder-trust dialog must never be auto-denied, however worded"
        );
    }

    #[test]
    fn permission_prompt_vetoes_a_numbered_bypass_permissions_dialog() {
        // Same veto for the launch consent screen, here given numbered Yes/No
        // rows it does not currently have.
        let output = "WARNING: Claude Code running in Bypass Permissions mode\n\
                      Do you want to proceed?\n\
                      \u{276f} 1. Yes, I accept\n\
                        2. No, exit (esc)";
        assert!(permission_prompt_visible(output, 16).is_none());
    }

    #[test]
    fn permission_prompt_requires_yes_as_first_option() {
        // Question phrased like a permission prompt, but the options are not
        // the approve/decline pair — so it is not the dialog class Escape has
        // a defined meaning on.
        let output = "Do you want to pick a branch?\n\
                      \u{276f} 1. main\n\
                        2. No branch (esc)";
        assert!(permission_prompt_visible(output, 16).is_none());
    }

    #[test]
    fn permission_prompt_takes_the_live_dialog_not_a_scrollback_quote() {
        // An older dialog quoted in scrollback above a live one: the LIVE
        // (last) question must win, or the daemon would deny based on the
        // text of a prompt that is already answered.
        let output = "Do you want to proceed?\n\
                      \u{276f} 1. Yes\n\
                        2. No, and tell Claude what to do differently (esc)\n\
                      \u{25cf} Bash(git push)\n\
                      Bash command\n\
                        git push --force\n\
                      Do you want to proceed with the force push?\n\
                      \u{276f} 1. Yes\n\
                        2. No, and tell Claude what to do differently (esc)";
        let p = permission_prompt_visible(output, 16).expect("live dialog");
        assert_eq!(p.question, "Do you want to proceed with the force push?");
    }

    #[test]
    fn permission_prompt_deny_key_is_escape() {
        // Load-bearing: a digit could land on "Yes, and don't ask again"
        // because option numbering shifts between dialogs. The monitor must
        // have exactly one key, and it must be the non-approving one.
        assert_eq!(PERMISSION_PROMPT_DENY_KEY, "Escape");
    }

    #[test]
    fn permission_prompt_context_is_length_capped() {
        // A pathological pane must not turn into a multi-kilobyte pingme.
        let mut output = String::new();
        for i in 0..40 {
            output.push_str(&format!("  filler line {} {}\n", i, "x".repeat(200)));
        }
        output.push_str(
            "Do you want to proceed?\n\
             \u{276f} 1. Yes\n\
               2. No, and tell Claude what to do differently (esc)",
        );
        let p = permission_prompt_visible(&output, 30).expect("dialog under filler");
        assert!(
            p.context.chars().count() <= PERMISSION_PROMPT_CONTEXT_MAX_CHARS + 1,
            "context must be capped, got {} chars",
            p.context.chars().count()
        );
    }

    #[test]
    fn parse_option_row_accepts_cursored_and_plain_rows() {
        assert_eq!(
            parse_option_row("\u{276f} 1. Yes"),
            Some((1, "yes".to_string()))
        );
        assert_eq!(
            parse_option_row("  2. No, and tell Claude what to do differently (esc)"),
            Some((
                2,
                "no, and tell claude what to do differently (esc)".to_string()
            ))
        );
        assert_eq!(
            parse_option_row("\u{2502}   3. Maybe   \u{2502}"),
            Some((3, "maybe".to_string()))
        );
        assert_eq!(parse_option_row("Do you want to proceed?"), None);
        assert_eq!(parse_option_row("\u{276f}"), None);
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

    // bypass_permissions_dialog_visible — the launch-time consent dialog
    // Claude Code renders under `--dangerously-skip-permissions` when the
    // acceptance is not persisted in settings. Verbatim capture of the pane
    // the daemon injected into on 2026-08-29 (Claude Code 2.1.251), which
    // exited Claude because the default selection is "No, exit".
    const BYPASS_DIALOG_PANE: &str = include_str!("../tests/fixtures/bypass_permissions_dialog.txt");

    #[test]
    fn bypass_permissions_dialog_matches_the_captured_pane() {
        assert!(bypass_permissions_dialog_visible(BYPASS_DIALOG_PANE));
    }

    #[test]
    fn bypass_permissions_dialog_not_fired_on_a_live_bypass_session() {
        // The running TUI renders a PERSISTENT bypass-permissions status line
        // on essentially every frame. That must never read as the dialog, or
        // the daemon would think a modal is up for the whole session.
        let output = "\u{25cf} Brewed for 12s\n\
                      ─────────────\n\
                      \u{276f}\n\
                      ─────────────\n\
                      \u{23f5}\u{23f5} bypass permissions on (shift+tab to cycle) \u{00b7} esc to interrupt";
        assert!(!bypass_permissions_dialog_visible(output));
    }

    #[test]
    fn bypass_permissions_dialog_needs_both_markers() {
        // Title without the confirm label (e.g. the warning scrolled past in
        // conversation text) is not the dialog.
        let title_only = "WARNING: Claude Code running in Bypass Permissions mode\n\u{276f}";
        assert!(!bypass_permissions_dialog_visible(title_only));
        // Confirm label without the mode wording is some other dialog.
        let label_only = "  Do the thing?\n\u{276f} No, exit\n  Yes, I accept";
        assert!(!bypass_permissions_dialog_visible(label_only));
    }

    #[test]
    fn bypass_permissions_dialog_ignores_old_scrollback() {
        // The dialog text 40 lines up (a transcript, this doc read into the
        // pane) is history, not a live modal.
        let mut lines = vec!["scrollback".to_string(); 40];
        lines.insert(0, BYPASS_DIALOG_PANE.to_string());
        assert!(!bypass_permissions_dialog_visible(&lines.join("\n")));
    }

    #[test]
    fn bypass_permissions_dialog_reads_as_an_interactive_prompt() {
        // The whole bug: the dialog's `❯ No, exit` row satisfies the bare-`❯`
        // idle check, so every inject guard must see it as a live prompt.
        assert!(check_lines_for_idle_prompt(BYPASS_DIALOG_PANE));
        assert!(interactive_prompt_visible(BYPASS_DIALOG_PANE));
        assert!(!idle_prompt_without_bypass_dialog(BYPASS_DIALOG_PANE));
    }

    // login_dialog_visible — the `/login` OAuth modal claude-watch opens
    // itself (2026-09-17: the daemon typed into its own dialog and resolved
    // its own expiry window behind it).

    /// The code-paste phase, as the modal renders once a method is picked.
    /// Note what is NOT here: no `❯`, no status line, no token count, and no
    /// "Your login expires in N days" — all of it covered by the modal.
    const LOGIN_CODE_PANE: &str = "\
 Claude Code\n\
\n\
 Browser didn't open? Use the url below to sign in (c to copy):\n\
\n\
 https://claude.com/cai/oauth/authorize?code=true&client_id=REDACTED&response_type=code\n\
\n\
 Paste code here if prompted > \n";

    /// The method-picker phase, which `self-login` drives with Down/Enter.
    const LOGIN_MENU_PANE: &str = "\
 Select login method:\n\
\n\
\u{276f} Claude account with subscription\n\
  Anthropic Console account\n";

    #[test]
    fn login_dialog_matches_both_phases_of_the_modal() {
        assert!(login_dialog_visible(LOGIN_CODE_PANE));
        assert!(login_dialog_visible(LOGIN_MENU_PANE));
    }

    #[test]
    fn login_dialog_matches_the_ascii_and_typographic_apostrophe() {
        assert!(login_dialog_visible("  Browser didn't open? Use the url below"));
        assert!(login_dialog_visible(
            "  Browser didn\u{2019}t open? Use the url below"
        ));
    }

    #[test]
    fn login_dialog_not_fired_on_a_healthy_pane() {
        let output = "\u{25cf} Done\n\u{276f} \n\
                      \u{23f5}\u{23f5} bypass permissions on · 4 monitors\n\
                                                       91928 tokens";
        assert!(!login_dialog_visible(output));
    }

    #[test]
    fn login_dialog_ignores_old_scrollback() {
        // The modal 40 lines up is a transcript, not a live dialog.
        let mut lines = vec!["scrollback".to_string(); 40];
        lines.insert(0, LOGIN_CODE_PANE.to_string());
        assert!(!login_dialog_visible(&lines.join("\n")));
    }

    #[test]
    fn login_dialog_suppresses_injects() {
        // The regression. The code-paste phase draws NO `❯`, so it does not
        // even read as idle; the method picker DOES (`❯ Claude account …`),
        // which is exactly the shape that slipped past every guard. Both must
        // now suppress.
        assert!(interactive_prompt_visible(LOGIN_CODE_PANE));
        assert!(interactive_prompt_visible(LOGIN_MENU_PANE));
    }

    #[test]
    fn login_code_prompt_has_no_prompt_line_for_the_insert_probe_to_undo() {
        // Why `ensure_insert_mode` must skip the probe on this modal rather
        // than rely on its literal-`i` cleanup: that cleanup diffs
        // `prompt_line_text`, which keys on `❯`. There is none here, so the
        // before/after diff can never fire and the `i` stays in the code
        // field — one per inject, the observed `…iiiiii`.
        assert_eq!(prompt_line_text(LOGIN_CODE_PANE), None);
        assert!(!insert_key_landed_literal(None, None));
    }

    #[test]
    fn idle_prompt_without_bypass_dialog_still_matches_a_real_prompt() {
        let output = "\u{25cf} Done\n\u{276f} \n\
                      \u{23f5}\u{23f5} bypass permissions on (shift+tab to cycle)";
        assert!(idle_prompt_without_bypass_dialog(output));
    }

    #[test]
    fn bypass_permissions_accept_keys_move_off_the_default_then_confirm() {
        // The dialog is rendered cancel-first with the cancel row focused
        // ("❯ No, exit" above "Yes, I accept"), and hides option indexes — so
        // a bare Enter picks "No, exit" (which is what exited Claude) and
        // there is no number-key shortcut. Exactly one Down, then Enter.
        assert_eq!(BYPASS_PERMISSIONS_ACCEPT_KEYS, ["Down", "Enter"]);

        // Assert that against the rendered option order rather than trusting
        // the constant: count the rows between the cursor and the confirm
        // label in the captured pane.
        let rows: Vec<&str> = BYPASS_DIALOG_PANE
            .lines()
            .map(|l| l.trim())
            .filter(|l| l.ends_with("No, exit") || *l == "Yes, I accept")
            .collect();
        assert_eq!(rows, vec!["\u{276f} No, exit", "Yes, I accept"]);
        let cursor_row = rows
            .iter()
            .position(|l| l.starts_with('\u{276f}'))
            .expect("cursor row");
        let confirm_row = rows
            .iter()
            .position(|l| l.contains("Yes, I accept"))
            .expect("confirm row");
        let downs = confirm_row - cursor_row;
        assert_eq!(downs, 1, "one Down moves from the default to the confirm row");
        assert_eq!(
            BYPASS_PERMISSIONS_ACCEPT_KEYS.iter().filter(|k| **k == "Down").count(),
            downs
        );
        assert_eq!(*BYPASS_PERMISSIONS_ACCEPT_KEYS.last().unwrap(), "Enter");
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

    // --- check_lines_for_401_banner: the in-TUI access-token-expired banner ---
    //
    // The real thing, as observed: the TUI fully intact (tokens footer,
    // "bypass permissions on", ❯ prompt) with ONE inline line from Claude
    // Code. `check_lines_for_reauth` must keep saying no to this frame (the
    // TUI guard above is load-bearing), and this detector must say yes — the
    // decision to ACT is then the caller's, made against the credential store.
    const BANNER_401_WITH_TUI: &str = concat!(
        "● Please run /login · API Error: 401 OAuth access token has expired. ",
        "Re-authenticate to continue.\n",
        "\n",
        "❯ \n",
        "  bypass permissions on · 871,864 tokens\n"
    );

    #[test]
    fn test_401_banner_detected_with_tui_up() {
        assert!(check_lines_for_401_banner(BANNER_401_WITH_TUI));
        // ...and the login-screen detector still leaves this frame alone.
        assert!(!check_lines_for_reauth(BANNER_401_WITH_TUI));
    }

    #[test]
    fn test_401_banner_detected_on_the_older_json_form() {
        // The older render: banner line plus the raw error JSON underneath.
        // Same frame `test_reauth_not_detected_401_text_with_tui` holds for.
        let output = concat!(
            "Please run /login · API Error: 401\n",
            "{\"type\":\"error\",\"error\":{\"type\":\"authentication_error\",",
            "\"message\":\"Invalid authentication credentials\"}}\n",
            "❯ \n",
            "-- INSERT --  871864 tokens"
        );
        assert!(check_lines_for_401_banner(output));
    }

    #[test]
    fn test_401_banner_survives_tmux_hard_wrap() {
        // A narrow pane wraps the banner mid-phrase with no separator.
        let output = concat!(
            "● Please run /login · API Err\n",
            "or: 401 OAuth access token has exp\n",
            "ired. Re-authenticate to continue.\n",
            "❯ \n",
            "  12,345 tokens\n"
        );
        assert!(check_lines_for_401_banner(output));
    }

    #[test]
    fn test_401_banner_not_detected_when_tui_gone() {
        // With the TUI gone this is a login-screen frame, not a banner frame —
        // the other detector owns it and this one must not double-claim.
        let output = "Please run /login · API Error: 401 OAuth access token has expired.\n";
        assert!(!check_lines_for_401_banner(output));
        // (and the other detector does pick it up via "re-authenticate")
        let output = "API Error: 401 OAuth access token has expired. Re-authenticate to continue.";
        assert!(check_lines_for_reauth(output));
    }

    #[test]
    fn test_401_banner_requires_the_combination_not_one_phrase() {
        // "/login" alone (the proactive expiry warning, say) is not a 401.
        assert!(!check_lines_for_401_banner(
            "Your login expires in 2 days · run /login to renew\n❯ \n 1,234 tokens"
        ));
        assert!(!check_lines_for_401_banner("Please run /login\n❯ \n 1,234 tokens"));
        // A 401 alone (a curl in the conversation) is not the banner.
        assert!(!check_lines_for_401_banner(
            "curl: HTTP/1.1 401 Unauthorized\nAPI Error: 401\n❯ \n 1,234 tokens"
        ));
        assert!(!check_lines_for_401_banner(
            "the OAuth access token has expired on the other host\n❯ \n 1,234 tokens"
        ));
        assert!(!check_lines_for_401_banner(""));
    }

    // --- classify_reauth_frame: one frame, one verdict ---

    #[test]
    fn test_reauth_frame_login_screen_wins_and_carries_the_url() {
        // TUI gone, login screen up with the authorize URL: phase 2, with URL.
        let frame = concat!(
            "Browser didn't open? Use the url below to sign in:\n",
            "https://claude.com/cai/oauth/authorize?code=true&client_id=abc\n",
            "Paste code here if prompted >\n"
        );
        match classify_reauth_frame(frame) {
            ReauthSignal::LoginScreen { url } => {
                assert!(url.starts_with("https://claude.com/cai/oauth/authorize"), "{url}")
            }
            other => panic!("expected LoginScreen, got {other:?}"),
        }
        // TUI gone, login-ish text but no URL yet: phase 2 with an empty URL,
        // which is what tells the caller to inject `/login` and wait.
        assert_eq!(
            classify_reauth_frame("Session expired. Login required.\n"),
            ReauthSignal::LoginScreen { url: String::new() }
        );
    }

    #[test]
    fn test_reauth_frame_banner_with_tui_is_banner401() {
        assert_eq!(
            classify_reauth_frame(BANNER_401_WITH_TUI),
            ReauthSignal::Banner401
        );
    }

    #[test]
    fn test_reauth_frame_normal_tui_is_none() {
        assert_eq!(
            classify_reauth_frame("● Done.\n\n❯ \n  bypass permissions on · 57,129 tokens\n"),
            ReauthSignal::None
        );
        assert_eq!(classify_reauth_frame(""), ReauthSignal::None);
        // The proactive warning is NOT this path's business.
        assert_eq!(
            classify_reauth_frame(
                "Your login expires in 2 days · run /login to renew\n❯ \n 1,234 tokens"
            ),
            ReauthSignal::None
        );
    }

    #[test]
    fn test_401_banner_text_in_conversation_is_still_detected_as_text() {
        // Somebody reading THIS file has the banner on the pane. The detector
        // says "banner present" — it is text, and it cannot know better. The
        // false-positive guard lives one layer up, in the credential
        // corroboration (policy::decide_banner_action), and that is where the
        // healthy-credentials case is asserted silent.
        let output = concat!(
            "    /// ● Please run /login · API Error: 401 OAuth access token has expired.\n",
            "❯ \n",
            "  bypass permissions on · 55,000 tokens\n"
        );
        assert!(check_lines_for_401_banner(output));
    }

    // ---- Usage-credit exhaustion ----

    /// The pane as it looked during the real incident: TUI fully intact, one
    /// line answering the turn.
    const CREDITS_EXHAUSTED_PANE: &str = concat!(
        "● You're out of usage credits. Run /usage-credits to keep using Fable 5 or /model to switch models.\n",
        "\n",
        "❯ \n",
        "  bypass permissions on · 143,201 tokens\n"
    );

    #[test]
    fn credit_exhaustion_is_detected_on_a_live_tui() {
        let banner = detect_credit_exhaustion(CREDITS_EXHAUSTED_PANE)
            .expect("the incident pane must be detected");
        assert_eq!(
            banner.exhausted_model.as_deref(),
            Some("Fable 5"),
            "the push notification has to name the model as the operator knows it"
        );
    }

    #[test]
    fn credit_exhaustion_survives_a_hard_wrap_mid_word() {
        // tmux wraps with NO separator and no hyphenation, so the sentence
        // can split at any column — including inside "credits" and inside
        // the model name.
        let wrapped = concat!(
            "● You're out of usage cre\n",
            "dits. Run /usage-credi\n",
            "ts to keep using Fabl\n",
            "e 5 or /model to switch models.\n",
            "❯ \n",
            "  bypass permissions on · 143,201 tokens\n"
        );
        let banner = detect_credit_exhaustion(wrapped).expect("wrapped pane must still match");
        // The display form degrades on a mid-word wrap; what must survive is
        // the identity of the model, which is all any decision uses (see
        // policy::model_ids_match, which compares alphanumerics only).
        let alnum: String = banner
            .exhausted_model
            .as_deref()
            .expect("a model name")
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .flat_map(|c| c.to_lowercase())
            .collect();
        assert_eq!(alnum, "fable5");
    }

    #[test]
    fn credit_exhaustion_needs_the_remedy_command_too() {
        // Prose that happens to contain the phrase, without the command next
        // to it, is not the banner.
        let prose = concat!(
            "● We ran out of usage credits about an hour before the weekly reset.\n",
            "❯ \n",
            "  bypass permissions on · 143,201 tokens\n"
        );
        assert!(detect_credit_exhaustion(prose).is_none());
    }

    #[test]
    fn credit_exhaustion_needs_a_live_tui() {
        // No TUI on the pane = somebody else's detector (login screen, dead
        // process). Guessing here would step on them.
        let no_tui = "You're out of usage credits. Run /usage-credits to keep using Fable 5 or /model to switch models.\n";
        assert!(detect_credit_exhaustion(no_tui).is_none());
    }

    #[test]
    fn credit_exhaustion_text_in_conversation_is_still_detected_as_text() {
        // Somebody reading THIS file, or the PR that added it, has the
        // sentence on their pane while their account is in perfectly good
        // standing. The detector says "message present" — it is text, and it
        // cannot know better. The false-positive guard lives one layer up, in
        // the transcript corroboration (policy::decide_credit_action), and
        // that is where the healthy case is asserted silent.
        let quoting = concat!(
            "  /// You're out of usage credits. Run /usage-credits to keep using Fable 5 or /model to switch models.\n",
            "❯ \n",
            "  bypass permissions on · 55,000 tokens\n"
        );
        assert!(detect_credit_exhaustion(quoting).is_some());
    }

    #[test]
    fn credit_exhaustion_model_name_is_optional() {
        // A pane where the offer clause is missing or reworded still carries
        // the banner; only the name is unknown.
        let no_offer = concat!(
            "● You're out of usage credits. Run /usage-credits to continue.\n",
            "❯ \n",
            "  bypass permissions on · 55,000 tokens\n"
        );
        let banner = detect_credit_exhaustion(no_offer).expect("banner without the offer clause");
        assert_eq!(banner.exhausted_model, None);
    }

    #[test]
    fn credit_exhaustion_model_capture_is_bounded() {
        // Two far-apart fragments must not let the capture swallow the screen
        // between them into a "model name".
        let far_apart = format!(
            "● You're out of usage credits. Run /usage-credits to keep using\n{}\nor /model to switch models.\n❯ \n  57,129 tokens\n",
            "x".repeat(200)
        );
        let banner = detect_credit_exhaustion(&far_apart).expect("still the banner");
        assert_eq!(banner.exhausted_model, None, "capture must stay bounded");
    }

    #[test]
    fn a_healthy_pane_has_no_credit_banner() {
        assert!(detect_credit_exhaustion("● Done.\n\n❯ \n  57,129 tokens\n").is_none());
        assert!(detect_credit_exhaustion("").is_none());
        // Neither of the auth signals is this one.
        assert!(detect_credit_exhaustion(BANNER_401_WITH_TUI).is_none());
    }

    // ---- The `/model` switch confirmation ----

    /// The pane an operator found after the daemon's demotion "fired": the
    /// command typed, the confirmation up, nothing answering it. Transcribed
    /// from that capture.
    const MODEL_SWITCH_PANE: &str = include_str!("../tests/fixtures/model_switch_dialog.txt");

    #[test]
    fn model_switch_dialog_matches_the_captured_pane() {
        assert!(model_switch_dialog_visible(MODEL_SWITCH_PANE));
        assert_eq!(
            model_switch_cursor(MODEL_SWITCH_PANE),
            Some(ModelSwitchCursor::Confirm),
            "the dialog opens with the confirm row selected"
        );
    }

    #[test]
    fn model_switch_dialog_needs_both_markers() {
        // A title alone (prose, a changelog, this file read into the pane).
        assert!(!model_switch_dialog_visible("  Switch model?\n❯ "));
        // An option row alone, without the dialog around it.
        assert!(!model_switch_dialog_visible("  Yes, switch to Opus 5\n❯ "));
        // A healthy session, and the out-of-credits pane that PRECEDES the
        // dialog: neither is the dialog.
        assert!(!model_switch_dialog_visible("● Done.\n❯ \n  57,129 tokens\n"));
        assert!(!model_switch_dialog_visible(CREDITS_EXHAUSTED_PANE));
        assert!(!model_switch_dialog_visible(""));
    }

    #[test]
    fn model_switch_dialog_ignores_old_scrollback() {
        // The dialog 40 lines up was answered long ago; pressing Enter at it
        // now types into the conversation.
        let mut lines = vec!["scrollback".to_string(); 40];
        lines.insert(0, MODEL_SWITCH_PANE.to_string());
        assert!(!model_switch_dialog_visible(&lines.join("\n")));
    }

    /// The safety property of the whole confirmation path: keystrokes are
    /// produced ONLY for a frame that shows the dialog and says which row is
    /// selected. Everything else presses nothing.
    #[test]
    fn model_switch_keys_are_produced_only_for_a_visible_answerable_dialog() {
        // Idle prompt, the credits banner, an unrelated permission menu,
        // the login modal, empty output -> press NOTHING.
        assert_eq!(model_switch_answer_keys("● Done.\n❯ \n  57,129 tokens\n"), None);
        assert_eq!(model_switch_answer_keys(CREDITS_EXHAUSTED_PANE), None);
        assert_eq!(
            model_switch_answer_keys("  Do you want to proceed?\n❯ 1. Yes\n  2. No\n"),
            None
        );
        assert_eq!(model_switch_answer_keys(LOGIN_MENU_PANE), None);
        assert_eq!(model_switch_answer_keys(""), None);

        // The real dialog, confirm row selected -> a single Enter. No digit is
        // ever sent: a stray `1` would land on the prompt as text.
        assert_eq!(model_switch_answer_keys(MODEL_SWITCH_PANE), Some(&["Enter"][..]));

        // Cursor parked on "No, go back" -> move up first, then confirm.
        let declined = MODEL_SWITCH_PANE
            .replace("❯ 1. Yes", "  1. Yes")
            .replace("  2. No, go back", "❯ 2. No, go back");
        assert_eq!(
            model_switch_cursor(&declined),
            Some(ModelSwitchCursor::Decline)
        );
        assert_eq!(model_switch_answer_keys(&declined), Some(&["Up", "Enter"][..]));

        // Dialog up but no cursor anywhere (a render this code does not know):
        // never guess a key.
        let cursorless = MODEL_SWITCH_PANE.replace("❯ 1. Yes", "  1. Yes");
        assert!(model_switch_dialog_visible(&cursorless));
        assert_eq!(
            model_switch_cursor(&cursorless),
            Some(ModelSwitchCursor::Unknown)
        );
        assert_eq!(model_switch_answer_keys(&cursorless), None);
    }

    #[test]
    fn model_switch_dialog_reads_as_an_interactive_prompt() {
        // The bordered option rows do not start with the cursor glyph, so the
        // numbered-row signature misses them — the dialog has to be matched
        // explicitly or an unrelated inject types into it.
        assert!(interactive_prompt_visible(MODEL_SWITCH_PANE));
    }

    #[test]
    fn an_applied_model_switch_is_recognised() {
        assert!(model_switch_applied("  ⎿  Set model to Opus 5 (1M context)\n❯ \n"));
        assert!(model_switch_applied("  Model set to opus\n❯ \n"));
        // The dialog still being up is not an applied switch.
        assert!(!model_switch_applied(MODEL_SWITCH_PANE));
        assert!(!model_switch_applied("● Done.\n❯ \n"));
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
    fn test_extract_login_url_current_claude_com_endpoint() {
        // The endpoint CURRENT Claude Code builds actually print. Matching only
        // the legacy claude.ai host produced an empty URL in the reauth alert.
        let output = "Browser didn't open? Use the url below to sign in\n\nhttps://claude.com/cai/oauth/authorize?code=true&client_id=abc123\n\nPaste code here if prompted > ";
        assert_eq!(
            extract_login_url(output),
            Some("https://claude.com/cai/oauth/authorize?code=true&client_id=abc123".to_string())
        );
    }

    #[test]
    fn test_extract_login_url_console_endpoint() {
        let output = "https://platform.claude.com/oauth/authorize?code=true&client_id=xyz\n";
        assert_eq!(
            extract_login_url(output),
            Some("https://platform.claude.com/oauth/authorize?code=true&client_id=xyz".to_string())
        );
    }

    #[test]
    fn test_extract_login_url_current_endpoint_wrapped() {
        // Same hard-wrap reassembly, on the current host.
        let output = "https://claude.com/cai/oauth/authorize?code=true&client_id=abc123&code_chall\nenge=xyz789&code_challenge_method=S256";
        assert_eq!(
            extract_login_url(output),
            Some("https://claude.com/cai/oauth/authorize?code=true&client_id=abc123&code_challenge=xyz789&code_challenge_method=S256".to_string())
        );
    }

    #[test]
    fn test_reauth_detected_on_current_authorize_host() {
        // TUI gone + the current authorize URL => reauth screen.
        let output = "Login\n\nBrowser didn't open? Use the url below to sign in\n\nhttps://claude.com/cai/oauth/authorize?code=true\n";
        assert!(check_lines_for_reauth(output));
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
    fn test_api_retry_waiting_for_api_response_banner() {
        // The self-contained spinner banner: no 5xx line, no "attempt N/M",
        // just the client's own countdown. Seen on a flaky endpoint, where it
        // previously read as normal activity.
        let output = "\
\u{276f} run the deploy\n\
\u{2731} Waiting for API response\u{2026} (836k tokens \u{00b7} will retry in 1m 14s \u{00b7} check your network)\n\
";
        assert!(check_lines_for_api_retry(output));
    }

    #[test]
    fn test_api_retry_waiting_banner_seconds_only() {
        let output = "\
\u{2731} Waiting for API response \u{00b7} will retry in 45s \u{00b7} check your network\n\
";
        assert!(check_lines_for_api_retry(output));
    }

    #[test]
    fn test_not_api_retry_waiting_without_countdown() {
        // A plain in-flight request is NOT a retry state — the countdown is
        // the load-bearing half of the banner.
        let output = "\
\u{2731} Waiting for API response\u{2026} (12s \u{00b7} esc to interrupt)\n\
";
        assert!(!check_lines_for_api_retry(output));
    }

    #[test]
    fn test_not_api_retry_countdown_without_waiting_banner() {
        // "retry in" prose in chat history, with no banner and no error cue.
        let output = "\
\u{276f} the workload will retry in 30s if the lock is held\n\
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

#[cfg(test)]
mod inject_and_selfclear_coord_tests {
    use super::*;
    use std::os::unix::io::AsRawFd;

    #[test]
    fn insert_key_literal_detected_when_i_appended() {
        assert!(insert_key_landed_literal(Some(""), Some("i")));
        assert!(insert_key_landed_literal(None, Some("i")));
        assert!(insert_key_landed_literal(Some("hello"), Some("helloi")));
    }

    #[test]
    fn insert_key_not_literal_on_mode_switch() {
        assert!(!insert_key_landed_literal(Some(""), Some("")));
        assert!(!insert_key_landed_literal(Some("hello"), Some("hello")));
        assert!(!insert_key_landed_literal(Some("hello"), Some("helloX")));
        assert!(!insert_key_landed_literal(Some("hello"), Some("helloii")));
        assert!(!insert_key_landed_literal(Some("hello"), None));
        assert!(!insert_key_landed_literal(None, None));
    }

    #[test]
    fn self_clear_lock_path_prefers_env_then_xdg_then_default() {
        assert_eq!(
            resolve_self_clear_lock_path(Some("/tmp/custom.lock"), Some("/run/user/1000")),
            "/tmp/custom.lock"
        );
        assert_eq!(
            resolve_self_clear_lock_path(Some("   "), Some("/run/user/1000")),
            "/run/user/1000/claude-self-clear.lock"
        );
        assert_eq!(
            resolve_self_clear_lock_path(None, Some("/run/user/1000/")),
            "/run/user/1000/claude-self-clear.lock"
        );
        assert_eq!(
            resolve_self_clear_lock_path(None, None),
            "/var/run/claude/claude-self-clear.lock"
        );
        assert_eq!(
            resolve_self_clear_lock_path(None, Some("  ")),
            "/var/run/claude/claude-self-clear.lock"
        );
    }

    #[test]
    fn self_clear_handoff_path_prefers_env_then_xdg_then_default() {
        // Mirrors the lockfile resolution EXACTLY (must agree with
        // container/bin/self-clear's _default_handoff_file()).
        assert_eq!(
            resolve_self_clear_handoff_path(Some("/tmp/custom.handoff"), Some("/run/user/1000")),
            "/tmp/custom.handoff"
        );
        assert_eq!(
            resolve_self_clear_handoff_path(Some("   "), Some("/run/user/1000")),
            "/run/user/1000/claude-self-clear-handoff"
        );
        assert_eq!(
            resolve_self_clear_handoff_path(None, Some("/run/user/1000/")),
            "/run/user/1000/claude-self-clear-handoff"
        );
        assert_eq!(
            resolve_self_clear_handoff_path(None, None),
            "/var/run/claude/claude-self-clear-handoff"
        );
        assert_eq!(
            resolve_self_clear_handoff_path(None, Some("  ")),
            "/var/run/claude/claude-self-clear-handoff"
        );
    }

    #[test]
    fn handoff_is_recent_window_semantics() {
        let now = 1_000_000.0;
        // Absent marker => never recent.
        assert!(!handoff_is_recent(None, now, 120));
        // Just stamped => recent.
        assert!(handoff_is_recent(Some(now), now, 120));
        // Within the window => recent.
        assert!(handoff_is_recent(Some(now - 119.0), now, 120));
        // Exactly at the window edge => recent (inclusive).
        assert!(handoff_is_recent(Some(now - 120.0), now, 120));
        // Older than the window => NOT recent.
        assert!(!handoff_is_recent(Some(now - 120.1), now, 120));
        // Small clock-skew future mtime => still recent.
        assert!(handoff_is_recent(Some(now + 5.0), now, 120));
        // grace 0 disables at the caller layer, but the pure fn with a 0 window
        // only treats an exactly-now (or future) marker as recent.
        assert!(!handoff_is_recent(Some(now - 1.0), now, 0));
    }

    #[test]
    fn lockfile_held_false_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nope.lock");
        assert!(!lockfile_held(path.to_str().unwrap()));
    }

    #[test]
    fn lockfile_held_true_while_locked_false_after_release() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("self-clear.lock");
        let path_s = path.to_str().unwrap().to_string();
        // Hold an exclusive flock via a SEPARATE open file description; flock
        // treats independent opens (even same-process) as conflicting.
        let holder = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        let hfd = holder.as_raw_fd();
        assert_eq!(unsafe { libc::flock(hfd, libc::LOCK_EX | libc::LOCK_NB) }, 0);
        assert!(lockfile_held(&path_s), "held lock must be detected");
        assert_eq!(unsafe { libc::flock(hfd, libc::LOCK_UN) }, 0);
        assert!(!lockfile_held(&path_s), "released lock must read as free");
        drop(holder);
    }
}
