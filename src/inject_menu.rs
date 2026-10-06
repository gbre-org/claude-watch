//! Answering the selection menu a submitted slash command opens.
//!
//! Some slash commands do not finish when they are submitted. `/model <name>`
//! is the one that bit: Claude Code answers it with
//!
//! ```text
//! Switch model?
//! Your next response will be slower and use more tokens
//! ...
//! ❯ 1. Yes, switch to Fable 5
//!   2. No, go back
//! ```
//!
//! and waits. `claude-watch inject` used to return `submitted` the moment the
//! payload left the prompt line, so a caller that injected `/model` to switch
//! its own model got exit 0 while the session sat on the confirmation until a
//! human pressed a key. The inject reported success; the switch had not
//! happened.
//!
//! This module is the second half of a slash-command inject: after the
//! submit, watch the pane briefly for a menu, and deal with it.
//!
//! ## What gets answered, and what does not
//!
//! * `--answer N` — the caller named the option. Whatever menu the submit
//!   opened, select row `N` and press Enter.
//! * No `--answer` — only an ALLOWLISTED menu is answered on the caller's
//!   behalf. Today that is exactly one: the `/model` switch confirmation, and
//!   only when the injected payload itself was a `/model` command (the caller
//!   asked for the switch; the dialog only asks whether they meant it). The
//!   row chosen is the one that applies the switch.
//! * Anything else is REPORTED — its options and which one is selected — and
//!   left alone. A menu nobody has seen before might be a destructive
//!   confirmation; guessing at it is the one thing this path must never do.
//!
//! ## Safety rails (the same asymmetry as the daemon's credit-demote path)
//!
//! An unanswered menu is visible and recoverable. A keystroke typed at a pane
//! that is NOT showing the menu lands in the conversation and cannot be taken
//! back. So:
//!
//! * Every keystroke is preceded by a FRESH capture that still shows the same
//!   menu (same option labels) with a readable cursor.
//! * Selection is by `Up`/`Down` one row at a time, each move verified before
//!   the next, then `Enter` only once the cursor sits on the target row. A
//!   DIGIT is never sent: if the menu closes underneath us, a `1` becomes text
//!   on the prompt, whereas Enter at an empty prompt does nothing.
//! * Keystrokes are capped (rows + a small margin), and the menu must go away
//!   after Enter or the answer is reported as failed, not as done.
//! * A menu is only recognised with a `❯`-selected numbered row AND no live
//!   prompt line below it, so a numbered list in Claude's reply, or a menu
//!   quoted in scrollback above the idle prompt, is not a menu.

use crate::tmux;
use std::time::Duration;
use tokio::time::{sleep, Instant};

/// How many lines at the bottom of the pane a live menu can occupy.
const MENU_TAIL_LINES: usize = 30;

/// Default watch window for a slash command that is not `/model`.
pub const DEFAULT_MENU_WAIT_SECS: u64 = 3;

/// Watch window for `/model`, whose confirmation is the known case. Longer
/// because the dialog can take a beat to render under load, and cheap because
/// the watch ends as soon as the dialog shows or the switch is applied.
pub const MODEL_MENU_WAIT_SECS: u64 = 10;

/// Hard cap on how long a `/model` watch keeps waiting when the submit was
/// QUEUED behind a running turn. Claude Code only processes a queued slash
/// command at the turn boundary, so the confirmation can appear minutes after
/// the submit; a fixed 10s window would give up first and leave the dialog
/// waiting for a human (the failure this guards).
pub const MODEL_QUEUED_WAIT_SECS: u64 = 120;

/// Pure function: should the phase-1 watch keep waiting past its normal
/// window? Only for a `/model` payload (the allowlisted case) while the pane
/// is still busy (the command is queued behind a turn, not applied or
/// dropped) and the hard cap has not been reached. Idle pane + no menu means
/// the command is finished or never opened one: stop.
pub fn should_extend_watch(payload: &str, pane_idle: bool, elapsed: Duration) -> bool {
    is_model_command(payload) && !pane_idle && elapsed < Duration::from_secs(MODEL_QUEUED_WAIT_SECS)
}

/// How long to wait for the cursor to move after one Up/Down, and for the
/// menu to close after Enter.
const STEP_SETTLE: Duration = Duration::from_millis(1500);
const CLOSE_SETTLE: Duration = Duration::from_secs(5);
const POLL: Duration = Duration::from_millis(250);

/// One option row of a menu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuOption {
    pub number: u32,
    pub label: String,
}

/// A selection menu as read off one pane frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Menu {
    /// The non-empty lines above option 1 (title, body), top to bottom.
    pub context: Vec<String>,
    pub options: Vec<MenuOption>,
    /// The number of the row the `❯` cursor sits on.
    pub selected: u32,
}

impl Menu {
    fn labels(&self) -> Vec<&str> {
        self.options.iter().map(|o| o.label.as_str()).collect()
    }

    /// One-line summary for logs and stderr.
    pub fn summary(&self) -> String {
        let title = self
            .context
            .first()
            .map(String::as_str)
            .unwrap_or("(untitled)");
        let opts: Vec<String> = self
            .options
            .iter()
            .map(|o| {
                let mark = if o.number == self.selected { "*" } else { "" };
                format!("{}{}. {}", mark, o.number, o.label)
            })
            .collect();
        format!("{:?} [{}]", title, opts.join(" | "))
    }
}

/// Strip box-drawing borders (and ASCII `|`) from both ends of a line.
fn strip_border(line: &str) -> &str {
    let is_border = |c: char| ('\u{2500}'..='\u{257f}').contains(&c) || c == '|';
    line.trim()
        .trim_start_matches(is_border)
        .trim_end_matches(is_border)
        .trim()
}

/// Parse one stripped line as a numbered option row.
///
/// Returns `(number, label, has_cursor)`. The cursor is `❯` (or a plain `>`,
/// which some renders use) in front of the number.
fn parse_option_row(stripped: &str) -> Option<(u32, String, bool)> {
    let (cursor, rest) = if let Some(r) = stripped.strip_prefix('\u{276f}') {
        (true, r.trim_start())
    } else if let Some(r) = stripped.strip_prefix('>') {
        (true, r.trim_start())
    } else {
        (false, stripped)
    };
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() || digits.len() > 2 {
        return None;
    }
    let after = &rest[digits.len()..];
    let label = after.strip_prefix(". ")?.trim();
    if label.is_empty() {
        return None;
    }
    Some((digits.parse().ok()?, label.to_string(), cursor))
}

/// Pure function: the live selection menu at the bottom of this frame, if any.
///
/// Recognised only when ALL of these hold, because a false positive here is a
/// keystroke into a live session:
///
/// * a run of numbered rows `1.`, `2.`, … in order (description lines between
///   rows are allowed), at least two of them;
/// * exactly one of those rows carries the `❯` cursor;
/// * no `❯` prompt line BELOW the run. A live menu replaces the input box; a
///   menu with the idle prompt under it is scrollback (or a quote of one in
///   Claude's reply) and is not answerable.
pub fn parse_menu(pane_output: &str) -> Option<Menu> {
    let lines: Vec<&str> = pane_output.lines().collect();
    let start = lines.len().saturating_sub(MENU_TAIL_LINES);
    let tail = &lines[start..];

    // (index of option 1, options, selected-rows) of the run being built.
    let mut run: Option<(usize, Vec<MenuOption>, Vec<u32>)> = None;
    for (i, raw) in tail.iter().enumerate() {
        let stripped = strip_border(raw);
        match parse_option_row(stripped) {
            Some((1, label, cursor)) => {
                run = Some((
                    i,
                    vec![MenuOption { number: 1, label }],
                    if cursor { vec![1] } else { vec![] },
                ));
            }
            Some((n, label, cursor)) => {
                let continues = run
                    .as_ref()
                    .and_then(|(_, opts, _)| opts.last())
                    .map(|last| last.number + 1 == n)
                    .unwrap_or(false);
                if continues {
                    let (_, opts, sel) = run.as_mut().expect("checked above");
                    opts.push(MenuOption { number: n, label });
                    if cursor {
                        sel.push(n);
                    }
                } else {
                    run = None;
                }
            }
            None => {
                // A prompt line (or any other `❯` line) after the run means
                // the run is not the live bottom of the pane.
                if run.is_some() && stripped.contains('\u{276f}') {
                    run = None;
                }
            }
        }
    }

    let (first, options, selected) = run?;
    if options.len() < 2 || selected.len() != 1 {
        return None;
    }
    // Context: the lines above option 1, up to the box's top border or the
    // echoed command line, whichever comes first.
    let mut context: Vec<String> = Vec::new();
    for raw in tail[..first].iter().rev() {
        let stripped = strip_border(raw);
        // A horizontal rule (the box's top edge), not an empty `│   │` row.
        let pure_border = stripped.is_empty() && raw.chars().any(|c| matches!(c, '─' | '━' | '═'));
        if pure_border || stripped.starts_with('\u{276f}') || stripped.starts_with('>') {
            break;
        }
        if !stripped.is_empty() {
            context.push(stripped.to_string());
        }
        if context.len() == 8 {
            break;
        }
    }
    context.reverse();
    Some(Menu {
        context,
        options,
        selected: selected[0],
    })
}

/// Is this payload a `/model` command (the one allowlisted self-inject)?
pub fn is_model_command(payload: &str) -> bool {
    let p = payload.trim_start();
    p == "/model" || p.starts_with("/model ")
}

/// Is this payload a slash command at all (which is when the menu watch runs
/// without an explicit `--answer`)?
pub fn is_slash_payload(payload: &str) -> bool {
    payload.trim_start().starts_with('/')
}

/// Pure function: which option to press on this menu WITHOUT an explicit
/// `--answer`, or `None` to leave it alone and report it.
///
/// The allowlist, deliberately one entry long: the `/model` switch
/// confirmation, when the caller's own payload was `/model`. The answer is
/// the row that applies the switch, found by its label rather than assumed to
/// be `1`.
pub fn auto_answer(payload: &str, frame: &str, menu: &Menu) -> Option<u32> {
    if !is_model_command(payload) || !tmux::model_switch_dialog_visible(frame) {
        return None;
    }
    menu.options
        .iter()
        .find(|o| o.label.to_lowercase().starts_with("yes, switch to"))
        .map(|o| o.number)
}

/// Pure function: the single next key that moves `menu`'s cursor toward
/// `target`, or `Enter` when it is already there. `None` if `target` is not a
/// row of this menu.
pub fn next_key(menu: &Menu, target: u32) -> Option<&'static str> {
    if !menu.options.iter().any(|o| o.number == target) {
        return None;
    }
    Some(match menu.selected.cmp(&target) {
        std::cmp::Ordering::Less => "Down",
        std::cmp::Ordering::Greater => "Up",
        std::cmp::Ordering::Equal => "Enter",
    })
}

/// What happened after the submit, as far as menus go.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MenuOutcome {
    /// No menu appeared in the watch window (or the watch did not run).
    None,
    /// A menu appeared; option `answer` was selected and the menu closed.
    Answered { menu: Menu, answer: u32, auto: bool },
    /// A menu appeared that is not allowlisted and no `--answer` was given.
    /// Left alone.
    Unanswered { menu: Menu },
    /// An answer was attempted (or requested) and did not land.
    AnswerFailed {
        menu: Menu,
        answer: u32,
        auto: bool,
        reason: String,
    },
}

impl MenuOutcome {
    pub fn to_json(&self) -> serde_json::Value {
        let menu_json = |m: &Menu| {
            serde_json::json!({
                "context": m.context,
                "options": m.options.iter().map(|o| serde_json::json!({
                    "number": o.number, "label": o.label
                })).collect::<Vec<_>>(),
                "selected": m.selected,
            })
        };
        match self {
            MenuOutcome::None => serde_json::json!({"status": "none"}),
            MenuOutcome::Answered { menu, answer, auto } => serde_json::json!({
                "status": "answered", "answer": answer, "auto": auto, "menu": menu_json(menu)
            }),
            MenuOutcome::Unanswered { menu } => serde_json::json!({
                "status": "unanswered", "menu": menu_json(menu)
            }),
            MenuOutcome::AnswerFailed {
                menu,
                answer,
                auto,
                reason,
            } => serde_json::json!({
                "status": "answer_failed", "answer": answer, "auto": auto,
                "reason": reason, "menu": menu_json(menu)
            }),
        }
    }
}

/// Options for the post-submit menu watch.
#[derive(Debug, Clone)]
pub struct MenuPolicy {
    /// `--answer N`.
    pub answer: Option<u32>,
    /// Watch window override (`--menu-wait`); `None` = per-payload default.
    pub wait_secs: Option<u64>,
    /// `--no-auto-answer`: report even allowlisted menus.
    pub auto_answer: bool,
}

/// Should the watch run at all for this payload?
pub fn should_watch(payload: &str, policy: &MenuPolicy) -> bool {
    if policy.wait_secs == Some(0) {
        return false;
    }
    policy.answer.is_some() || is_slash_payload(payload)
}

fn wait_window(payload: &str, policy: &MenuPolicy) -> Duration {
    Duration::from_secs(policy.wait_secs.unwrap_or(if is_model_command(payload) {
        MODEL_MENU_WAIT_SECS
    } else {
        DEFAULT_MENU_WAIT_SECS
    }))
}

/// After a submit: watch for a menu, answer it if the caller said to (or it
/// is allowlisted), report it otherwise.
pub async fn settle_menu(pane: &str, payload: &str, policy: &MenuPolicy) -> MenuOutcome {
    if !should_watch(payload, policy) {
        return MenuOutcome::None;
    }

    // Phase 1: does a menu appear?
    let started = Instant::now();
    let deadline = started + wait_window(payload, policy);
    let (frame, menu) = loop {
        if let Some(frame) = tmux::capture_pane(pane).await {
            if let Some(menu) = parse_menu(&frame) {
                break (frame, menu);
            }
            // `/model` that applied straight away: nothing will appear.
            if is_model_command(payload) && tmux::model_switch_applied(&frame) {
                return MenuOutcome::None;
            }
        }
        if Instant::now() >= deadline {
            // A `/model` queued behind a running turn confirms later.
            let idle = match tmux::capture_pane(pane).await {
                Some(f) => tmux::check_lines_for_idle_prompt(&f),
                None => true,
            };
            if !(policy.wait_secs.is_none()
                && should_extend_watch(payload, idle, started.elapsed()))
            {
                return MenuOutcome::None;
            }
        }
        sleep(POLL).await;
    };

    // Phase 2: decide.
    let (target, auto) = match policy.answer {
        Some(n) => (n, false),
        None => match auto_answer(payload, &frame, &menu).filter(|_| policy.auto_answer) {
            Some(n) => (n, true),
            None => return MenuOutcome::Unanswered { menu },
        },
    };
    let fail = |reason: &str| MenuOutcome::AnswerFailed {
        menu: menu.clone(),
        answer: target,
        auto,
        reason: reason.to_string(),
    };
    if next_key(&menu, target).is_none() {
        return fail("the menu has no such option; nothing was pressed");
    }

    // Phase 3: drive the cursor one verified row at a time, then Enter.
    let labels: Vec<String> = menu.labels().iter().map(|s| s.to_string()).collect();
    let max_presses = menu.options.len() + 2;
    let mut current = menu.clone();
    for _ in 0..max_presses {
        let Some(key) = next_key(&current, target) else {
            return fail("the menu changed under the cursor; stopped pressing");
        };
        tmux::send_keys(pane, &[key]).await;
        if key == "Enter" {
            // Phase 4: the menu must go away.
            let close_deadline = Instant::now() + CLOSE_SETTLE;
            loop {
                sleep(POLL).await;
                match tmux::capture_pane(pane).await.map(|f| parse_menu(&f)) {
                    Some(Some(m)) if m.labels() == labels => {}
                    // Gone, or replaced by something else: our menu closed.
                    Some(_) => {
                        return MenuOutcome::Answered {
                            menu,
                            answer: target,
                            auto,
                        }
                    }
                    None => {}
                }
                if Instant::now() >= close_deadline {
                    return fail("Enter was pressed but the menu is still on the pane");
                }
            }
        }
        // Wait for the cursor to move by exactly one row, on the same menu.
        let expected = if key == "Down" {
            current.selected + 1
        } else {
            current.selected - 1
        };
        let step_deadline = Instant::now() + STEP_SETTLE;
        let moved = loop {
            sleep(POLL).await;
            if let Some(m) = tmux::capture_pane(pane).await.and_then(|f| parse_menu(&f)) {
                if m.labels() != labels {
                    return fail("the menu changed while navigating; stopped pressing");
                }
                if m.selected == expected {
                    break Some(m);
                }
            }
            if Instant::now() >= step_deadline {
                break None;
            }
        };
        match moved {
            Some(m) => current = m,
            None => return fail("the cursor did not move as expected; stopped pressing"),
        }
    }
    fail("keystroke budget spent before the menu was answered")
}

#[cfg(test)]
mod tests {
    use super::*;

    const MODEL_SWITCH_PANE: &str = include_str!("../tests/fixtures/model_switch_dialog.txt");

    /// The exact render from the self-inject report: `/model` submitted, the
    /// confirmation up, nothing answering it.
    const SELF_INJECT_PANE: &str = "\
> /model claude-fable-5[1m]

 Switch model?
 Your next response will be slower and use more tokens

 This conversation is cached for the current model. Switching to Fable 5 means the full history gets re-read on your next message.

 \u{276f} 1. Yes, switch to Fable 5
   2. No, go back
";

    #[test]
    fn parses_the_bordered_model_dialog() {
        let m = parse_menu(MODEL_SWITCH_PANE).expect("menu");
        assert_eq!(m.selected, 1);
        assert_eq!(m.options.len(), 2);
        assert_eq!(m.options[0].label, "Yes, switch to Opus 5 (1M context)");
        assert_eq!(m.options[1].label, "No, go back");
        assert_eq!(m.context.first().map(String::as_str), Some("Switch model?"));
    }

    #[test]
    fn parses_the_unbordered_self_inject_render() {
        let m = parse_menu(SELF_INJECT_PANE).expect("menu");
        assert_eq!(m.selected, 1);
        assert_eq!(m.options[0].label, "Yes, switch to Fable 5");
        assert_eq!(m.context.first().map(String::as_str), Some("Switch model?"));
    }

    #[test]
    fn ordinary_output_is_not_a_menu() {
        // A numbered list in a reply: no cursor.
        assert_eq!(
            parse_menu("● Steps:\n  1. build\n  2. test\n\n\u{276f} \n"),
            None
        );
        // Idle prompt.
        assert_eq!(parse_menu("● Done.\n\u{276f} \n  57,129 tokens\n"), None);
        // A single option is not a choice.
        assert_eq!(parse_menu("\u{276f} 1. Only\n"), None);
        // Numbering that does not run 1,2,…
        assert_eq!(parse_menu("\u{276f} 1. a\n  3. c\n"), None);
        assert_eq!(parse_menu(""), None);
    }

    #[test]
    fn a_menu_above_the_idle_prompt_is_scrollback() {
        // The dialog, then the idle prompt under it: answered long ago (or
        // quoted in a reply). Pressing anything now types into the chat.
        let pane = format!(
            "{}\n\u{2713} Set model to Opus\n\n\u{276f} \n",
            MODEL_SWITCH_PANE
        );
        assert_eq!(parse_menu(&pane), None);
        let pane = format!("{}\n\u{276f} some half-typed text\n", SELF_INJECT_PANE);
        assert_eq!(parse_menu(&pane), None);
    }

    #[test]
    fn a_menu_with_two_cursors_is_not_read() {
        let pane = SELF_INJECT_PANE.replace("   2. No", " \u{276f} 2. No");
        assert_eq!(parse_menu(&pane), None);
    }

    #[test]
    fn descriptions_between_rows_and_footer_are_tolerated() {
        let pane = "\
 Which approach?

 \u{276f} 1. Fast
      Skips the cache
   2. Safe
      Rebuilds everything
   3. Type something.

 Enter to select \u{b7} \u{2191}/\u{2193} to navigate \u{b7} Esc to cancel
";
        let m = parse_menu(pane).expect("menu");
        assert_eq!(m.options.len(), 3);
        assert_eq!(m.selected, 1);
        assert_eq!(m.context, vec!["Which approach?".to_string()]);
    }

    #[test]
    fn auto_answer_is_allowlisted_to_the_model_confirmation() {
        let m = parse_menu(SELF_INJECT_PANE).unwrap();
        assert_eq!(
            auto_answer("/model claude-fable-5[1m]", SELF_INJECT_PANE, &m),
            Some(1)
        );
        assert_eq!(auto_answer("  /model opus", SELF_INJECT_PANE, &m), Some(1));

        // Same dialog, but the caller did not ask for a model switch.
        assert_eq!(auto_answer("/clear", SELF_INJECT_PANE, &m), None);
        assert_eq!(auto_answer("/models-report", SELF_INJECT_PANE, &m), None);

        // `/model` that opened some OTHER menu (e.g. the picker): report it.
        let picker = "\
 Select model
 \u{276f} 1. Default (recommended)
   2. Opus
   3. Haiku
";
        let pm = parse_menu(picker).unwrap();
        assert_eq!(auto_answer("/model", picker, &pm), None);

        // A permission prompt: never auto-answered.
        let perm = " Do you want to proceed?\n \u{276f} 1. Yes\n   2. No\n";
        let permm = parse_menu(perm).unwrap();
        assert_eq!(auto_answer("/model opus", perm, &permm), None);
    }

    #[test]
    fn auto_answer_finds_the_confirm_row_by_label_not_position() {
        let swapped = SELF_INJECT_PANE
            .replace("1. Yes, switch to Fable 5", "1. No, go back")
            .replace("2. No, go back", "2. Yes, switch to Fable 5");
        let m = parse_menu(&swapped).unwrap();
        assert_eq!(auto_answer("/model fable", &swapped, &m), Some(2));
    }

    #[test]
    fn navigation_is_one_row_at_a_time_and_never_a_digit() {
        let m = parse_menu(SELF_INJECT_PANE).unwrap();
        assert_eq!(next_key(&m, 1), Some("Enter"));
        assert_eq!(next_key(&m, 2), Some("Down"));
        assert_eq!(next_key(&m, 3), None, "no such row: press nothing");
        assert_eq!(next_key(&m, 0), None);
        let on_two = Menu { selected: 2, ..m };
        assert_eq!(next_key(&on_two, 1), Some("Up"));
    }

    #[test]
    fn watch_runs_for_slash_commands_or_an_explicit_answer() {
        let p = |answer, wait_secs| MenuPolicy {
            answer,
            wait_secs,
            auto_answer: true,
        };
        assert!(should_watch("/model x", &p(None, None)));
        assert!(!should_watch("[CLAUDE-WATCH] alert", &p(None, None)));
        assert!(should_watch("please pick", &p(Some(2), None)));
        assert!(
            !should_watch("/model x", &p(Some(1), Some(0))),
            "--menu-wait 0 disables"
        );
        assert_eq!(
            wait_window("/model x", &p(None, None)).as_secs(),
            MODEL_MENU_WAIT_SECS
        );
        assert_eq!(
            wait_window("/clear", &p(None, None)).as_secs(),
            DEFAULT_MENU_WAIT_SECS
        );
        assert_eq!(wait_window("/clear", &p(None, Some(7))).as_secs(), 7);
    }

    #[test]
    fn queued_model_watch_extends_only_while_busy_and_under_cap() {
        let s = Duration::from_secs;
        assert!(should_extend_watch("/model fable", false, s(11)));
        assert!(!should_extend_watch("/model fable", true, s(11)));
        assert!(!should_extend_watch(
            "/model fable",
            false,
            s(MODEL_QUEUED_WAIT_SECS)
        ));
        assert!(!should_extend_watch("/compact", false, s(11)));
    }

    #[test]
    fn model_command_match_is_exact() {
        assert!(is_model_command("/model"));
        assert!(is_model_command("/model opus"));
        assert!(!is_model_command("/models"));
        assert!(!is_model_command("model opus"));
    }
}
